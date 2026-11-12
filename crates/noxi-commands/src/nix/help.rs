//! `nix help` and `nix help-stores`, following CppNix 2.35.2 `showHelp`.
//!
//! CppNix renders a manual page generated from its command documentation,
//! which noxi does not embed. The page lookup and its usage error match the
//! reference; rendering an existing page fails explicitly.
use super::{Parsed, table::COMMANDS, unsupported};
use crate::cli::Failure;
use std::process::ExitCode;

/// `NixArgs::aliases`, which `showHelp` applies to a single word.
const ALIASES: &[(&str, &[&str])] = &[
	("add-to-store", &["store", "add-path"]),
	("cat-nar", &["nar", "cat"]),
	("cat-store", &["store", "cat"]),
	("copy-sigs", &["store", "copy-sigs"]),
	("dev-shell", &["develop"]),
	("diff-closures", &["store", "diff-closures"]),
	("dump-path", &["store", "dump-path"]),
	("hash-file", &["hash", "file"]),
	("hash-path", &["hash", "path"]),
	("ls-nar", &["nar", "ls"]),
	("ls-store", &["store", "ls"]),
	(
		"make-content-addressable",
		&["store", "make-content-addressed"],
	),
	("optimise-store", &["store", "optimise"]),
	("ping-store", &["store", "info"]),
	("realisation", &["store", "build-trace"]),
	("sign-paths", &["store", "sign"]),
	("shell", &["env", "shell"]),
	("show-derivation", &["derivation", "show"]),
	("show-config", &["config", "show"]),
	("to-base16", &["hash", "to-base16"]),
	("to-base32", &["hash", "to-base32"]),
	("to-base64", &["hash", "to-base64"]),
	("verify", &["store", "verify"]),
	("doctor", &["config", "check"]),
];

pub fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	let mut subcommand: Vec<String> = if parsed.command == "help-stores" {
		vec!["help-stores".into()]
	} else {
		parsed.operands.clone()
	};
	if let [word] = subcommand.as_slice() {
		if let Some((_, replacement)) = ALIASES.iter().find(|(name, _)| name == word) {
			subcommand = replacement.iter().map(|s| (*s).to_owned()).collect();
		}
	}
	// The generated pages are `nix` and `nix3-PATH` for every command, with
	// the words of its path joined by dashes.
	let page = subcommand.join("-");
	let exists = page.is_empty()
		|| COMMANDS
			.iter()
			.any(|command| command.path.replace(' ', "-") == page);
	if !exists {
		return Err(Failure::Usage(format!(
			"Nix has no subcommand '{}'",
			subcommand.concat()
		)));
	}
	let name = if page.is_empty() {
		"nix".to_owned()
	} else {
		format!("nix3-{page}")
	};
	Err(unsupported(&format!(
		"{} rendering the '{name}' manual page",
		parsed.command
	)))
}
