//! `nix-collect-garbage`, following CppNix 2.35.2's
//! `src/nix/nix-collect-garbage/nix-collect-garbage.cc`.
//!
//! Supported: `--delete-old`/`-d`, `--delete-older-than`, `--dry-run`,
//! `--max-freed` and settings flags, on a local store. Collecting through
//! the daemon is not supported yet.

use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_rs_store::gc::{GcAction, GcOptions, render_size};
use nix_rs_store::{LocalStoreConfig, profiles};

use crate::cli::{
	ArgStream, Failure, apply_common_flag, parse_int_with_unit, print_setting_warnings,
};
use crate::store;

/// How `--delete-old` and `--delete-older-than` prune each profile.
struct Pruning {
	older_than: Option<String>,
	dry_run: bool,
}

fn access(path: &Path, mode: libc::c_int) -> bool {
	let Ok(path) = CString::new(path.as_os_str().as_bytes()) else {
		return false;
	};
	// SAFETY: `path` is a valid NUL-terminated string.
	unsafe { libc::access(path.as_ptr(), mode) == 0 }
}

/// `removeOldGenerations`: prune every profile below `dir`, recognised as
/// a writable symlink whose target mentions `link`.
fn remove_old_generations(dir: &Path, pruning: &Pruning) -> Result<(), Failure> {
	if !access(dir, libc::R_OK) {
		return Ok(());
	}
	let can_write = access(dir, libc::W_OK);
	let entries = fs::read_dir(dir)
		.map_err(|e| Failure::Error(format!("reading directory \"{}\": {e}", dir.display())))?;
	// Generations are deleted at `lvlNotice`.
	let mut log = |message: &str| {
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Notice) {
			eprintln!("{message}");
		}
	};
	for entry in entries {
		let path = entry?.path();
		let Ok(metadata) = fs::symlink_metadata(&path) else {
			continue;
		};
		if metadata.file_type().is_symlink() && can_write {
			let link = match fs::read_link(&path) {
				Ok(link) => link,
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
				Err(e) => return Err(e.into()),
			};
			if link.to_string_lossy().contains("link") {
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
					eprintln!("removing old generations of profile {}", path.display());
				}
				match &pruning.older_than {
					Some(spec) => {
						let time = profiles::parse_older_than_time_spec(spec, now())
							.map_err(Failure::Usage)?;
						profiles::delete_generations_older_than(
							&path,
							time,
							pruning.dry_run,
							&mut log,
						)?;
					}
					None => profiles::delete_old_generations(&path, pruning.dry_run, &mut log)?,
				}
			}
		} else if metadata.is_dir() {
			remove_old_generations(&path, pruning)?;
		}
	}
	Ok(())
}

fn now() -> i64 {
	std::time::SystemTime::now()
		.duration_since(std::time::UNIX_EPOCH)
		.map(|d| d.as_secs() as i64)
		.unwrap_or(0)
}

/// Run `nix-collect-garbage` with the arguments after the program name.
pub fn main(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	let mut remove_old = false;
	let mut pruning = Pruning {
		older_than: None,
		dry_run: false,
	};
	let mut max_freed = None;
	// LegacyArgs applies `--store` without marking the setting as set.
	let mut store = None;
	let mut args = ArgStream::new(args);
	while let Some(arg) = args.next_arg() {
		let value = |args: &mut ArgStream| {
			args.next()
				.ok_or_else(|| Failure::Usage(format!("'{arg}' requires an argument")))
		};
		if let Some(result) = apply_common_flag(&arg, &mut args, &mut settings) {
			result?;
			continue;
		}
		match arg.as_str() {
			"--version" => {
				println!("nix-collect-garbage (Nix) 2.35.2");
				return Ok(ExitCode::SUCCESS);
			}
			"--delete-old" | "-d" => remove_old = true,
			"--delete-older-than" => {
				remove_old = true;
				pruning.older_than = Some(value(&mut args)?);
			}
			"--dry-run" => pruning.dry_run = true,
			"--store" => store = Some(value(&mut args)?),
			"--no-gc-warning" => {}
			"--" => {
				if let Some(arg) = args.next() {
					return Err(Failure::Usage(format!("unexpected argument '{arg}'")));
				}
			}
			"--max-freed" => {
				let size = parse_int_with_unit(&value(&mut args)?).map_err(Failure::Usage)?;
				max_freed = Some(size.max(0) as u64);
			}
			flag if flag.starts_with('-') => match settings.apply_flag(flag, || args.next()) {
				Some(result) => result.map_err(|error| Failure::Usage(error.to_string()))?,
				None => return Err(Failure::Usage(format!("unrecognised flag '{flag}'"))),
			},
			_ => return Err(Failure::Usage(format!("unexpected argument '{arg}'"))),
		}
	}
	print_setting_warnings(&mut settings);
	if max_freed.is_some() && pruning.dry_run {
		return Err(Failure::Usage(
			"options --max-freed and --dry-run cannot be combined".into(),
		));
	}

	if remove_old {
		let state_dir = LocalStoreConfig::system().state_dir;
		let use_xdg = settings.get_bool("use-xdg-base-directories", false);
		let default_profile = profiles::default_profile(&state_dir, use_xdg);
		let dirs: BTreeSet<PathBuf> = [
			profiles::profiles_dir(&state_dir)?,
			state_dir.join("profiles"),
			default_profile
				.parent()
				.unwrap_or(Path::new("/"))
				.to_owned(),
		]
		.into();
		for dir in &dirs {
			remove_old_generations(dir, &pruning)?;
		}
	}

	let uri = store::store_uri(store.as_deref(), &settings);
	let Some((store, _)) = store::open_local(&uri, &settings)? else {
		return Err(Failure::Error(format!(
			"store '{uri}' does not support garbage collection"
		)));
	};
	let mut options = GcOptions::new(if pruning.dry_run {
		GcAction::ReturnDead
	} else {
		GcAction::DeleteDead
	});
	if let Some(max_freed) = max_freed {
		options.max_freed = max_freed;
	}
	options.keep_outputs = settings.get_bool("keep-outputs", false);
	options.keep_derivations = settings.get_bool("keep-derivations", true);
	let result = store.collect_garbage(&options, &mut |message| {
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
			eprintln!("{message}");
		}
	});
	// `printFreed` reports what was freed even when collection fails.
	let results = match &result {
		Ok(results) => results.clone(),
		Err(_) => Default::default(),
	};
	if pruning.dry_run {
		println!("{} store paths would be deleted", results.paths.len());
	} else {
		println!(
			"{} store paths deleted, {} freed",
			results.paths.len(),
			render_size(results.bytes_freed as i64, false)
		);
	}
	result?;
	Ok(ExitCode::SUCCESS)
}
