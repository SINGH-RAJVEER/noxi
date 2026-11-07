//! Store path listings shared by the legacy commands.

use std::collections::BTreeSet;
use std::path::Path;

use nix_compat::store_path::{STORE_DIR, StorePath};
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
	while !path.starts_with(STORE_DIR) || path == Path::new(STORE_DIR) {
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
		Ok((store_path, _)) if path.starts_with(STORE_DIR) => Ok(store_path),
		_ => Err(Failure::Error(format!(
			"path \"{}\" is not in the Nix store",
			path.display()
		))),
	}
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

/// CppNix's `printMissing`, on standard error at `lvlInfo`.
pub fn print_missing(store: &dyn Store, missing: &Missing) -> Result<(), Failure> {
	if !nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
		return Ok(());
	}
	if !missing.will_build.is_empty() {
		if missing.will_build.len() == 1 {
			eprintln!("this derivation will be built:");
		} else {
			eprintln!(
				"these {} derivations will be built:",
				missing.will_build.len()
			);
		}
		for path in topo_sort(store, &missing.will_build)?.iter().rev() {
			eprintln!("  {}", print(path));
		}
	}
	if !missing.will_substitute.is_empty() {
		let sizes = format!(
			"{} download, {} unpacked",
			render_size(missing.download_size as i64, false),
			render_size(missing.nar_size as i64, false)
		);
		if missing.will_substitute.len() == 1 {
			eprintln!("this path will be fetched ({sizes}):");
		} else {
			eprintln!(
				"these {} paths will be fetched ({sizes}):",
				missing.will_substitute.len()
			);
		}
		let mut sorted: Vec<&StorePath> = missing.will_substitute.iter().collect();
		sorted.sort_by(|a, b| (a.name(), print(a)).cmp(&(b.name(), print(b))));
		for path in sorted {
			eprintln!("  {}", print(path));
		}
	}
	if !missing.unknown.is_empty() {
		eprintln!("don't know how to build these paths:");
		for path in &missing.unknown {
			eprintln!("  {}", print(path));
		}
	}
	Ok(())
}
