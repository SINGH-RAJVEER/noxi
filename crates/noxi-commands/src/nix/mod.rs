//! New-style `nix` commands. Unsupported library operations fail explicitly.
mod args;
mod command_store;
mod config;
mod develop;
pub mod execution;
mod expression;
pub(crate) mod flakes;
pub mod hash;
mod inspection;
mod profile;
pub(crate) mod pure;
mod registry;
mod table;
mod transfer;

use crate::cli::{Failure, apply_common_flag, print_setting_warnings};
use nix_compat::nixcpp::settings::Settings;
use std::collections::BTreeMap;
use std::process::ExitCode;

fn unsupported(operation: &str) -> Failure {
	Failure::Error(format!("nix {operation} is not supported by noxi yet"))
}

pub(super) struct Parsed {
	pub command: &'static str,
	pub settings: Settings,
	pub flags: BTreeMap<String, Vec<Vec<String>>>,
	/// The command's own flags in the order they were given.
	pub sequence: Vec<(String, Vec<String>)>,
	pub operands: Vec<String>,
}
impl Parsed {
	pub fn has(&self, name: &str) -> bool {
		self.flags.contains_key(name)
	}
	pub fn value(&self, name: &str) -> Option<&str> {
		self.flags.get(name)?.last()?.first().map(String::as_str)
	}
	pub fn unsupported(&self, names: &[&str]) -> Result<(), Failure> {
		for name in names {
			if self.has(name) {
				return Err(unsupported(&format!("{} --{name}", self.command)));
			}
		}
		Ok(())
	}
}
struct Program {
	settings: Settings,
	flags: BTreeMap<String, Vec<Vec<String>>>,
}
struct Handler {
	command: &'static str,
	flags: BTreeMap<String, Vec<Vec<String>>>,
	sequence: Vec<(String, Vec<String>)>,
	operands: Vec<String>,
}
impl args::Handler<Program> for Handler {
	fn flag(&mut self, ctx: &mut Program, name: &str, values: Vec<String>) -> Result<(), Failure> {
		hash::check_flag(&ctx.settings, self.command, name, &values)?;
		if name == "no-link" {
			self.flags.remove("out-link");
		}
		if name == "out-link" {
			self.flags.remove("no-link");
		}
		if name == "command" && values.is_empty() {
			return Err(Failure::Usage(
				"--command requires at least one argument".into(),
			));
		}
		if name == "set-env-var" {
			if self
				.flags
				.get("set-env-var")
				.is_some_and(|items| items.iter().any(|v| v[0] == values[0]))
			{
				return Err(Failure::Usage(format!(
					"Duplicate definition of environment variable '{}' with '--set-env-var' is ambiguous",
					values[0]
				)));
			}
			if ["unset-env-var", "unset"]
				.into_iter()
				.filter_map(|key| self.flags.get(key))
				.flatten()
				.any(|v| v[0] == values[0])
			{
				return Err(Failure::Usage(format!(
					"Cannot set environment variable '{}' that is unset with '--unset-env-var'",
					values[0]
				)));
			}
		}
		if matches!(name, "unset-env-var" | "unset")
			&& self
				.flags
				.get("set-env-var")
				.is_some_and(|items| items.iter().any(|v| v[0] == values[0]))
		{
			return Err(Failure::Usage(format!(
				"Cannot unset environment variable '{}' that is set with '--set-env-var'",
				values[0]
			)));
		}
		if name == "impure" {
			ctx.settings
				.set("pure-eval", "false")
				.map_err(|e| e.to_string())?;
		}
		self.sequence.push((name.into(), values.clone()));
		self.flags.entry(name.into()).or_default().push(values);
		Ok(())
	}
	fn args(&mut self, _: &mut Program, _: &str, values: Vec<String>) -> Result<(), Failure> {
		self.operands.extend(values);
		Ok(())
	}
}
impl args::Program for Program {
	type Handler = Handler;
	fn handler(&mut self, command: &'static args::Command) -> Handler {
		Handler {
			command: command.path,
			flags: BTreeMap::new(),
			sequence: Vec::new(),
			operands: Vec::new(),
		}
	}
	fn warn(&mut self, message: &str) {
		warn(message);
	}
	fn global_flag(
		&mut self,
		flag: &'static args::Flag,
		values: Vec<String>,
	) -> Result<(), Failure> {
		match flag.name {
			"help" | "version" => {}
			"offline" | "no-net" => {
				self.settings
					.set("substitute", "false")
					.map_err(|e| e.to_string())?;
			}
			"print-build-logs" => nix_compat::log::set_print_build_logs(true),
			"refresh" => return Err(unsupported("--refresh")),
			_ => apply_setting_flag(&mut self.settings, flag, values.clone())?,
		}
		self.flags.entry(flag.name.into()).or_default().push(values);
		Ok(())
	}
}

/// The global flags of `nix` that the legacy commands do not have.
const NIX_ONLY_FLAGS: &[&str] = &[
	"help",
	"version",
	"offline",
	"no-net",
	"refresh",
	"print-build-logs",
];

fn warn(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
		eprintln!("warning: {message}");
	}
}

/// Apply one of `MixCommonArgs`' flags: the logging flags, `--max-jobs`,
/// `--option`, whose invalid values are only warned about, and the flags
/// generated from the settings.
fn apply_setting_flag(
	settings: &mut Settings,
	flag: &'static args::Flag,
	values: Vec<String>,
) -> Result<(), Failure> {
	let canonical = format!("--{}", flag.name);
	let mut iter = values.into_iter();
	if let Some(result) = apply_common_flag(&canonical, &mut iter, settings) {
		return result;
	}
	if flag.name == "option" {
		let (name, value) = (
			iter.next().unwrap_or_default(),
			iter.next().unwrap_or_default(),
		);
		if let Err(error) = settings.set(&name, &value) {
			warn(&error.to_string());
		}
		return Ok(());
	}
	match settings.apply_flag(&canonical, || iter.next()) {
		Some(result) => result.map_err(|e| Failure::Usage(e.to_string())),
		None => Err(unsupported(&canonical)),
	}
}

fn gate(settings: &Settings, feature: &str) -> Result<(), Failure> {
	if settings.experimental_features().contains(feature) {
		return Ok(());
	}
	Err(Failure::Error(nix_compat::nixcpp::hash::missing_feature(
		feature,
	)))
}

/// `haveInternet`: a non-loopback IPv4 address, a non-loopback and
/// non-link-local IPv6 address, or a proxy variable.
fn have_internet() -> bool {
	let mut addrs: *mut libc::ifaddrs = std::ptr::null_mut();
	// SAFETY: getifaddrs fills in a list that freeifaddrs releases below.
	if unsafe { libc::getifaddrs(&mut addrs) } != 0 {
		return true;
	}
	let mut found = false;
	let mut cursor = addrs;
	while !cursor.is_null() && !found {
		// SAFETY: cursor walks the list getifaddrs returned.
		let ifa = unsafe { &*cursor };
		cursor = ifa.ifa_next;
		if ifa.ifa_addr.is_null() {
			continue;
		}
		// SAFETY: the family says which sockaddr type the address is.
		match i32::from(unsafe { (*ifa.ifa_addr).sa_family }) {
			libc::AF_INET => {
				let addr = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in) };
				found = u32::from_be(addr.sin_addr.s_addr) != libc::INADDR_LOOPBACK;
			}
			libc::AF_INET6 => {
				let addr = unsafe { &*(ifa.ifa_addr as *const libc::sockaddr_in6) };
				let ip = std::net::Ipv6Addr::from(addr.sin6_addr.s6_addr);
				found = !ip.is_loopback() && !ip.is_unicast_link_local();
			}
			_ => {}
		}
	}
	// SAFETY: addrs came from getifaddrs.
	unsafe { libc::freeifaddrs(addrs) };
	// CppNix's upper-case proxy names are empty, so only these count.
	found
		|| ["http_proxy", "https_proxy", "ftp_proxy", "all_proxy"]
			.iter()
			.any(|name| std::env::var_os(name).is_some())
}

/// `NixMultiCommand::run` without a subcommand.
fn requires_subcommand(path: &str) -> Failure {
	let mut names: Vec<&str> = args::Command::subcommands(path).collect();
	names.sort_by_key(|name| format!("{name}`"));
	let list: Vec<String> = names.iter().map(|name| format!("  · {name}")).collect();
	Failure::Usage(format!(
		"nix {path} requires a sub-command. Available sub-commands:\n\n{}",
		list.join("\n")
	))
}

pub fn main(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	nix_compat::log::set_print_build_logs(false);
	if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
		nix_compat::log::set_verbosity(nix_compat::log::VerbosityLevel::Notice as u64);
	}
	if let Ok(path) = std::env::var("NIX_PATH") {
		settings
			.set("nix-path", &noxi_eval::parse_nix_path(&path).join(" "))
			.map_err(|e| e.to_string())?;
	}
	let mut program = Program {
		settings,
		flags: BTreeMap::new(),
	};
	// CppNix ignores usage errors when help was requested.
	let parsed = match args::parse(&mut program, args) {
		Err(Failure::Usage(_)) if program.flags.contains_key("help") => None,
		parsed => Some(parsed?),
	};
	print_setting_warnings(&mut program.settings);
	if let Some(parsed) = &parsed {
		for feature in &parsed.flag_features {
			gate(&program.settings, feature)?;
		}
	}
	if program.flags.contains_key("help") {
		return Err(unsupported("--help"));
	}
	let parsed = parsed.expect("parsed without help");
	if program.flags.contains_key("version") {
		if nix_compat::log::verbosity() > nix_compat::log::VerbosityLevel::Info as u64 {
			return Err(unsupported("--version with --verbose"));
		}
		println!("nix (Nix) 2.35.2");
		return Ok(ExitCode::SUCCESS);
	}
	let mut levels = parsed.levels;
	let top = levels
		.first()
		.ok_or_else(|| Failure::Usage("no subcommand specified".into()))?;
	if let Some(feature) = top.command.feature {
		gate(&program.settings, feature)?;
	}
	if !program.flags.contains_key("offline")
		&& !program.flags.contains_key("no-net")
		&& !have_internet()
	{
		warn("you don't have Internet access; disabling some network-dependent features");
		if program.settings.get("substitute").is_none() {
			program
				.settings
				.set("substitute", "false")
				.map_err(|e| e.to_string())?;
		}
	}
	let level = levels.pop().expect("a selected command");
	if level.command.is_multi() {
		return Err(requires_subcommand(level.command.path));
	}
	let supported: &[&str] = match level.command.path {
		"config show" => &["json"],
		"profile add" => &[
			"profile", "priority", "expr", "file", "arg", "argstr", "include", "impure",
		],
		"profile list" => &["profile", "json"],
		"profile remove" => &["profile", "all"],
		"profile rollback" => &["profile", "dry-run"],
		"profile wipe-history" => &["profile", "dry-run", "older-than"],
		"registry add" | "registry remove" | "registry pin" => &["registry"],
		"develop" => &[
			"arg",
			"argstr",
			"expr",
			"file",
			"include",
			"impure",
			"command",
			"ignore-env",
			"ignore-environment",
			"keep-env-var",
			"keep",
			"unset-env-var",
			"unset",
			"set-env-var",
			"profile",
			"phase",
			"unpack",
			"configure",
			"build",
			"check",
			"install",
			"installcheck",
			"redirect",
		],
		"print-dev-env" => &[
			"arg", "argstr", "expr", "file", "include", "impure", "json", "profile", "redirect",
		],
		"derivation show" => &[
			"arg",
			"argstr",
			"expr",
			"file",
			"include",
			"impure",
			"recursive",
		],
		"log" => &["arg", "argstr", "expr", "file", "include", "impure"],
		"copy" => &[
			"to",
			"from",
			"no-check-sigs",
			"arg",
			"argstr",
			"expr",
			"file",
			"include",
			"impure",
		],
		"env shell" | "run" => &[
			"arg",
			"argstr",
			"expr",
			"file",
			"include",
			"impure",
			"command",
			"ignore-env",
			"ignore-environment",
			"keep-env-var",
			"keep",
			"unset-env-var",
			"unset",
			"set-env-var",
		],
		"eval" => &[
			"apply",
			"arg",
			"argstr",
			"expr",
			"file",
			"include",
			"impure",
			"raw",
			"json",
			"read-only",
		],
		"build" => &[
			"arg",
			"argstr",
			"expr",
			"file",
			"include",
			"impure",
			"json",
			"dry-run",
			"no-link",
			"out-link",
			"print-out-paths",
		],
		"path-info" => &[
			"json",
			"json-format",
			"recursive",
			"size",
			"closure-size",
			"human-readable",
			"sigs",
		],
		"store add" | "store add-path" | "store add-file" => &["name", "mode", "hash-algo"],
		"store ls" => &["recursive", "long", "directory", "json"],
		"store delete" => &["recursive", "ignore-liveness"],
		"store gc" => &["dry-run", "max"],
		"store verify" => &["recursive", "no-contents", "no-trust", "sigs-needed"],
		"store info" => &["json"],
		_ => &[],
	};
	for name in level.handler.flags.keys() {
		if !level.command.path.starts_with("hash ")
			&& !supported.contains(&name.as_str())
			&& !(matches!(
				level.command.path,
				"eval" | "build" | "develop" | "print-dev-env" | "env shell" | "run"
			) && [
				"reference-lock-file",
				"output-lock-file",
				"no-write-lock-file",
				"no-update-lock-file",
				"override-input",
				"update-input",
				"recreate-lock-file",
			]
			.contains(&name.as_str()))
			&& !(level.command.path.starts_with("flake ")
				&& [
					"flake",
					"all-systems",
					"legacy",
					"no-build",
					"json",
					"reference-lock-file",
					"output-lock-file",
					"no-write-lock-file",
					"no-update-lock-file",
					"override-input",
					"update-input",
					"recreate-lock-file",
				]
				.contains(&name.as_str()))
		{
			return Err(unsupported(&format!("{} --{name}", level.command.path)));
		}
	}
	let mut flags = program.flags;
	for (name, values) in level.handler.flags {
		flags.insert(name, values);
	}
	let parsed = Parsed {
		command: level.command.path,
		settings: program.settings,
		flags,
		sequence: level.handler.sequence,
		operands: level.handler.operands,
	};
	if parsed.command.starts_with("hash ") {
		return hash::run(parsed);
	}
	if parsed.command == "config show" {
		return config::show(parsed);
	}
	// With build logs, CppNix's `bar` formats (the default) print each
	// build log line after `NAME> `; the shared build backend writes raw
	// output, which only matches the `raw-with-logs` format.
	let bar = parsed
		.value("log-format")
		.is_none_or(|format| format.starts_with("bar"));
	if bar
		&& nix_compat::log::print_build_logs()
		&& matches!(
			parsed.command,
			"build" | "develop" | "print-dev-env" | "env shell" | "run" | "eval"
		) {
		return Err(unsupported(&format!(
			"{} with build logs in the bar log format (use --log-format raw-with-logs)",
			parsed.command
		)));
	}
	ensure_diverted_root(&parsed)?;
	match parsed.command {
		"eval" => expression::eval(parsed),
		"build" => expression::build(parsed),
		"path-info" => command_store::path_info(parsed),
		"derivation show" => inspection::show(parsed),
		"log" => inspection::log(parsed),
		"copy" => transfer::copy(parsed),
		"env shell" => execution::shell(parsed),
		"run" => execution::run(parsed),
		"develop" => develop::develop(parsed),
		"print-dev-env" => develop::print_dev_env(parsed),
		command if command.starts_with("profile ") => profile::run(parsed),
		command if command.starts_with("registry ") => registry::run(parsed),
		command if command.starts_with("flake ") => flakes::run(parsed),
		command if command.starts_with("store ") => command_store::run(parsed),
		command => Err(unsupported(command)),
	}
}

/// Modern Nix creates a missing diverted root. The shared legacy opener
/// requires one to exist; validate its existing ancestor before creating it.
fn ensure_diverted_root(parsed: &Parsed) -> Result<(), Failure> {
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let Some((scheme, query)) = uri.split_once('?') else {
		return Ok(());
	};
	if !matches!(scheme, "local" | "auto") {
		return Ok(());
	}
	if query.split('&').any(|p| p == "read-only=true") {
		return Ok(());
	}
	let Some(root) = query.split('&').find_map(|p| p.strip_prefix("root=")) else {
		return Ok(());
	};
	if root.is_empty() {
		return Ok(());
	}
	let root =
		crate::instantiate::absolute_path(&std::env::current_dir()?, std::path::Path::new(root));
	if root.exists() {
		return Ok(());
	}
	let ancestor = root
		.ancestors()
		.find(|p| p.exists())
		.ok_or_else(|| Failure::Error("store root has no existing ancestor".into()))?
		.canonicalize()?;
	if root.starts_with("/nix") || ancestor.starts_with("/nix") {
		return Err(Failure::Error(format!(
			"store root '{}' must be a directory outside /nix",
			root.display()
		)));
	}
	std::fs::create_dir_all(root)?;
	Ok(())
}
