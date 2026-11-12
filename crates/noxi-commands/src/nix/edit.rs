//! `nix edit`, following CppNix 2.35.2 `edit.cc`, `findPackageFilename`
//! and `editorFor`.
use std::path::PathBuf;
use std::process::{Command, ExitCode};

use bstr::ByteSlice;
use noxi_eval::Value;

use super::Parsed;
use crate::cli::Failure;
use crate::instantiate::{self, nix_string_literal};

fn text(value: &Value) -> Option<String> {
	let value = match value {
		Value::Thunk(thunk) if thunk.is_evaluated() => thunk.value().clone(),
		value => value.clone(),
	};
	match value {
		Value::String(s) => s.to_str().ok().map(str::to_owned),
		_ => None,
	}
}

pub(super) fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	let mut options = super::expression::options(&parsed, true)?;
	// The installable is forced on its own first, so selection errors carry
	// no frames of the position lookup below.
	options.strict = false;
	options.read_write_mode = true;
	options.json = false;
	let flake = !parsed.has("expr") && !parsed.has("file");
	let operand = parsed.operands.first().cloned().unwrap_or_default();
	let label = if flake {
		let (reference, fragment) = operand
			.split_once('#')
			.unwrap_or((if operand.is_empty() { "." } else { &operand }, ""));
		let system = parsed.settings.get("system").unwrap_or("x86_64-linux");
		let first = super::expression::flake_attr_paths("edit", fragment, system)
			.into_iter()
			.next()
			.unwrap_or_default();
		// `InstallableFlake::what`: the reference and its first attribute path.
		let first = if let Some(exact) = fragment.strip_prefix('.') {
			exact.to_owned()
		} else if fragment.is_empty() {
			first
		} else {
			fragment.to_owned()
		};
		format!(
			"(__noxi_flake_label {} + \"#\" + {})",
			nix_string_literal(reference),
			nix_string_literal(&first)
		)
	} else {
		nix_string_literal(&operand)
	};
	let root = options.inputs.pop().expect("one installable");
	options.inputs = vec![
		root.clone(),
		format!(
			"let v = ({root}\n); in builtins.toJSON [ {label} (if builtins.isAttrs v && v ? meta && builtins.isAttrs v.meta && v.meta ? position then toString v.meta.position else null) ]"
		),
	];
	let evaluated = match instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	let result: serde_json::Value = match evaluated.results.get(1).and_then(text) {
		Some(json) => serde_json::from_str(&json).map_err(|e| e.to_string())?,
		None => return Err(Failure::Error("edit evaluation produced no value".into())),
	};
	let what = result[0].as_str().unwrap_or_default().to_owned();
	let Some(position) = result[1].as_str().map(str::to_owned) else {
		return Err(Failure::Error(format!(
			"cannot find position information for '{what}"
		)));
	};
	if !position.starts_with('/') {
		return Err(Failure::Error(format!(
			"string '{position}' doesn't represent an absolute path"
		)));
	}
	let fail = || {
		Failure::Error(format!(
			"cannot parse 'meta.position' attribute '{position}'"
		))
	};
	let colon = position.rfind(':').ok_or_else(fail)?;
	let line: u32 = position[colon + 1..].parse().map_err(|_| fail())?;
	let file = &position[..colon];
	let path = match file.strip_prefix(nix_compat::store_path::store_dir_with_slash()) {
		Some(rest) => {
			let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
			match crate::store::open(&uri, &parsed.settings)?.and_then(|s| s.real_store_dir) {
				Some(real) => real.join(rest),
				None => PathBuf::from(file),
			}
		}
		None => PathBuf::from(file),
	};
	drop(evaluated);
	let editor = std::env::var("EDITOR")
		.ok()
		.filter(|e| !e.is_empty())
		.unwrap_or_else(|| "cat".into());
	let mut args: Vec<String> = editor
		.split([' ', '\t', '\n', '\r'])
		.filter(|s| !s.is_empty())
		.map(str::to_owned)
		.collect();
	if line > 0
		&& ["emacs", "nano", "vim", "kak"]
			.iter()
			.any(|e| editor.contains(e))
	{
		args.push(format!("+{line}"));
	}
	if args.is_empty() {
		return Err(Failure::Error("no editor given in EDITOR".into()));
	}
	let program = args.remove(0);
	let status = Command::new(&program)
		.args(&args)
		.arg(&path)
		.status()
		.map_err(|e| Failure::Error(format!("executing '{program}': {e}")))?;
	if !status.success() {
		return Err(Failure::Error(format!(
			"program '{program}' failed with exit code {}",
			status.code().unwrap_or(1)
		)));
	}
	Ok(ExitCode::SUCCESS)
}
