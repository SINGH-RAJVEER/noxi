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
use std::path::Path;
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
	targets
		.store
		.build_paths_with_results(&targets.paths, noxi_store::BuildMode::Normal)?;
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
		let bin = resolve_symlinks(
			targets.store.as_ref(),
			&Path::new(&path.to_absolute_path()).join("bin"),
		)?;
		if !bin.starts_with(nix_compat::store_path::store_dir_with_slash()) {
			return Err(Failure::Error(format!(
				"path '{bin}' is not in the Nix store"
			)));
		}
		bins.push(bin);
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

/// `SourceAccessor::resolveSymlinks` on the store's view of the file system:
/// the path `path` names once every symlink in it is followed. Components
/// that do not exist are kept.
fn resolve_symlinks(store: &dyn noxi_store::Store, path: &Path) -> Result<String, Failure> {
	use std::path::{Component, PathBuf};
	let physical =
		|path: &Path| match nix_compat::store_path::StorePath::from_absolute_path_full(path) {
			Ok((object, rest)) if rest.as_os_str().is_empty() => store.real_path(&object),
			Ok((object, rest)) => store.real_path(&object).join(rest),
			Err(_) => path.to_owned(),
		};
	let mut resolved = PathBuf::from("/");
	let mut pending: Vec<std::ffi::OsString> = Vec::new();
	let push = |pending: &mut Vec<std::ffi::OsString>, path: &Path| {
		pending.extend(
			path.components()
				.rev()
				.filter(|c| matches!(c, Component::Normal(_) | Component::ParentDir))
				.map(|c| c.as_os_str().to_owned()),
		);
	};
	push(&mut pending, path);
	let mut links = 0;
	while let Some(component) = pending.pop() {
		if component == ".." {
			resolved.pop();
			continue;
		}
		let next = resolved.join(&component);
		match fs::read_link(physical(&next)) {
			Ok(target) => {
				links += 1;
				if links >= 1024 {
					return Err(Failure::Error(format!(
						"infinite symlink recursion in path '{}'",
						path.display()
					)));
				}
				if target.is_absolute() {
					resolved = PathBuf::from("/");
				}
				push(&mut pending, &target);
			}
			Err(_) => resolved = next,
		}
	}
	Ok(resolved.to_string_lossy().into_owned())
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
	let flake = !parsed.has("expr") && !parsed.has("file");
	let expected = if !flake
		&& matches!(
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
		if flake {
			*input = format!(
				"let v = ({input}); in if v.type == \"app\" then v.program else if v.type == \"derivation\" then let n = v.meta.mainProgram or (v.pname or (builtins.parseDrvName v.name).name); in v.outPath + \"/bin/\" + n else throw {type_error}"
			);
			continue;
		}
		*input = format!(
			"let v = ({input}); in if v.type != \"{expected}\" then throw {type_error} else if v.type == \"app\" then v.program else let n = v.meta.mainProgram or (v.pname or (builtins.parseDrvName v.name).name); in v.outPath + \"/bin/\" + n"
		);
	}
	let evaluated = match instantiate::evaluate(&selection, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	let Some(noxi_eval::Value::String(program)) = evaluated.results.first() else {
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
			use noxi_eval::NixContextElement;
			use noxi_store::DerivedPath;
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
	store.build_paths_with_results(&paths, noxi_store::BuildMode::Normal)?;
	let mut program = std::str::from_utf8(program.as_bytes())
		.map_err(|e| e.to_string())?
		.to_owned();
	// Content-addressed outputs are placeholders until they are built.
	for path in &paths {
		if let noxi_store::DerivedPath::Built { drv, .. } = path {
			for (name, output) in expression::outputs(store.as_ref(), path)? {
				let placeholder =
					nix_compat::store_path::downstream_placeholder(drv, name.as_bytes());
				program = program.replace(&placeholder, &output.to_absolute_path());
			}
		}
	}
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
			.arg("__noxi_exec_in_store")
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
	// `run.cc` stops the logger before handing the terminal over.
	nix_compat::log::logger::stop();
	let program = child.get_program().to_string_lossy().into_owned();
	Err(Failure::Error(format!(
		"unable to execute '{program}': {}",
		child.exec()
	)))
}

/// `chrootHelper`'s path for a missing logical store directory: make a
/// temporary root holding `real_store` at `store_dir`, bind the host's
/// top-level directories into it and copy its top-level symlinks, then
/// chroot there and return to the current directory. Must run in a private
/// mount namespace.
#[cfg(target_os = "linux")]
fn chroot_with_store(real_store: &str, store_dir: &str) -> Result<(), String> {
	use std::os::unix::ffi::OsStrExt;
	use std::os::unix::fs::DirBuilderExt;
	use std::path::Path;
	let c_path = |path: &Path| CString::new(path.as_os_str().as_bytes()).map_err(|e| e.to_string());
	let bind = |src: &Path, dst: &Path, flags: libc::c_ulong| -> Result<(), String> {
		let (src_c, dst_c) = (c_path(src)?, c_path(dst)?);
		// SAFETY: both paths are NUL-terminated and outlive the call.
		if unsafe {
			libc::mount(
				src_c.as_ptr(),
				dst_c.as_ptr(),
				c"".as_ptr(),
				flags,
				std::ptr::null(),
			)
		} != 0
		{
			return Err(format!(
				"mounting \"{}\" on \"{}\": {}",
				src.display(),
				dst.display(),
				std::io::Error::last_os_error()
			));
		}
		Ok(())
	};
	let template = std::env::temp_dir().join("nix-XXXXXX");
	let mut template = c_path(&template)?.into_bytes_with_nul();
	// SAFETY: `template` is a writable NUL-terminated mkdtemp template.
	if unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) }.is_null() {
		return Err(format!(
			"creating a temporary directory: {}",
			std::io::Error::last_os_error()
		));
	}
	template.pop();
	let root = std::path::PathBuf::from(std::ffi::OsStr::from_bytes(&template));
	let store_mount = root.join(store_dir.trim_start_matches('/'));
	fs::create_dir_all(&store_mount)
		.map_err(|e| format!("creating directory \"{}\": {e}", store_mount.display()))?;
	bind(Path::new(real_store), &store_mount, libc::MS_BIND)?;
	for entry in fs::read_dir("/").map_err(|e| format!("reading \"/\": {e}"))? {
		let entry = entry.map_err(|e| format!("reading \"/\": {e}"))?;
		let src = entry.path();
		let dst = root.join(entry.file_name());
		if fs::symlink_metadata(&dst).is_ok() {
			continue;
		}
		let file_type = entry
			.file_type()
			.map_err(|e| format!("reading \"{}\": {e}", src.display()))?;
		if file_type.is_dir() {
			fs::DirBuilder::new()
				.mode(0o700)
				.create(&dst)
				.map_err(|e| format!("creating directory \"{}\": {e}", dst.display()))?;
			bind(&src, &dst, libc::MS_BIND | libc::MS_REC)?;
		} else if file_type.is_symlink() {
			let target = fs::read_link(&src)
				.map_err(|e| format!("reading link \"{}\": {e}", src.display()))?;
			std::os::unix::fs::symlink(target, &dst)
				.map_err(|e| format!("creating symlink \"{}\": {e}", dst.display()))?;
		}
	}
	let cwd = std::env::current_dir().map_err(|e| format!("getting current directory: {e}"))?;
	let root_c = c_path(&root)?;
	// SAFETY: `root_c` is a NUL-terminated path.
	if unsafe { libc::chroot(root_c.as_ptr()) } != 0 {
		return Err(format!(
			"chrooting into \"{}\": {}",
			root.display(),
			std::io::Error::last_os_error()
		));
	}
	std::env::set_current_dir(&cwd)
		.map_err(|e| format!("chdir to '{}' in chroot: {e}", cwd.display()))
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
			let store_dir = nix_compat::store_path::store_dir();
			let target = CString::new(store_dir).map_err(|e| e.to_string())?;
			let options = CString::new(format!("lowerdir={store_dir}:{}", args[0]))
				.map_err(|e| e.to_string())?;
			if fs::symlink_metadata(store_dir).is_err() {
				// With no mount point, as on a host without /nix, run in a
				// chroot holding the store and the children of /.
				chroot_with_store(&args[0], store_dir)?;
			} else if libc::mount(
				c"overlay".as_ptr(),
				target.as_ptr(),
				c"overlay".as_ptr(),
				libc::MS_MGC_VAL,
				options.as_ptr().cast(),
			) != 0
				&& libc::mount(
					store.as_ptr(),
					target.as_ptr(),
					std::ptr::null(),
					libc::MS_BIND,
					std::ptr::null(),
				) != 0
			{
				return Err(format!(
					"mounting '{}' on '{store_dir}': {}",
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
