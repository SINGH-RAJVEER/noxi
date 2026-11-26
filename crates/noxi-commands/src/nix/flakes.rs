//! Flake command and installable adapter over the shared graph implementation.
use super::{Parsed, unsupported};
use crate::{
	cli::Failure,
	instantiate::{self, Options},
};
use bstr::ByteSlice;
use noxi_eval::{
	AddContext, ErrorKind, Value,
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
	pub keep_going: bool,
	pub json: bool,
	pub system: String,
	pub commit_lock: bool,
	pub commit_summary: Option<String>,
	pub inputs_from: Vec<String>,
	pub registry_overrides: Vec<(String, String)>,
	pub output_lock_file: Option<PathBuf>,
	/// `warn-dirty`: report lock files left unwritten because of unlocked inputs.
	pub warn_dirty: bool,
	/// `nix search`: the patterns and output format of the search builtin.
	pub search: Option<super::search::Request>,
}
pub struct State {
	pub request: Request,
	pub store: Rc<EvaluatorStore>,
}
#[noxi_eval::builtin_macros::builtins(state = "Rc<State>")]
mod operations {
	use super::*;
	#[builtin("search")]
	async fn builtin_search(
		state: Rc<State>,
		co: GenCo,
		root: Value,
		candidates: Value,
		label: Value,
	) -> Result<Value, ErrorKind> {
		super::super::search::builtin(state, co, root, candidates, label).await
	}
	#[builtin("flake_select")]
	async fn builtin_select(
		state: Rc<State>,
		co: GenCo,
		root: Value,
		candidates: Value,
		label: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &state;
		super::super::search::select_builtin(co, root, candidates, label, false).await
	}
	#[builtin("flake_select_app")]
	async fn builtin_select_app(
		state: Rc<State>,
		co: GenCo,
		root: Value,
		candidates: Value,
		label: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &state;
		super::super::search::select_builtin(co, root, candidates, label, true).await
	}
	#[builtin("repl_print")]
	async fn builtin_repl_print(
		state: Rc<State>,
		co: GenCo,
		value: Value,
		depth: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &state;
		super::super::repl::print_builtin(co, value, depth).await
	}
	#[builtin("eval_print")]
	async fn builtin_eval_print(
		state: Rc<State>,
		co: GenCo,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &state;
		super::super::repl::eval_print_builtin(co, value).await
	}
	#[builtin("repl_get")]
	async fn builtin_repl_get(
		state: Rc<State>,
		co: GenCo,
		set: Value,
		name: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &state;
		super::super::repl::get_builtin(co, set, name).await
	}
	#[builtin("repl_type")]
	async fn builtin_repl_type(
		state: Rc<State>,
		co: GenCo,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &state;
		super::super::repl::type_builtin(co, value).await
	}
	#[builtin("repl_names")]
	async fn builtin_repl_names(
		state: Rc<State>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &state;
		super::super::repl::names_builtin(co, value).await
	}
	/// The parsed, unresolved form of a reference, as installables print it.
	#[builtin("flake_label")]
	async fn builtin_flake_label(
		state: Rc<State>,
		co: GenCo,
		reference: Value,
	) -> Result<Value, ErrorKind> {
		let _ = &co;
		let reference = reference.to_contextful_str()?.to_str()?.to_owned();
		Ok(Value::from(
			noxi_glue::fetch::flake::parse_reference(&state.store, &reference)?.to_string(),
		))
	}
	#[builtin("flake")]
	async fn builtin_flake(
		state: Rc<State>,
		co: GenCo,
		reference: Value,
	) -> Result<Value, ErrorKind> {
		let reference = reference.to_contextful_str()?.to_str()?.to_owned();
		let store = &state.store;
		let request = &state.request;
		store.extend_flake_overrides(request.registry_overrides.clone());
		for source in &request.inputs_from {
			let inputs = noxi_flake::lock(
				store,
				&co,
				source,
				&LockOptions {
					allow_unlocked: Some(true),
					..Default::default()
				},
			)
			.await?;
			for name in inputs.lock_file.nodes[0].inputs.keys() {
				if let Some(node) = inputs
					.lock_file
					.find_input(&[name.clone()])
					.map_err(|e| ErrorKind::EvalError(e.to_string()))?
				{
					if let Some(locked) = &inputs.lock_file.nodes[node].locked {
						store
							.override_flake_input(name, &locked.locked_ref)
							.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
					}
				}
			}
		}
		if request.operation == "prefetch" {
			return Ok(Value::from(
				noxi_flake::prefetch(store, &reference)?.to_string(),
			));
		}
		if request.operation == "complete-inputs" {
			use noxi_flakeref::registry::UseRegistries;
			use noxi_glue::fetch::flake::{fetch_source, parse_reference};
			let reference = parse_reference(store, &reference)?;
			let source = fetch_source(store, &reference, UseRegistries::All)?;
			let import = noxi_eval::generators::request_builtin(&co, "import").await;
			let value = noxi_eval::generators::request_call_with(
				&co,
				import,
				[Value::Path(Box::new(source.out_path.join("flake.nix")))],
			)
			.await;
			return Ok(value);
		}
		let mut flake = if request.operation == "update" {
			noxi_flake::update(store, &co, &reference, &request.locks).await?
		} else {
			noxi_flake::lock(store, &co, &reference, &request.locks).await?
		};
		// Only local root sources have a writable lock file. Remote inputs remain immutable.
		let local = flake.original_ref.input.get_source_path();
		let local = local.map(|path| path.join(&flake.original_ref.subdir));
		let lock_path = local.as_ref().map(|path| path.join("flake.lock"));
		let local = local
			.as_ref()
			.is_some_and(|path| path.join("flake.nix").is_file());
		let changed = flake
			.lock_file
			.render()
			.map_err(|e| ErrorKind::EvalError(e.to_string()))?
			!= flake
				.original_lock_file
				.render()
				.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
		// `lockFlake` decides in this order whether it may write, and reports each
		// refusal under the lock-file trace frame.
		let lock_error = |message: String| {
			ErrorKind::EvalError(message).context(format!(
				"updating the lock file of flake '{}'",
				flake.locked_ref
			))
		};
		let diff = {
			use noxi_flakeref::lockfile::LockFile;
			String::from_utf8(nix_compat::log::filter_ansi_escapes(
				LockFile::diff(&flake.original_lock_file, &flake.lock_file).as_bytes(),
				true,
			))
			.expect("UTF-8 lock diff")
		};
		let mut write = false;
		if changed || request.output_lock_file.is_some() {
			if request.no_write {
				eprintln!(
					"warning: not writing modified lock file of flake '{}':\n{}",
					flake.original_ref,
					diff.trim_end()
				);
			} else if !local && request.output_lock_file.is_none() {
				return Err(lock_error(format!(
					"cannot write modified lock file of flake '{}' (use '--no-write-lock-file' to ignore)",
					flake.original_ref
				)));
			} else if let Some(unlocked) = flake
				.lock_file
				.is_unlocked(&store.fetch_settings())
				.map_err(|e| ErrorKind::EvalError(e.to_string()))?
			{
				if request.operation == "lock" {
					return Err(lock_error(format!(
						"Not writing lock file of flake '{}' because it has an unlocked input ('{unlocked}'). Use '--allow-dirty-locks' to allow this anyway.",
						flake.original_ref
					)));
				}
				if request.warn_dirty {
					eprintln!(
						"warning: not writing lock file of flake '{}' because it has an unlocked input ('{unlocked}')",
						flake.original_ref
					);
				}
			} else if request.no_update {
				return Err(lock_error(format!(
					"flake '{}' requires lock file changes but they're not allowed due to '--no-update-lock-file'",
					flake.original_ref
				)));
			} else if request.commit_lock && request.output_lock_file.is_some() {
				return Err(lock_error(
					"'--commit-lock-file' and '--output-lock-file' are incompatible".into(),
				));
			} else {
				write = true;
			}
		}
		if write {
			let existed = lock_path.as_ref().is_some_and(|path| path.exists());
			let path = request
				.output_lock_file
				.as_ref()
				.or(lock_path.as_ref())
				.expect("lock path");
			if request.output_lock_file.is_none() {
				let diff = diff.trim_end();
				if !path.exists() {
					eprintln!(
						"warning: creating lock file \"{}\": \n{diff}",
						path.display()
					);
				} else if diff.is_empty() {
					eprintln!("warning: updating lock file \"{}\"", path.display());
				} else {
					eprintln!(
						"warning: updating lock file \"{}\":\n{diff}",
						path.display()
					);
				}
			}
			flake
				.write_lock_file(path)
				.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
			if request.output_lock_file.is_none() {
				flake
					.track_mercurial_lock_file(path)
					.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
			}
			if request.output_lock_file.is_none()
				&& flake.original_ref.input.scheme == Some(noxi_flakeref::input::Scheme::Git)
			{
				let repo = flake
					.original_ref
					.input
					.get_source_path()
					.expect("local Git root");
				let relative = PathBuf::from(&flake.original_ref.subdir).join("flake.lock");
				let git = |args: &[&std::ffi::OsStr]| -> Result<std::process::Output, ErrorKind> {
					std::process::Command::new("git")
						.arg("-C")
						.arg(&repo)
						.args(args)
						.output()
						.map_err(|e| ErrorKind::EvalError(e.to_string()))
				};
				let ignored = git(&[
					"check-ignore".as_ref(),
					"--quiet".as_ref(),
					"--".as_ref(),
					relative.as_os_str(),
				])?;
				if ignored.status.code() == Some(1) {
					let added = git(&[
						"add".as_ref(),
						"--intent-to-add".as_ref(),
						"--".as_ref(),
						relative.as_os_str(),
					])?;
					if !added.status.success() {
						return Err(ErrorKind::EvalError(
							String::from_utf8_lossy(&added.stderr).into_owned(),
						));
					}
					if request.commit_lock {
						use std::io::Write;
						let summary = request
							.commit_summary
							.clone()
							.filter(|s| !s.is_empty())
							.unwrap_or_else(|| {
								format!(
									"{}: {}",
									relative.display(),
									if existed { "Update" } else { "Add" }
								)
							});
						let diff = noxi_flakeref::lockfile::LockFile::diff(
							&flake.original_lock_file,
							&flake.lock_file,
						);
						let diff = nix_compat::log::filter_ansi_escapes(diff.as_bytes(), true);
						let mut message = tempfile::NamedTempFile::new()
							.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
						write!(message, "{summary}\n\nFlake lock file updates:\n\n")
							.and_then(|_| message.write_all(&diff))
							.map_err(|e| ErrorKind::EvalError(e.to_string()))?;
						let committed = git(&[
							"commit".as_ref(),
							relative.as_os_str(),
							"-F".as_ref(),
							message.path().as_os_str(),
						])?;
						if !committed.status.success() {
							return Err(ErrorKind::EvalError(
								String::from_utf8_lossy(&committed.stderr).into_owned(),
							));
						}
					}
				} else if !ignored.status.success() {
					return Err(ErrorKind::EvalError(
						String::from_utf8_lossy(&ignored.stderr).into_owned(),
					));
				}
			}
			// Re-resolve with the same evaluator source cache. The pinned command only
			// re-reads the written flake, so the lock warnings are not repeated.
			if request.output_lock_file.is_none() {
				let _quiet = noxi_flakeref::SuppressWarnings::new();
				flake = noxi_flake::lock(store, &co, &reference, &request.locks).await?;
			}
		}
		let json = match request.operation.as_str() {
			"lock" | "update" => return Ok(Value::Null),
			"prefetch-inputs" => {
				let mut failed = false;
				for node in &flake.lock_file.nodes {
					if let Some(locked) = &node.locked {
						show_diagnostic(&format!("fetching '{}'...", locked.locked_ref));
						if let Err(error) =
							noxi_flake::prefetch(store, &locked.locked_ref.to_string())
						{
							eprintln!("{}", nix_compat::log::format_error(&error.to_string()));
							failed = true;
						}
					}
				}
				return Ok(Value::Bool(!failed));
			}
			"metadata" => noxi_flake::metadata(store, &flake)?,
			"archive" => noxi_flake::archive(store, &flake)?,
			"outputs" => return flake.evaluate(store, &co).await,
			"show" => {
				let report = noxi_flake::show(
					store,
					&co,
					&flake,
					&noxi_flake::ShowOptions {
						system: request.system.clone(),
						all_systems: request.all_systems,
						legacy: request.legacy,
						text: !request.json,
						diagnostic: Some(show_diagnostic),
						..Default::default()
					},
				)
				.await?;
				if request.json {
					report.json
				} else {
					return Ok(Value::from(serde_json::json!({"tree": report.json, "url": flake.locked_ref.to_string(), "warnings": report.warnings}).to_string()));
				}
			}
			"check" => {
				let report = noxi_flake::check(
					store,
					&co,
					&flake,
					&noxi_flake::CheckOptions {
						system: request.system.clone(),
						all_systems: request.all_systems,
						build: false,
						keep_going: request.keep_going,
					},
				)
				.await?;
				for warning in report.warnings {
					eprintln!("warning: {warning}");
				}
				for error in &report.errors {
					eprintln!("error: {error}");
				}
				if !report.errors.is_empty() {
					return Ok(Value::Bool(false));
				}
				let prefix = format!("checks.{}.", request.system);
				let checks: std::collections::BTreeMap<_, _> = report
					.derivations
					.into_iter()
					.filter(|(attr, _)| attr.starts_with(&prefix))
					.collect();
				return Ok(Value::from(
					serde_json::to_string(&checks).expect("serializable checks"),
				));
			}
			_ => return Err(ErrorKind::EvalError("unsupported flake operation".into())),
		};
		Ok(Value::from(json.to_string()))
	}
}
pub fn builtins(request: Request, store: Rc<EvaluatorStore>) -> Vec<(&'static str, Value)> {
	operations::builtins(Rc::new(State { request, store }))
}

pub(super) fn complete_inputs(
	parsed: &Parsed,
	prefix: &str,
) -> Result<Vec<(String, String)>, Failure> {
	let default = [parsed.value("flake").unwrap_or(".").to_owned()];
	let refs = if parsed.command == "flake update" || parsed.operands.is_empty() {
		&default[..]
	} else {
		&parsed.operands
	};
	let mut options = Options {
		eval: true,
		strict: true,
		from_args: true,
		store: parsed.value("store").map(str::to_owned),
		settings: parsed.settings.clone(),
		flake: Some(Request {
			operation: "complete-inputs".into(),
			..Default::default()
		}),
		..Default::default()
	};
	options
		.settings
		.set("pure-eval", "false")
		.map_err(|e| e.to_string())?;
	let cwd = std::env::current_dir()?;
	let mut result = Vec::new();
	for reference in refs {
		let reference = reference.split(['#', '^']).next().unwrap_or(reference);
		let reference = reference
			.strip_prefix("~/")
			.and_then(|suffix| {
				std::env::var_os("HOME").map(|home| PathBuf::from(home).join(suffix))
			})
			.map_or_else(
				|| reference.to_owned(),
				|path| path.to_string_lossy().into_owned(),
			);
		let reference = instantiate::nix_string_literal(&reference);
		options.inputs = vec![format!(
			"let f = __noxi_flake {reference}; in builtins.filter (n: n != \"self\") (builtins.attrNames ((builtins.functionArgs f.outputs) // (f.inputs or {{}})))"
		)];
		if let Ok(evaluated) = instantiate::evaluate(&options, &cwd)
			&& let Some(Value::List(names)) = evaluated.results.first()
		{
			for name in names.iter() {
				let name = name.to_str().map_err(|e| e.to_string())?;
				let name = name.to_str_lossy();
				if name.starts_with(prefix) {
					result.push((name.into_owned(), String::new()));
				}
			}
		}
	}
	Ok(result)
}

fn render_text(operation: &str, value: &serde_json::Value) -> Result<(), Failure> {
	let text = |key: &str| value[key].as_str().unwrap_or("");
	match operation {
		"archive" => {}
		"show" => {
			println!("\x1b[1m{}\x1b[0m", text("url"));
			print_show(&value["tree"], &value["warnings"], &mut Vec::new(), "", "");
		}
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

	let mut locks = LockOptions {
		allow_unlocked: Some(true),
		recreate: parsed.has("recreate-lock-file"),
		reference_lock_file: parsed.value("reference-lock-file").map(PathBuf::from),
		..Default::default()
	};
	if let Some(overrides) = parsed.flags.get("override-input") {
		for pair in overrides {
			locks
				.override_inputs
				.insert(input_path(&pair[0], "--override-input")?, pair[1].clone());
		}
	}
	if let Some(updates) = parsed.flags.get("update-input") {
		for entry in updates {
			locks
				.update_inputs
				.insert(input_path(&entry[0], "--update-input")?);
		}
	}
	Ok(Request {
		operation: operation.into(),
		commit_lock: parsed.has("commit-lock-file"),
		commit_summary: parsed
			.settings
			.get("commit-lock-file-summary")
			.map(str::to_owned),
		inputs_from: parsed
			.flags
			.get("inputs-from")
			.into_iter()
			.flatten()
			.map(|v| v[0].clone())
			.collect(),
		registry_overrides: parsed
			.flags
			.get("override-flake")
			.into_iter()
			.flatten()
			.map(|v| (v[0].clone(), v[1].clone()))
			.collect(),
		locks,
		no_write: parsed.has("no-write-lock-file")
			|| parsed.has("override-input") && !matches!(operation, "lock" | "update"),
		no_update: parsed.has("no-update-lock-file"),
		all_systems: parsed.has("all-systems"),
		legacy: parsed.has("legacy"),
		keep_going: parsed.settings.get_bool("keep-going", false) || parsed.has("keep-going"),
		json: parsed.has("json"),
		system: parsed
			.settings
			.get("system")
			.map(str::to_owned)
			.unwrap_or_else(|| format!("{}-linux", std::env::consts::ARCH)),
		output_lock_file: parsed.value("output-lock-file").map(PathBuf::from),
		warn_dirty: parsed.settings.get_bool("warn-dirty", true),
		search: None,
	})
}
pub(super) fn run(mut parsed: Parsed) -> Result<std::process::ExitCode, Failure> {
	let operation = parsed.command.strip_prefix("flake ").unwrap();
	let operation = if operation == "info" {
		eprintln!("warning: 'nix flake info' is a deprecated alias for 'nix flake metadata'");
		"metadata"
	} else {
		operation
	};
	if matches!(operation, "init" | "new") {
		return super::flake_init::init(parsed, operation);
	}
	if operation == "clone" {
		return super::flake_init::clone(parsed);
	}
	if !matches!(
		operation,
		"lock"
			| "update"
			| "metadata"
			| "archive"
			| "prefetch"
			| "prefetch-inputs"
			| "show"
			| "check"
	) {
		return Err(unsupported(parsed.command));
	}
	if !matches!(operation, "prefetch" | "check") {
		parsed.unsupported(&["out-link"])?;
	}
	if operation != "update" && parsed.operands.len() > 1 {
		return Err(Failure::Usage("unexpected flake argument".into()));
	}
	if operation == "update" {
		parsed
			.settings
			.set("tarball-ttl", "0")
			.map_err(|e| e.to_string())?;
	}
	if parsed.has("no-registries") {
		parsed
			.settings
			.set("use-registries", "false")
			.map_err(|e| e.to_string())?;
	}
	if matches!(operation, "show" | "check")
		&& parsed
			.settings
			.get("allow-import-from-derivation")
			.is_none()
	{
		parsed
			.settings
			.set("allow-import-from-derivation", "false")
			.map_err(|e| e.to_string())?;
	}
	let mut request = request(&parsed, operation)?;
	let reference = if operation == "update" {
		for input in &parsed.operands {
			let path = input_path(input, "input path to be updated").map_err(|e| {
				eprintln!("warning: Invalid flake input '{input}'. To update a specific flake, use 'nix flake update --flake {input}' instead."); e
			})?;
			if !request.locks.update_inputs.insert(path) {
				eprintln!(
					"warning: Input '{input}' was specified multiple times. You may have done this by accident."
				);
			}
		}
		parsed.value("flake").unwrap_or(".")
	} else {
		parsed.operands.first().map(String::as_str).unwrap_or(".")
	};

	let mut options = Options {
		eval: true,
		strict: true,
		read_write_mode: !parsed.has("dry-run"),
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
			if let Some(Value::Bool(false)) = result.results.first() {
				return Ok(std::process::ExitCode::FAILURE);
			}
			if let Some(Value::String(text)) = result.results.first() {
				let text = text.to_str().map_err(|e| e.to_string())?;
				if operation == "check" {
					return finish_checks(&parsed, result.store.as_deref(), text);
				}
				if operation == "prefetch" {
					if let Some(link) = parsed.value("out-link") {
						let json: serde_json::Value =
							serde_json::from_str(text).map_err(|e| e.to_string())?;
						let path = nix_compat::store_path::StorePath::from_absolute_path(
							json["storePath"].as_str().unwrap_or("").as_bytes(),
						)
						.map_err(|e| e.to_string())?;
						let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
						let store = crate::store::open(&uri, &parsed.settings)?
							.ok_or_else(|| unsupported("prefetch --out-link on this store"))?;
						store
							.store
							.add_perm_root(&path, &std::env::current_dir()?.join(link))?;
					}
				}
				if parsed.has("json") {
					if parsed.has("pretty") {
						let value: serde_json::Value =
							serde_json::from_str(text).map_err(|e| e.to_string())?;
						println!(
							"{}",
							serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
						);
					} else {
						println!("{text}");
					}
				} else {
					render_text(
						operation,
						&serde_json::from_str(text).map_err(|e| e.to_string())?,
					)?;
				}
				if operation == "archive" && parsed.has("to") && !parsed.has("dry-run") {
					fn paths(value: &serde_json::Value, result: &mut Vec<String>) {
						if let Some(path) = value["path"].as_str() {
							result.push(path.into());
						}
						if let Some(inputs) = value["inputs"].as_object() {
							for child in inputs.values() {
								paths(child, result);
							}
						}
					}
					let value = serde_json::from_str(text).map_err(|e| e.to_string())?;
					let mut sources = Vec::new();
					paths(&value, &mut sources);
					parsed.command = "copy";
					parsed.operands = sources;
					parsed
						.flags
						.retain(|key, _| matches!(key.as_str(), "to" | "store" | "no-check-sigs"));
					return super::transfer::copy(parsed);
				}
			}
			Ok(std::process::ExitCode::SUCCESS)
		}
		Err(code) => Ok(code),
	}
}

fn finish_checks(
	parsed: &Parsed,
	store: Option<&dyn noxi_store::Store>,
	checks: &str,
) -> Result<std::process::ExitCode, Failure> {
	use nix_compat::log::{VerbosityLevel, logger};
	use nix_compat::store_path::StorePath;
	use noxi_store::{BuildMode, DerivedPath};
	use std::collections::{BTreeMap, BTreeSet};
	use std::process::ExitCode;

	if parsed.has("no-build") {
		return Ok(ExitCode::SUCCESS);
	}
	let checks: BTreeMap<String, String> =
		serde_json::from_str(checks).map_err(|e| e.to_string())?;
	let mut derivations = BTreeMap::<_, Vec<_>>::new();
	for (attr, drv) in checks {
		let drv = StorePath::from_absolute_path(drv.as_bytes()).map_err(|e| e.to_string())?;
		derivations.entry(drv).or_default().push(attr);
	}
	if derivations.is_empty() {
		logger::log(VerbosityLevel::Info, "all checks passed!");
		return Ok(ExitCode::SUCCESS);
	}
	let store = store
		.ok_or_else(|| Failure::Error("flake check builds require a writable store".into()))?;
	let targets = derivations
		.keys()
		.map(|drv| DerivedPath::Built {
			drv: drv.clone(),
			outputs: BTreeSet::new(),
		})
		.collect::<Vec<_>>();
	let missing = store.query_missing(&targets)?;
	let print_paths = parsed.has("print-out-paths");
	let out_link = parsed.value("out-link");
	let keep_going = parsed.settings.get_bool("keep-going", false) || parsed.has("keep-going");
	let mut failed = false;
	let mut all_outputs = Vec::new();
	{
		let _activity = logger::Activity::new(
			VerbosityLevel::Info,
			nix_compat::log::ActivityType::Unknown,
			format!("running {} flake checks", missing.will_build.len()),
			Vec::new(),
			0,
		);
		for (drv, target) in derivations.keys().zip(&targets) {
			if out_link.is_none() && !print_paths && !missing.will_build.contains(drv) {
				continue;
			}
			match store.build_paths_with_results(std::slice::from_ref(target), BuildMode::Normal) {
				Ok(resolved) => {
					if print_paths || out_link.is_some() {
						all_outputs
							.push(super::expression::built_outputs(store, target, &resolved)?);
					}
				}
				Err(error) => {
					for attr in &derivations[drv] {
						let message = format!(
							"failed to build attribute '{attr}', build of '{}^*' failed: {error}",
							drv.to_absolute_path()
						);
						if !keep_going {
							return Err(Failure::Error(message));
						}
						crate::cli::print_error(&message);
					}
					failed = true;
				}
			}
		}
	}
	if failed {
		return Err(Failure::Error(
			"some errors were encountered during the evaluation".into(),
		));
	}
	logger::log(VerbosityLevel::Info, "all checks passed!");
	if print_paths {
		for path in all_outputs.iter().flat_map(|outputs| outputs.values()) {
			logger::write_stdout(&path.to_absolute_path());
		}
	}
	if let Some(link) = out_link {
		let cwd = std::env::current_dir()?;
		for (index, outputs) in all_outputs.into_iter().enumerate() {
			for (name, path) in outputs {
				let mut link = link.to_owned();
				if index > 0 {
					link.push_str(&format!("-{index}"));
				}
				if name != "out" {
					link.push_str(&format!("-{name}"));
				}
				store.add_perm_root(&path, &cwd.join(link))?;
			}
		}
	}
	Ok(ExitCode::SUCCESS)
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
				let date = lock["nodes"][target]["locked"]["lastModified"]
					.as_u64()
					.map(|time| {
						let date = noxi_flakeref::lockfile::format_last_modified_date(time);
						format!(
							" ({}-{}-{} {}:{}:{})",
							&date[..4],
							&date[4..6],
							&date[6..8],
							&date[8..10],
							&date[10..12],
							&date[12..14]
						)
					})
					.unwrap_or_default();
				println!("{prefix}{branch}\x1b[1m{name}\x1b[0m: {reference}{date}");
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

fn print_show(
	value: &serde_json::Value,
	warnings: &serde_json::Value,
	path: &mut Vec<String>,
	header: &str,
	prefix: &str,
) {
	let first = path.first().map(String::as_str).unwrap_or("");
	let omitted = warnings.as_array().and_then(|warnings| {
		warnings
			.iter()
			.filter_map(|v| v.as_str())
			.find_map(|warning| warning.strip_prefix(&format!("{} omitted", path.join("."))))
	});
	if let Some(reason) = omitted {
		println!("{header} \x1b[35;1momitted\x1b[0m{reason}");
		return;
	}
	if let Some(kind) = value["type"].as_str() {
		match kind {
			"derivation" => {
				let label = if first == "devShell" || first == "devShells" {
					"development environment"
				} else if first == "checks" || first == "hydraJobs" {
					"derivation"
				} else {
					"package"
				};
				println!(
					"{header}: {label} '{}'",
					value["name"].as_str().unwrap_or("")
				);
			}
			"app" | "template" => println!(
				"{header}: {kind}: \x1b[1m{}\x1b[0m",
				value["description"].as_str().unwrap_or("no description")
			),
			_ => {
				let label = match kind {
					"nixpkgs-overlay" => "Nixpkgs overlay",
					"nixos-configuration" => "NixOS configuration",
					"nixos-module" => "NixOS module",
					_ => "unknown",
				};
				println!("{header}: \x1b[35;1m{label}\x1b[0m");
			}
		}
	} else if let Some(children) = value.as_object() {
		if !path.is_empty() && children.is_empty() {
			return;
		}
		if !path.is_empty() {
			println!("{header}");
		}
		for (index, (name, child)) in children.iter().enumerate() {
			let last = index + 1 == children.len();
			path.push(name.clone());
			print_show(
				child,
				warnings,
				path,
				&format!(
					"\x1b[32;1m{prefix}{}\x1b[0m\x1b[1m{name}\x1b[0m",
					if last { "└───" } else { "├───" }
				),
				&format!("{prefix}{}", if last { "    " } else { "│   " }),
			);
			path.pop();
		}
	}
}

fn show_diagnostic(message: &str) {
	let level = if message.starts_with("warning:") {
		nix_compat::log::VerbosityLevel::Warn
	} else {
		nix_compat::log::VerbosityLevel::Info
	};
	if nix_compat::log::log_enabled(level) {
		eprintln!("{message}");
	}
}

fn input_path(input: &str, flag: &str) -> Result<Vec<String>, Failure> {
	let path = noxi_flakeref::lockfile::parse_input_attr_path(input)
		.map_err(|e| Failure::Error(e.to_string()))?;
	if path.is_empty() {
		return Err(Failure::Usage(if flag.starts_with("--") {
			format!(
				"{flag} was passed a zero-length input path, which would refer to the flake itself, not an input"
			)
		} else {
			"input path to be updated cannot be zero-length; it would refer to the flake itself, not an input".into()
		}));
	}
	Ok(path)
}

/// Hide only the synthetic adapter call, retaining imported Nix source traces.
pub(crate) fn command_error(error: &noxi_eval::Error) -> noxi_eval::Error {
	let mut error = error.clone();
	error.traces.retain(|frame| !adapter_frame(&frame.hint));
	error.contexts.retain(|frame| !adapter_frame(&frame.hint));
	match &mut error.kind {
		ErrorKind::NativeError { err, .. } | ErrorKind::BytecodeError(err) => {
			**err = command_error(err)
		}
		ErrorKind::CatchableError(caught) => {
			if let Some(trace) = caught.trace() {
				trace
					.traces
					.borrow_mut()
					.retain(|frame| !adapter_frame(&frame.hint));
			}
		}
		_ => {}
	}
	error
}

/// The command adapters' own builtin frames, which CppNix has no counterpart for.
fn adapter_frame(hint: &str) -> bool {
	[
		"while calling the 'flake' builtin",
		"while calling the 'search' builtin",
		"while calling the 'flake_label' builtin",
		"while calling the 'repl_print' builtin",
		"while calling the 'repl_get' builtin",
		"while calling the 'eval_print' builtin",
		"while calling the 'repl_type' builtin",
		"while calling the 'repl_names' builtin",
		"while calling the 'flake_select' builtin",
	]
	.contains(&hint)
}
