//! Flake command and installable adapter over the shared graph implementation.
use super::{Parsed, unsupported};
use crate::{
	cli::Failure,
	instantiate::{self, Options},
};
use bstr::ByteSlice;
use noxi_eval::{
	ErrorKind, Value,
	generators::{Gen, GenCo},
};
use noxi_glue::{EvaluatorStore, fetch::flake::LockOptions};
use std::{path::PathBuf, rc::Rc};

#[derive(Clone, Debug, Default)]
pub struct Request {
	pub operation: String,
	pub locks: LockOptions,
	pub no_write: bool,
	pub no_update: bool,
	pub all_systems: bool,
	pub legacy: bool,
	pub no_build: bool,
}
pub struct State {
	pub request: Request,
	pub store: Rc<EvaluatorStore>,
}
#[noxi_eval::builtin_macros::builtins(state = "Rc<State>")]
mod operations {
	use super::*;
	#[builtin("flake")]
	async fn builtin_flake(
		state: Rc<State>,
		co: GenCo,
		reference: Value,
	) -> Result<Value, ErrorKind> {
		let reference = reference.to_contextful_str()?.to_str()?.to_owned();
		let store = &state.store;
		let request = &state.request;
		if request.operation == "prefetch" {
			return Ok(Value::from(
				noxi_flake::prefetch(store, &reference)?.to_string(),
			));
		}
		let flake = if request.operation == "update" {
			noxi_flake::update(store, &co, &reference, &request.locks).await?
		} else {
			noxi_flake::lock(store, &co, &reference, &request.locks).await?
		};
		// Only local root sources have a writable lock file. Remote inputs remain immutable.
		let local = if let Some(url) = reference.strip_prefix("git+") {
			url::Url::parse(url)
				.ok()
				.filter(|url| url.scheme() == "file")
				.and_then(|url| url.to_file_path().ok())
		} else if reference.starts_with("path:") || !reference.contains(':') {
			Some(PathBuf::from(
				reference
					.strip_prefix("path:")
					.unwrap_or(&reference)
					.split('?')
					.next()
					.unwrap_or(""),
			))
		} else {
			None
		};
		let local = local.map(|path| {
			if path.is_absolute() {
				path
			} else {
				std::env::current_dir().unwrap_or_default().join(path)
			}
		});
		let local = local.map(|path| path.join(&flake.original_ref.subdir));
		let lock_path = local.as_ref().map(|path| path.join("flake.lock"));
		let local = local
			.as_ref()
			.is_some_and(|path| path.join("flake.nix").is_file());
		let rendered = flake
			.lock_file
			.render()
			.map_err(|e| ErrorKind::EvalError(e.to_string()))?
			+ "\n";
		let changed = (flake.lock_file.nodes.len() > 1
			|| lock_path.as_ref().is_some_and(|path| path.exists()))
			&& lock_path
				.as_ref()
				.and_then(|path| std::fs::read_to_string(path).ok())
				.unwrap_or_default()
				!= rendered;
		if changed && request.no_update && local {
			return Err(ErrorKind::EvalError("flake requires lock file changes but they're not allowed due to '--no-update-lock-file'".into()));
		}
		if !request.no_write
			&& local
			&& (flake.lock_file.nodes.len() > 1
				|| lock_path.as_ref().is_some_and(|path| path.exists()))
		{
			flake
				.write_lock_file(lock_path.as_ref().expect("local lock path"))
				.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
		}
		let json = match request.operation.as_str() {
			"lock" | "update" => return Ok(Value::Null),
			"metadata" => noxi_flake::metadata(store, &flake)?,
			"archive" => noxi_flake::archive(store, &flake)?,
			"outputs" => return flake.evaluate(store, &co).await,
			"show" => {
				noxi_flake::show(
					store,
					&co,
					&flake,
					&noxi_flake::ShowOptions {
						all_systems: request.all_systems,
						legacy: request.legacy,
						..Default::default()
					},
				)
				.await?
				.json
			}
			"check" => {
				let report = noxi_flake::check(
					store,
					&co,
					&flake,
					&noxi_flake::CheckOptions {
						all_systems: request.all_systems,
						build: !request.no_build,
						..Default::default()
					},
				)
				.await?;
				for warning in report.warnings {
					eprintln!("warning: {warning}");
				}
				return Ok(Value::Null);
			}
			_ => return Err(ErrorKind::EvalError("unsupported flake operation".into())),
		};
		Ok(Value::from(json.to_string()))
	}
}
pub fn builtins(request: Request, store: Rc<EvaluatorStore>) -> Vec<(&'static str, Value)> {
	operations::builtins(Rc::new(State { request, store }))
}
pub(super) fn request(parsed: &Parsed, operation: &str) -> Result<Request, Failure> {
	if !parsed.settings.experimental_features().contains("flakes") {
		return Err(Failure::Error(nix_compat::nixcpp::hash::missing_feature(
			"flakes",
		)));
	}
	parsed.unsupported(&[
		"commit-lock-file",
		"reference-lock-file",
		"output-lock-file",
		"inputs-from",
	])?;
	let mut locks = LockOptions {
		allow_unlocked: Some(true),
		recreate: parsed.has("recreate-lock-file"),
		..Default::default()
	};
	if let Some(overrides) = parsed.flags.get("override-input") {
		for pair in overrides {
			locks.override_inputs.insert(
				pair[0].split('/').map(str::to_owned).collect(),
				pair[1].clone(),
			);
		}
	}
	if let Some(updates) = parsed.flags.get("update-input") {
		for entry in updates {
			locks
				.update_inputs
				.insert(entry[0].split('/').map(str::to_owned).collect());
		}
	}
	Ok(Request {
		operation: operation.into(),
		locks,
		no_write: parsed.has("no-write-lock-file"),
		no_update: parsed.has("no-update-lock-file"),
		all_systems: parsed.has("all-systems"),
		legacy: parsed.has("legacy"),
		no_build: parsed.has("no-build"),
	})
}
pub(super) fn run(parsed: Parsed) -> Result<std::process::ExitCode, Failure> {
	let operation = parsed.command.strip_prefix("flake ").unwrap();
	if !matches!(
		operation,
		"lock" | "update" | "metadata" | "archive" | "prefetch" | "show" | "check"
	) {
		return Err(unsupported(parsed.command));
	}
	if !matches!(operation, "lock" | "update" | "check") && !parsed.has("json") {
		return Err(unsupported("flake text output; use --json"));
	}
	if operation != "update" && parsed.operands.len() > 1 {
		return Err(Failure::Usage("unexpected flake argument".into()));
	}
	let mut request = request(&parsed, operation)?;
	let reference = if operation == "update" {
		for input in &parsed.operands {
			request
				.locks
				.update_inputs
				.insert(input.split('/').map(str::to_owned).collect());
		}
		parsed.value("flake").unwrap_or(".")
	} else {
		parsed.operands.first().map(String::as_str).unwrap_or(".")
	};
	let mut options = Options {
		eval: true,
		strict: true,
		read_write_mode: true,
		from_args: true,
		store: parsed.value("store").map(str::to_owned),
		settings: parsed.settings.clone(),
		flake: Some(request),
		..Default::default()
	};
	options
		.settings
		.set("pure-eval", "false")
		.map_err(|e| e.to_string())?;
	options.inputs.push(format!(
		"__noxi_flake {}",
		instantiate::nix_string_literal(reference)
	));
	match instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(result) => {
			if let Some(Value::String(text)) = result.results.first() {
				println!("{}", text.to_str().map_err(|e| e.to_string())?);
			}
			Ok(std::process::ExitCode::SUCCESS)
		}
		Err(code) => Ok(code),
	}
}
