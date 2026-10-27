//! Local `nix-store` queries, realization, verification and maintenance.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use noxi_build::{local::LocalBuilder, settings::BuildSettings};
use noxi_store::{
	LocalStore, LocalStoreConfig,
	binary_cache::BinaryCache,
	gc::{GcAction, GcOptions},
};

#[derive(Debug)]
pub struct Options {
	pub root: PathBuf,
	pub operation: String,
	pub query: String,
	pub paths: Vec<String>,
	pub check_contents: bool,
	pub settings: Settings,
}

pub fn parse_args(
	args: impl IntoIterator<Item = String>,
	mut settings: Settings,
) -> Result<Options, String> {
	let mut root = None;
	let mut operation = String::new();
	let mut query = String::new();
	let mut paths = Vec::new();
	let mut check_contents = false;
	let mut args = crate::cli::ArgStream::new(args);
	while let Some(arg) = args.next_arg() {
		if let Some(result) = crate::cli::apply_common_flag(&arg, &mut args, &mut settings) {
			result.map_err(|failure| match failure {
				crate::cli::Failure::Usage(message)
				| crate::cli::Failure::Error(message)
				| crate::cli::Failure::Exit(_, message) => message,
			})?;
			continue;
		}
		match arg.as_str() {
			"--store" => {
				let uri = args.next().ok_or("--store requires an argument")?;
				root = Some(PathBuf::from(
					uri.strip_prefix("local?root=")
						.filter(|p| {
							!p.is_empty()
								&& !p.contains(['&', '?', '%'])
								&& Path::new(p).is_absolute()
						})
						.ok_or("nix-store requires --store local?root=ABSOLUTE_ROOT")?,
				));
			}
			"--query" | "-q" => operation = "query".into(),
			"--realise" | "-r" => operation = "realise".into(),
			"--gc" => operation = "gc".into(),
			"--delete" => operation = "delete".into(),
			"--dump" => operation = "dump".into(),
			"--verify" => operation = "verify".into(),
			"--optimise" => operation = "optimise".into(),
			"--check-contents" => check_contents = true,
			"--hash" | "--size" | "--references" | "--referrers" | "--deriver" | "--outputs"
			| "--requisites" | "-R" | "--print-live" | "--print-dead" | "--print-roots" => query = arg,
			"--" => paths.extend(args.by_ref()),
			flag if flag.starts_with('-') => settings
				.apply_flag(flag, || args.next())
				.ok_or_else(|| format!("unrecognised flag '{flag}'"))?
				.map_err(|error| error.to_string())?,
			_ => paths.push(arg),
		}
	}
	if operation.is_empty() {
		return Err("no operation specified".into());
	}
	Ok(Options {
		root: root.ok_or("nix-store requires --store local?root=ABSOLUTE_ROOT")?,
		operation,
		query,
		paths,
		check_contents,
		settings,
	})
}

pub fn run(options: &Options, stdout: &mut dyn Write, stderr: &mut dyn Write) -> ExitCode {
	match execute(options, stdout, stderr) {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			let _ = writeln!(stderr, "error: {error}");
			ExitCode::FAILURE
		}
	}
}

fn execute(
	options: &Options,
	stdout: &mut dyn Write,
	stderr: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
	let mut config = LocalStoreConfig::diverted(&options.root)?;
	config.apply_config(&options.settings);
	let store = LocalStore::open(config)?;
	let paths = options
		.paths
		.iter()
		.map(|path| StorePath::from_absolute_path(path.as_bytes()).map(|path| path.to_owned()))
		.collect::<Result<Vec<_>, _>>()?;
	let print = |paths: BTreeSet<StorePath>, stdout: &mut dyn Write| -> std::io::Result<()> {
		for path in paths {
			writeln!(stdout, "{}", path.to_absolute_path())?;
		}
		Ok(())
	};
	match options.operation.as_str() {
		"query" => {
			for path in paths {
				let info = store
					.query_path_info(&path)?
					.ok_or_else(|| noxi_store::Error::InvalidPath(path.to_absolute_path()))?;
				match options.query.as_str() {
					"--hash" => writeln!(stdout, "{}", info.nar_hash_nix32())?,
					"--size" => writeln!(stdout, "{}", info.nar_size)?,
					"--references" => print(info.references, stdout)?,
					"--referrers" => print(store.query_referrers(&path)?, stdout)?,
					"--deriver" => writeln!(
						stdout,
						"{}",
						info.deriver
							.map(|path| path.to_absolute_path())
							.unwrap_or_else(|| "unknown-deriver".into())
					)?,
					"--outputs" | "" => print(
						store
							.query_derivation_outputs(&path)?
							.into_values()
							.collect(),
						stdout,
					)?,
					"--requisites" | "-R" => {
						let mut closure = BTreeSet::new();
						let mut todo = vec![path];
						while let Some(path) = todo.pop() {
							if closure.insert(path.clone()) {
								todo.extend(
									store
										.query_path_info(&path)?
										.ok_or_else(|| {
											noxi_store::Error::InvalidPath(path.to_absolute_path())
										})?
										.references,
								);
							}
						}
						print(closure, stdout)?;
					}
					_ => return Err("unsupported query".into()),
				}
			}
		}
		"realise" => {
			let mut settings = BuildSettings::default();
			settings.apply_config(&options.settings);
			let caches = if settings.use_substitutes {
				options
					.settings
					.get_list("substituters", &["https://cache.nixos.org/"])
					.into_iter()
					.map(BinaryCache::open)
					.collect::<noxi_store::Result<Vec<_>>>()?
			} else {
				Vec::new()
			};
			let builder = LocalBuilder::new(&store, &settings).with_substituters(&caches);
			for path in paths {
				store.add_temp_root(&path)?;
				if path.to_absolute_path().ends_with(".drv") {
					let mut log_error = None;
					let outputs = builder.realise(&path, &mut |data| {
						if let Err(error) = stderr.write_all(data) {
							log_error = Some(error);
						}
					})?;
					if let Some(error) = log_error {
						return Err(error.into());
					}
					print(outputs.into_values().collect(), stdout)?;
				} else {
					if !store.is_valid_path(&path)? {
						store.substitute(&caches, &path)?;
					}
					print(BTreeSet::from([path]), stdout)?;
				}
			}
		}
		"dump" => {
			if paths.len() != 1 {
				return Err("--dump requires one path".into());
			}
			store.nar_from_path(&paths[0], stdout)?;
		}
		"optimise" => {
			let stats = store.optimise()?;
			writeln!(
				stderr,
				"{} freed by hard-linking {} files",
				noxi_store::gc::render_size(stats.bytes_freed as i64, false),
				stats.files_linked
			)?;
		}
		"verify" => {
			let mut messages = Vec::new();
			let errors = store.verify_store(options.check_contents, &mut |_, message| {
				messages.push(message.to_owned())
			})?;
			for message in messages {
				writeln!(stderr, "{message}")?;
			}
			if errors {
				return Err("store verification failed".into());
			}
		}
		"gc" | "delete" => {
			if options.query == "--print-roots" {
				for (path, roots) in store.find_roots(false, &mut |_| {})? {
					for root in roots {
						writeln!(stdout, "{root} -> {}", path.to_absolute_path())?;
					}
				}
			} else {
				let action = if options.operation == "delete" {
					GcAction::DeleteSpecific
				} else {
					match options.query.as_str() {
						"--print-live" => GcAction::ReturnLive,
						"--print-dead" => GcAction::ReturnDead,
						"" => GcAction::DeleteDead,
						_ => return Err("unsupported GC query".into()),
					}
				};
				let mut gc = GcOptions::new(action);
				gc.keep_outputs = options.settings.get_bool("keep-outputs", false);
				gc.keep_derivations = options.settings.get_bool("keep-derivations", true);
				if options.operation == "delete" {
					gc.paths = Some(paths.into_iter().collect());
				}
				let result = store.collect_garbage(&gc, &mut |message| {
					if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
						let _ = writeln!(stderr, "{message}");
					}
				})?;
				if matches!(action, GcAction::ReturnLive | GcAction::ReturnDead) {
					for path in result.paths {
						writeln!(stdout, "{path}")?;
					}
				} else {
					writeln!(
						stdout,
						"{} store paths deleted, {} freed",
						result.paths.len(),
						noxi_store::gc::render_size(result.bytes_freed as i64, false)
					)?;
				}
			}
		}
		_ => return Err("unsupported store operation".into()),
	}
	Ok(())
}
