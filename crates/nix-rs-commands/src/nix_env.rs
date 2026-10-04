//! `nix-env`, following CppNix 2.35.2's `src/nix/nix-env/nix-env.cc`.
//!
//! Supported: `--install` and `--set` with store path arguments,
//! `--uninstall`, `--query` of the installed elements (with `--no-name`,
//! `--out-path`, `--drv-path` and `--system`), `--rollback`,
//! `--switch-generation`/`-G`, `--list-generations`,
//! `--delete-generations`, `--switch-profile`/`-S` and `--version`, with
//! `--profile`/`-p`, `--dry-run` and settings flags. Selecting
//! derivations from Nix expressions, `--upgrade`, `--set-flag` and the
//! other query options fail explicitly.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use nix_rs_store::gc::replace_symlink;
use nix_rs_store::{DerivedPath, LocalStoreConfig, profiles};

use crate::cli::{Failure, print_setting_warnings};
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
	let mut op = None;
	let mut op_flags = Vec::new();
	let mut op_args = Vec::new();
	let mut profile = None;
	let mut dry_run = false;
	let mut store = None;
	let mut args = args.into_iter();
	while let Some(arg) = args.next() {
		let old_op = op;
		let value = |args: &mut std::vec::IntoIter<String>| {
			args.next()
				.ok_or_else(|| Failure::Usage(format!("'{arg}' requires an argument")))
		};
		if arg == "--store" {
			store = Some(value(&mut args)?);
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
					"nix-env --help is not supported by nix-rs yet".into(),
				));
			}
			"--version" => op = Some(Operation::Version),
			"--install" | "-i" => op = Some(Operation::Install),
			"--force-name" => drop(value(&mut args)?),
			"--uninstall" | "-e" => op = Some(Operation::Uninstall),
			"--upgrade" | "-u" => op = Some(Operation::Upgrade),
			"--set-flag" => op = Some(Operation::SetFlag),
			"--set" => op = Some(Operation::Set),
			"--query" | "-q" => op = Some(Operation::Query),
			"--profile" | "-p" => {
				profile = Some(absolute_path(&cwd, Path::new(&value(&mut args)?)))
			}
			"--file" | "-f" => {
				value(&mut args)?;
				return Err(Failure::Error(
					"nix-env --file is not supported by nix-rs yet".into(),
				));
			}
			"--switch-profile" | "-S" => op = Some(Operation::SwitchProfile),
			"--switch-generation" | "-G" => op = Some(Operation::SwitchGeneration),
			"--rollback" => op = Some(Operation::Rollback),
			"--list-generations" => op = Some(Operation::ListGenerations),
			"--delete-generations" => op = Some(Operation::DeleteGenerations),
			"--dry-run" => {
				eprintln!("(dry run; not doing anything)");
				dry_run = true;
			}
			"--system-filter" => drop(value(&mut args)?),
			"--prebuilt-only" | "-b" => {}
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
	};
	let mut log = |message: &str| eprintln!("{message}");
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
		Operation::Upgrade | Operation::SetFlag => {
			return Err(Failure::Error(
				"nix-env --upgrade and --set-flag are not supported by nix-rs yet".into(),
			));
		}
	}
	Ok(ExitCode::SUCCESS)
}

/// `opSet`: make the single selected path the profile's new generation.
fn op_set(globals: &Globals, flags: &[String], args: &[String]) -> Result<(), Failure> {
	for flag in flags {
		match flag.as_str() {
			"--from-expression" | "-E" | "--from-profile" | "--attr" | "-A" => {
				return Err(Failure::Error(format!(
					"nix-env --set {flag} is not supported by nix-rs yet"
				)));
			}
			_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
		}
	}
	// Without a path, CppNix selects from the default Nix expression.
	if !args.first().is_some_and(|arg| arg.contains('/')) {
		if args.is_empty() {
			return Err(Failure::Error(
				"--set requires exactly one derivation".into(),
			));
		}
		return Err(Failure::Error(
			"nix-env selecting derivations from Nix expressions is not supported by nix-rs yet"
				.into(),
		));
	}
	let paths = args
		.iter()
		.map(|arg| follow_links_to_store_path(&globals.cwd, arg))
		.collect::<Result<Vec<StorePath>, Failure>>()?;
	let [path] = &paths[..] else {
		return Err(Failure::Error(
			"--set requires exactly one derivation".into(),
		));
	};
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

/// `--from-expression`, `--from-profile` and `--attr` select from Nix
/// expressions or another profile, which nix-rs does not do yet.
fn unsupported_source(op: &str, flag: &str) -> Failure {
	Failure::Error(format!(
		"nix-env {op} {flag} is not supported by nix-rs yet"
	))
}

/// `queryInstSources` for store path arguments.
fn store_path_elements(
	globals: &Globals,
	store: &dyn nix_rs_store::Store,
	args: &[String],
) -> Result<Vec<Element>, Failure> {
	if !args.first().is_some_and(|arg| arg.contains('/')) {
		return Err(Failure::Error(
			"nix-env selecting derivations from Nix expressions is not supported by nix-rs yet"
				.into(),
		));
	}
	args.iter()
		.map(|arg| {
			let path = follow_links_to_store_path(&globals.cwd, arg)?;
			Element::from_store_path(store, path)
		})
		.collect()
}

/// `printMissing` for elements: what installing them builds or fetches.
fn print_missing_elements(
	store: &dyn nix_rs_store::Store,
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
	let mut flags = flags.iter();
	while let Some(flag) = flags.next() {
		match flag.as_str() {
			"--from-expression" | "-E" | "--from-profile" | "--attr" | "-A" => {
				return Err(unsupported_source("--install", flag));
			}
			"--preserve-installed" | "-P" => preserve_installed = true,
			"--remove-all" | "-r" => remove_all = true,
			"--priority" => priority = flags.next().and_then(|p| p.parse::<i64>().ok()),
			_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
		}
	}
	let opened = open_store(globals)?;
	let store = opened.store.as_ref();
	let mut new = store_path_elements(globals, store, args)?;
	if let Some(priority) = priority {
		for element in &mut new {
			element.set_meta("priority", nix_rs_eval::Value::Integer(priority));
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
					eprintln!("replacing old '{}'", element.name);
				} else {
					all.push(element);
				}
			}
			for element in &new {
				eprintln!("installing '{}'", element.name);
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
				eprintln!("warning: selector '{selector}' matched no installed derivations");
			}
			// `std::partition` keeps the order of neither part; the kept
			// elements are only written back, so their order is CppNix's
			// only when nothing is removed before them.
			let mut kept = Vec::new();
			for (element, remove) in elements.into_iter().zip(removed) {
				if remove {
					eprintln!("uninstalling '{}'", element.name);
				} else {
					kept.push(element);
				}
			}
			elements = kept;
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
	for flag in flags {
		match flag.as_str() {
			"--no-name" => print_name = false,
			"--out-path" => print_out_path = true,
			"--drv-path" => print_drv_path = true,
			"--system" => print_system = true,
			"--installed" => {}
			"--status" | "-s" | "--description" | "--compare-versions" | "-c" | "--meta"
			| "--available" | "-a" | "--xml" | "--json" | "--attr-path" | "-P" | "--attr"
			| "-A" => {
				return Err(Failure::Error(format!(
					"nix-env --query {flag} is not supported by nix-rs yet"
				)));
			}
			_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
		}
	}
	let installed =
		match user_env::query_installed(&globals.settings, &globals.cwd, &globals.profile)? {
			Ok(installed) => installed,
			Err(code) => return Ok(code),
		};
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
	let mut table: Vec<Vec<String>> = Vec::new();
	for element in &elements {
		let mut columns = Vec::new();
		if print_name {
			columns.push(element.name.clone());
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
		table.push(columns);
	}
	print_table(&table);
	Ok(ExitCode::SUCCESS)
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
