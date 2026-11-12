//! `nix search`, following CppNix 2.35.2 `search.cc`.
//!
//! The walk runs inside the evaluation as a command builtin, so values are
//! forced lazily in the reference's order and the `evaluating '...'`
//! activities interleave with evaluation diagnostics. Thrown errors under
//! `legacyPackages` are skipped as CppNix skips them; other evaluation
//! failures, which the evaluator does not let a builtin catch, abort the
//! search.
use std::future::Future;
use std::pin::Pin;
use std::process::ExitCode;
use std::rc::Rc;

use bstr::ByteSlice;
use noxi_eval::{
	ErrorKind, NixAttrs, Value,
	generators::{self, GenCo},
};
use regex::{Regex, RegexBuilder};

use super::Parsed;
use super::flakes::State;
use crate::cli::Failure;
use crate::instantiate::{self, nix_string_literal};

/// The options the search builtin reads from the command request.
#[derive(Clone, Debug, Default)]
pub struct Request {
	pub regexes: Vec<String>,
	pub excludes: Vec<String>,
	pub json: bool,
	pub pretty: bool,
}

type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ErrorKind>> + 'a>>;

struct Walk {
	regexes: Vec<Regex>,
	excludes: Vec<Regex>,
	json: Option<serde_json::Map<String, serde_json::Value>>,
	results: u64,
}

/// `DrvName`: the name ends before the first dash followed by a non-letter.
fn drv_name(full: &str) -> (&str, &str) {
	let bytes = full.as_bytes();
	for pos in 0..bytes.len() {
		if bytes[pos] == b'-' && pos + 1 < bytes.len() && !bytes[pos + 1].is_ascii_alphabetic() {
			return (&full[..pos], &full[pos + 1..]);
		}
	}
	(full, "")
}

/// `hiliteMatches`: wrap the merged matches in `prefix` and `postfix`.
fn hilite(s: &str, mut matches: Vec<(usize, usize)>, prefix: &str, postfix: &str) -> String {
	if matches.is_empty() {
		return s.to_owned();
	}
	matches.sort_by_key(|m| m.0);
	let mut out = String::new();
	let mut last_end = 0;
	let mut index = 0;
	while index < matches.len() {
		let (start, mut end) = matches[index];
		out.push_str(&s[last_end..start]);
		index += 1;
		while index < matches.len() && matches[index].0 <= end {
			end = end.max(matches[index].1);
			index += 1;
		}
		out.push_str(prefix);
		out.push_str(&s[start..end]);
		out.push_str(postfix);
		last_end = end;
	}
	out.push_str(&s[last_end..]);
	out
}

fn info(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
		eprintln!("{message}");
	}
}

async fn text(co: &GenCo, value: Value) -> Result<Result<String, Value>, ErrorKind> {
	let value = generators::request_force(co, value).await;
	if value.is_catchable() {
		return Ok(Err(value));
	}
	Ok(Ok(value.to_contextful_str()?.to_str()?.to_owned()))
}

/// The eval cache's `maybeGetAttr`: absent unless the value is a set.
async fn attrs(co: &GenCo, value: Value) -> Result<Result<Option<NixAttrs>, Value>, ErrorKind> {
	let value = generators::request_force(co, value).await;
	Ok(match value {
		Value::Catchable(_) => Err(value),
		Value::Attrs(attrs) => Ok(Some(attrs)),
		_ => Ok(None),
	})
}

macro_rules! caught {
	($e:expr) => {
		match $e {
			Ok(value) => value,
			Err(thrown) => return Ok(Err(thrown)),
		}
	};
}

fn visit<'a>(
	co: &'a GenCo,
	walk: &'a mut Walk,
	value: Value,
	path: Vec<String>,
	initial: bool,
) -> LocalFuture<'a, Result<(), Value>> {
	Box::pin(async move {
		let shown = path.join(".");
		info(&format!("evaluating '{shown}'..."));
		let legacy = path.first().is_some_and(|p| p == "legacyPackages");
		let result = visit_inner(co, walk, value, &path, &shown, initial).await?;
		match result {
			Err(_) if legacy => Ok(Ok(())),
			result => Ok(result),
		}
	})
}

async fn visit_inner(
	co: &GenCo,
	walk: &mut Walk,
	value: Value,
	path: &[String],
	shown: &str,
	initial: bool,
) -> Result<Result<(), Value>, ErrorKind> {
	let fields = caught!(attrs(co, value).await?);
	let derivation = match fields.as_ref().and_then(|f| f.select("type")) {
		Some(kind) => caught!(text(co, kind.clone()).await?) == "derivation",
		None => false,
	};
	if derivation {
		let fields = fields.expect("derivation attributes");
		let Some(name) = fields.select("name") else {
			return Err(ErrorKind::EvalError(format!(
				"attribute 'name' missing in '{shown}'"
			)));
		};
		let full = caught!(text(co, name.clone()).await?);
		let (name, version) = drv_name(&full);
		let mut description = String::new();
		if let Some(meta) = fields.select("meta") {
			if let Some(meta) = caught!(attrs(co, meta.clone()).await?) {
				if let Some(value) = meta.select("description") {
					description = caught!(text(co, value.clone()).await?);
				}
			}
		}
		let description = description.replace('\n', " ");
		let any = |s: &str| walk.excludes.iter().any(|r| r.is_match(s));
		if any(shown) || any(name) || any(&description) {
			return Ok(Ok(()));
		}
		let mut path_matches = Vec::new();
		let mut description_matches = Vec::new();
		let mut found = false;
		for regex in &walk.regexes {
			found = false;
			for (target, mut matches) in [
				(shown, Some(&mut path_matches)),
				(name, None),
				(description.as_str(), Some(&mut description_matches)),
			] {
				for m in regex.find_iter(target) {
					found = true;
					if let Some(matches) = matches.as_deref_mut() {
						matches.push((m.start(), m.end()));
					}
				}
			}
			if !found {
				break;
			}
		}
		if found {
			walk.results += 1;
			if let Some(json) = &mut walk.json {
				json.insert(
					shown.to_owned(),
					serde_json::json!({
						"pname": name,
						"version": version,
						"description": description,
					}),
				);
			} else {
				if walk.results > 1 {
					println!();
				}
				let version = if version.is_empty() {
					String::new()
				} else {
					format!(" ({version})")
				};
				println!(
					"* \x1b[0;1m{}\x1b[0m{version}",
					hilite(shown, path_matches, "\x1b[32;1m", "\x1b[0;1m")
				);
				if !description.is_empty() {
					println!(
						"  {}",
						hilite(&description, description_matches, "\x1b[32;1m", "\x1b[0m")
					);
				}
			}
		}
		return Ok(Ok(()));
	}
	let first = path.first().map(String::as_str).unwrap_or("");
	let recurse = if path.is_empty()
		|| matches!(first, "legacyPackages" | "packages") && path.len() <= 2
		|| initial
	{
		true
	} else if first == "legacyPackages" && path.len() > 2 {
		match fields
			.as_ref()
			.and_then(|f| f.select("recurseForDerivations"))
		{
			Some(flag) => {
				let flag = generators::request_force(co, flag.clone()).await;
				if flag.is_catchable() {
					return Ok(Err(flag));
				}
				flag.as_bool()?
			}
			None => false,
		}
	} else {
		false
	};
	if recurse {
		let Some(fields) = fields else {
			return Err(ErrorKind::EvalError(format!(
				"'{shown}' is not an attribute set"
			)));
		};
		for (name, child) in fields.iter_sorted() {
			let mut child_path = path.to_vec();
			child_path.push(name.to_str()?.to_owned());
			caught!(visit(co, walk, child.clone(), child_path, false).await?);
		}
	}
	Ok(Ok(()))
}

/// The installable's cursors: each candidate attribute path that exists.
async fn cursors(
	co: &GenCo,
	root: Value,
	candidates: &[String],
) -> Result<Result<Vec<(Vec<String>, Value)>, Value>, ErrorKind> {
	let mut found = Vec::new();
	'candidates: for candidate in candidates {
		let path = instantiate::parse_attr_path(candidate).map_err(ErrorKind::EvalError)?;
		let mut value = root.clone();
		for name in &path {
			match caught!(attrs(co, value).await?) {
				Some(fields) => match fields.select(name.as_str()) {
					Some(child) => value = child.clone(),
					None => continue 'candidates,
				},
				// CppNix continues after a type error on the path.
				None => continue 'candidates,
			}
		}
		found.push((path, value));
	}
	Ok(Ok(found))
}

pub async fn builtin(
	state: Rc<State>,
	co: GenCo,
	root: Value,
	candidates: Value,
	label: Value,
) -> Result<Value, ErrorKind> {
	let request = state
		.request
		.search
		.clone()
		.ok_or_else(|| ErrorKind::EvalError("search request missing".into()))?;
	let compile = |patterns: &[String]| -> Result<Vec<Regex>, ErrorKind> {
		patterns
			.iter()
			.map(|re| {
				RegexBuilder::new(re)
					.case_insensitive(true)
					.build()
					.map_err(|e| ErrorKind::EvalError(e.to_string()))
			})
			.collect()
	};
	let mut walk = Walk {
		regexes: compile(&request.regexes)?,
		excludes: compile(&request.excludes)?,
		json: request.json.then(serde_json::Map::new),
		results: 0,
	};
	let mut names = Vec::new();
	for candidate in candidates.to_list()?.iter() {
		let candidate = generators::request_force(&co, candidate.clone()).await;
		names.push(candidate.to_contextful_str()?.to_str()?.to_owned());
	}
	let label = label.to_contextful_str()?.to_str()?.to_owned();
	let found = match cursors(&co, root, &names).await? {
		Ok(found) => found,
		Err(thrown) => return Ok(thrown),
	};
	if found.is_empty() {
		return Err(missing(&label, &names));
	}
	for (path, value) in found {
		if let Err(thrown) = visit(&co, &mut walk, value, path, true).await? {
			return Ok(thrown);
		}
	}
	Ok(match walk.json {
		Some(json) => {
			let json = serde_json::Value::Object(json);
			Value::from(if request.pretty {
				serde_json::to_string_pretty(&json)
					.map_err(|e| ErrorKind::EvalError(e.to_string()))?
			} else {
				json.to_string()
			})
		}
		None => Value::Integer(walk.results as i64),
	})
}

pub(super) fn run(mut parsed: Parsed) -> Result<ExitCode, Failure> {
	let regexes = if parsed.operands.is_empty() {
		Vec::new()
	} else {
		parsed.operands.split_off(1)
	};
	if regexes.is_empty() {
		return Err(Failure::Usage(
			"Must provide at least one regex! To match all packages, use 'nix search <installable> ^'."
				.into(),
		));
	}
	if parsed
		.settings
		.get("allow-import-from-derivation")
		.is_none()
	{
		parsed
			.settings
			.set("allow-import-from-derivation", "false")
			.map_err(|e| e.to_string())?;
	}
	let pretty = parsed.has("pretty")
		|| !parsed.has("no-pretty") && std::io::IsTerminal::is_terminal(&std::io::stdout());
	let search = Request {
		regexes,
		excludes: parsed
			.flags
			.get("exclude")
			.into_iter()
			.flatten()
			.map(|v| v[0].clone())
			.collect(),
		json: parsed.has("json"),
		pretty,
	};
	let flake = !parsed.has("expr") && !parsed.has("file");
	let installable = parsed.operands.first().cloned().unwrap_or_default();
	if flake {
		// The flake reference is selected below; only the request is kept.
		parsed.operands.clear();
	}
	let mut options = super::expression::options(&parsed, true)?;
	let mut request = options
		.flake
		.take()
		.unwrap_or_else(|| super::flakes::Request {
			operation: "outputs".into(),
			..Default::default()
		});
	request.search = Some(search);
	options.flake = Some(request);
	options.read_write_mode = true;
	options.strict = true;
	options.json = false;
	let input = if flake {
		let (reference, fragment) = installable.split_once('#').unwrap_or((
			if installable.is_empty() {
				"."
			} else {
				&installable
			},
			"",
		));
		let system = parsed.settings.get("system").unwrap_or("x86_64-linux");
		let candidates: Vec<String> = if fragment.is_empty() {
			vec![
				format!("packages.{system}"),
				format!("legacyPackages.{system}"),
			]
		} else if let Some(exact) = fragment.strip_prefix('.') {
			vec![exact.to_owned()]
		} else {
			vec![
				format!("packages.{system}.{fragment}"),
				format!("legacyPackages.{system}.{fragment}"),
				fragment.to_owned(),
			]
		};
		let candidates: Vec<String> = candidates.iter().map(|c| nix_string_literal(c)).collect();
		format!(
			"__noxi_search (__noxi_flake {}) [ {} ] (__noxi_flake_label {})",
			nix_string_literal(reference),
			candidates.join(" "),
			nix_string_literal(reference),
		)
	} else {
		let root = options.inputs.pop().expect("one selection");
		format!("__noxi_search ({root}\n) [ \"\" ] \"\"")
	};
	options.inputs = vec![input];
	let evaluated = match instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	match evaluated.results.first() {
		Some(Value::String(json)) => {
			println!("{}", json.to_str().map_err(|e| e.to_string())?);
			Ok(ExitCode::SUCCESS)
		}
		Some(Value::Integer(0)) => Err(Failure::Error(
			"no results for the given search term(s)!".into(),
		)),
		_ => Ok(ExitCode::SUCCESS),
	}
}

/// `InstallableFlake::getCursors` when no candidate exists.
fn missing(label: &str, names: &[String]) -> ErrorKind {
	let shown: Vec<String> = names.iter().map(|n| format!("'{n}'")).collect();
	let shown = match shown.split_last() {
		Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
		_ => shown.join(""),
	};
	ErrorKind::EvalError(format!(
		"flake '{label}' does not provide attribute {shown}"
	))
}

/// The first existing candidate attribute path of a flake's outputs, selected
/// without the attribute frames a Nix selection expression would add.
pub async fn select_builtin(
	co: GenCo,
	root: Value,
	candidates: Value,
	label: Value,
) -> Result<Value, ErrorKind> {
	let mut names = Vec::new();
	for candidate in candidates.to_list()?.iter() {
		let candidate = generators::request_force(&co, candidate.clone()).await;
		names.push(candidate.to_contextful_str()?.to_str()?.to_owned());
	}
	let label = label.to_contextful_str()?.to_str()?.to_owned();
	match cursors(&co, root, &names).await? {
		Err(thrown) => Ok(thrown),
		Ok(found) => match found.into_iter().next() {
			Some((_, value)) => Ok(generators::request_force(&co, value).await),
			None => Err(missing(&label, &names)),
		},
	}
}
