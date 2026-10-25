//! New-style `nix` commands. Unsupported library operations fail explicitly.
mod args;
mod command_store;
pub mod execution;
mod expression;
mod inspection;
pub(crate) mod pure;
mod table;
mod transfer;

use crate::cli::{Failure, apply_common_flag, print_setting_warnings};
use nix_compat::nixcpp::settings::Settings;
use std::collections::BTreeMap;
use std::process::ExitCode;

fn unsupported(operation: &str) -> Failure {
	Failure::Error(format!("nix {operation} is not supported by nix-rs yet"))
}

pub(super) struct Parsed {
	pub command: &'static str,
	pub settings: Settings,
	pub flags: BTreeMap<String, Vec<Vec<String>>>,
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
#[derive(Default)]
struct Handler {
	flags: BTreeMap<String, Vec<Vec<String>>>,
	operands: Vec<String>,
}
impl args::Handler<Program> for Handler {
	fn flag(&mut self, ctx: &mut Program, name: &str, values: Vec<String>) -> Result<(), Failure> {
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
	fn handler(&mut self, _: &'static args::Command) -> Handler {
		Handler::default()
	}
	fn warn(&mut self, message: &str) {
		eprintln!("warning: {message}");
	}
	fn global_flag(
		&mut self,
		flag: &'static args::Flag,
		values: Vec<String>,
	) -> Result<(), Failure> {
		let canonical = format!("--{}", flag.name);
		let mut iter = values.clone().into_iter();
		if let Some(result) = apply_common_flag(&canonical, &mut iter, &mut self.settings) {
			result?;
		} else if let Some(result) = self.settings.apply_flag(&canonical, || iter.next()) {
			result.map_err(|e| Failure::Usage(e.to_string()))?;
		} else {
			match flag.name {
				"offline" | "no-net" => {
					self.settings
						.set("substitute", "false")
						.map_err(|e| e.to_string())?;
				}
				"version" | "help" | "help-config" | "store" | "print-build-logs"
				| "no-gc-warning" => {}
				_ => return Err(unsupported(&canonical)),
			}
		}
		if flag.name == "print-build-logs" {
			nix_compat::log::set_print_build_logs(true);
		}
		self.flags.entry(flag.name.into()).or_default().push(values);
		Ok(())
	}
}
fn gate(settings: &Settings, feature: &str) -> Result<(), Failure> {
	if settings.experimental_features().contains(feature) {
		return Ok(());
	}
	Err(Failure::Error(format!(
		"experimental Nix feature '{feature}' is disabled; add '--extra-experimental-features {feature}' to enable it"
	)))
}

pub fn main(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	nix_compat::log::set_print_build_logs(false);
	if let Ok(path) = std::env::var("NIX_PATH") {
		settings
			.set("nix-path", &nix_rs_eval::parse_nix_path(&path).join(" "))
			.map_err(|e| e.to_string())?;
	}
	let mut program = Program {
		settings,
		flags: BTreeMap::new(),
	};
	let parsed = args::parse(&mut program, args)?;
	if program.flags.contains_key("version") {
		println!("nix (Nix) 2.35.2");
		return Ok(ExitCode::SUCCESS);
	}
	if program.flags.contains_key("help") {
		return Err(unsupported("--help"));
	}
	let mut levels = parsed.levels;
	let level = levels
		.pop()
		.ok_or_else(|| Failure::Usage("no subcommand specified".into()))?;
	if level.command.is_multi() {
		return Err(Failure::Usage("no subcommand specified".into()));
	}
	print_setting_warnings(&mut program.settings);
	if let Some(feature) = level.command.feature {
		gate(&program.settings, feature)?;
	}
	for feature in parsed.flag_features {
		gate(&program.settings, feature)?;
	}
	let supported: &[&str] = match level.command.path {
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
		if !supported.contains(&name.as_str()) {
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
		operands: level.handler.operands,
	};
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
		"develop" => Err(unsupported(
			"develop (build-environment capture is not available in the shared library)",
		)),
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
