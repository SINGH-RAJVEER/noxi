//! Derivation JSON and local build-log retrieval.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufReader};
use std::process::ExitCode;

use super::{Parsed, expression, unsupported};
use crate::cli::Failure;
use nix_compat::cppnix_json::derivation::{Derivation, InputNode, Output};
use nix_compat::derivation::OutputHashMode;
use nix_compat::nixhash::CAHash;
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
		let drv = json_derivation(target.store.as_ref(), &path)?;
		result.insert(path.to_string(), drv.to_json());
	}
	println!("{}", json!({"version":4,"derivations":result}));
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
	let Some((local, _)) = crate::store::open_local(&uri, &parsed.settings)? else {
		return Err(unsupported("log for this store"));
	};
	if let Some(drv) = drv {
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
					if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
						eprintln!("got build log for '{what}' from 'local://'");
					}
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
	if !parsed
		.settings
		.get_list("substituters", &["https://cache.nixos.org/"])
		.is_empty()
	{
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
) -> Result<Derivation, Failure> {
	let utf8 = |bytes: &[u8]| {
		std::str::from_utf8(bytes)
			.map(str::to_owned)
			.map_err(|_| unsupported("derivation show of non-UTF-8 fields"))
	};
	let drv = store.read_derivation(path)?;
	let outputs = if let Some(hash) = drv.outputs().as_fixed_output_hash() {
		let ca = if hash.mode == OutputHashMode::Recursive {
			CAHash::Nar(hash.hash.clone())
		} else {
			CAHash::Flat(hash.hash.clone())
		};
		drv.outputs()
			.names()
			.map(|name| (name.clone(), Output::Fixed(ca.clone())))
			.collect()
	} else if drv.outputs().is_deferred() {
		drv.outputs()
			.names()
			.map(|name| (name.clone(), Output::Deferred))
			.collect()
	} else {
		drv.outputs()
			.iter()
			.map(|(name, path)| (name.clone(), Output::InputAddressed(path.clone())))
			.collect()
	};
	let mut env = drv
		.environment()
		.iter()
		.map(|(key, value)| Ok((utf8(key)?, utf8(value)?)))
		.collect::<Result<BTreeMap<_, _>, Failure>>()?;
	let structured_attrs = env
		.remove("__json")
		.map(|text| serde_json::from_str(&text))
		.transpose()
		.map_err(|e| e.to_string())?;
	Ok(Derivation {
		name: path.name().strip_suffix(".drv").unwrap().to_owned(),
		outputs,
		system: utf8(drv.system())?,
		builder: utf8(drv.command())?,
		args: drv
			.arguments()
			.iter()
			.map(|a| utf8(a))
			.collect::<Result<_, _>>()?,
		env,
		input_sources: drv.input_sources().clone(),
		input_derivations: drv
			.input_derivations()
			.iter()
			.map(|(path, outputs)| {
				(
					path.clone(),
					InputNode {
						outputs: outputs.clone(),
						dynamic_outputs: BTreeMap::new(),
					},
				)
			})
			.collect(),
		structured_attrs,
	})
}
