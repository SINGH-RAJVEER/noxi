//! Exec a command after realizing its expression-selected packages.
//! Diverted stores use a separate single-threaded mount-namespace helper,
//! matching CppNix's `execProgramInStore` without changing host mounts.
use super::{Parsed, expression, unsupported};
use crate::cli::Failure;
use crate::instantiate;
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs;
use std::os::unix::process::CommandExt;
use std::process::{Command, ExitCode};

pub(super) fn shell(mut parsed: Parsed) -> Result<ExitCode, Failure> {
	let command = parsed
		.flags
		.remove("command")
		.and_then(|mut v| v.pop())
		.unwrap_or_else(|| vec![std::env::var("SHELL").unwrap_or_else(|_| "bash".into())]);
	if command.is_empty() {
		return Err(Failure::Usage("--command requires a command".into()));
	}
	let targets = match expression::targets(&parsed)? {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	if parsed.settings.get_bool("print-missing", true) {
		let missing = targets.store.query_missing(&targets.paths)?;
		crate::paths::print_missing(targets.store.as_ref(), &missing)?;
	}
	targets.store.build_paths(&targets.paths)?;
	let mut bins = Vec::new();
	let mut seen = BTreeSet::new();
	let mut pending = Vec::new();
	for path in &targets.paths {
		pending.extend(expression::outputs(targets.store.as_ref(), path)?.into_values());
	}
	let mut index = 0;
	while index < pending.len() {
		let path = pending[index].clone();
		index += 1;
		if !seen.insert(path.clone()) {
			continue;
		}
		bins.push(format!("{}/bin", path.to_absolute_path()));
		let propagated = targets
			.store
			.real_path(&path)
			.join("nix-support/propagated-user-env-packages");
		match fs::read_to_string(propagated) {
			Ok(text) => {
				for path in text.split_whitespace() {
					pending.push(
						nix_compat::store_path::StorePath::from_absolute_path(path.as_bytes())
							.map_err(|e| e.to_string())?,
					);
				}
			}
			// `pathExists` also treats a file output, ENOTDIR, as absent.
			Err(error)
				if matches!(
					error.kind(),
					std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
				) => {}
			Err(error) => return Err(error.into()),
		}
	}
	let mut child = environment(&parsed, &command[0])?;
	let old_path = match child.get_envs().find(|(key, _)| *key == "PATH") {
		Some((_, value)) => value
			.map(|v| v.to_string_lossy().into_owned())
			.unwrap_or_default(),
		None if parsed.has("ignore-env") || parsed.has("ignore-environment") => String::new(),
		None => std::env::var("PATH").unwrap_or_default(),
	};
	let path = bins
		.into_iter()
		.chain((!old_path.is_empty()).then_some(old_path))
		.collect::<Vec<_>>()
		.join(":");
	child.env("PATH", path).args(&command[1..]);
	exec_store(&parsed, child)
}

pub(super) fn run(mut parsed: Parsed) -> Result<ExitCode, Failure> {
	// The first positional argument is an attribute selection, the rest are argv.
	// An empty selection selects the expression root before child arguments.
	let argv = if parsed.operands.is_empty() {
		Vec::new()
	} else {
		parsed.operands.split_off(1)
	};
	let mut selection = expression::options(&parsed, true)?;
	selection.strict = true;
	selection.read_write_mode = true;
	let attr = parsed.operands.first().map(String::as_str).unwrap_or("");
	let expected = if matches!(
		instantiate::parse_attr_path(attr)?
			.first()
			.map(String::as_str),
		Some("apps" | "defaultApp")
	) {
		"app"
	} else {
		"derivation"
	};
	let type_error = instantiate::nix_string_literal(&format!(
		"attribute '{attr}' should have type '{expected}'"
	));
	// Resolve the reference's app/derivation selection in the evaluator so
	// meta.mainProgram and pname can remain lazy Nix values.
	for input in &mut selection.inputs {
		*input = format!(
			"let v = ({input}); in if v.type != \"{expected}\" then throw {type_error} else if v.type == \"app\" then v.program else let n = v.meta.mainProgram or (v.pname or (builtins.parseDrvName v.name).name); in v.outPath + \"/bin/\" + n"
		);
	}
	let evaluated = match instantiate::evaluate(&selection, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	let Some(nix_rs_eval::Value::String(program)) = evaluated.results.first() else {
		return Err(Failure::Error("application program is not a string".into()));
	};
	let store = evaluated
		.store
		.ok_or_else(|| unsupported("run without a writable store"))?;
	let paths = program
		.iter_context()
		.flat_map(|context| context.iter())
		.map(|element| {
			use nix_compat::store_path::StorePath;
			use nix_rs_eval::NixContextElement;
			use nix_rs_store::DerivedPath;
			Ok(match element {
				NixContextElement::Plain(path) => DerivedPath::Opaque(
					StorePath::from_absolute_path(path.as_bytes()).map_err(|e| e.to_string())?,
				),
				NixContextElement::Derivation(path) => DerivedPath::Built {
					drv: StorePath::from_absolute_path(path.as_bytes())
						.map_err(|e| e.to_string())?,
					outputs: BTreeSet::new(),
				},
				NixContextElement::Single { derivation, name } => DerivedPath::Built {
					drv: StorePath::from_absolute_path(derivation.as_bytes())
						.map_err(|e| e.to_string())?,
					outputs: BTreeSet::from([name.clone()]),
				},
			})
		})
		.collect::<Result<Vec<_>, Failure>>()?;
	// `AppInstallable::resolve` builds the context as `Installable::build` does.
	if parsed.settings.get_bool("print-missing", true) {
		crate::paths::print_missing(store.as_ref(), &store.query_missing(&paths)?)?;
	}
	store.build_paths(&paths)?;
	let program = std::str::from_utf8(program.as_bytes())
		.map_err(|e| e.to_string())?
		.to_owned();
	if !program.starts_with('/') {
		return Err(unsupported("run with a relative application program"));
	}
	let mut child = environment(&parsed, &program)?;
	child.args(argv);
	exec_store(&parsed, child)
}

pub(super) fn environment(parsed: &Parsed, program: &str) -> Result<Command, Failure> {
	let ignore = parsed.has("ignore-env") || parsed.has("ignore-environment");
	if ignore && (parsed.has("unset-env-var") || parsed.has("unset")) {
		return Err(Failure::Usage(
			"--unset-env-var does not make sense with --ignore-env".into(),
		));
	}
	if !ignore && (parsed.has("keep-env-var") || parsed.has("keep")) {
		return Err(Failure::Usage(
			"--keep-env-var does not make sense without --ignore-env".into(),
		));
	}
	let mut child = Command::new(program);
	if parsed.has("ignore-env") || parsed.has("ignore-environment") {
		child.env_clear();
	}
	for keep in ["keep-env-var", "keep"]
		.into_iter()
		.filter_map(|name| parsed.flags.get(name))
	{
		for values in keep {
			if let Ok(value) = std::env::var(&values[0]) {
				child.env(&values[0], value);
			}
		}
	}
	if let Some(set) = parsed.flags.get("set-env-var") {
		for values in set {
			child.env(&values[0], &values[1]);
		}
	}
	for unset in ["unset-env-var", "unset"]
		.into_iter()
		.filter_map(|name| parsed.flags.get(name))
	{
		for values in unset {
			child.env_remove(&values[0]);
		}
	}
	Ok(child)
}

pub(super) fn exec_store(parsed: &Parsed, mut child: Command) -> Result<ExitCode, Failure> {
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let Some((_, real)) = crate::store::open_local(&uri, &parsed.settings)? else {
		return Err(unsupported("execution with non-local stores"));
	};
	if let Some(real) = real {
		let program = child.get_program().to_owned();
		let args: Vec<_> = child.get_args().map(|arg| arg.to_owned()).collect();
		let env: Vec<_> = child
			.get_envs()
			.map(|(key, value)| (key.to_owned(), value.map(|v| v.to_owned())))
			.collect();
		let mut helper = Command::new(std::env::current_exe()?);
		helper
			.arg("__nixrs_exec_in_store")
			.arg(real)
			.arg(program)
			.args(args);
		if parsed.has("ignore-env") || parsed.has("ignore-environment") {
			helper.env_clear();
		}
		for (key, value) in env {
			if let Some(value) = value {
				helper.env(key, value);
			} else {
				helper.env_remove(key);
			}
		}
		child = helper;
	}
	let program = child.get_program().to_string_lossy().into_owned();
	Err(Failure::Error(format!(
		"unable to execute '{program}': {}",
		child.exec()
	)))
}

/// Called before the normal CLI creates its evaluation thread.
pub fn helper(args: Vec<String>) -> ExitCode {
	let result = helper_inner(args);
	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			crate::cli::print_error(&error);
			ExitCode::FAILURE
		}
	}
}
fn helper_inner(args: Vec<String>) -> Result<(), String> {
	if args.len() < 2 {
		return Err("store execution helper requires a store directory and program".into());
	}
	#[cfg(target_os = "linux")]
	{
		let store = CString::new(args[0].as_bytes()).map_err(|e| e.to_string())?;
		let uid = unsafe { libc::getuid() };
		let gid = unsafe { libc::getgid() };
		// SAFETY: unshare and mount use constant flags and NUL-terminated strings.
		// This helper is single-threaded and never modifies mounts outside its namespace.
		unsafe {
			if libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) != 0
				&& libc::unshare(libc::CLONE_NEWNS) != 0
			{
				return Err(format!(
					"setting up a private mount namespace: {}",
					std::io::Error::last_os_error()
				));
			}
			if libc::mount(
				std::ptr::null(),
				c"/".as_ptr(),
				std::ptr::null(),
				libc::MS_REC | libc::MS_PRIVATE,
				std::ptr::null(),
			) != 0
			{
				return Err(format!(
					"making mounts private: {}",
					std::io::Error::last_os_error()
				));
			}
			let options = CString::new(format!("lowerdir=/nix/store:{}", args[0]))
				.map_err(|e| e.to_string())?;
			if libc::mount(
				c"overlay".as_ptr(),
				c"/nix/store".as_ptr(),
				c"overlay".as_ptr(),
				libc::MS_MGC_VAL,
				options.as_ptr().cast(),
			) != 0 && libc::mount(
				store.as_ptr(),
				c"/nix/store".as_ptr(),
				std::ptr::null(),
				libc::MS_BIND,
				std::ptr::null(),
			) != 0
			{
				return Err(format!(
					"mounting '{}' on '/nix/store': {}",
					args[0],
					std::io::Error::last_os_error()
				));
			}
		}
		fs::write("/proc/self/setgroups", "deny").map_err(|e| e.to_string())?;
		fs::write("/proc/self/uid_map", format!("{uid} {uid} 1")).map_err(|e| e.to_string())?;
		fs::write("/proc/self/gid_map", format!("{gid} {gid} 1")).map_err(|e| e.to_string())?;
		let error = Command::new(&args[1]).args(&args[2..]).exec();
		Err(format!("unable to execute '{}': {error}", args[1]))
	}
	#[cfg(not(target_os = "linux"))]
	{
		Err("execution in diverted stores is not supported on this platform".into())
	}
}
