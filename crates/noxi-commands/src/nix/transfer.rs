//! Copies between stores, and to and from local binary caches.
use super::{Parsed, expression, unsupported};
use crate::cli::Failure;
use nix_compat::narinfo::parse_keypair;
use noxi_store::Store;
use noxi_store::binary_cache::BinaryCache;
use std::collections::BTreeSet;
use std::fs;
use std::process::ExitCode;

pub fn copy(mut parsed: Parsed) -> Result<ExitCode, Failure> {
	parsed.unsupported(&["out-link", "profile", "substitute-on-destination"])?;
	let from = parsed.value("from").map(str::to_owned);
	let to = parsed.value("to").map(str::to_owned);
	let is_store = |uri: &Option<String>| uri.as_deref().is_none_or(|u| !u.starts_with("file://"));
	if (from.is_some() || to.is_some()) && is_store(&from) && is_store(&to) {
		return copy_between_stores(&parsed, from, to);
	}
	if let Some(uri) = parsed.value("from").map(str::to_owned) {
		if !uri.starts_with("file://")
			|| parsed.has("to")
			|| parsed.has("expr")
			|| parsed.has("file")
		{
			return Err(unsupported(
				"copy --from except file:// paths into the selected local store",
			));
		}
		if parsed.has("no-check-sigs") {
			parsed
				.settings
				.set("require-sigs", "false")
				.map_err(|e| e.to_string())?;
		}
		let destination_uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
		let (destination, _) = crate::store::open_local(&destination_uri, &parsed.settings)?
			.ok_or_else(|| unsupported("copy into this store"))?;
		let cache = BinaryCache::open(&uri)?;
		if parsed.operands.is_empty() {
			return Err(unsupported("copy of implicit flake installables"));
		}
		let mut missing = BTreeSet::new();
		let mut pending = parsed
			.operands
			.iter()
			.map(|path| {
				nix_compat::store_path::StorePath::from_absolute_path(path.as_bytes())
					.map_err(|e| e.to_string())
			})
			.collect::<Result<Vec<_>, _>>()?;
		while let Some(path) = pending.pop() {
			if destination.is_valid_path(&path)? || !missing.insert(path.clone()) {
				continue;
			}
			let cached = cache.query_path_info(&path)?.ok_or_else(|| {
				Failure::Error(format!("path '{}' is not valid", path.to_absolute_path()))
			})?;
			pending.extend(cached.info.references);
		}
		if !missing.is_empty() {
			log(&format!("copying {} paths...", missing.len()));
		}
		for path in &missing {
			log(&format!(
				"copying path '{}' from '{}'...",
				path.to_absolute_path(),
				uri.split('?').next().unwrap_or(&uri)
			));
		}
		for path in &parsed.operands {
			let path = nix_compat::store_path::StorePath::from_absolute_path(path.as_bytes())
				.map_err(|e| e.to_string())?;
			destination.substitute(std::slice::from_ref(&cache), &path)?;
		}
		return Ok(ExitCode::SUCCESS);
	}
	let uri = parsed
		.value("to")
		.ok_or_else(|| Failure::Usage("you must pass '--from' and/or '--to'".into()))?;
	if !uri.starts_with("file://") {
		return Err(unsupported("copy to stores other than file://"));
	}
	let params: Vec<_> = uri
		.split_once('?')
		.map_or("", |(_, params)| params)
		.split('&')
		.filter(|p| !p.is_empty())
		.collect();
	if !params.contains(&"compression=none") {
		return Err(unsupported(
			"copy to compressed file:// caches; use ?compression=none (the upload API supports uncompressed NARs)",
		));
	}
	let mut key_files = Vec::new();
	for param in &params {
		if *param == "compression=none" {
			continue;
		}
		if let Some(path) = param.strip_prefix("secret-key=") {
			key_files.push(path);
		} else if let Some(paths) = param.strip_prefix("secret-keys=") {
			key_files.extend(paths.split(','));
		} else {
			return Err(unsupported("copy with these destination parameters"));
		}
	}
	if key_files.is_empty() {
		return Err(unsupported(
			"copy to unsigned file:// caches (the upload API requires a signing key)",
		));
	}
	let keys = key_files
		.iter()
		.map(|path| {
			parse_keypair(fs::read_to_string(path)?.trim())
				.map(|(key, _)| key)
				.map_err(|e| std::io::Error::other(e.to_string()))
		})
		.collect::<Result<Vec<_>, _>>()?;
	let target = match expression::targets(&parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(code),
	};
	let source_uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let (source, _) = crate::store::open_local(&source_uri, &parsed.settings)?
		.ok_or_else(|| unsupported("copy from this store"))?;
	let destination = BinaryCache::open(uri)?;
	let mut roots = BTreeSet::new();
	for path in target.paths {
		roots.extend(expression::outputs(target.store.as_ref(), &path)?.into_values());
	}
	let closure = source.compute_closure(&roots)?;
	let mut missing = BTreeSet::new();
	for path in &closure {
		if destination.query_path_info(path)?.is_none() {
			missing.insert(path.clone());
		}
	}
	for path in &closure {
		source.add_temp_root(path)?;
	}
	if !missing.is_empty() {
		log(&format!("copying {} paths...", missing.len()));
	}
	for path in crate::paths::topo_sort(&source, &missing)?
		.into_iter()
		.rev()
	{
		log(&format!(
			"copying path '{}' to '{}'...",
			path.to_absolute_path(),
			uri.split('?').next().unwrap_or(uri)
		));
		destination.upload_closure(&source, &path, &keys)?;
	}
	Ok(ExitCode::SUCCESS)
}

/// `copyPaths` from a local store into another store: the closure's
/// paths the destination lacks, dependencies first, in one stream.
fn copy_between_stores(
	parsed: &Parsed,
	from: Option<String>,
	to: Option<String>,
) -> Result<ExitCode, Failure> {
	if parsed.has("expr") || parsed.has("file") {
		return Err(unsupported("copy of expressions between stores"));
	}
	if parsed.operands.is_empty() {
		return Err(unsupported("copy of implicit flake installables"));
	}
	let default = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let source_uri = from.unwrap_or_else(|| default.clone());
	let destination_uri = to.unwrap_or(default);
	let (source, _) = crate::store::open_local(&source_uri, &parsed.settings)?
		.ok_or_else(|| unsupported("copy from this store"))?;
	let destination = crate::store::open(&destination_uri, &parsed.settings)?
		.ok_or_else(|| unsupported("copy into this store"))?;
	let roots = parsed
		.operands
		.iter()
		.map(|path| {
			nix_compat::store_path::StorePath::from_absolute_path(path.as_bytes())
				.map_err(|_| unsupported("copy of installables other than store paths"))
		})
		.collect::<Result<BTreeSet<_>, _>>()?;
	let closure = source.compute_closure(&roots)?;
	for path in &closure {
		source.add_temp_root(path)?;
	}
	let valid = destination.store.query_valid_paths(&closure)?;
	let missing: BTreeSet<_> = closure.difference(&valid).cloned().collect();
	if missing.is_empty() {
		return Ok(ExitCode::SUCCESS);
	}
	log(&format!("copying {} paths...", missing.len()));
	let mut infos = Vec::new();
	for path in crate::paths::topo_sort(&source, &missing)?
		.into_iter()
		.rev()
	{
		infos.push(
			source
				.query_path_info(&path)?
				.ok_or_else(|| noxi_store::Error::InvalidPath(path.to_absolute_path()))?,
		);
	}
	let shown = destination_uri
		.split('?')
		.next()
		.unwrap_or(&destination_uri);
	let shown = if shown == "local" { "local://" } else { shown };
	destination.store.add_multiple_to_store(
		&infos,
		&mut |path, sink| {
			log(&format!(
				"copying path '{}' to '{shown}'...",
				path.to_absolute_path()
			));
			source.nar_from_path(path, sink)
		},
		false,
		!parsed.has("no-check-sigs"),
	)?;
	Ok(ExitCode::SUCCESS)
}

fn log(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
		eprintln!("{message}");
	}
}
