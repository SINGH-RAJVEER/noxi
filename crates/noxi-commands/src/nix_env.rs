//! `nix-env`, following CppNix 2.35.2's `src/nix/nix-env/nix-env.cc`.
//!
//! Profile transactions, expression and store-path installation sources, upgrades,
//! and installed/available queries are implemented in Rust. Differential evidence
//! and remaining compatibility gaps are recorded in docs/reference-inventory.md.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use noxi_store::gc::replace_symlink;
use noxi_store::{DerivedPath, LocalStoreConfig, profiles};

use crate::cli::{ArgStream, Failure, apply_common_flag, print_setting_warnings};
use crate::instantiate::absolute_path;
use crate::paths::{follow_links_to_store_path, print_missing};
use crate::store;
use crate::user_env::{self, DrvName, Element};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
	Version,
	Install,
	Uninstall,
	Upgrade,
	SetFlag,
	Set,
	Query,
	SwitchProfile,
	SwitchGeneration,
	Rollback,
	ListGenerations,
	DeleteGenerations,
}

/// The state shared by the operations.
struct Globals {
	settings: Settings,
	/// `--store`, which LegacyArgs applies without marking the setting as
	/// set, so a daemon is not sent it.
	store: Option<String>,
	profile: PathBuf,
	dry_run: bool,
	cwd: PathBuf,
	file: Option<String>,
	include: Vec<String>,
	auto_args: Vec<(String, crate::instantiate::AutoArg)>,
	system_filter: String,
	force_name: Option<String>,
	prebuilt_only: bool,
}

fn is_root_user() -> bool {
	// SAFETY: `getuid` has no preconditions.
	unsafe { libc::getuid() == 0 }
}

/// `getNixDefExpr`.
fn nix_def_expr(settings: &Settings) -> PathBuf {
	if settings.get_bool("use-xdg-base-directories", false) {
		let state = std::env::var_os("NIX_STATE_HOME")
			.filter(|dir| !dir.is_empty())
			.map(PathBuf::from)
			.unwrap_or_else(|| {
				std::env::var_os("XDG_STATE_HOME")
					.filter(|dir| !dir.is_empty())
					.map(PathBuf::from)
					.unwrap_or_else(|| profiles::home_dir().join(".local/state"))
					.join("nix")
			});
		state.join("defexpr")
	} else {
		profiles::home_dir().join(".nix-defexpr")
	}
}

/// Create the default expression directory with its channel links, as
/// `nix-env` does on every run when it is missing.
fn ensure_def_expr(settings: &Settings, state_dir: &Path) {
	let dir = nix_def_expr(settings);
	if std::fs::symlink_metadata(&dir).is_ok() {
		return;
	}
	let _ = (|| -> Result<(), Failure> {
		std::fs::create_dir_all(&dir)?;
		replace_symlink(
			&profiles::profiles_dir(state_dir)?.join("channels"),
			&dir.join("channels"),
		)?;
		if !is_root_user() {
			replace_symlink(
				&profiles::root_profiles_dir(state_dir).join("channels"),
				&dir.join("channels_root"),
			)?;
		}
		Ok(())
	})();
}

/// `string2Int<GenerationNumber>`.
fn generation_number(s: &str) -> Option<u64> {
	if s.starts_with('+') {
		None
	} else {
		s.parse().ok()
	}
}

fn no_flags(flags: &[String]) -> Result<(), Failure> {
	match flags.first() {
		Some(flag) => Err(Failure::Usage(format!("unknown flag '{flag}'"))),
		None => Ok(()),
	}
}

/// Run `nix-env` with the arguments after the program name.
pub fn main(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	let state_dir = LocalStoreConfig::system().state_dir;
	ensure_def_expr(&settings, &state_dir);
	let cwd = std::env::current_dir()?;
	let mut file = None;
	let mut include = Vec::new();
	let mut auto_args = Vec::new();
	let mut force_name = None;
	let mut prebuilt_only = false;
	let mut system_filter = "*".to_owned();
	let mut op = None;
	let mut op_flags = Vec::new();
	let mut op_args = Vec::new();
	let mut profile = None;
	let mut dry_run = false;
	let mut store = None;
	let mut args = ArgStream::new(args);
	while let Some(arg) = args.next_arg() {
		let old_op = op;
		let value = |args: &mut ArgStream| {
			args.next()
				.ok_or_else(|| Failure::Usage(format!("'{arg}' requires an argument")))
		};
		if let Some(result) = apply_common_flag(&arg, &mut args, &mut settings) {
			result?;
			continue;
		}
		if arg == "--store" {
			store = Some(value(&mut args)?);
			continue;
		}
		if arg == "--no-gc-warning" {
			continue;
		}
		// CppNix keeps `nix-env`'s own `--system` rather than the setting's.
		if arg.starts_with('-')
			&& arg != "--system"
			&& let Some(result) = settings.apply_flag(&arg, || args.next())
		{
			result.map_err(|error| Failure::Usage(error.to_string()))?;
			continue;
		}
		match arg.as_str() {
			"--help" => {
				return Err(Failure::Error(
					"nix-env --help is not supported by noxi yet".into(),
				));
			}
			"--version" => op = Some(Operation::Version),
			"--install" | "-i" => op = Some(Operation::Install),
			"--force-name" => force_name = Some(value(&mut args)?),
			"-I" | "--include" => include.push(value(&mut args)?),
			"--arg" | "--argstr" => {
				let name = value(&mut args)?;
				let val = value(&mut args)?;
				auto_args.push((
					name,
					if arg == "--arg" {
						crate::instantiate::AutoArg::Expr(val)
					} else {
						crate::instantiate::AutoArg::Str(val)
					},
				));
			}
			"--uninstall" | "-e" => op = Some(Operation::Uninstall),
			"--upgrade" | "-u" => op = Some(Operation::Upgrade),
			"--set-flag" => op = Some(Operation::SetFlag),
			"--set" => op = Some(Operation::Set),
			"--query" | "-q" => op = Some(Operation::Query),
			"--profile" | "-p" => {
				profile = Some(absolute_path(&cwd, Path::new(&value(&mut args)?)))
			}
			"--file" | "-f" => file = Some(value(&mut args)?),
			"--switch-profile" | "-S" => op = Some(Operation::SwitchProfile),
			"--switch-generation" | "-G" => op = Some(Operation::SwitchGeneration),
			"--rollback" => op = Some(Operation::Rollback),
			"--list-generations" => op = Some(Operation::ListGenerations),
			"--delete-generations" => op = Some(Operation::DeleteGenerations),
			"--dry-run" => {
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
					eprintln!("(dry run; not doing anything)");
				}
				dry_run = true;
			}
			"--system-filter" => system_filter = value(&mut args)?,
			"--prebuilt-only" | "-b" => prebuilt_only = true,
			"--" => op_args.extend(args.by_ref()),
			flag if !flag.is_empty() && flag.starts_with('-') => {
				op_flags.push(arg.clone());
				let takes_value = flag == "--from-profile"
					|| (op == Some(Operation::Query) && matches!(flag, "--attr" | "-A"))
					|| (op == Some(Operation::Install) && flag == "--priority");
				if takes_value {
					op_flags.push(value(&mut args)?);
				}
			}
			_ => op_args.push(arg.clone()),
		}
		if old_op.is_some() && old_op != op {
			return Err(Failure::Usage("only one operation may be specified".into()));
		}
	}
	print_setting_warnings(&mut settings);
	let Some(op) = op else {
		return Err(Failure::Usage("no operation specified".into()));
	};
	if op == Operation::Version {
		println!("nix-env (Nix) 2.35.2");
		return Ok(ExitCode::SUCCESS);
	}
	let use_xdg = settings.get_bool("use-xdg-base-directories", false);
	let profile = profile
		.or_else(|| {
			std::env::var_os("NIX_PROFILE")
				.filter(|p| !p.is_empty())
				.map(PathBuf::from)
		})
		.unwrap_or_else(|| profiles::default_profile(&state_dir, use_xdg));
	let globals = Globals {
		settings,
		store,
		profile,
		dry_run,
		cwd,
		file,
		include,
		auto_args,
		system_filter,
		force_name,
		prebuilt_only,
	};
	// Profile changes are reported at `lvlNotice`.
	let mut log = |message: &str| {
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Notice) {
			eprintln!("{message}");
		}
	};
	match op {
		Operation::Version => unreachable!(),
		Operation::Set => op_set(&globals, &op_flags, &op_args)?,
		Operation::SwitchProfile => {
			no_flags(&op_flags)?;
			let [target] = &op_args[..] else {
				return Err(Failure::Usage("exactly one argument expected".into()));
			};
			let target = absolute_path(&globals.cwd, Path::new(target));
			let link = if use_xdg {
				profiles::create_nix_state_dir()?.join("profile")
			} else {
				profiles::home_dir().join(".nix-profile")
			};
			profiles::switch_link(&link, &target)?;
		}
		Operation::SwitchGeneration => {
			no_flags(&op_flags)?;
			let [number] = &op_args[..] else {
				return Err(Failure::Usage("exactly one argument expected".into()));
			};
			let number = generation_number(number)
				.ok_or_else(|| Failure::Usage("expected a generation number".into()))?;
			profiles::switch_generation(&globals.profile, Some(number), dry_run, &mut log)?;
		}
		Operation::Rollback => {
			no_flags(&op_flags)?;
			if !op_args.is_empty() {
				return Err(Failure::Usage("no arguments expected".into()));
			}
			profiles::switch_generation(&globals.profile, None, dry_run, &mut log)?;
		}
		Operation::ListGenerations => {
			no_flags(&op_flags)?;
			if !op_args.is_empty() {
				return Err(Failure::Usage("no arguments expected".into()));
			}
			list_generations(&globals.profile)?;
		}
		Operation::DeleteGenerations => {
			no_flags(&op_flags)?;
			delete_generations(&globals.profile, &op_args, dry_run, &mut log)?;
		}
		Operation::Install => return op_install(&globals, &op_flags, &op_args),
		Operation::Uninstall => return op_uninstall(&globals, &op_flags, &op_args),
		Operation::Query => return op_query(&globals, &op_flags, &op_args),
		Operation::SetFlag => return op_set_flag(&globals, &op_flags, &op_args),
		Operation::Upgrade => return op_upgrade(&globals, &op_flags, &op_args),
	}
	Ok(ExitCode::SUCCESS)
}

fn op_set_flag(globals: &Globals, flags: &[String], args: &[String]) -> Result<ExitCode, Failure> {
	no_flags(flags)?;
	if args.len() < 2 {
		return Err(Failure::Usage(
			"not enough arguments to '--set-flag'".into(),
		));
	}
	let opened = open_store(globals)?;
	loop {
		let token = user_env::lock_token(&globals.profile);
		let mut elements =
			match user_env::query_installed(&globals.settings, &globals.cwd, &globals.profile)? {
				Ok(v) => v,
				Err(code) => return Ok(code),
			};
		let selectors = args[2..].to_vec();
		let mut hits = vec![false; selectors.len()];
		for element in &mut elements {
			for (i, selector) in selectors.iter().enumerate() {
				if DrvName::new(selector).matches(&DrvName::new(&element.name)) {
					hits[i] = true;
					if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
						eprintln!("setting flag on '{}'", element.name);
					}
					element.set_meta(&args[0], noxi_eval::Value::from(args[1].clone()));
					break;
				}
			}
		}
		for (selector, hit) in selectors.iter().zip(hits) {
			if !hit {
				return Err(Failure::Error(format!(
					"selector '{selector}' matches no derivations"
				)));
			}
		}
		match user_env::create_user_env(
			opened.store.as_ref(),
			&globals.settings,
			globals.store.as_deref(),
			&globals.cwd,
			&elements,
			&globals.profile,
			globals.settings.get_bool("keep-env-derivations", false),
			&token,
		)? {
			Ok(true) => return Ok(ExitCode::SUCCESS),
			Ok(false) => continue,
			Err(code) => return Ok(code),
		}
	}
}

/// `opSet`: make the single selected path the profile's new generation.
fn op_set(globals: &Globals, flags: &[String], args: &[String]) -> Result<(), Failure> {
	let mut iter = flags.iter();
	while let Some(flag) = iter.next() {
		match flag.as_str() {
			"--from-expression" | "-E" | "--attr" | "-A" => {}
			"--from-profile" => {
				iter.next();
			}
			_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
		}
	}
	let opened = open_store(globals)?;
	let selected = match source_elements(globals, opened.store.as_ref(), flags, args, true)? {
		Ok(v) => v,
		Err(_) => {
			return Err(Failure::Error(
				"evaluating the selected derivation failed".into(),
			));
		}
	};
	let [element] = &selected[..] else {
		return Err(Failure::Error(
			"--set requires exactly one derivation".into(),
		));
	};
	let path = element.drv_path.as_ref().unwrap_or(&element.out_path);
	let uri = store::store_uri(globals.store.as_deref(), &globals.settings);
	let Some(opened) = store::open(&uri, &globals.settings)? else {
		return Err(Failure::Error(
			"--set is not supported for this Nix store".into(),
		));
	};
	let store = opened.store.as_ref();
	let (derived, out_path) = if path.name().ends_with(".drv") {
		let outputs = store.query_derivation_outputs(path)?;
		let out = outputs.get("out").cloned().ok_or_else(|| {
			Failure::Error(format!(
				"derivation '{}' does not have an output named 'out'",
				path.to_absolute_path()
			))
		})?;
		(
			DerivedPath::Built {
				drv: path.clone(),
				outputs: BTreeSet::new(),
			},
			out,
		)
	} else {
		(DerivedPath::Opaque(path.clone()), path.clone())
	};
	let derived = [derived];
	print_missing(store, &store.query_missing(&derived)?)?;
	if globals.dry_run {
		return Ok(());
	}
	store.build_paths(&derived)?;
	let generation = profiles::create_generation(store, &globals.profile, &out_path)?;
	profiles::switch_link(&globals.profile, &generation)?;
	Ok(())
}

/// `opListGenerations`: each generation's number and local creation time.
fn list_generations(profile: &Path) -> Result<(), Failure> {
	let _lock = profiles::lock_profile(profile)?;
	let (generations, current) = profiles::find_generations(profile)?;
	for generation in generations {
		// SAFETY: `tm` is plain data that `localtime_r` fills in.
		let mut tm: libc::tm = unsafe { std::mem::zeroed() };
		let time = generation.creation_time as libc::time_t;
		// SAFETY: both pointers are valid for the call.
		if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
			return Err(Failure::Error("cannot convert time".into()));
		}
		println!(
			"{:>4}   {:>4}-{:02}-{:02} {:02}:{:02}:{:02}   {}",
			generation.number,
			tm.tm_year + 1900,
			tm.tm_mon + 1,
			tm.tm_mday,
			tm.tm_hour,
			tm.tm_min,
			tm.tm_sec,
			if Some(generation.number) == current {
				"(current)"
			} else {
				""
			}
		);
	}
	Ok(())
}

/// `opDeleteGenerations`: `old`, `Nd`, `+N`, or generation numbers.
fn delete_generations(
	profile: &Path,
	args: &[String],
	dry_run: bool,
	log: &mut dyn FnMut(&str),
) -> Result<(), Failure> {
	match args {
		[arg] if arg == "old" => profiles::delete_old_generations(profile, dry_run, log)?,
		[arg] if arg.contains('d') => {
			let now = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.map(|d| d.as_secs() as i64)
				.unwrap_or(0);
			let time = profiles::parse_older_than_time_spec(arg, now).map_err(Failure::Usage)?;
			profiles::delete_generations_older_than(profile, time, dry_run, log)?;
		}
		[arg] if arg.contains('+') => {
			if arg.len() < 2 {
				return Err(Failure::Error(format!(
					"invalid number of generations '{arg}'"
				)));
			}
			let max = generation_number(&arg[1..]).ok_or_else(|| {
				Failure::Error(format!("invalid number of generations to keep '{arg}'"))
			})?;
			profiles::delete_generations_greater_than(profile, max, dry_run, log)?;
		}
		_ => {
			let numbers = args
				.iter()
				.map(|arg| {
					generation_number(arg)
						.ok_or_else(|| Failure::Usage(format!("invalid generation number '{arg}'")))
				})
				.collect::<Result<BTreeSet<u64>, Failure>>()?;
			profiles::delete_generations(profile, &numbers, dry_run, log)?;
		}
	}
	Ok(())
}

/// Open the store the operations on user environments use.
fn open_store(globals: &Globals) -> Result<store::OpenStore, Failure> {
	let uri = store::store_uri(globals.store.as_deref(), &globals.settings);
	store::open(&uri, &globals.settings)?
		.ok_or_else(|| Failure::Error(format!("store '{uri}' cannot hold user environments")))
}

fn source_root(globals: &Globals) -> Result<String, Failure> {
	let path = globals
		.file
		.as_ref()
		.map(PathBuf::from)
		.unwrap_or_else(|| nix_def_expr(&globals.settings));
	if path.to_str() == Some("-") {
		return Ok(std::io::read_to_string(std::io::stdin())?);
	}
	if crate::instantiate::is_lookup_path(&path.to_string_lossy()) {
		return Ok(format!("import {}", path.display()));
	}
	let path = absolute_path(&globals.cwd, &path);
	if path.is_dir() && !path.join("default.nix").exists() {
		fn expressions(
			dir: &Path,
			seen: &mut BTreeSet<String>,
			entries: &mut Vec<String>,
		) -> Result<(), Failure> {
			let mut paths = std::fs::read_dir(dir)?
				.map(|e| e.map(|e| e.path()))
				.collect::<Result<Vec<_>, _>>()?;
			paths.sort();
			for path in paths {
				let name = path.file_name().unwrap().to_string_lossy();
				if name == "manifest.nix" || !path.exists() {
					continue;
				}
				if path.is_file() && name.ends_with(".nix")
					|| path.is_dir() && path.join("default.nix").exists()
				{
					let name = name.strip_suffix(".nix").unwrap_or(&name).to_owned();
					if !seen.insert(name.clone()) {
						eprintln!(
							"warning: name collision in input Nix expressions, skipping '{}'",
							path.display()
						);
						continue;
					}
					entries.push(format!(
						"{} = import {};",
						crate::instantiate::nix_string_literal(&name),
						crate::instantiate::nix_string_literal(&path.to_string_lossy())
					));
				} else if path.is_dir() {
					expressions(&path, seen, entries)?;
				}
			}
			Ok(())
		}
		let mut entries = Vec::new();
		expressions(&path, &mut BTreeSet::new(), &mut entries)?;
		return Ok(format!("{{ {} }}", entries.join(" ")));
	}
	Ok(format!(
		"import {}",
		crate::instantiate::nix_string_literal(&path.to_string_lossy())
	))
}

fn select_elements(
	globals: &Globals,
	all: Vec<Element>,
	args: &[String],
	newest: bool,
) -> Result<Vec<Element>, Failure> {
	let mut result = Vec::new();
	let selectors = if args.is_empty() {
		vec!["*".to_owned()]
	} else {
		args.to_vec()
	};
	for selector in selectors {
		let wanted = DrvName::new(&selector);
		let mut matches: Vec<_> = all
			.iter()
			.filter(|e| wanted.matches(&DrvName::new(&e.name)))
			.cloned()
			.collect();
		if matches.is_empty() && selector != "*" {
			let suggestions: BTreeSet<_> = all
				.iter()
				.map(|e| DrvName::new(&e.name).name)
				.filter(|n| n.starts_with(&wanted.name))
				.take(3)
				.collect();
			let suffix = if suggestions.is_empty() {
				String::new()
			} else {
				format!(
					", maybe you meant:\n{}",
					suggestions.into_iter().collect::<Vec<_>>().join("\n")
				)
			};
			return Err(Failure::Error(format!(
				"selector '{selector}' matches no derivations{suffix}"
			)));
		}
		if newest {
			let mut best = std::collections::BTreeMap::<String, Element>::new();
			for e in matches {
				let name = DrvName::new(&e.name).name;
				let replace = best.get(&name).is_none_or(|old| {
					let system = globals.settings.get("system").unwrap_or("x86_64-linux");
					(e.system == system)
						.cmp(&(old.system == system))
						.then_with(|| priority(old).cmp(&priority(&e)))
						.then_with(|| {
							compare_versions(
								&DrvName::new(&e.name).version,
								&DrvName::new(&old.name).version,
							)
						})
						.is_gt()
				});
				if replace {
					best.insert(name, e);
				}
			}
			matches = best.into_values().collect();
		}
		for e in matches {
			if !result
				.iter()
				.any(|old: &Element| old.out_path == e.out_path && old.attr_path == e.attr_path)
			{
				result.push(e);
			}
		}
	}
	Ok(result)
}

fn priority(e: &Element) -> i64 {
	e.meta("priority")
		.and_then(|v| match v {
			noxi_eval::Value::Integer(i) => Some(i),
			noxi_eval::Value::String(s) => String::from_utf8_lossy(s.as_bytes()).parse().ok(),
			_ => None,
		})
		.unwrap_or(0)
}

fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
	fn parts(s: &str) -> Vec<&str> {
		let mut rest = s;
		let mut result = Vec::new();
		while !rest.is_empty() {
			rest = rest.trim_start_matches(['.', '-']);
			if rest.is_empty() {
				break;
			}
			let digit = rest.as_bytes()[0].is_ascii_digit();
			let end = rest
				.bytes()
				.position(|c| c == b'.' || c == b'-' || c.is_ascii_digit() != digit)
				.unwrap_or(rest.len());
			result.push(&rest[..end]);
			rest = &rest[end..];
		}
		result
	}
	let a = parts(a);
	let b = parts(b);
	for i in 0..a.len().max(b.len()) {
		let x = a.get(i).copied().unwrap_or("");
		let y = b.get(i).copied().unwrap_or("");
		let numeric = |s: &str| !s.is_empty() && s.as_bytes()[0].is_ascii_digit();
		let cmp = if x == y {
			std::cmp::Ordering::Equal
		} else if x == "pre" {
			std::cmp::Ordering::Less
		} else if y == "pre" {
			std::cmp::Ordering::Greater
		} else if numeric(x) && numeric(y) {
			let x = x.trim_start_matches('0');
			let y = y.trim_start_matches('0');
			x.len().cmp(&y.len()).then_with(|| x.cmp(y))
		} else {
			numeric(x).cmp(&numeric(y)).then_with(|| x.cmp(y))
		};
		if !cmp.is_eq() {
			return cmp;
		}
	}
	std::cmp::Ordering::Equal
}

fn auto_apply(globals: &Globals, expression: &str) -> String {
	let args = globals
		.auto_args
		.iter()
		.map(|(name, value)| {
			format!(
				"{} = {};",
				crate::instantiate::nix_string_literal(name),
				match value {
					crate::instantiate::AutoArg::Expr(e) => format!("({e})"),
					crate::instantiate::AutoArg::Str(s) =>
						crate::instantiate::nix_string_literal(s),
				}
			)
		})
		.collect::<Vec<_>>()
		.join(" ");
	format!(
		"(let v = ({expression}); in if builtins.isFunction v then v (builtins.intersectAttrs (builtins.functionArgs v) {{ {args} }}) else v)"
	)
}

fn source_elements(
	globals: &Globals,
	store: &dyn noxi_store::Store,
	flags: &[String],
	args: &[String],
	newest: bool,
) -> Result<Result<Vec<Element>, ExitCode>, Failure> {
	if let Some(i) = flags.iter().position(|s| s == "--from-profile") {
		let profile = flags
			.get(i + 1)
			.ok_or_else(|| Failure::Usage("'--from-profile' requires an argument".into()))?;
		let all =
			match user_env::query_installed(&globals.settings, &globals.cwd, Path::new(profile))? {
				Ok(v) => v,
				Err(c) => return Ok(Err(c)),
			};
		return Ok(Ok(select_elements(globals, all, args, newest)?));
	}
	let expression = flags
		.iter()
		.any(|f| matches!(f.as_str(), "--from-expression" | "-E"));
	let attr = flags.iter().any(|f| matches!(f.as_str(), "--attr" | "-A"));
	if !expression && !attr && args.first().is_some_and(|s| s.contains('/')) {
		return Ok(Ok(args
			.iter()
			.map(|a| Element::from_store_path(store, follow_links_to_store_path(&globals.cwd, a)?))
			.collect::<Result<_, Failure>>()?));
	}
	let root = source_root(globals)?;
	let roots = if expression {
		args.iter().map(|a| format!("({a}) ({root})")).collect()
	} else if attr {
		args.iter()
			.map(|a| {
				let mut selected = auto_apply(globals, &root);
				for name in crate::instantiate::parse_attr_path(a).map_err(Failure::Usage)? {
					selected = auto_apply(
						globals,
						&format!(
							"({selected}).{}",
							crate::instantiate::nix_string_literal(&name)
						),
					);
				}
				Ok(selected)
			})
			.collect::<Result<Vec<_>, Failure>>()?
	} else {
		vec![root]
	};
	let options = crate::instantiate::Options {
		settings: globals.settings.clone(),
		store: globals.store.clone(),
		include: globals.include.clone(),
		auto_args: globals.auto_args.clone(),
		read_write_mode: true,
		..Default::default()
	};
	let mut all = Vec::new();
	for root in roots {
		match user_env::query_expression(&options, &globals.cwd, &root)? {
			Ok(v) => all.extend(v),
			Err(c) => return Ok(Err(c)),
		}
	}
	all.retain(|e| globals.system_filter == "*" || e.system == globals.system_filter);
	if globals.prebuilt_only {
		let mut prebuilt = Vec::new();
		for e in all {
			let missing = store.query_missing(&[DerivedPath::Opaque(e.out_path.clone())])?;
			if missing.unknown.is_empty() {
				prebuilt.push(e);
			}
		}
		all = prebuilt;
	}
	Ok(Ok(if expression || attr {
		all
	} else {
		select_elements(globals, all, args, newest)?
	}))
}

/// `printMissing` for elements: what installing them builds or fetches.
fn print_missing_elements(
	store: &dyn noxi_store::Store,
	elements: &[Element],
) -> Result<(), Failure> {
	let targets: Vec<DerivedPath> = elements
		.iter()
		.map(|e| match &e.drv_path {
			Some(drv) => DerivedPath::Built {
				drv: drv.clone(),
				outputs: BTreeSet::new(),
			},
			None => DerivedPath::Opaque(e.out_path.clone()),
		})
		.collect();
	print_missing(store, &store.query_missing(&targets)?)
}

/// `opInstall` and `installDerivations`.
fn op_install(globals: &Globals, flags: &[String], args: &[String]) -> Result<ExitCode, Failure> {
	let mut preserve_installed = false;
	let mut remove_all = false;
	let mut priority = None;
	let source_flags = flags;
	let mut flags = flags.iter();
	while let Some(flag) = flags.next() {
		match flag.as_str() {
			"--from-expression" | "-E" | "--from-profile" | "--attr" | "-A" => {
				if flag == "--from-profile" {
					flags.next();
				}
			}
			"--preserve-installed" | "-P" => preserve_installed = true,
			"--remove-all" | "-r" => remove_all = true,
			"--priority" => {
				let value = flags
					.next()
					.ok_or_else(|| Failure::Usage("'--priority' requires an argument".into()))?;
				priority = Some(i64::from(value.parse::<i32>().map_err(|_| {
					Failure::Usage("'--priority' requires an integer argument".into())
				})?));
			}
			_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
		}
	}
	let opened = open_store(globals)?;
	let store = opened.store.as_ref();
	let mut new = match source_elements(globals, store, source_flags, args, true)? {
		Ok(v) => v,
		Err(c) => return Ok(c),
	};
	if let Some(name) = &globals.force_name {
		for e in &mut new {
			e.name = name.clone();
		}
	}
	if let Some(priority) = priority {
		for element in &mut new {
			element.set_meta("priority", noxi_eval::Value::Integer(priority));
		}
	}
	let new_names: BTreeSet<String> = new.iter().map(|e| DrvName::new(&e.name).name).collect();
	let keep_derivations = globals.settings.get_bool("keep-env-derivations", false);
	loop {
		let token = user_env::lock_token(&globals.profile);
		let mut all = new.clone();
		if !remove_all {
			let installed =
				match user_env::query_installed(&globals.settings, &globals.cwd, &globals.profile)?
				{
					Ok(installed) => installed,
					Err(code) => return Ok(code),
				};
			for element in installed {
				let name = DrvName::new(&element.name).name;
				if !preserve_installed
					&& new_names.contains(&name)
					&& !element.meta_bool("keep", false)
				{
					if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
						eprintln!("replacing old '{}'", element.name);
					}
				} else {
					all.push(element);
				}
			}
			for element in &new {
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
					eprintln!("installing '{}'", element.name);
				}
			}
		}
		print_missing_elements(store, &new)?;
		if globals.dry_run {
			return Ok(ExitCode::SUCCESS);
		}
		match user_env::create_user_env(
			store,
			&globals.settings,
			globals.store.as_deref(),
			&globals.cwd,
			&all,
			&globals.profile,
			keep_derivations,
			&token,
		)? {
			Ok(true) => return Ok(ExitCode::SUCCESS),
			Ok(false) => continue,
			Err(code) => return Ok(code),
		}
	}
}

fn op_upgrade(globals: &Globals, flags: &[String], args: &[String]) -> Result<ExitCode, Failure> {
	let mut mode = "--lt";
	let mut iter = flags.iter();
	while let Some(flag) = iter.next() {
		match flag.as_str() {
			"--lt" | "--leq" | "--eq" | "--always" => mode = flag,
			"--attr" | "-A" | "--from-expression" | "-E" => {}
			"--from-profile" => {
				iter.next();
			}
			_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
		}
	}
	let opened = open_store(globals)?;
	let available = match source_elements(globals, opened.store.as_ref(), flags, args, false)? {
		Ok(v) => v,
		Err(c) => return Ok(c),
	};
	loop {
		let token = user_env::lock_token(&globals.profile);
		let mut installed =
			match user_env::query_installed(&globals.settings, &globals.cwd, &globals.profile)? {
				Ok(v) => v,
				Err(c) => return Ok(c),
			};
		for old in &mut installed {
			if old.meta_bool("keep", false) {
				continue;
			}
			let name = DrvName::new(&old.name);
			let best = available
				.iter()
				.filter(|new| {
					let n = DrvName::new(&new.name);
					let cmp = compare_versions(&name.version, &n.version);
					name.name == n.name
						&& priority(new) <= priority(old)
						&& match mode {
							"--always" => true,
							"--eq" => cmp.is_eq(),
							"--leq" => !cmp.is_gt(),
							_ => cmp.is_lt(),
						}
				})
				.max_by(|a, b| {
					priority(b).cmp(&priority(a)).then_with(|| {
						compare_versions(
							&DrvName::new(&a.name).version,
							&DrvName::new(&b.name).version,
						)
					})
				});
			if let Some(new) = best
				&& old.out_path != new.out_path
			{
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
					eprintln!(
						"{} '{}' to '{}'",
						if compare_versions(&name.version, &DrvName::new(&new.name).version).is_gt()
						{
							"downgrading"
						} else {
							"upgrading"
						},
						old.name,
						new.name
					);
				}
				*old = new.clone();
			}
		}
		print_missing_elements(opened.store.as_ref(), &installed)?;
		if globals.dry_run {
			return Ok(ExitCode::SUCCESS);
		}
		match user_env::create_user_env(
			opened.store.as_ref(),
			&globals.settings,
			globals.store.as_deref(),
			&globals.cwd,
			&installed,
			&globals.profile,
			globals.settings.get_bool("keep-env-derivations", false),
			&token,
		)? {
			Ok(true) => return Ok(ExitCode::SUCCESS),
			Ok(false) => {}
			Err(c) => return Ok(c),
		}
	}
}

/// `opUninstall` and `uninstallDerivations`.
fn op_uninstall(globals: &Globals, flags: &[String], args: &[String]) -> Result<ExitCode, Failure> {
	no_flags(flags)?;
	let opened = open_store(globals)?;
	let store = opened.store.as_ref();
	let keep_derivations = globals.settings.get_bool("keep-env-derivations", false);
	loop {
		let token = user_env::lock_token(&globals.profile);
		let mut elements =
			match user_env::query_installed(&globals.settings, &globals.cwd, &globals.profile)? {
				Ok(installed) => installed,
				Err(code) => return Ok(code),
			};
		for selector in args {
			let removed: Vec<bool> = if selector.contains('/') {
				let path = follow_links_to_store_path(&globals.cwd, selector)?;
				elements.iter().map(|e| e.out_path == path).collect()
			} else {
				let wanted = DrvName::new(selector);
				elements
					.iter()
					.map(|e| wanted.matches(&DrvName::new(&e.name)))
					.collect()
			};
			if !removed.contains(&true) {
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
					eprintln!("warning: selector '{selector}' matched no installed derivations");
				}
			}
			// libstdc++ partitions bidirectional ranges by swapping from both ends.
			let mut removed = removed;
			let mut first = 0;
			let mut last = elements.len();
			loop {
				while first < last && !removed[first] {
					first += 1;
				}
				if first == last {
					break;
				}
				loop {
					last -= 1;
					if first == last || !removed[last] {
						break;
					}
				}
				if first == last {
					break;
				}
				elements.swap(first, last);
				removed.swap(first, last);
				first += 1;
			}
			for element in &elements[first..] {
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
					eprintln!("uninstalling '{}'", element.name);
				}
			}
			elements.truncate(first);
		}
		if globals.dry_run {
			return Ok(ExitCode::SUCCESS);
		}
		match user_env::create_user_env(
			store,
			&globals.settings,
			globals.store.as_deref(),
			&globals.cwd,
			&elements,
			&globals.profile,
			keep_derivations,
			&token,
		)? {
			Ok(true) => return Ok(ExitCode::SUCCESS),
			Ok(false) => continue,
			Err(code) => return Ok(code),
		}
	}
}

/// `opQuery` of the installed elements, printed as `printTable` does.
fn op_query(globals: &Globals, flags: &[String], args: &[String]) -> Result<ExitCode, Failure> {
	let mut print_name = true;
	let mut print_out_path = false;
	let mut print_drv_path = false;
	let mut print_system = false;
	let mut available = false;
	let mut print_attr = false;
	let mut print_description = false;
	let mut print_meta = false;
	let mut print_status = false;
	let mut compare = false;
	let mut json_output = false;
	let mut xml_output = false;
	let mut attr = None;
	let mut iter = flags.iter();
	while let Some(flag) = iter.next() {
		match flag.as_str() {
			"--no-name" => print_name = false,
			"--out-path" => print_out_path = true,
			"--drv-path" => print_drv_path = true,
			"--system" => print_system = true,
			"--installed" => available = false,
			"--available" | "-a" => available = true,
			"--attr-path" | "-P" => print_attr = true,
			"--description" => print_description = true,
			"--meta" => print_meta = true,
			"--status" | "-s" => print_status = true,
			"--compare-versions" | "-c" => compare = true,
			"--json" => json_output = true,
			"--xml" => xml_output = true,
			"--attr" | "-A" => attr = iter.next().cloned(),
			_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
		}
	}
	if print_attr && !available {
		return Err(Failure::Usage(
			"--attr-path(-P) only works with --available".into(),
		));
	}
	let opened = open_store(globals)?;
	let installed =
		match user_env::query_installed(&globals.settings, &globals.cwd, &globals.profile)? {
			Ok(v) => v,
			Err(c) => return Ok(c),
		};
	let avail = if available || compare {
		let mut source = source_root(globals)?;
		if let Some(attr) = &attr {
			for name in crate::instantiate::parse_attr_path(&attr).map_err(Failure::Usage)? {
				source = format!(
					"({source}).{}",
					crate::instantiate::nix_string_literal(&name)
				);
			}
		}
		let options = crate::instantiate::Options {
			settings: globals.settings.clone(),
			store: globals.store.clone(),
			include: globals.include.clone(),
			auto_args: globals.auto_args.clone(),
			..Default::default()
		};
		match user_env::query_available(
			&options,
			&globals.cwd,
			&source,
			print_out_path || print_status || globals.prebuilt_only,
			print_drv_path,
			print_meta || print_description,
		)? {
			Ok(mut v) => {
				if let Some(prefix) = &attr {
					for e in &mut v {
						e.attr_path = if e.attr_path.is_empty() {
							prefix.clone()
						} else {
							format!("{prefix}.{}", e.attr_path)
						};
					}
				}
				v
			}
			Err(c) => return Ok(c),
		}
	} else {
		Vec::new()
	};
	let other = if available {
		installed.clone()
	} else {
		avail.clone()
	};
	let installed_paths: BTreeSet<_> = installed.iter().map(|e| e.out_path.clone()).collect();
	let mut installed = if available { avail } else { installed };
	if available {
		installed.retain(|e| globals.system_filter == "*" || e.system == globals.system_filter);
	}
	// `filterBySelector`: every selector must match something.
	let mut elements = if args.is_empty() {
		installed
	} else {
		let mut selected = Vec::new();
		let mut taken = vec![false; installed.len()];
		for selector in args {
			let wanted = DrvName::new(selector);
			let mut matched = false;
			for (i, element) in installed.iter().enumerate() {
				if wanted.matches(&DrvName::new(&element.name)) {
					matched = true;
					if !taken[i] {
						taken[i] = true;
						selected.push(element.clone());
					}
				}
			}
			if !matched {
				return Err(Failure::Error(format!(
					"selector '{selector}' matches no derivations"
				)));
			}
		}
		selected
	};
	// `cmpElemByName`: case-insensitive, stable.
	elements.sort_by(|a, b| {
		a.name
			.bytes()
			.map(|c| c.to_ascii_uppercase())
			.cmp(b.name.bytes().map(|c| c.to_ascii_uppercase()))
	});
	if json_output {
		let mut result = serde_json::Map::new();
		for e in &elements {
			let name = DrvName::new(&e.name);
			let outputs: serde_json::Map<_, _> = e
				.outputs
				.iter()
				.map(|(n, p)| {
					(
						n.clone(),
						if print_out_path {
							serde_json::json!(p.to_absolute_path())
						} else {
							serde_json::Value::Null
						},
					)
				})
				.collect();
			let mut value = serde_json::json!({"name":e.name,"pname":name.name,"version":name.version,"system":e.system,"outputName":e.output_name,"outputs":outputs});
			if print_drv_path && let Some(p) = &e.drv_path {
				value["drvPath"] = serde_json::json!(p.to_absolute_path());
			}
			if print_meta {
				value["meta"] = meta_json(&e.meta);
			}
			result.insert(e.attr_path.clone(), value);
		}
		println!(
			"{}",
			serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?
		);
		return Ok(ExitCode::SUCCESS);
	}
	if globals.prebuilt_only {
		let mut prebuilt = Vec::new();
		for element in elements {
			let missing = opened
				.store
				.query_missing(&[DerivedPath::Opaque(element.out_path.clone())])?;
			if !missing.unknown.contains(&element.out_path) {
				prebuilt.push(element);
			}
		}
		elements = prebuilt;
	}
	if xml_output {
		println!("<?xml version='1.0' encoding='utf-8'?>\n<items>");
	}
	let mut table: Vec<Vec<String>> = Vec::new();
	for element in &elements {
		let mut columns = Vec::new();
		let mut attrs = std::collections::BTreeMap::<String, String>::new();
		let mut attribute = |k: &str, v: String| {
			attrs.insert(k.into(), v);
		};
		if print_status {
			let valid = opened.store.is_valid_path(&element.out_path)?;
			let installed = installed_paths.contains(&element.out_path);
			let subs = opened
				.store
				.query_missing(&[DerivedPath::Opaque(element.out_path.clone())])?
				.will_substitute
				.contains(&element.out_path);
			if xml_output {
				attribute("installed", u8::from(installed).to_string());
				attribute("valid", u8::from(valid).to_string());
				attribute("substitutable", u8::from(subs).to_string());
			} else {
				columns.push(format!(
					"{}{}{}",
					if installed { "I" } else { "-" },
					if valid { "P" } else { "-" },
					if subs { "S" } else { "-" }
				));
			}
		}
		let name = DrvName::new(&element.name);
		if xml_output {
			attribute("attrPath", element.attr_path.clone());
			attribute("name", element.name.clone());
			attribute("pname", name.name.clone());
			attribute("version", name.version.clone());
			attribute("outputName", element.output_name.clone());
			if !element.system.is_empty() {
				attribute("system", element.system.clone());
			}
			if print_drv_path && let Some(p) = &element.drv_path {
				attribute("drvPath", p.to_absolute_path());
			}
		} else if print_attr {
			columns.push(element.attr_path.clone());
		}
		if print_name {
			columns.push(element.name.clone());
		}
		if compare {
			let best = other
				.iter()
				.filter(|e| DrvName::new(&e.name).name == name.name)
				.map(|e| DrvName::new(&e.name).version)
				.max_by(|a, b| compare_versions(a, b));
			let (ch, version) = match best {
				None => ('-', String::new()),
				Some(v) => (
					match compare_versions(&name.version, &v) {
						std::cmp::Ordering::Less => '<',
						std::cmp::Ordering::Equal => '=',
						std::cmp::Ordering::Greater => '>',
					},
					v,
				),
			};
			if xml_output {
				if ch != '-' {
					attribute("versionDiff", ch.to_string());
					attribute("maxComparedVersion", version);
				}
			} else {
				columns.push(format!("{ch} {version}"));
			}
		}

		if print_system {
			columns.push(element.system.clone());
		}
		if print_drv_path {
			columns.push(
				element
					.drv_path
					.as_ref()
					.map_or("-".to_owned(), StorePath::to_absolute_path),
			);
		}
		if print_out_path {
			let mut text = String::new();
			for (name, path) in &element.outputs {
				if !text.is_empty() {
					text.push(';');
				}
				if name != "out" {
					text.push_str(name);
					text.push('=');
				}
				text.push_str(&path.to_absolute_path());
			}
			columns.push(text);
		}
		if print_description {
			let description = element
				.meta("description")
				.and_then(|v| {
					v.to_str()
						.ok()
						.map(|s| String::from_utf8_lossy(s.as_bytes()).into_owned())
				})
				.unwrap_or_default();
			if xml_output {
				if !description.is_empty() {
					attrs.insert("description".into(), description);
				}
			} else {
				columns.push(description);
			}
		}
		if xml_output {
			println!("  <item{}>", xml_attrs(&attrs));
			for (name, path) in &element.outputs {
				let mut attrs = std::collections::BTreeMap::from([("name".into(), name.clone())]);
				if print_out_path {
					attrs.insert("path".into(), path.to_absolute_path());
				}
				println!("    <output{} />", xml_attrs(&attrs));
			}
			if print_meta {
				if let serde_json::Value::Object(meta) = meta_json(&element.meta) {
					for (name, value) in meta {
						let mut attrs = std::collections::BTreeMap::from([("name".into(), name)]);
						match value {
							serde_json::Value::Array(values) => {
								attrs.insert("type".into(), "strings".into());
								println!("    <meta{}>", xml_attrs(&attrs));
								for v in values {
									if let Some(v) = v.as_str() {
										println!(
											"      <string{} />",
											xml_attrs(&std::collections::BTreeMap::from([(
												"value".into(),
												v.into()
											)]))
										);
									}
								}
								println!("    </meta>");
							}
							serde_json::Value::Object(values) => {
								attrs.insert("type".into(), "strings".into());
								println!("    <meta{}>", xml_attrs(&attrs));
								for (k, v) in values {
									if let Some(v) = v.as_str() {
										println!(
											"      <string{} />",
											xml_attrs(&std::collections::BTreeMap::from([
												("type".into(), k),
												("value".into(), v.into())
											]))
										);
									}
								}
								println!("    </meta>");
							}
							value => {
								let kind = if value.is_string() {
									"string"
								} else if value.is_boolean() {
									"bool"
								} else if value.is_i64() {
									"int"
								} else {
									"float"
								};
								attrs.insert("type".into(), kind.into());
								attrs.insert(
									"value".into(),
									value
										.as_str()
										.map(str::to_owned)
										.unwrap_or_else(|| value.to_string()),
								);
								println!("    <meta{} />", xml_attrs(&attrs));
							}
						}
					}
				}
			}
			println!("  </item>");
		} else {
			table.push(columns);
		}
	}
	if xml_output {
		println!("</items>");
	} else {
		print_table(&table);
	}
	Ok(ExitCode::SUCCESS)
}

fn meta_json(value: &noxi_eval::Value) -> serde_json::Value {
	use noxi_eval::Value;
	match value {
		Value::Thunk(t) => meta_json(&t.value()),
		Value::Bool(b) => serde_json::json!(b),
		Value::Integer(i) => serde_json::json!(i),
		Value::Float(f) => serde_json::json!(f),
		Value::String(s) => serde_json::json!(String::from_utf8_lossy(s.as_bytes())),
		Value::List(l) => serde_json::Value::Array(l.iter().map(meta_json).collect()),
		Value::Attrs(a) => serde_json::Value::Object(
			a.iter_sorted()
				.map(|(n, v)| {
					(
						String::from_utf8_lossy(n.as_bytes()).into_owned(),
						meta_json(v),
					)
				})
				.collect(),
		),
		_ => serde_json::Value::Null,
	}
}
fn xml_attrs(attrs: &std::collections::BTreeMap<String, String>) -> String {
	attrs
		.iter()
		.map(|(k, v)| {
			format!(
				" {k}=\"{}\"",
				v.replace('&', "&amp;")
					.replace('<', "&lt;")
					.replace('>', "&gt;")
					.replace('\"', "&quot;")
					.replace('\n', "&#xA;")
					.replace('\r', "&#xD;")
					.replace('\t', "&#x9;")
			)
		})
		.collect()
}

/// `printTable`: columns padded to their widest cell plus two spaces.
fn print_table(table: &[Vec<String>]) {
	let columns = table.first().map_or(0, Vec::len);
	let mut widths = vec![0; columns];
	for row in table {
		for (width, cell) in widths.iter_mut().zip(row) {
			*width = (*width).max(cell.len());
		}
	}
	for row in table {
		let mut line = String::new();
		for (i, cell) in row.iter().enumerate() {
			let cell = cell.replace('\n', " ");
			line.push_str(&cell);
			if i + 1 < columns {
				line.push_str(&" ".repeat(widths[i] - cell.len() + 2));
			}
		}
		println!("{line}");
	}
}
