//! New-style `nix` commands. Unsupported library operations fail explicitly.
mod args;
mod command_store;
mod expression;
mod table;

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
	fn flag(&mut self, _: &mut Program, name: &str, values: Vec<String>) -> Result<(), Failure> {
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

pub fn main(settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
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
		"eval" => &[
			"apply", "arg", "argstr", "expr", "file", "include", "impure", "raw", "json",
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
	let mut parsed = Parsed {
		command: level.command.path,
		settings: program.settings,
		flags,
		operands: level.handler.operands,
	};
	if parsed.has("impure") {
		parsed
			.settings
			.set("pure-eval", "false")
			.map_err(|e| e.to_string())?;
	}
	match parsed.command {
		"eval" => expression::eval(parsed),
		"build" => expression::build(parsed),
		"path-info" => command_store::path_info(parsed),
		command if command.starts_with("store ") => command_store::run(parsed),
		command => Err(unsupported(command)),
	}
}
