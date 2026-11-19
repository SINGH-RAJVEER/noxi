//! `noxi system option`: the native `nixos-option`.
//!
//! Upstream `nixos-option` (nixpkgs `pkgs/by-name/ni/nixos-option/`) is a
//! shell script that picks a NixOS configuration and runs
//! `nix-instantiate --eval --json` on `nixos-option.nix`, piping the string
//! result through `jq -r`. This command evaluates the same expression,
//! vendored as `option.nix`, with noxi's in-process `nix-instantiate`, and
//! prints the string it returns.
//!
//! The configuration is chosen as upstream chooses it, highest first:
//!
//! 1. `--flake REF[#HOST]`, or `--file PATH [--attr ATTR]` as
//!    `noxi system build` takes it;
//! 2. `<nixpkgs/nixos>` when `NIXOS_CONFIG` is set, `nixos-config` is in the
//!    lookup path, `-I` is given or `--no-flake` is given;
//! 3. the flake `/etc/nixos/flake.nix` is in, when it exists;
//! 4. `<nixpkgs/nixos>`.
//!
//! A flake without `#HOST` selects `nixosConfigurations.HOSTNAME`, or
//! `default` when the host name is empty. A local flake directory inside a
//! Git work tree is evaluated as `git+file://` of that work tree, as
//! upstream does, with a `dir` parameter when the flake is in a
//! subdirectory (upstream drops the subdirectory).

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noxi_events::Styling;

use crate::rebuild::{DEFAULT_FILE, DEFAULT_FLAKE_DIR, STACK_SIZE, load_settings};

pub const USAGE: &str = "usage: noxi system option [-r | --recursive] [--flake REF[#HOST] | --file PATH [--attr ATTR] | --no-flake] [-I PATH] [--show-trace] [OPTION.PATH]";

/// The upstream expression, taking `nixos`, `path` and `recursive`.
pub const OPTION_NIX: &str = include_str!("option.nix");

/// Which configuration to evaluate.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Source {
	/// Upstream's detection order.
	#[default]
	Default,
	/// `--no-flake`: `<nixpkgs/nixos>`.
	NoFlake,
	/// `--flake REF[#HOST]`.
	Flake(String),
	/// `--file PATH [--attr ATTR]`: a NixOS evaluation, such as
	/// `<nixpkgs/nixos>`; a function is called with no arguments.
	File {
		path: String,
		attribute: Option<String>,
	},
}

/// A parsed `noxi system option` command line.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Options {
	/// The option path, without a trailing dot; empty for the top level.
	pub path: String,
	pub recursive: bool,
	pub source: Source,
	/// `-I` entries, in order.
	pub includes: Vec<String>,
	pub show_trace: bool,
}

/// Parse the arguments after `noxi system option`; `None` for `--help`.
pub fn parse(args: Vec<String>) -> Result<Option<Options>, String> {
	let mut options = Options::default();
	let mut flake = None;
	let mut file = None;
	let mut attribute = None;
	let mut no_flake = false;
	let mut positional = Vec::new();
	let mut args = args.into_iter();
	let value = |flag: &str, args: &mut dyn Iterator<Item = String>| {
		args.next()
			.ok_or_else(|| format!("flag '{flag}' requires an argument"))
	};
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"-h" | "--help" => return Ok(None),
			"-r" | "--recursive" => options.recursive = true,
			"-I" => options.includes.push(value(&arg, &mut args)?),
			"-F" | "--flake" => flake = Some(value(&arg, &mut args)?),
			"--file" | "-f" => file = Some(value(&arg, &mut args)?),
			"--attr" | "-A" => attribute = Some(value(&arg, &mut args)?),
			"--no-flake" => no_flake = true,
			"--show-trace" => options.show_trace = true,
			flag if flag.starts_with('-') => return Err(format!("unsupported option '{flag}'")),
			_ => positional.push(arg),
		}
	}
	options.path = match positional.as_slice() {
		[] => String::new(),
		// As upstream: a trailing dot is dropped, for shell completion.
		[path] => path.strip_suffix('.').unwrap_or(path).to_owned(),
		_ => return Err("only one option path can be provided".to_owned()),
	};
	options.source = match (flake, file, attribute) {
		(Some(_), Some(_), _) => {
			return Err("'--flake' and '--file' are mutually exclusive".to_owned());
		}
		(_, None, Some(_)) => return Err("'--attr' requires '--file'".to_owned()),
		(Some(reference), None, None) => Source::Flake(reference),
		(None, Some(path), attribute) => Source::File { path, attribute },
		(None, None, None) if no_flake => Source::NoFlake,
		(None, None, None) => Source::Default,
	};
	Ok(Some(options))
}

/// The directory of the Git work tree containing `dir`, if any: the
/// nearest ancestor with a `.git` directory or file.
pub fn git_work_tree(dir: &Path) -> Option<PathBuf> {
	dir.ancestors()
		.find(|candidate| candidate.join(".git").exists())
		.map(Path::to_path_buf)
}

/// The flake reference `builtins.getFlake` is given for `reference`: a
/// local directory inside a Git work tree becomes `git+file://` of the work
/// tree, with `?dir=` for a subdirectory, and other local directories
/// become `path:` references.
pub fn flake_reference(reference: &str) -> Result<String, String> {
	let local = Path::new(reference);
	if reference.contains(':') || !local.is_dir() {
		return Ok(reference.to_owned());
	}
	let dir = fs::canonicalize(local)
		.map_err(|e| format!("cannot open flake directory '{reference}': {e}"))?;
	Ok(match git_work_tree(&dir) {
		Some(tree) => {
			let sub = dir.strip_prefix(&tree).unwrap_or(Path::new(""));
			if sub.as_os_str().is_empty() {
				format!("git+file://{}", tree.display())
			} else {
				format!("git+file://{}?dir={}", tree.display(), sub.display())
			}
		}
		None => format!("path:{}", dir.display()),
	})
}

/// The Nix expression of the NixOS evaluation for a flake reference with
/// an optional `#HOST`.
pub fn flake_expression(reference: &str, hostname: Option<&str>) -> Result<String, String> {
	let (reference, host) = match reference.rsplit_once('#') {
		Some((reference, host)) => (reference, Some(host)),
		None => (reference, None),
	};
	let host = match host {
		Some(host) if host.contains('"') => return Err(format!("'{host}' is not a host name")),
		Some(host) => host,
		None => hostname
			.filter(|name| !name.is_empty())
			.unwrap_or("default"),
	};
	let reference = flake_reference(if reference.is_empty() { "." } else { reference })?;
	Ok(format!(
		"(builtins.getFlake {}).nixosConfigurations.{}",
		noxi_commands::instantiate::nix_string_literal(&reference),
		noxi_commands::instantiate::nix_string_literal(host)
	))
}

/// The Nix expression of the NixOS evaluation in a file: `import FILE`,
/// called with no arguments when it is a function, then `ATTR`.
pub fn file_expression(path: &str, attribute: Option<&str>) -> Result<String, String> {
	let import = if path.starts_with('<') && path.ends_with('>') {
		path.to_owned()
	} else {
		let cwd = std::env::current_dir()
			.map_err(|e| format!("cannot read the current directory: {e}"))?;
		let file = noxi_commands::instantiate::absolute_path(&cwd, Path::new(path));
		noxi_commands::instantiate::nix_string_literal(&file.to_string_lossy())
	};
	let attribute = attribute.map_or_else(String::new, |attribute| format!(".{attribute}"));
	Ok(format!(
		"(let f = import {import}; in if builtins.isFunction f then f {{ }} else f){attribute}"
	))
}

/// Whether `<nixos-config>` resolves in the lookup path `entries`, as
/// upstream's `nix-instantiate --find-file nixos-config` decides it: the
/// file an entry names must exist.
pub fn finds_nixos_config(entries: &[String]) -> bool {
	let search_path = noxi_eval::NixSearchPath::new(
		entries
			.iter()
			.map(|entry| noxi_eval::NixSearchPathEntry::parse(entry))
			.collect(),
		None,
	);
	let io: Box<dyn noxi_eval::EvalIO> = Box::new(noxi_eval::StdIO);
	search_path.resolve(&io, "nixos-config").is_ok()
}

/// The expression for `nixos`, choosing the configuration as upstream
/// does. `lookup_path` is the evaluator's lookup path with the `-I`
/// entries, and `env` reads `NIXOS_CONFIG`.
pub fn nixos_expression(
	options: &Options,
	lookup_path: &[String],
	env: &dyn Fn(&str) -> Option<String>,
	default_flake_dir: &Path,
	hostname: Option<&str>,
) -> Result<String, String> {
	let default_file = || file_expression(DEFAULT_FILE, None);
	match &options.source {
		Source::Flake(reference) => flake_expression(reference, hostname),
		Source::File { path, attribute } => file_expression(path, attribute.as_deref()),
		Source::NoFlake => default_file(),
		Source::Default => {
			let no_flake = !options.includes.is_empty()
				|| env("NIXOS_CONFIG").is_some_and(|value| !value.is_empty())
				|| finds_nixos_config(lookup_path);
			let flake = default_flake_dir.join("flake.nix");
			if !no_flake && flake.exists() {
				let flake = fs::canonicalize(&flake)
					.map_err(|e| format!("cannot resolve \"{}\": {e}", flake.display()))?;
				let dir = flake.parent().unwrap_or(Path::new("/"));
				flake_expression(&dir.to_string_lossy(), hostname)
			} else {
				default_file()
			}
		}
	}
}

/// Evaluate `options` and write the rendered option to `out`.
pub fn run(options: Options, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	let styling = Styling::for_stderr();
	let _guard = crate::flake::LOCK
		.lock()
		.unwrap_or_else(|poisoned| poisoned.into_inner());
	let evaluation = std::thread::Builder::new()
		.stack_size(STACK_SIZE)
		.spawn(move || evaluate(&options))
		.map_err(|e| format!("cannot start the evaluator thread: {e}"))
		.and_then(|thread| {
			thread
				.join()
				.map_err(|_| "the evaluator panicked".to_owned())
		});
	match evaluation {
		Ok(Ok(mut text)) => {
			if !text.ends_with(b"\n") {
				text.push(b'\n');
			}
			let _ = out.write_all(&text);
			ExitCode::SUCCESS
		}
		// The evaluator has reported the error itself.
		Ok(Err(None)) => ExitCode::FAILURE,
		Ok(Err(Some(message))) | Err(message) => crate::fail(err, &styling, None, &message),
	}
}

/// The evaluation on the evaluator thread: the option text, or an error
/// message when the evaluator has not already reported it.
fn evaluate(options: &Options) -> Result<Vec<u8>, Option<String>> {
	let settings = load_settings(true).map_err(Some)?;
	let mut prefix = Vec::new();
	for include in &options.includes {
		prefix.extend(["-I".to_owned(), include.clone()]);
	}
	if options.show_trace {
		prefix.push("--show-trace".to_owned());
	}
	let lookup =
		noxi_commands::instantiate::parse_args(prefix.clone(), settings.clone()).map_err(Some)?;
	let lookup_path = noxi_commands::instantiate::search_path(&lookup);
	let nixos = nixos_expression(
		options,
		&lookup_path,
		&|name| std::env::var(name).ok(),
		Path::new(DEFAULT_FLAKE_DIR),
		crate::snapshot::host_name().as_deref(),
	)
	.map_err(Some)?;
	let mut args = prefix;
	args.extend([
		"--eval".to_owned(),
		"--raw".to_owned(),
		"--arg".to_owned(),
		"nixos".to_owned(),
		nixos,
		"--argstr".to_owned(),
		"path".to_owned(),
		options.path.clone(),
		"--arg".to_owned(),
		"recursive".to_owned(),
		options.recursive.to_string(),
		"--expr".to_owned(),
		OPTION_NIX.to_owned(),
	]);
	let parsed = noxi_commands::instantiate::parse_args(args, settings).map_err(Some)?;
	let cwd = std::env::current_dir()
		.map_err(|e| Some(format!("cannot read the current directory: {e}")))?;
	let mut text = Vec::new();
	if noxi_commands::instantiate::run(&parsed, &cwd, &mut text) == ExitCode::SUCCESS {
		Ok(text)
	} else {
		Err(None)
	}
}

pub fn main(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	match parse(args) {
		Ok(Some(options)) => run(options, out, err),
		Ok(None) => {
			let _ = writeln!(out, "{USAGE}");
			ExitCode::SUCCESS
		}
		Err(message) => {
			let _ = writeln!(err, "error: {message}\n{USAGE}");
			ExitCode::from(2)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(list: &[&str]) -> Vec<String> {
		list.iter().map(|arg| (*arg).to_owned()).collect()
	}

	#[test]
	fn parses_like_nixos_option() {
		let options = parse(args(&["-r", "-I", "nixpkgs=/n", "services.openssh."]))
			.unwrap()
			.unwrap();
		assert!(options.recursive);
		assert_eq!(options.path, "services.openssh");
		assert_eq!(options.includes, ["nixpkgs=/n"]);
		assert_eq!(options.source, Source::Default);
		assert!(parse(args(&["a", "b"])).is_err());
		assert!(parse(args(&["--bogus"])).is_err());
		assert!(parse(args(&["--attr", "x"])).is_err());
		assert_eq!(
			parse(args(&["--no-flake"])).unwrap().unwrap().source,
			Source::NoFlake
		);
	}

	#[test]
	fn default_source_follows_upstream_order() {
		let dir = tempfile::tempdir().unwrap();
		let etc = dir.path().join("etc-nixos");
		fs::create_dir(&etc).unwrap();
		fs::write(etc.join("flake.nix"), "{ }").unwrap();
		let none = |_: &str| None;
		let options = Options::default();
		let flake = nixos_expression(&options, &[], &none, &etc, Some("box")).unwrap();
		let canonical = fs::canonicalize(&etc).unwrap();
		assert_eq!(
			flake,
			format!(
				"(builtins.getFlake \"path:{}\").nixosConfigurations.\"box\"",
				canonical.display()
			)
		);
		let file =
			"(let f = import <nixpkgs/nixos>; in if builtins.isFunction f then f { } else f)";
		// `nixos-config` in the lookup path, `NIXOS_CONFIG` and `-I` each
		// select <nixpkgs/nixos>.
		fs::write(dir.path().join("configuration.nix"), "{ }").unwrap();
		let lookup = [format!(
			"nixos-config={}",
			dir.path().join("configuration.nix").display()
		)];
		assert_eq!(
			nixos_expression(&options, &lookup, &none, &etc, None).unwrap(),
			file
		);
		let set = |name: &str| (name == "NIXOS_CONFIG").then(|| "/c.nix".to_owned());
		assert_eq!(
			nixos_expression(&options, &[], &set, &etc, None).unwrap(),
			file
		);
		let with_include = Options {
			includes: vec!["nixpkgs=/n".to_owned()],
			..Options::default()
		};
		assert_eq!(
			nixos_expression(&with_include, &[], &none, &etc, None).unwrap(),
			file
		);
		// Without /etc/nixos/flake.nix.
		assert_eq!(
			nixos_expression(&options, &[], &none, &dir.path().join("none"), None).unwrap(),
			file
		);
	}

	#[test]
	fn flake_references() {
		let dir = tempfile::tempdir().unwrap();
		let tree = fs::canonicalize(dir.path()).unwrap();
		fs::create_dir_all(tree.join(".git")).unwrap();
		fs::create_dir_all(tree.join("hosts")).unwrap();
		let at = |path: &Path| path.to_str().unwrap().to_owned();
		assert_eq!(
			flake_reference(&at(&tree)).unwrap(),
			format!("git+file://{}", tree.display())
		);
		assert_eq!(
			flake_reference(&at(&tree.join("hosts"))).unwrap(),
			format!("git+file://{}?dir=hosts", tree.display())
		);
		assert_eq!(
			flake_reference("github:o/r").unwrap(),
			"github:o/r".to_owned()
		);
		assert_eq!(
			flake_expression("github:o/r#web", Some("box")).unwrap(),
			"(builtins.getFlake \"github:o/r\").nixosConfigurations.\"web\""
		);
		assert_eq!(
			flake_expression("github:o/r", Some("")).unwrap(),
			"(builtins.getFlake \"github:o/r\").nixosConfigurations.\"default\""
		);
		assert!(flake_expression("github:o/r#a\"b", None).is_err());
	}

	#[test]
	fn lookup_path_entries() {
		let dir = tempfile::tempdir().unwrap();
		assert!(!finds_nixos_config(&["nixpkgs=/n".to_owned()]));
		// A `nixos-config=` entry naming a missing file does not count.
		assert!(!finds_nixos_config(&[format!(
			"nixos-config={}",
			dir.path().join("missing.nix").display()
		)]));
		let entry = dir.path().to_str().unwrap().to_owned();
		assert!(!finds_nixos_config(std::slice::from_ref(&entry)));
		fs::write(dir.path().join("nixos-config"), "").unwrap();
		assert!(finds_nixos_config(&[entry]));
	}
}
