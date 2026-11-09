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
	pub output_lock_file: Option<PathBuf>,
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
		let comparison_lock = request
			.locks
			.reference_lock_file
			.as_ref()
			.or(lock_path.as_ref());
		let changed = (flake.lock_file.nodes.len() > 1
			|| lock_path.as_ref().is_some_and(|path| path.exists()))
			&& comparison_lock
				.and_then(|path| std::fs::read_to_string(path).ok())
				.unwrap_or_default()
				!= rendered;
		if changed && request.no_update && local {
			return Err(ErrorKind::EvalError("flake requires lock file changes but they're not allowed due to '--no-update-lock-file'".into()));
		}
		if !request.no_write
			&& local
			&& (request.output_lock_file.is_some()
				|| flake.lock_file.nodes.len() > 1
				|| lock_path.as_ref().is_some_and(|path| path.exists()))
		{
			flake
				.write_lock_file(
					request
						.output_lock_file
						.as_ref()
						.or(lock_path.as_ref())
						.expect("local lock path"),
				)
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

fn render_text(operation: &str, value: &serde_json::Value) -> Result<(), Failure> {
	let text = |key: &str| value[key].as_str().unwrap_or("");
	match operation {
		"archive" => {}
		"prefetch" => {
			let attrs =
				noxi_flakeref::input::json_to_attrs(&value["locked"]).map_err(|e| e.to_string())?;
			let reference = noxi_flakeref::flake_ref::FlakeRef::from_attrs(
				&noxi_flakeref::input::FetchSettings {
					flakes: true,
					..Default::default()
				},
				&attrs,
			)
			.map_err(|e| e.to_string())?;
			if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
				eprintln!(
					"Downloaded '{reference}' to '{}' (hash '{}').",
					text("storePath"),
					text("hash")
				);
			}
		}
		"metadata" => {
			let line = |label: &str, value: &str| {
				println!(
					"\x1b[1m{label}:\x1b[0m{} {value}",
					" ".repeat(13usize.saturating_sub(label.len()))
				)
			};
			line("Resolved URL", text("resolvedUrl"));
			line("Locked URL", text("url"));
			if value.get("description").is_some() {
				line("Description", text("description"));
			}
			line("Path", text("path"));
			if value.get("revision").is_some() {
				line("Revision", text("revision"));
			}
			if value.get("dirtyRevision").is_some() {
				line("Dirty revision", text("dirtyRevision"));
			}
			if let Some(count) = value["revCount"].as_u64() {
				line("Revisions", &count.to_string());
			}
			if let Some(time) = value["lastModified"].as_i64() {
				let time = time as libc::time_t;
				// SAFETY: localtime_r fills the valid tm pointer.
				let mut tm: libc::tm = unsafe { std::mem::zeroed() };
				if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
					return Err(Failure::Error("cannot convert flake timestamp".into()));
				}
				line(
					"Last modified",
					&format!(
						"{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
						tm.tm_year + 1900,
						tm.tm_mon + 1,
						tm.tm_mday,
						tm.tm_hour,
						tm.tm_min,
						tm.tm_sec
					),
				);
			}
			if value.get("fingerprint").is_some() {
				line("Fingerprint", text("fingerprint"));
			}
			if value["locks"]["nodes"]
				.as_object()
				.is_some_and(|nodes| nodes.len() > 1)
			{
				println!("\x1b[1mInputs:\x1b[0m");
				print_inputs(
					&value["locks"],
					value["locks"]["root"].as_str().unwrap_or("root"),
					"",
					&mut std::collections::BTreeSet::new(),
				)?;
			}
		}
		_ => return Err(super::unsupported("flake text output")),
	}
	Ok(())
}
pub(super) fn request(parsed: &Parsed, operation: &str) -> Result<Request, Failure> {
	if !parsed.settings.experimental_features().contains("flakes") {
		return Err(Failure::Error(nix_compat::nixcpp::hash::missing_feature(
			"flakes",
		)));
	}
	parsed.unsupported(&["commit-lock-file", "inputs-from"])?;
	let mut locks = LockOptions {
		allow_unlocked: Some(true),
		recreate: parsed.has("recreate-lock-file"),
		reference_lock_file: parsed.value("reference-lock-file").map(PathBuf::from),
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
		output_lock_file: parsed.value("output-lock-file").map(PathBuf::from),
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
	if operation == "show" && !parsed.has("json") {
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
				let text = text.to_str().map_err(|e| e.to_string())?;
				if parsed.has("json") {
					println!("{text}");
				} else {
					render_text(
						operation,
						&serde_json::from_str(text).map_err(|e| e.to_string())?,
					)?;
				}
			}
			Ok(std::process::ExitCode::SUCCESS)
		}
		Err(code) => Ok(code),
	}
}

fn print_inputs(
	lock: &serde_json::Value,
	node: &str,
	prefix: &str,
	seen: &mut std::collections::BTreeSet<String>,
) -> Result<(), Failure> {
	if !seen.insert(node.into()) {
		return Ok(());
	}
	if let Some(inputs) = lock["nodes"][node]["inputs"].as_object() {
		for (index, (name, edge)) in inputs.iter().enumerate() {
			let last = index + 1 == inputs.len();
			let branch = if last { "└───" } else { "├───" };
			if let Some(target) = edge.as_str() {
				let attrs = noxi_flakeref::input::json_to_attrs(&lock["nodes"][target]["locked"])
					.map_err(|e| e.to_string())?;
				let reference = noxi_flakeref::flake_ref::FlakeRef::from_attrs(
					&noxi_flakeref::input::FetchSettings {
						flakes: true,
						..Default::default()
					},
					&attrs,
				)
				.map_err(|e| e.to_string())?;
				println!("{prefix}{branch}\x1b[1m{name}\x1b[0m: {reference}");
				print_inputs(
					lock,
					target,
					&format!("{prefix}{}", if last { "    " } else { "│   " }),
					seen,
				)?;
			} else if let Some(path) = edge.as_array() {
				println!(
					"{prefix}{branch}\x1b[1m{name}\x1b[0m follows input '{}'",
					path.iter()
						.filter_map(|v| v.as_str())
						.collect::<Vec<_>>()
						.join("/")
				);
			}
		}
	}
	seen.remove(node);
	Ok(())
}
