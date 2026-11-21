//! `noxi install`: the native `nixos-install`.
//!
//! It follows nixpkgs' `pkgs/by-name/ni/nixos-install/nixos-install.sh`
//! step by step, running the compatibility commands the script calls
//! (`nix build`, `nix-build`, `nix-env`) in-process and `noxi system enter`
//! where the script calls `nixos-enter`:
//!
//! 1. Check that the root exists and that every directory up to it is
//!    readable and searchable by others, find the configuration (a flake
//!    with `--flake`, a file with `--file`/`--attr`, a closure with
//!    `--system`, or else `system.nix`/`default.nix` upwards from the
//!    current directory, `/etc/nixos/system.nix`, `/etc/nixos/default.nix`,
//!    `$NIXOS_CONFIG` and `ROOT/etc/nixos/configuration.nix`, in the
//!    script's order), and print the plan of what will be written into the
//!    root.
//! 2. Create a temporary directory in the root and point `TMPDIR` at it,
//!    so builds use the target's disk.
//! 3. Copy the root `nixos` channel into the root's store and channels
//!    profile, unless the configuration is a flake or `--no-channel-copy`
//!    is given. The script copies the channel for flakes as well; a flake
//!    system does not read it, so `noxi install` does not.
//! 4. Build the system into the root's store (`--store ROOT`) with
//!    `--extra-substituters "auto?trusted=1"`, so every path the running
//!    system's store already holds is copied rather than rebuilt.
//! 5. Point `ROOT/nix/var/nix/profiles/system` at the system, as
//!    `nix-env --set` does, and create `ROOT/etc/NIXOS`.
//! 6. Unless `--no-bootloader` is given, link `ROOT/etc/mtab` to
//!    `/proc/mounts` and run the script's bootloader snippet through
//!    `noxi system enter` with `NIXOS_INSTALL_BOOTLOADER=1`:
//!    `switch-to-configuration boot` inside the root.
//! 7. Unless `--no-root-passwd` is given or standard input is not a
//!    terminal, run `passwd` inside the root when the system has it.
//!
//! The script resolves a flake to its locked URL with `nix flake metadata`
//! before building; `noxi install` builds the reference as given, which
//! locks it the same way. Partitioning, file systems and the configuration
//! itself are the caller's (see `noxi system init`). Usage errors exit with
//! status 2 instead of the script's 1, as every `noxi` command does, and
//! failures are rustc-style diagnostics.

use std::ffi::OsString;
use std::fs;
use std::io::{IsTerminal, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

use nix_compat::nixcpp::settings::Settings;
use noxi_commands::cli::Failure;
use noxi_events::{Styling, Tone, status};

use crate::enter::DEFAULT_ROOT;
use crate::snapshot::DEFAULT_PROFILE;

pub const USAGE: &str = "usage: noxi install [--root ROOT] [--flake FLAKE#NAME | --file FILE [--attr ATTR] | --system PATH] [--channel PATH] [--no-channel-copy] [--no-root-passwd] [--no-bootloader] [--option NAME VALUE] [-j|--max-jobs N] [--cores N] [-I PATH] [--substituters URIS] [--show-trace] [--impure] [--keep-going] [-v]";

/// The substituter that copies paths from the running system's store.
pub const SUBSTITUTER: &str = "auto?trusted=1";

/// The root's channels profile, relative to the root.
const CHANNELS_PROFILE: &str = "/nix/var/nix/profiles/per-user/root/channels";

/// The bootloader step the script runs through `nixos-enter`, verbatim. It
/// bind-mounts the root onto `$mountPoint` inside the chroot so paths that
/// name the mount point stay valid while `switch-to-configuration` runs.
const BOOTLOADER_SCRIPT: &str = r#"
      set -e
      # Clear the cache for executable locations. They were invalidated by the chroot.
      hash -r
      # Create a bind mount for each of the mount points inside the target file
      # system. This preserves the validity of their absolute paths after changing
      # the root with `nixos-enter`.
      # Without this the bootloader installation may fail due to options that
      # contain paths referenced during evaluation, like initrd.secrets.
      # when not root, re-execute the script in an unshared namespace
      mount --rbind --mkdir / "$mountPoint"
      mount --make-rslave "$mountPoint"
      /run/current-system/bin/switch-to-configuration boot
      umount -R "$mountPoint" && (rmdir "$mountPoint" 2>/dev/null || true)
"#;

/// How the system to install is chosen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Method {
	/// `--flake FLAKE#NAME`: `nixosConfigurations."NAME"` of the flake.
	Flake { flake: String, name: String },
	/// `--file`/`--attr`, or a `system.nix`/`default.nix` found by search:
	/// `[ATTR.]config.system.build.toplevel` of the file.
	File {
		file: Option<PathBuf>,
		attr: Option<String>,
	},
	/// A `configuration.nix` module built through `<nixpkgs/nixos>`;
	/// `None` until it is found.
	Module(Option<PathBuf>),
	/// `--system PATH`: an already built system closure.
	System(PathBuf),
}

/// A parsed `noxi install` command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
	pub root: PathBuf,
	/// `None` when no method was requested and one is searched for.
	pub method: Option<Method>,
	/// Flags passed to every build and profile command.
	pub build_flags: Vec<String>,
	/// Flake lock flags passed to the flake build.
	pub lock_flags: Vec<String>,
	pub verbosity: Vec<String>,
	pub channel: Option<PathBuf>,
	pub no_channel_copy: bool,
	pub no_root_passwd: bool,
	pub no_bootloader: bool,
}

impl Default for Options {
	fn default() -> Self {
		Options {
			root: PathBuf::from(DEFAULT_ROOT),
			method: None,
			build_flags: Vec::new(),
			lock_flags: Vec::new(),
			verbosity: Vec::new(),
			channel: None,
			no_channel_copy: false,
			no_root_passwd: false,
			no_bootloader: false,
		}
	}
}

/// Parse the command line as the script's option loop does. `None` means
/// `--help`.
pub fn parse(args: Vec<String>) -> Result<Option<Options>, String> {
	let mut options = Options::default();
	// The build methods requested, in the script's terms.
	let mut requested: Vec<&'static str> = Vec::new();
	let mut flake = None;
	let mut file = None;
	let mut attr = None;
	let mut system = None;
	let mut args = args.into_iter();
	let value = |args: &mut std::vec::IntoIter<String>, flag: &str| {
		args.next()
			.ok_or_else(|| format!("'{flag}' requires an argument"))
	};
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--max-jobs" | "-j" | "--cores" | "-I" | "--substituters" | "--log-format" => {
				let v = value(&mut args, &arg)?;
				options.build_flags.extend([arg, v]);
			}
			"--option" => {
				let name = value(&mut args, &arg)?;
				let v = value(&mut args, &arg)?;
				options.build_flags.extend([arg, name, v]);
			}
			"--root" => options.root = PathBuf::from(value(&mut args, &arg)?),
			"--system" | "--closure" | "--store-path" => {
				system = Some(PathBuf::from(value(&mut args, &arg)?));
				requested.push("system");
			}
			"--flake" => {
				flake = Some(value(&mut args, &arg)?);
				requested.push("flake");
			}
			"--file" | "-f" => {
				let path = PathBuf::from(value(&mut args, &arg)?);
				file = Some(if path.is_file() {
					path
				} else if path.join("system.nix").is_file() {
					path.join("system.nix")
				} else {
					path.join("default.nix")
				});
				requested.push("by-attrset");
			}
			"--attr" | "-A" => {
				attr = Some(value(&mut args, &arg)?);
				requested.push("by-attrset");
			}
			"--recreate-lock-file"
			| "--no-update-lock-file"
			| "--no-write-lock-file"
			| "--no-registries"
			| "--commit-lock-file" => options.lock_flags.push(arg),
			"--update-input" => {
				let v = value(&mut args, &arg)?;
				options.lock_flags.extend([arg, v]);
			}
			"--override-input" => {
				let input = value(&mut args, &arg)?;
				let v = value(&mut args, &arg)?;
				options.lock_flags.extend([arg, input, v]);
			}
			"--channel" => options.channel = Some(PathBuf::from(value(&mut args, &arg)?)),
			"--no-channel-copy" => options.no_channel_copy = true,
			"--no-root-password" | "--no-root-passwd" => options.no_root_passwd = true,
			"--no-bootloader" => options.no_bootloader = true,
			"--show-trace" | "--impure" | "--keep-going" => options.build_flags.push(arg),
			"--help" | "-h" => return Ok(None),
			// The script's `set -x`; there is no shell to trace.
			"--debug" => {}
			"--verbose" => options.verbosity.push(arg),
			_ if arg.starts_with("-v") && arg[1..].chars().all(|c| c == 'v') => {
				options.verbosity.push(arg)
			}
			other => return Err(format!("unknown option '{other}'")),
		}
	}
	requested.sort_unstable();
	requested.dedup();
	if requested.len() > 1 {
		return Err(format!(
			"multiple build methods requested: {}",
			requested.join(" ")
		));
	}
	options.method = match requested.first().copied() {
		None => None,
		Some("flake") => {
			let flake = flake.unwrap_or_default();
			let Some((reference, name)) = flake
				.rsplit_once('#')
				.filter(|(_, name)| !name.is_empty() && !name.contains('"'))
			else {
				return Err("please specify the name of the NixOS configuration to be installed, as a URI fragment in the flake-uri; for example, to use the output nixosConfigurations.foo from the flake.nix, append \"#foo\" to the flake-uri".to_owned());
			};
			Some(Method::Flake {
				flake: reference.to_owned(),
				name: name.to_owned(),
			})
		}
		Some("system") => system.map(Method::System),
		_ => Some(Method::File { file, attr }),
	};
	Ok(Some(options))
}

/// `FILE` in `dir` or the nearest parent that has it (`findInParents`).
fn find_in_parents(dir: &Path, name: &str) -> Option<PathBuf> {
	dir.ancestors()
		.map(|dir| dir.join(name))
		.find(|path| path.is_file())
}

/// The script's `findByAttrset`.
fn find_by_attrset(cwd: &Path) -> Option<PathBuf> {
	find_in_parents(cwd, "system.nix")
		.or_else(|| find_in_parents(cwd, "default.nix"))
		.or_else(|| {
			["/etc/nixos/system.nix", "/etc/nixos/default.nix"]
				.into_iter()
				.map(PathBuf::from)
				.find(|path| path.is_file())
		})
}

/// The script's `findModule`.
fn find_module(root: &Path, nixos_config: Option<OsString>) -> Option<PathBuf> {
	nixos_config
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
		.or_else(|| {
			let path = under(root, "/etc/nixos/configuration.nix");
			path.is_file().then_some(path)
		})
}

/// `ROOT/PATH` for the absolute path `path`.
fn under(root: &Path, path: &str) -> PathBuf {
	root.join(path.trim_start_matches('/'))
}

/// Fill in the build method the way the script finds one, and check that
/// its configuration exists. `cwd` and `nixos_config` are the current
/// directory and `$NIXOS_CONFIG`.
pub fn resolve(
	options: &mut Options,
	cwd: &Path,
	nixos_config: Option<OsString>,
) -> Result<(), String> {
	if !options.root.exists() {
		return Err(format!(
			"mount point {} doesn't exist",
			options.root.display()
		));
	}
	let real = fs::canonicalize(&options.root)
		.map_err(|e| format!("cannot resolve {}: {e}", options.root.display()))?;
	for dir in real.ancestors().filter(|dir| *dir != Path::new("/")) {
		let mode = fs::metadata(dir)
			.map_err(|e| format!("cannot read {}: {e}", dir.display()))?
			.permissions()
			.mode();
		if mode & 0o7 < 5 {
			return Err(format!(
				"path {} should have permissions 755, but had permissions {:o}. Consider running 'chmod o+rx {}'.",
				dir.display(),
				mode & 0o7777,
				dir.display()
			));
		}
	}
	let method = match options.method.take() {
		Some(Method::File { file: None, attr }) => Method::File {
			file: find_by_attrset(cwd),
			attr,
		},
		Some(method) => method,
		None => match find_by_attrset(cwd) {
			Some(file) => Method::File {
				file: Some(file),
				attr: None,
			},
			None => match find_module(&options.root, nixos_config) {
				Some(module) => Method::Module(Some(module)),
				None => return Err("no build method found".to_owned()),
			},
		},
	};
	match &method {
		Method::Module(Some(module)) if !module.is_absolute() => {
			return Err("$NIXOS_CONFIG is not an absolute path".to_owned());
		}
		Method::Module(Some(module)) if !module.exists() => {
			return Err(format!(
				"configuration file {} doesn't exist",
				module.display()
			));
		}
		Method::File { file, .. } if !file.as_deref().is_some_and(Path::exists) => {
			return Err(format!(
				"configuration file {} doesn't exist",
				file.as_deref().unwrap_or(Path::new("")).display()
			));
		}
		_ => {}
	}
	options.method = Some(method);
	Ok(())
}

/// The channel the running system's root uses, as `nix-env -p
/// /nix/var/nix/profiles/per-user/root/channels -q nixos --out-path`
/// reports it: the path in `store` holding the profile's `nixos` entry.
pub fn running_channel(profile: &Path, store: &Path) -> Option<PathBuf> {
	let target = fs::canonicalize(profile.join("nixos")).ok()?;
	let name = target.strip_prefix(store).ok()?.components().next()?;
	Some(store.join(name))
}

/// What one step of the plan will do, or why it is skipped.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Plan {
	pub root: PathBuf,
	pub source: String,
	pub channel: Result<PathBuf, &'static str>,
	pub bootloader: bool,
	pub root_password: Result<(), &'static str>,
}

impl Plan {
	pub fn new(options: &Options, channel: Option<PathBuf>, terminal: bool) -> Plan {
		let source = match options.method.as_ref() {
			Some(Method::Flake { flake, name }) => {
				format!("flake {flake}#{name} (nixosConfigurations.\"{name}\")")
			}
			Some(Method::File { file, attr }) => format!(
				"{}{}",
				file.as_deref().unwrap_or(Path::new("")).display(),
				attr.as_ref()
					.map(|attr| format!(", attribute {attr}"))
					.unwrap_or_default()
			),
			Some(Method::Module(module)) => format!(
				"module {}",
				module.as_deref().unwrap_or(Path::new("")).display()
			),
			Some(Method::System(path)) => format!("the built system {}", path.display()),
			None => String::new(),
		};
		let channel = if options.no_channel_copy {
			Err("--no-channel-copy")
		} else if matches!(options.method, Some(Method::Flake { .. })) {
			Err("a flake system does not use it")
		} else {
			channel.ok_or("the running system has no root `nixos` channel")
		};
		let root_password = if options.no_root_passwd {
			Err("--no-root-passwd")
		} else if !terminal {
			Err("standard input is not a terminal")
		} else {
			Ok(())
		};
		Plan {
			root: options.root.clone(),
			source,
			channel,
			bootloader: !options.no_bootloader,
			root_password,
		}
	}

	/// The plan as status lines.
	pub fn render(&self, err: &mut dyn Write, styling: &Styling) {
		let root = self.root.display();
		let line = |err: &mut dyn Write, verb: &str, text: String| {
			let _ = status::status(err, styling, verb, Tone::Progress, &text);
		};
		line(err, "Planning", format!("installation into {root}"));
		line(err, "Source", self.source.clone());
		line(
			err,
			"Store",
			format!(
				"build into {root}/nix/store, copying paths from the running system's store ({SUBSTITUTER})"
			),
		);
		line(
			err,
			"Profile",
			format!("{}{DEFAULT_PROFILE} and {root}/etc/NIXOS", root),
		);
		line(
			err,
			"Channel",
			match &self.channel {
				Ok(path) => format!("{} into {root}{CHANNELS_PROFILE}", path.display()),
				Err(reason) => format!("skipped: {reason}"),
			},
		);
		line(
			err,
			"Bootloader",
			if self.bootloader {
				format!("switch-to-configuration boot inside {root}")
			} else {
				"skipped: --no-bootloader".to_owned()
			},
		);
		line(
			err,
			"Password",
			match self.root_password {
				Ok(()) => format!("passwd for root inside {root}, asked on the terminal"),
				Err(reason) => format!("skipped: {reason}"),
			},
		);
	}
}

/// Run a compatibility command entry in-process, on a thread with the
/// evaluator's stack and under the lock that serialises the shared
/// implementation's process-wide state, as `noxi system` does.
fn compat(
	name: &str,
	flakes: bool,
	entry: fn(Settings, Vec<String>) -> Result<ExitCode, Failure>,
	args: Vec<String>,
) -> Result<(), String> {
	let _guard = crate::flake::LOCK
		.lock()
		.unwrap_or_else(|poisoned| poisoned.into_inner());
	let settings = crate::rebuild::load_settings(flakes)?;
	let shown = format!("{name} {}", args.join(" "));
	let result = std::thread::Builder::new()
		.stack_size(crate::rebuild::STACK_SIZE)
		.spawn(move || entry(settings, args))
		.map_err(|e| format!("cannot start the evaluator thread: {e}"))?
		.join()
		.map_err(|_| format!("`{shown}` panicked"))?;
	match result {
		Ok(code) if code == ExitCode::SUCCESS => Ok(()),
		Ok(_) => Err(format!("`{shown}` failed; the cause is reported above")),
		Err(Failure::Usage(message) | Failure::Error(message) | Failure::Exit(_, message)) => {
			Err(format!("`{shown}` failed: {message}"))
		}
	}
}

fn path_arg(path: &Path) -> String {
	path.to_string_lossy().into_owned()
}

/// `noxi system enter --root ROOT -c COMMAND` as a child process: entering
/// replaces the process and needs a single-threaded one.
fn enter(root: &Path, command: &str, bootloader: bool) -> Result<std::process::ExitStatus, String> {
	let exe = std::env::current_exe().map_err(|e| format!("cannot find the noxi binary: {e}"))?;
	let mut child = Command::new(exe);
	child
		.args(["system", "enter", "--root"])
		.arg(root)
		.args(["-c", command]);
	if bootloader {
		child
			.env("NIXOS_INSTALL_BOOTLOADER", "1")
			.env("mountPoint", root);
	}
	child
		.status()
		.map_err(|e| format!("cannot run noxi system enter: {e}"))
}

/// Run the installation `options` describes; `options` is resolved.
pub fn install(options: &Options, err: &mut dyn Write, styling: &Styling) -> Result<(), String> {
	let root = &options.root;
	let terminal = std::io::stdin().is_terminal();
	let channel = options.channel.clone().or_else(|| {
		running_channel(
			Path::new(CHANNELS_PROFILE),
			Path::new(nix_compat::store_path::store_dir()),
		)
	});
	let plan = Plan::new(options, channel, terminal);
	plan.render(err, styling);
	let step = |err: &mut dyn Write, verb: &str, text: &str| {
		let _ = status::status(err, styling, verb, Tone::Progress, text);
	};

	// The script's `mktemp -d -p "$mountPoint"` and TMPDIR default.
	let tmp = tempfile::Builder::new()
		.prefix("tmp.")
		.tempdir_in(root)
		.map_err(|e| {
			format!(
				"cannot create a temporary directory in {}: {e}",
				root.display()
			)
		})?;
	if std::env::var_os("TMPDIR").is_none() {
		// SAFETY: `noxi install` sets this before it starts any thread.
		unsafe { std::env::set_var("TMPDIR", tmp.path()) };
	}
	let store = path_arg(root);
	let common = |mut args: Vec<String>| {
		args.extend(["--store".to_owned(), store.clone()]);
		args.extend(options.build_flags.iter().cloned());
		args.extend(["--extra-substituters".to_owned(), SUBSTITUTER.to_owned()]);
		args
	};

	if let Ok(channel) = &plan.channel {
		step(err, "Copying", "channel");
		let profile = under(root, CHANNELS_PROFILE);
		fs::create_dir_all(profile.parent().expect("profile has a parent"))
			.map_err(|e| format!("cannot create {}: {e}", profile.display()))?;
		let mut args = common(Vec::new());
		args.extend([
			"-p".to_owned(),
			path_arg(&profile),
			"--set".to_owned(),
			path_arg(channel),
			"--quiet".to_owned(),
		]);
		args.extend(options.verbosity.iter().cloned());
		compat("nix-env", false, noxi_commands::nix_env::main, args)?;
		let defexpr = under(root, "/root/.nix-defexpr");
		fs::create_dir_all(&defexpr)
			.and_then(|()| fs::set_permissions(&defexpr, fs::Permissions::from_mode(0o700)))
			.map_err(|e| format!("cannot create {}: {e}", defexpr.display()))?;
		replace_symlink(Path::new(CHANNELS_PROFILE), &defexpr.join("channels"))?;
	}

	let system = match options.method.as_ref().expect("resolved") {
		Method::System(path) => path.clone(),
		method => {
			let link = tmp.path().join("system");
			let link_arg = path_arg(&link);
			match method {
				Method::Flake { flake, name } => {
					step(err, "Building", &format!("the flake in {flake}"));
					let mut args = vec![
						"build".to_owned(),
						format!(
							"{flake}#nixosConfigurations.\"{name}\".config.system.build.toplevel"
						),
					];
					args = common(args);
					args.extend(options.verbosity.iter().cloned());
					args.extend(options.lock_flags.iter().cloned());
					args.extend(["--out-link".to_owned(), link_arg]);
					compat("nix", true, noxi_commands::nix::main, args)?;
				}
				Method::Module(module) => {
					let module = module.as_deref().expect("resolved");
					step(
						err,
						"Building",
						&format!("the configuration in {}", module.display()),
					);
					let mut args = common(vec!["--out-link".to_owned(), link_arg]);
					args.extend([
						"<nixpkgs/nixos>".to_owned(),
						"-A".to_owned(),
						"system".to_owned(),
						"-I".to_owned(),
						format!("nixos-config={}", module.display()),
					]);
					args.extend(options.verbosity.iter().cloned());
					compat("nix-build", false, noxi_commands::nix_build::main, args)?;
				}
				Method::File { file, attr } => {
					let file = file.as_deref().expect("resolved");
					step(
						err,
						"Building",
						&match attr {
							Some(attr) => format!(
								"the configuration in {} and attribute {attr}",
								file.display()
							),
							None => format!("the configuration in {}", file.display()),
						},
					);
					let mut args = common(vec!["--out-link".to_owned(), link_arg]);
					args.extend([
						path_arg(file),
						"-A".to_owned(),
						format!(
							"{}config.system.build.toplevel",
							attr.as_ref().map(|a| format!("{a}.")).unwrap_or_default()
						),
					]);
					args.extend(options.verbosity.iter().cloned());
					compat("nix-build", false, noxi_commands::nix_build::main, args)?;
				}
				Method::System(_) => unreachable!(),
			}
			fs::read_link(&link).map_err(|e| {
				format!(
					"the build did not leave {} pointing at a system: {e}",
					link.display()
				)
			})?
		}
	};

	step(
		err,
		"Profile",
		&format!("{} set to {}", DEFAULT_PROFILE, system.display()),
	);
	let mut args = common(Vec::new());
	args.extend([
		"-p".to_owned(),
		path_arg(&under(root, DEFAULT_PROFILE)),
		"--set".to_owned(),
		path_arg(&system),
	]);
	args.extend(options.verbosity.iter().cloned());
	compat("nix-env", false, noxi_commands::nix_env::main, args)?;

	// switch-to-configuration refuses a root without it.
	let etc = under(root, "/etc");
	fs::create_dir_all(&etc)
		.and_then(|()| fs::set_permissions(&etc, fs::Permissions::from_mode(0o755)))
		.and_then(|()| {
			fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(etc.join("NIXOS"))
				.map(drop)
		})
		.map_err(|e| format!("cannot create {}/NIXOS: {e}", etc.display()))?;

	if plan.bootloader {
		step(err, "Installing", "the boot loader");
		// GRUB needs an mtab.
		replace_symlink(Path::new("/proc/mounts"), &etc.join("mtab"))?;
		let status = enter(root, BOOTLOADER_SCRIPT, true)?;
		if !status.success() {
			return Err(format!("installing the boot loader failed ({status})"));
		}
	}

	if plan.root_password.is_ok()
		&& enter(
			root,
			"test -e /nix/var/nix/profiles/system/sw/bin/passwd",
			false,
		)?
		.success()
	{
		let status = enter(
			root,
			"echo \"setting root password...\" && /nix/var/nix/profiles/system/sw/bin/passwd",
			false,
		)?;
		if !status.success() {
			return Err(format!(
				"setting a root password failed with the above printed error; you can set it manually by running `noxi system enter --root {}` and then `passwd` in the shell of the new system",
				root.display()
			));
		}
	}
	let _ = writeln!(err, "installation finished!");
	Ok(())
}

/// `ln -sfn TARGET LINK`.
fn replace_symlink(target: &Path, link: &Path) -> Result<(), String> {
	match fs::remove_file(link) {
		Ok(()) => {}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
		Err(e) => return Err(format!("cannot replace {}: {e}", link.display())),
	}
	symlink(target, link).map_err(|e| format!("cannot link {}: {e}", link.display()))
}

pub fn main(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	let styling = Styling::for_stderr();
	let mut options = match parse(args) {
		Ok(Some(options)) => options,
		Ok(None) => {
			let _ = writeln!(out, "{USAGE}");
			return ExitCode::SUCCESS;
		}
		Err(message) => {
			let _ = writeln!(err, "error: {message}\n{USAGE}");
			return ExitCode::from(2);
		}
	};
	let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"));
	if let Err(message) = resolve(&mut options, &cwd, std::env::var_os("NIXOS_CONFIG")) {
		return crate::fail(err, &styling, None, &message);
	}
	match install(&options, err, &styling) {
		Ok(()) => ExitCode::SUCCESS,
		Err(message) => crate::fail(err, &styling, None, &message),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn args(list: &[&str]) -> Vec<String> {
		list.iter().map(|arg| (*arg).to_owned()).collect()
	}

	#[test]
	fn flags_follow_the_script() {
		let options = parse(args(&[
			"--root",
			"/target",
			"--flake",
			"/etc/nixos#host",
			"-j",
			"4",
			"--option",
			"a",
			"b",
			"--no-root-passwd",
			"--no-bootloader",
			"--no-channel-copy",
			"-vv",
			"--no-write-lock-file",
		]))
		.unwrap()
		.unwrap();
		assert_eq!(options.root, PathBuf::from("/target"));
		assert_eq!(
			options.method,
			Some(Method::Flake {
				flake: "/etc/nixos".to_owned(),
				name: "host".to_owned()
			})
		);
		assert_eq!(
			options.build_flags,
			args(&["-j", "4", "--option", "a", "b"])
		);
		assert_eq!(options.lock_flags, args(&["--no-write-lock-file"]));
		assert_eq!(options.verbosity, args(&["-vv"]));
		assert!(options.no_root_passwd && options.no_bootloader && options.no_channel_copy);
		assert_eq!(parse(args(&["--help"])).unwrap(), None);
	}

	#[test]
	fn rejects_what_the_script_rejects() {
		assert!(
			parse(args(&["--flake", "/etc/nixos"]))
				.unwrap_err()
				.contains("URI fragment")
		);
		assert!(
			parse(args(&["--flake", "/a#b", "--system", "/nix/store/x"]))
				.unwrap_err()
				.contains("multiple build methods")
		);
		assert!(
			parse(args(&["--bogus"]))
				.unwrap_err()
				.contains("unknown option")
		);
		assert!(
			parse(args(&["--root"]))
				.unwrap_err()
				.contains("requires an argument")
		);
	}

	#[test]
	fn finds_the_configuration_like_the_script() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("mnt");
		fs::create_dir_all(root.join("etc/nixos")).unwrap();
		fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
		let cwd = dir.path().join("work");
		fs::create_dir_all(&cwd).unwrap();
		let mut options = Options {
			root: root.clone(),
			..Options::default()
		};
		// Nothing to build.
		if find_by_attrset(&cwd).is_none() {
			assert_eq!(
				resolve(&mut options.clone(), &cwd, None).unwrap_err(),
				"no build method found"
			);
		}
		// The root's configuration.nix, when no system.nix or default.nix
		// is found upwards.
		fs::write(root.join("etc/nixos/configuration.nix"), "{ }").unwrap();
		if find_by_attrset(&cwd).is_none() {
			resolve(&mut options, &cwd, None).unwrap();
			assert_eq!(
				options.method,
				Some(Method::Module(Some(
					root.join("etc/nixos/configuration.nix")
				)))
			);
		}
		// A system.nix in a parent of the current directory comes first.
		fs::write(dir.path().join("system.nix"), "{ }").unwrap();
		let mut options = Options {
			root: root.clone(),
			..Options::default()
		};
		resolve(&mut options, &cwd, None).unwrap();
		assert_eq!(
			options.method,
			Some(Method::File {
				file: Some(dir.path().join("system.nix")),
				attr: None
			})
		);
		// A relative NIXOS_CONFIG is refused.
		let mut options = Options {
			root: root.clone(),
			method: Some(Method::Module(Some(PathBuf::from("configuration.nix")))),
			..Options::default()
		};
		assert!(
			resolve(&mut options, &cwd, None)
				.unwrap_err()
				.contains("not an absolute path")
		);
	}

	#[test]
	fn refuses_a_root_others_cannot_enter() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().join("mnt");
		fs::create_dir(&root).unwrap();
		fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
		fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
		let mut options = Options {
			root: root.clone(),
			method: Some(Method::System(PathBuf::from("/nix/store/x"))),
			..Options::default()
		};
		let error = resolve(&mut options, dir.path(), None).unwrap_err();
		assert!(error.contains("should have permissions 755"), "{error}");
		let mut options = Options {
			root: dir.path().join("missing"),
			..Options::default()
		};
		assert!(
			resolve(&mut options, dir.path(), None)
				.unwrap_err()
				.contains("doesn't exist")
		);
	}

	#[test]
	fn plan_skips_the_channel_for_flakes_and_the_password_without_a_terminal() {
		let options = Options {
			method: Some(Method::Flake {
				flake: "/mnt/etc/nixos".to_owned(),
				name: "host".to_owned(),
			}),
			..Options::default()
		};
		let plan = Plan::new(&options, Some(PathBuf::from("/nix/store/c-nixos")), false);
		assert_eq!(plan.channel, Err("a flake system does not use it"));
		assert_eq!(plan.root_password, Err("standard input is not a terminal"));
		assert!(plan.bootloader);
		let mut rendered = Vec::new();
		plan.render(&mut rendered, &Styling::PLAIN);
		let rendered = String::from_utf8(rendered).unwrap();
		assert!(rendered.contains("flake /mnt/etc/nixos#host"), "{rendered}");
		assert!(rendered.contains("auto?trusted=1"), "{rendered}");
		let options = Options {
			method: Some(Method::Module(Some(PathBuf::from(
				"/mnt/etc/nixos/configuration.nix",
			)))),
			..Options::default()
		};
		let plan = Plan::new(&options, Some(PathBuf::from("/nix/store/c-nixos")), true);
		assert_eq!(plan.channel, Ok(PathBuf::from("/nix/store/c-nixos")));
		assert_eq!(plan.root_password, Ok(()));
	}

	#[test]
	fn running_channel_is_the_store_path_of_the_nixos_entry() {
		let dir = tempfile::tempdir().unwrap();
		let store = dir.path().join("store");
		let channel = store.join("00000000000000000000000000000000-nixos-26.11");
		fs::create_dir_all(channel.join("nixos")).unwrap();
		let profile = dir.path().join("channels");
		fs::create_dir(&profile).unwrap();
		symlink(channel.join("nixos"), profile.join("nixos")).unwrap();
		let store = fs::canonicalize(&store).unwrap();
		assert_eq!(
			running_channel(&profile, &store),
			Some(fs::canonicalize(&channel).unwrap())
		);
		assert_eq!(running_channel(&dir.path().join("none"), &store), None);
	}
}
