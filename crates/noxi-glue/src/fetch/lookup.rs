//! Fetched lookup path entries: pseudo-URLs, and the `flake:` hook of
//! CppNix 2.35.2's `common-eval-args.cc`.

use std::path::PathBuf;

use noxi_eval::LookupError;
use noxi_flakeref::error::{Error, Result};
use noxi_flakeref::flake_ref::{ParseOptions, parse_flake_ref};
use noxi_flakeref::input::{Attrs, Input};
use noxi_flakeref::registry::UseRegistries;

use crate::store::EvaluatorStore;

/// `EvalSettings::resolvePseudoUrl`.
fn resolve_pseudo_url(url: &str) -> String {
	match url.strip_prefix("channel:") {
		Some(channel) => format!("https://channels.nixos.org/{channel}/nixexprs.tar.xz"),
		None => url.to_owned(),
	}
}

/// The store path of the tree a fetched lookup path entry names.
pub(crate) fn resolve(store: &EvaluatorStore, value: &str) -> std::io::Result<PathBuf> {
	match value.strip_prefix("flake:") {
		Some(reference) => resolve_flake(store, reference),
		None => resolve_tarball(store, &resolve_pseudo_url(value)),
	}
	.map_err(|error| {
		std::io::Error::other(LookupError::Fatal {
			message: error.message,
			// `Error` records its context innermost first.
			traces: error.traces.into_iter().rev().collect(),
		})
	})
}

/// `fetchers::downloadTarball`. Unlike a fetched flake, the result is not
/// added to the paths restricted evaluation may access.
fn resolve_tarball(store: &EvaluatorStore, url: &str) -> Result<PathBuf> {
	let attrs: Attrs = [
		("type".into(), "tarball".into()),
		("url".into(), url.into()),
	]
	.into();
	let input = Input::from_attrs(&store.fetch_settings(), attrs)?;
	let (mount, _, locked, _) = super::get_cached(store, &input, UseRegistries::No)?;
	let (path, _) = store.with_hidden_mounts(|| super::add_tree(store, &locked, &mount))?;
	Ok(path.to_absolute_path().into())
}

/// The reference has no base directory, and the whole source tree is the
/// entry's directory whatever the reference's `dir` is.
fn resolve_flake(store: &EvaluatorStore, reference: &str) -> Result<PathBuf> {
	let settings = store.fetch_settings();
	if !settings.flakes {
		return Err(Error::new(nix_compat::nixcpp::hash::missing_feature(
			"flakes",
		)));
	}
	let reference = parse_flake_ref(
		&settings,
		reference,
		ParseOptions {
			allow_missing: true,
			is_flake: false,
			..ParseOptions::default()
		},
	)?;
	let (mount, _, mut locked, _) = super::get_cached(store, &reference.input, UseRegistries::All)?;
	let path = super::mount_input(store, &mut locked, &reference.input, &mount)?;
	Ok(path.to_absolute_path().into())
}

#[cfg(test)]
mod tests {
	use super::resolve_pseudo_url;

	#[test]
	fn resolves_pseudo_urls() {
		for (value, url) in [
			(
				"channel:nixos-26.05",
				"https://channels.nixos.org/nixos-26.05/nixexprs.tar.xz",
			),
			(
				"channel://x",
				"https://channels.nixos.org///x/nixexprs.tar.xz",
			),
			("file:///a.tar.gz", "file:///a.tar.gz"),
		] {
			assert_eq!(resolve_pseudo_url(value), url);
		}
	}
}
