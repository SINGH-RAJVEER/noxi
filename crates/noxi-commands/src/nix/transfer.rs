//! Copies between stores, and to and from local binary caches.
use super::{Parsed, expression, unsupported};
use crate::cli::Failure;
use nix_compat::narinfo::parse_keypair;
use noxi_store::Store;
use noxi_store::binary_cache::BinaryCache;
use std::collections::BTreeSet;
use std::fs;
use std::process::ExitCode;

pub fn copy(parsed: Parsed) -> Result<ExitCode, Failure> {
	parsed.unsupported(&["out-link", "profile", "substitute-on-destination"])?;
	let from = parsed.value("from").map(str::to_owned);
	let to = parsed.value("to").map(str::to_owned);
	let is_store = |uri: &Option<String>| uri.as_deref().is_none_or(|u| !u.starts_with("file://"));
	if (from.is_some() || to.is_some()) && is_store(&from) && is_store(&to) {
		return copy_between_stores(&parsed, from, to);
	}
	if let Some(uri) = parsed.value("from").map(str::to_owned) {
		return copy_from_cache(parsed, &uri);
	}
	let uri = parsed
		.value("to")
		.ok_or_else(|| Failure::Usage("you must pass '--from' and/or '--to'".into()))?;
	if !uri.starts_with("file://") {
		return Err(unsupported("copy to stores other than file://"));
	}
	// `LocalBinaryCacheStore`'s construction and `init`: settings and
	// signing keys are read, then the cache's directories are made.
	let destination = BinaryCache::open(uri).map_err(|e| {
		let message = e.to_string();
		if message.starts_with("option '") {
			Failure::Usage(message)
		} else {
			Failure::Error(message)
		}
	})?;
	let keys = destination
		.upload_settings()
		.secret_key_files
		.iter()
		.map(|path| {
			let text = fs::read_to_string(path).map_err(|e| {
				Failure::Error(format!(
					"opening file \"{path}\": {}",
					super::hash::strerror(&e)
				))
			})?;
			parse_keypair(text.trim())
				.map(|(key, _)| key)
				.map_err(|e| Failure::Error(e.to_string()))
		})
		.collect::<Result<Vec<_>, _>>()?;
	destination.init_for_upload()?;
	let target = match expression::targets(&parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(code),
	};
	// `BuiltPathsCommand` realises derivation outputs before copying them.
	let built: Vec<noxi_store::DerivedPath> = target
		.paths
		.iter()
		.filter(|path| matches!(path, noxi_store::DerivedPath::Built { .. }))
		.cloned()
		.collect();
	if !built.is_empty() {
		let missing = target.store.query_missing(&built)?;
		crate::paths::print_missing(target.store.as_ref(), &missing)?;
		target.store.build_paths(&built)?;
	}
	let source_uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let (source, _) = crate::store::open_local(&source_uri, &parsed.settings)?
		.ok_or_else(|| unsupported("copy from this store"))?;
	let mut roots = BTreeSet::new();
	let mut realisations = Realisations::new();
	for path in target.paths {
		roots.extend(realised_paths(&source, &path, &mut realisations)?);
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
	// `BinaryCacheStore::addMultipleToStore` starts the NAR uploads largest
	// first, which is when each path's copy is reported, and publishes the
	// narinfo files dependencies first.
	let mut by_size = Vec::new();
	for path in &missing {
		let info = source
			.query_path_info(path)?
			.ok_or_else(|| noxi_store::Error::InvalidPath(path.to_absolute_path()))?;
		by_size.push((std::cmp::Reverse(info.nar_size), path));
	}
	by_size.sort();
	for (_, path) in by_size {
		log(&format!(
			"copying path '{}' to '{}'...",
			path.to_absolute_path(),
			uri.split('?').next().unwrap_or(uri)
		));
	}
	for path in crate::paths::topo_sort(&source, &missing)?
		.into_iter()
		.rev()
	{
		// The cache signs with its own `secret-key` and `secret-keys`.
		destination.upload(&source, &path, &keys[..0])?;
	}
	// `copyPaths` registers the realisations once their outputs are in the
	// cache, as `build-trace-v2/<drv>/<output>.doi` files.
	for ((drv, name), realisation) in &realisations {
		destination.upload_realisation(drv, name, realisation)?;
	}
	Ok(ExitCode::SUCCESS)
}

type Realisations = std::collections::BTreeMap<
	(nix_compat::store_path::StorePath, String),
	noxi_store::Realisation,
>;

/// `toRealisedPaths`: the store paths of an installable's outputs. The
/// realisations of content-addressed outputs are copied with the outputs.
fn realised_paths(
	source: &dyn Store,
	path: &noxi_store::DerivedPath,
	realisations: &mut Realisations,
) -> Result<Vec<nix_compat::store_path::StorePath>, Failure> {
	let noxi_store::DerivedPath::Built { drv, outputs } = path else {
		return Ok(expression::outputs(source, path)?.into_values().collect());
	};
	if !source.ca_derivations() {
		return Ok(expression::outputs(source, path)?.into_values().collect());
	}
	let all: BTreeSet<String> = source
		.query_partial_derivation_output_map(drv)?
		.into_keys()
		.collect();
	if let Some(name) = outputs.difference(&all).next() {
		return Err(Failure::Error(format!(
			"derivation '{}' does not have an output named '{name}'",
			drv.to_absolute_path()
		)));
	}
	let names = if outputs.is_empty() {
		all
	} else {
		outputs.clone()
	};
	let mut paths = Vec::new();
	for name in names {
		let mut collect = |drv: &nix_compat::store_path::StorePath,
		                   name: &str,
		                   realisation: &noxi_store::Realisation| {
			realisations.insert((drv.clone(), name.to_owned()), realisation.clone());
		};
		let output = noxi_build::local::deep_query_output(source, drv, &name, &mut collect)
			.map_err(|e| Failure::Error(e.to_string()))?;
		paths.push(output.ok_or_else(|| {
			Failure::Error(format!(
				"output '{name}' of derivation '{}' has not been built",
				drv.to_absolute_path()
			))
		})?);
	}
	Ok(paths)
}

/// `nix copy --from file://CACHE`: the cache is the store installables are
/// evaluated and realised in.
fn copy_from_cache(mut parsed: Parsed, uri: &str) -> Result<ExitCode, Failure> {
	let destination_uri = match parsed.value("to") {
		Some(to) => to.to_owned(),
		None => crate::store::store_uri(parsed.value("store"), &parsed.settings),
	};
	let check_sigs = !parsed.has("no-check-sigs");
	parsed
		.flags
		.insert("store".to_owned(), vec![vec![uri.to_owned()]]);
	let target = match expression::targets(&parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(code),
	};
	let source = target.store;
	if target
		.paths
		.iter()
		.any(|path| matches!(path, noxi_store::DerivedPath::Built { .. }))
	{
		let missing = source.query_missing(&target.paths)?;
		crate::paths::print_missing(source.as_ref(), &missing)?;
		source.build_paths(&target.paths)?;
	}
	let mut roots = BTreeSet::new();
	let mut realisations = Realisations::new();
	for path in &target.paths {
		roots.extend(realised_paths(source.as_ref(), path, &mut realisations)?);
	}
	let destination = crate::store::open(&destination_uri, &parsed.settings)?
		.ok_or_else(|| unsupported("copy into this store"))?
		.store;
	let mut missing = BTreeSet::new();
	let mut pending: Vec<_> = roots.into_iter().collect();
	while let Some(path) = pending.pop() {
		if destination.is_valid_path(&path)? || !missing.insert(path.clone()) {
			continue;
		}
		let info = source.query_path_info(&path)?.ok_or_else(|| {
			Failure::Error(format!("path '{}' is not valid", path.to_absolute_path()))
		})?;
		pending.extend(info.references);
	}
	if !missing.is_empty() {
		log(&format!("copying {} paths...", missing.len()));
	}
	for path in &missing {
		log(&format!(
			"copying path '{}' from '{}'...",
			path.to_absolute_path(),
			uri.split('?').next().unwrap_or(uri)
		));
	}
	let mut infos = Vec::new();
	for path in crate::paths::topo_sort(source.as_ref(), &missing)?
		.into_iter()
		.rev()
	{
		let mut info = source
			.query_path_info(&path)?
			.ok_or_else(|| noxi_store::Error::InvalidPath(path.to_absolute_path()))?;
		// A copy is never ultimately trusted.
		info.ultimate = false;
		infos.push(info);
	}
	destination.add_multiple_to_store(
		&infos,
		&mut |path, sink| source.nar_from_path(path, sink),
		false,
		check_sigs,
	)?;
	for ((drv, name), realisation) in &realisations {
		if check_sigs {
			destination.register_checked_drv_output(drv, name, realisation)?;
		} else {
			destination.register_drv_output(drv, name, realisation)?;
		}
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
		let mut info = source
			.query_path_info(&path)?
			.ok_or_else(|| noxi_store::Error::InvalidPath(path.to_absolute_path()))?;
		// A copy is never ultimately trusted.
		info.ultimate = false;
		infos.push(info);
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
