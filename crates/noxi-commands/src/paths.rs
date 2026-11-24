//! Store path listings shared by the legacy commands.

use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;
use std::rc::Rc;

use nix_compat::store_path::{StorePath, store_dir};
use noxi_store::gc::render_size;
use noxi_store::{Missing, Store};
use rustc_hash::FxHashSet;

use crate::cli::Failure;

fn print(path: &StorePath) -> String {
	path.to_absolute_path()
}

/// `Store::followLinksToStorePath`: follow symlinks outside the store
/// until reaching a store path, relative to `cwd`.
pub fn follow_links_to_store_path(cwd: &Path, arg: &str) -> Result<StorePath, Failure> {
	let mut path = crate::instantiate::absolute_path(cwd, Path::new(arg));
	let mut follows = 0;
	while !path.starts_with(store_dir()) || path == Path::new(store_dir()) {
		match std::fs::symlink_metadata(&path) {
			Ok(meta) if meta.file_type().is_symlink() => {}
			_ => break,
		}
		follows += 1;
		if follows >= 1024 {
			return Err(Failure::Error(format!(
				"too many symbolic links encountered while resolving '{arg}'"
			)));
		}
		let target = std::fs::read_link(&path)?;
		let parent = path.parent().map(Path::to_owned).unwrap_or_default();
		path = crate::instantiate::absolute_path(&parent, &target);
	}
	match StorePath::from_absolute_path_full(&path) {
		Ok((store_path, _)) if path.starts_with(store_dir()) => Ok(store_path),
		_ => Err(Failure::Error(format!(
			"path \"{}\" is not in the Nix store",
			path.display()
		))),
	}
}

/// A store derivation given to `nix-build` or `nix-shell`, as
/// `PATH.drv!OUTPUT`, with the selected outputs.
pub type StoreDerivation = (StorePath, BTreeSet<String>);

/// Take the arguments that name store derivations, directly or through
/// symlinks, out of `inputs`; the rest are expressions to evaluate.
pub fn take_store_derivations(cwd: &Path, inputs: &mut Vec<String>) -> Vec<StoreDerivation> {
	let mut found = Vec::new();
	inputs.retain(|input| {
		if input == "-" {
			return true;
		}
		// `canonPath` resolves the symlinks of the part that exists.
		let path = crate::instantiate::absolute_path(cwd, Path::new(input));
		let resolved = std::fs::canonicalize(&path).unwrap_or_else(|_| {
			match (path.parent().map(std::fs::canonicalize), path.file_name()) {
				(Some(Ok(parent)), Some(name)) => parent.join(name),
				_ => path.clone(),
			}
		});
		let Some(text) = resolved.to_str() else {
			return true;
		};
		let (path, outputs) = text.split_once('!').unwrap_or((text, ""));
		let Ok(path) = StorePath::from_absolute_path(path.as_bytes()) else {
			return true;
		};
		if !path.name().ends_with(".drv") {
			return true;
		}
		let outputs = outputs
			.split(',')
			.filter(|output| !output.is_empty())
			.map(str::to_owned)
			.collect();
		found.push((path, outputs));
		false
	});
	found
}

/// `PackageInfo`: the output a store derivation argument selects, by default
/// the one the derivation's `outputName` names, or `out`.
pub fn store_derivation_output(
	store: &dyn Store,
	(path, outputs): &StoreDerivation,
) -> Result<(StorePath, String), Failure> {
	store.ensure_path(path)?;
	let drv = store.read_build_derivation(path)?;
	let mut selected = outputs.iter();
	let output = match (selected.next(), selected.next()) {
		(Some(output), None) => output.clone(),
		(Some(_), Some(_)) => {
			return Err(Failure::Error(format!(
				"building more than one derivation output is not supported, in '{}!{}'",
				print(path),
				outputs.iter().cloned().collect::<Vec<_>>().join(",")
			)));
		}
		(None, _) => drv
			.basic
			.environment()
			.get(b"outputName".as_slice())
			.map_or("out".into(), |name| {
				String::from_utf8_lossy(name).into_owned()
			}),
	};
	if !drv
		.basic
		.outputs()
		.names()
		.any(|name| name.as_bytes() == output.as_bytes())
	{
		return Err(Failure::Error(format!(
			"derivation '{}' does not have output '{output}'",
			print(path)
		)));
	}
	Ok((path.clone(), output))
}

/// The store and the derivations to build: the store derivations in
/// `arguments`, then those the expressions evaluate to. A failed evaluation
/// has printed its errors and gives the exit code.
#[allow(clippy::type_complexity)]
pub fn select_derivations(
	options: &crate::instantiate::Options,
	cwd: &Path,
	arguments: &[StoreDerivation],
) -> Result<Result<(Rc<dyn Store>, Vec<(StorePath, String)>), ExitCode>, Failure> {
	let no_store = || Failure::Error("the selected store cannot build derivations".to_owned());
	let (store, evaluated) = if options.inputs.is_empty() {
		let uri = crate::store::store_uri(options.store.as_deref(), &options.settings);
		let opened = crate::store::open(&uri, &options.settings)?.ok_or_else(no_store)?;
		(opened.store, Vec::new())
	} else {
		match crate::instantiate::evaluate(options, cwd) {
			Ok(evaluated) => (
				evaluated.store.ok_or_else(no_store)?,
				crate::instantiate::derivations(&evaluated.results)?,
			),
			Err(code) => return Ok(Err(code)),
		}
	};
	let mut derivations = Vec::new();
	for argument in arguments {
		derivations.push(store_derivation_output(store.as_ref(), argument)?);
	}
	derivations.extend(evaluated);
	Ok(Ok((store, derivations)))
}

/// CppNix's `topoSortPaths`: each path before the paths it refers to.
/// Paths outside `paths` are not followed.
pub fn topo_sort(
	store: &dyn Store,
	paths: &BTreeSet<StorePath>,
) -> Result<Vec<StorePath>, Failure> {
	topo_sort_by(paths, |path| {
		Ok(store
			.query_path_info(path)?
			.map(|info| info.references)
			.unwrap_or_default())
	})
}

/// [`topo_sort`] with the references of each path from `references`.
pub fn topo_sort_by(
	paths: &BTreeSet<StorePath>,
	mut references: impl FnMut(&StorePath) -> Result<BTreeSet<StorePath>, Failure>,
) -> Result<Vec<StorePath>, Failure> {
	fn visit(
		references: &mut dyn FnMut(&StorePath) -> Result<BTreeSet<StorePath>, Failure>,
		items: &FxHashSet<&StorePath>,
		path: &StorePath,
		parent: Option<&StorePath>,
		visited: &mut FxHashSet<StorePath>,
		parents: &mut FxHashSet<StorePath>,
		sorted: &mut Vec<StorePath>,
	) -> Result<(), Failure> {
		if parents.contains(path) {
			return Err(Failure::Error(format!(
				"cycle detected in the references of '{}' from '{}'",
				print(path),
				parent.map(print).unwrap_or_default()
			)));
		}
		if !visited.insert(path.clone()) {
			return Ok(());
		}
		parents.insert(path.clone());
		for reference in &references(path)? {
			if reference != path && items.contains(reference) {
				visit(
					references,
					items,
					reference,
					Some(path),
					visited,
					parents,
					sorted,
				)?;
			}
		}
		sorted.push(path.clone());
		parents.remove(path);
		Ok(())
	}

	// Membership only; iteration order comes from the sorted sets.
	let items: FxHashSet<&StorePath> = paths.iter().collect();
	let mut sorted = Vec::new();
	let mut visited = FxHashSet::default();
	let mut parents = FxHashSet::default();
	for path in paths {
		visit(
			&mut references,
			&items,
			path,
			None,
			&mut visited,
			&mut parents,
			&mut sorted,
		)?;
	}
	sorted.reverse();
	Ok(sorted)
}

/// CppNix's `printMissing`, through the logger at `lvlInfo`.
pub fn print_missing(store: &dyn Store, missing: &Missing) -> Result<(), Failure> {
	if !nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
		return Ok(());
	}
	let info = |message: &str| {
		nix_compat::log::logger::log(nix_compat::log::VerbosityLevel::Info, message)
	};
	if !missing.will_build.is_empty() {
		if missing.will_build.len() == 1 {
			info("this derivation will be built:");
		} else {
			info(&format!(
				"these {} derivations will be built:",
				missing.will_build.len()
			));
		}
		for path in topo_sort(store, &missing.will_build)?.iter().rev() {
			info(&format!("  {}", print(path)));
		}
	}
	if !missing.will_substitute.is_empty() {
		let sizes = format!(
			"{} download, {} unpacked",
			render_size(missing.download_size as i64, false),
			render_size(missing.nar_size as i64, false)
		);
		if missing.will_substitute.len() == 1 {
			info(&format!("this path will be fetched ({sizes}):"));
		} else {
			info(&format!(
				"these {} paths will be fetched ({sizes}):",
				missing.will_substitute.len()
			));
		}
		let mut sorted: Vec<&StorePath> = missing.will_substitute.iter().collect();
		sorted.sort_by(|a, b| (a.name(), print(a)).cmp(&(b.name(), print(b))));
		for path in sorted {
			info(&format!("  {}", print(path)));
		}
	}
	if !missing.unknown.is_empty() {
		info("don't know how to build these paths:");
		for path in &missing.unknown {
			info(&format!("  {}", print(path)));
		}
	}
	Ok(())
}
