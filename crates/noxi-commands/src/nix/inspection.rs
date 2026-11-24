//! Derivation JSON and local build-log retrieval.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufReader};
use std::process::ExitCode;

use super::{Parsed, expression, unsupported};
use crate::cli::Failure;
use nix_compat::cppnix_json::derivation::{Derivation, Features};
use nix_compat::nixcpp::settings::Settings;
use noxi_store::DerivedPath;
use serde_json::json;

pub fn show(parsed: Parsed) -> Result<ExitCode, Failure> {
	let target = match expression::targets(&parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(code),
	};
	let mut paths = BTreeSet::new();
	for path in target.paths {
		let path = match path {
			DerivedPath::Opaque(path) | DerivedPath::Built { drv: path, .. } => path,
		};
		if path.name().ends_with(".drv") {
			paths.insert(path);
		} else {
			paths.insert(
				target
					.store
					.query_path_info(&path)?
					.and_then(|info| info.deriver)
					.ok_or_else(|| {
						Failure::Error(format!(
							"path '{}' does not have a known deriver",
							path.to_absolute_path()
						))
					})?,
			);
		}
	}
	if parsed.has("recursive") {
		paths = target.store.compute_closure(&paths)?;
	}
	let mut result = BTreeMap::new();
	for path in paths {
		if !path.name().ends_with(".drv") {
			continue;
		}
		let drv = json_derivation(target.store.as_ref(), &path, features(&parsed.settings))?;
		result.insert(path.to_string(), drv.to_json());
	}
	// nlohmann's `dump()`, so structured-attribute floats print as the
	// reference prints them (`1.5e-07`).
	println!(
		"{}",
		nix_compat::cppnix_json::dump(&json!({"version": 4, "derivations": result}))
	);
	Ok(ExitCode::SUCCESS)
}

pub fn log(parsed: Parsed) -> Result<ExitCode, Failure> {
	let target = match expression::targets(&parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(code),
	};
	if target.paths.len() != 1 {
		return Err(Failure::Usage("exactly one installable is required".into()));
	}
	let original = &target.paths[0];
	let path = match original {
		DerivedPath::Opaque(path) | DerivedPath::Built { drv: path, .. } => path,
	};
	let what = parsed
		.operands
		.first()
		.cloned()
		.unwrap_or_else(|| path.to_absolute_path());
	let drv = if path.name().ends_with(".drv") {
		Some(path.clone())
	} else {
		target
			.store
			.query_path_info(path)?
			.and_then(|info| info.deriver)
	};
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	// `fetchBuildLog`: the store, then the substituters that keep logs.
	let found = |from: &str| {
		// `printInfo`.
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
			eprintln!("got build log for '{what}' from '{from}'");
		}
	};
	let own_cache = crate::store::cache(&uri)?;
	if let (Some(cache), Some(drv)) = (&own_cache, &drv)
		&& let Some(log) = cache.build_log(drv)?
	{
		found(cache.uri());
		io::Write::write_all(&mut io::stdout().lock(), &log)?;
		return Ok(ExitCode::SUCCESS);
	}
	if own_cache.is_none()
		&& let Some(drv) = &drv
	{
		let Some((local, _)) = crate::store::open_local(&uri, &parsed.settings)? else {
			return Err(unsupported("log for this store"));
		};
		let name = drv.to_string();
		let base = local
			.config()
			.log_dir
			.join("drvs")
			.join(&name[..2])
			.join(&name[2..]);
		for (path, compression) in [
			(base.clone(), None),
			(
				base.with_file_name(format!("{}.bz2", &name[2..])),
				Some("bzip2"),
			),
		] {
			match fs::File::open(&path) {
				Ok(file) => {
					found(local.config().human_uri());
					let mut reader = noxi_store::binary_cache::decompress(
						compression,
						Box::new(BufReader::new(file)),
					)?;
					io::copy(&mut reader, &mut io::stdout().lock())?;
					return Ok(ExitCode::SUCCESS);
				}
				Err(error) if error.kind() == io::ErrorKind::NotFound => {}
				Err(error) => return Err(error.into()),
			}
		}
	}
	let mut unread = false;
	for uri in parsed
		.settings
		.get_list("substituters", &["https://cache.nixos.org/"])
	{
		match (crate::store::cache(uri), &drv) {
			(Ok(Some(cache)), Some(drv)) => {
				if let Some(log) = cache.build_log(drv)? {
					found(cache.uri());
					io::Write::write_all(&mut io::stdout().lock(), &log)?;
					return Ok(ExitCode::SUCCESS);
				}
			}
			(Ok(Some(_)), None) => {}
			_ => unread = true,
		}
	}
	if unread {
		return Err(unsupported(
			"log from substituters (remote log retrieval API unavailable)",
		));
	}
	Err(Failure::Error(format!(
		"build log of '{what}' is not available"
	)))
}

/// The derivation at `path` in CppNix's JSON model, which `derivation show`
/// prints and `develop` rewrites.
pub(super) fn json_derivation(
	store: &dyn noxi_store::Store,
	path: &nix_compat::store_path::StorePath,
	features: Features,
) -> Result<Derivation, Failure> {
	let bytes = fs::read(store.real_path(path)).map_err(|error| {
		Failure::Error(format!(
			"reading derivation '{}': {error}",
			path.to_absolute_path()
		))
	})?;
	let name = path.name().strip_suffix(".drv").unwrap_or(path.name());
	Derivation::from_aterm(&bytes, name, features).map_err(|error| {
		Failure::Error(if error.is_format_error() {
			format!(
				"error parsing derivation '{}': {error}",
				path.to_absolute_path()
			)
		} else {
			error.to_string()
		})
	})
}

/// The experimental features `settings` enables.
pub(super) fn features(settings: &Settings) -> Features {
	let enabled = settings.experimental_features();
	Features {
		ca_derivations: enabled.contains("ca-derivations"),
		dynamic_derivations: enabled.contains("dynamic-derivations"),
		impure_derivations: enabled.contains("impure-derivations"),
		git_hashing: enabled.contains("git-hashing"),
	}
}

/// Import the version-4 derivation JSON read from stdin.
pub fn add(parsed: Parsed) -> Result<ExitCode, Failure> {
	let mut input = Vec::new();
	io::Read::read_to_end(&mut io::stdin().lock(), &mut input)?;
	let value: serde_json::Value = serde_json::from_slice(&input).map_err(|e| {
		unsupported(&format!(
			"derivation add of malformed JSON (nlohmann parse diagnostics are not reproduced: {e})"
		))
	})?;
	let features = features(&parsed.settings);
	let mut drv = Derivation::from_json(&value, features).map_err(|e| e.to_string())?;
	let opened = super::command_store::open(&parsed)?;
	let store = opened.store.as_ref();
	// `Derivation::parseJsonAndValidate`.
	let read =
		|path: &nix_compat::store_path::StorePath| json_derivation(store, path, features).ok();
	let mut warnings = Vec::new();
	drv.fill_in_output_paths(&read, &mut warnings)
		.map_err(|e| Failure::Error(e.to_string()))?;
	drv.check_invariants(&read, &mut warnings).map_err(|e| {
		Failure::Error(format!(
			"\n\u{2026} while checking derivation from JSON with name '{}'\n\nerror: {e}",
			drv.name
		))
	})?;
	for warning in warnings {
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
			eprintln!("warning: {warning}");
		}
	}
	let path = drv.path().map_err(|e| Failure::Error(e.to_string()))?;
	if !(parsed.has("dry-run") || parsed.settings.get_bool("read-only", false)) {
		let aterm = drv
			.unparse(false, None)
			.map_err(|e| Failure::Error(e.to_string()))?;
		let references: BTreeSet<_> = drv
			.input_sources
			.iter()
			.chain(drv.input_derivations.keys())
			.cloned()
			.collect();
		let written =
			store.add_text_to_store(&format!("{}.drv", drv.name), aterm.as_bytes(), &references)?;
		debug_assert_eq!(written, path);
	}
	println!("{}", path.to_absolute_path());
	Ok(ExitCode::SUCCESS)
}
