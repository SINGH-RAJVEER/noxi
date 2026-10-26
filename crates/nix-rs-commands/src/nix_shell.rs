//! `nix-shell` for expressions and store derivations, following the shell
//! mode of CppNix 2.35.2's `src/nix/nix-build/nix-build.cc`.
//!
//! The selected derivation's inputs are built, then `bash` runs an rc file
//! that sets up the derivation's environment, sources `$stdenv/setup` when
//! it exists, runs `shellHook` and then the `--run` or `--command` text.
//! The shell is `NIX_BUILD_SHELL`, or `bashInteractive` from `<nixpkgs>`,
//! or `bash` from `PATH`. Shebang scripts, `--check`, `--repair` and
//! `exportReferencesGraph` fail explicitly.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsString};
use std::io::IsTerminal;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_rs_build::options::{DerivationOptions, structured_attrs};
use nix_rs_store::DerivedPath;
use serde_json::Value;

use crate::cli::{Failure, print_setting_warnings};
use crate::instantiate::{self, absolute_path};
use crate::paths::print_missing;

/// The variables `--pure` keeps, with the lower-case proxy variables;
/// CppNix's upper-case proxy names come out empty.
const KEEP_VARS: &[&str] = &[
	"HOME",
	"XDG_RUNTIME_DIR",
	"USER",
	"LOGNAME",
	"DISPLAY",
	"WAYLAND_DISPLAY",
	"WAYLAND_SOCKET",
	"PATH",
	"TERM",
	"IN_NIX_SHELL",
	"NIX_SHELL_PRESERVE_PROMPT",
	"TZ",
	"PAGER",
	"NIX_BUILD_SHELL",
	"SHLVL",
	"http_proxy",
	"https_proxy",
	"ftp_proxy",
	"all_proxy",
	"no_proxy",
];

/// `escapeShellArgAlways`.
fn quote(s: &str) -> String {
	format!("'{}'", s.replace('\'', "'\\''"))
}

fn notice(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Notice) {
		eprintln!("{message}");
	}
}

/// `makeTempPath(defaultTempDir(), "nix-shell")` created as a directory.
fn create_temp_dir() -> Result<PathBuf, Failure> {
	let root = std::env::var_os("TMPDIR")
		.filter(|dir| !dir.is_empty())
		.map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
	let root = std::fs::canonicalize(&root).unwrap_or(root);
	let mut counter: u32 = rand_seed();
	loop {
		let dir = root.join(format!("nix-shell-{}-{counter}", std::process::id()));
		match std::fs::DirBuilder::new().create(&dir) {
			Ok(()) => return Ok(dir),
			Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
				counter = counter.wrapping_add(1);
			}
			Err(error) => {
				return Err(Failure::Error(format!(
					"creating directory \"{}\": {error}",
					dir.display()
				)));
			}
		}
	}
}

/// CppNix starts the counter at a random value.
fn rand_seed() -> u32 {
	use std::hash::{BuildHasher, Hasher};
	let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
	hasher.write_u32(std::process::id());
	hasher.finish() as u32
}

/// `resolveShellExprPath` for a file argument: a directory is replaced by
/// its `shell.nix`, else its `default.nix`.
fn resolve_shell_expr(cwd: &Path, input: &str) -> Result<String, Failure> {
	if instantiate::is_lookup_path(input) || input.contains("://") {
		return Ok(input.to_owned());
	}
	let path = absolute_path(cwd, Path::new(input));
	if std::fs::symlink_metadata(&path).is_err() {
		return Err(Failure::Error(format!(
			"path '{}' does not exist",
			path.display()
		)));
	}
	if std::fs::metadata(&path).is_ok_and(|m| m.is_dir()) {
		for name in ["shell.nix", "default.nix"] {
			if path.join(name).exists() {
				return Ok(path.join(name).to_string_lossy().into_owned());
			}
		}
		return Err(Failure::Error(format!(
			"neither 'shell.nix' nor 'default.nix' found in '{}'",
			path.display()
		)));
	}
	Ok(input.to_owned())
}

/// Run `nix-shell` with the arguments after the program name.
pub fn main(settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	let cwd = std::env::current_dir()?;
	if let Some(script) = args.first()
		&& std::fs::read(cwd.join(script)).is_ok_and(|contents| contents.starts_with(b"#!"))
	{
		return Err(Failure::Error(
			"nix-shell shebang scripts are not supported by nix-rs yet".into(),
		));
	}
	let mut pure = false;
	let mut packages = false;
	let mut dry_run = false;
	let mut interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
	let mut env_command = String::new();
	let mut keep: BTreeSet<String> = KEEP_VARS.iter().map(|s| (*s).to_owned()).collect();
	let mut exclude = Vec::new();
	let mut unsupported = None;
	let mut options = instantiate::parse_args_with(args, settings, |flag, rest| {
		let mut value = || {
			rest.next()
				.ok_or_else(|| format!("'{flag}' requires an argument"))
		};
		Some(match flag {
			"--version" => {
				println!("nix-shell (Nix) 2.35.2");
				std::process::exit(0);
			}
			"--add-drv-link" | "--indirect" | "--no-out-link" | "--no-link" => Ok(()),
			"--drv-link" | "--out-link" | "-o" => value().map(drop),
			"--dry-run" => {
				dry_run = true;
				Ok(())
			}
			"--run-env" => Ok(()),
			"--command" | "--run" => value().map(|command| {
				if flag == "--run" {
					interactive = false;
				}
				env_command = format!("{command}\nexit");
			}),
			"--exclude" => value().map(|regex| exclude.push(regex)),
			"--pure" => {
				pure = true;
				Ok(())
			}
			"--impure" => {
				pure = false;
				Ok(())
			}
			"--packages" | "-p" => {
				packages = true;
				Ok(())
			}
			"--keep" => value().map(|name| {
				keep.insert(name);
			}),
			"--check" | "--repair" | "--help" => {
				unsupported = Some(flag.to_owned());
				Ok(())
			}
			"--eval" | "--eval-only" | "--strict" | "--json" | "--xml" | "--add-root"
			| "--find-file" | "--read-write-mode" => Err(format!("unrecognised flag '{flag}'")),
			_ => return None,
		})
	})
	.map_err(Failure::Usage)?;
	if let Some(flag) = unsupported {
		return Err(Failure::Error(format!(
			"nix-shell {flag} is not supported by nix-rs yet"
		)));
	}
	print_setting_warnings(&mut options.settings);
	if packages && options.from_args {
		return Err(Failure::Usage(
			"'-p' and '-E' are mutually exclusive".into(),
		));
	}
	if packages {
		let mut expr = String::from(
			"{...}@args: with import <nixpkgs> args; (pkgs.runCommandCC or pkgs.runCommand) \"shell\" { buildInputs = [ ",
		);
		for package in &options.inputs {
			expr.push_str(&format!("({package}) "));
		}
		expr.push_str("]; } \"\"");
		options.from_args = true;
		options.inputs = vec![expr];
	} else if options.inputs.iter().any(|input| input == "-") {
		options.inputs = vec!["-".into()];
	} else if !options.from_args && options.inputs.is_empty() {
		if !cwd.join("shell.nix").exists() && !cwd.join("default.nix").exists() {
			return Err(Failure::Error(
				"no argument specified and no 'shell.nix' or 'default.nix' file found in the working directory"
					.into(),
			));
		}
		options.inputs = vec![".".into()];
	}
	if !options.from_args {
		options.inputs = options
			.inputs
			.iter()
			.map(|input| {
				if input == "-" {
					Ok(input.clone())
				} else {
					resolve_shell_expr(&cwd, input)
				}
			})
			.collect::<Result<_, _>>()?;
	}
	options.in_nix_shell = true;
	// SAFETY: the command runs on a single thread and evaluation reads it.
	unsafe { std::env::set_var("IN_NIX_SHELL", if pure { "pure" } else { "impure" }) };

	let evaluated = match instantiate::evaluate(&options, &cwd) {
		Ok(evaluated) => evaluated,
		Err(code) => return Ok(code),
	};
	let derivations = instantiate::derivations(&evaluated.results)?;
	let [(drv_path, _)] = derivations.as_slice() else {
		return Err(Failure::Usage(
			"nix-shell requires a single derivation".into(),
		));
	};
	let store = evaluated
		.store
		.ok_or_else(|| Failure::Error("the selected store cannot build derivations".to_owned()))?;
	let drv = store.read_derivation(drv_path)?;

	let mut to_build = Vec::new();
	let mut shell = std::env::var("NIX_BUILD_SHELL").ok();
	let mut shell_drv = None;
	// nix-rs skips `flake:` and URL lookup path entries, which the reference
	// resolves; `<nixpkgs>` from one would give it `bashInteractive`.
	if shell.is_none()
		&& instantiate::search_path(&options).iter().any(|entry| {
			let value = entry
				.split_once('=')
				.map_or(entry.as_str(), |(_, value)| value);
			value.starts_with("flake:") || value.contains("://") || value.starts_with("channel:")
		}) {
		return Err(Failure::Error(
			"nix-shell without NIX_BUILD_SHELL and with flake:, channel: or URL lookup path entries is not supported by nix-rs yet".into(),
		));
	}
	if shell.is_none() {
		let bash = instantiate::Options {
			from_args: true,
			inputs: vec!["(import <nixpkgs> {}).bashInteractive".into()],
			..options.clone()
		};
		let bash = instantiate::Options {
			in_nix_shell: false,
			auto_args: Vec::new(),
			attr_paths: Vec::new(),
			..bash
		};
		match instantiate::evaluate(&bash, &cwd)
			.ok()
			.and_then(|evaluated| instantiate::derivations(&evaluated.results).ok())
		{
			Some(found) if found.len() == 1 => {
				let drv = found[0].0.clone();
				to_build.push(DerivedPath::Built {
					drv: drv.clone(),
					outputs: BTreeSet::from(["out".to_owned()]),
				});
				shell_drv = Some(drv);
			}
			_ => {
				notice("uses bash from your environment");
				shell = Some("bash".into());
			}
		}
	}
	for (input, outputs) in drv.input_derivations() {
		let printed = input.to_absolute_path();
		let mut excluded = false;
		for pattern in &exclude {
			let regex = regex::Regex::new(pattern).map_err(|e| Failure::Error(e.to_string()))?;
			excluded |= regex.is_match(&printed);
		}
		if !excluded && !outputs.is_empty() {
			to_build.push(DerivedPath::Built {
				drv: input.clone(),
				outputs: outputs.iter().map(|o| o.to_string()).collect(),
			});
		}
	}
	for source in drv.input_sources() {
		to_build.push(DerivedPath::Opaque(source.clone()));
	}
	if options.settings.get_bool("print-missing", true) {
		print_missing(store.as_ref(), &store.query_missing(&to_build)?)?;
	}
	if dry_run {
		return Ok(ExitCode::SUCCESS);
	}
	store.build_paths(&to_build)?;
	if let Some(drv) = shell_drv {
		let out = store
			.query_derivation_outputs(&drv)?
			.remove("out")
			.ok_or_else(|| Failure::Error("bashInteractive has no 'out' output".into()))?;
		shell = Some(format!("{}/bin/bash", out.to_absolute_path()));
	}
	let shell = shell.expect("a shell was chosen");

	let tmp_dir = create_temp_dir()?;
	let tmp = tmp_dir.to_string_lossy().into_owned();
	let mut env: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
	if pure {
		env.retain(|name, _| keep.contains(&*name.to_string_lossy()));
		env.insert("__ETC_PROFILE_SOURCED".into(), "1".into());
	}
	for name in ["NIX_BUILD_TOP", "TMPDIR", "TEMPDIR", "TMP", "TEMP"] {
		env.insert(name.into(), tmp.clone().into());
	}
	env.insert("NIX_STORE".into(), "/nix/store".into());
	env.insert(
		"NIX_BUILD_CORES".into(),
		nix_rs_build::settings::BuildSettings::from_settings(&options.settings)
			.effective_cores()
			.to_string()
			.into(),
	);

	let parsed = structured_attrs(drv.environment()).map_err(|e| e.to_string())?;
	let mut warnings = Vec::new();
	let drv_options = DerivationOptions::new(
		drv.environment(),
		parsed.as_ref(),
		drv.input_derivations(),
		false,
		&mut warnings,
	)
	.map_err(|e| {
		Failure::Error(format!(
			"{e}\nwhile parsing derivation '{}'",
			drv_path.to_absolute_path()
		))
	})?;
	for warning in warnings {
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
			eprintln!("warning: {warning}");
		}
	}
	let mut file_nr = 0;
	for (name, value) in drv.environment() {
		let name = String::from_utf8_lossy(name).into_owned();
		// CppNix keeps structured attributes out of the environment.
		if parsed.is_some() && name == "__json" {
			continue;
		}
		if drv_options.pass_as_file.contains(&name) {
			let file = tmp_dir.join(format!(".attr-{file_nr}"));
			file_nr += 1;
			std::fs::write(&file, value)?;
			env.insert(format!("{name}Path").into(), file.into_os_string());
		} else {
			env.insert(name.into(), OsString::from_vec(value.to_vec()));
		}
	}
	let mut structured_rc = String::new();
	if let Some(mut json) = parsed {
		if !drv_options.export_references_graph.is_empty() {
			return Err(Failure::Error(
				"nix-shell with structured exportReferencesGraph is not supported by nix-rs yet"
					.into(),
			));
		}
		let outputs = drv
			.outputs()
			.iter()
			.map(|(name, _)| {
				let name = name.to_string();
				let placeholder = nix_compat::store_path::hash_placeholder(&name);
				(name, Value::String(placeholder))
			})
			.collect();
		json.insert("outputs".into(), Value::Object(outputs));
		structured_rc = nix_rs_build::structured::write_shell(&json);
		let json_file = tmp_dir.join(".attrs.json");
		std::fs::write(
			&json_file,
			nix_rs_build::structured::dump(&Value::Object(json)),
		)?;
		let sh_file = tmp_dir.join(".attrs.sh");
		std::fs::write(&sh_file, &structured_rc)?;
		env.insert("NIX_ATTRS_SH_FILE".into(), sh_file.into_os_string());
		env.insert("NIX_ATTRS_JSON_FILE".into(), json_file.into_os_string());
	}

	let shell_dir = Path::new(&shell)
		.parent()
		.map(|dir| dir.to_string_lossy().into_owned())
		.unwrap_or_default();
	let tz_export = std::env::var("TZ")
		.map(|tz| format!("export TZ={}; ", quote(&tz)))
		.unwrap_or_default();
	// SAFETY: getuid has no preconditions.
	let prompt = if unsafe { libc::getuid() } == 0 {
		r"PS1='\n\[\033[1;31m\][nix-shell:\w]\$\[\033[0m\] '; "
	} else {
		r"PS1='\n\[\033[1;32m\][nix-shell:\w]\$\[\033[0m\] '; "
	};
	let rc = format!(
		"_nix_shell_clean_tmpdir() {{ command rm -rf {tmp_quoted}; }};trap _nix_shell_clean_tmpdir EXIT; exitHooks+=(_nix_shell_clean_tmpdir); failureHooks+=(_nix_shell_clean_tmpdir); {bashrc}{save_path}unset PATH;dontAddDisableDepTrack=1;\n{structured_rc}\n[ -e $stdenv/setup ] && source $stdenv/setup; {restore_path}PATH={shell_dir}:\"$PATH\"; SHELL={shell_quoted}; BASH={shell_quoted}; set +e; [ -n \"$PS1\" -a -z \"$NIX_SHELL_PRESERVE_PROMPT\" ] && {prompt}if [ \"$(type -t runHook)\" = function ]; then runHook shellHook; fi; unset NIX_ENFORCE_PURITY; shopt -u nullglob; unset TZ; {tz_export}shopt -s execfail;{env_command}",
		tmp_quoted = quote(&tmp),
		bashrc = if pure {
			""
		} else {
			"[ -n \"$PS1\" ] && [ -e ~/.bashrc ] && source ~/.bashrc;"
		},
		save_path = if pure { "" } else { "p=$PATH; " },
		restore_path = if pure { "" } else { "PATH=$PATH:$p; unset p; " },
		shell_dir = quote(&shell_dir),
		shell_quoted = quote(&shell),
	);
	let rc_file = tmp_dir.join("rc");
	std::fs::write(&rc_file, rc)?;

	let rc_arg = rc_file.to_string_lossy().into_owned();
	let args: Vec<&str> = if interactive {
		vec!["bash", "--rcfile", &rc_arg]
	} else {
		vec!["bash", &rc_arg]
	};
	let c_string = |bytes: &[u8]| CString::new(bytes).map_err(|e| Failure::Error(e.to_string()));
	let argv = args
		.iter()
		.map(|arg| c_string(arg.as_bytes()))
		.collect::<Result<Vec<_>, _>>()?;
	let program = c_string(shell.as_bytes())?;
	let mut argv_ptrs: Vec<*const libc::c_char> = argv.iter().map(|a| a.as_ptr()).collect();
	argv_ptrs.push(std::ptr::null());
	// Like CppNix, replace the environment and then search its PATH.
	for (name, _) in std::env::vars_os() {
		// SAFETY: the command runs on a single thread.
		unsafe { std::env::remove_var(name) };
	}
	for (name, value) in &env {
		if !name.is_empty() && !name.as_bytes().contains(&b'=') && !value.as_bytes().contains(&0) {
			// SAFETY: as above.
			unsafe { std::env::set_var(name, value) };
		}
	}
	// SAFETY: the pointers stay valid until execvp replaces the process.
	unsafe { libc::execvp(program.as_ptr(), argv_ptrs.as_ptr()) };
	let error = std::io::Error::last_os_error();
	Err(Failure::Error(format!(
		"executing shell '{shell}': {}",
		crate::nix::hash::strerror(&error)
	)))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn quotes_like_escape_shell_arg_always() {
		assert_eq!(quote("a'b"), r"'a'\''b'");
		assert_eq!(quote(""), "''");
	}
}
