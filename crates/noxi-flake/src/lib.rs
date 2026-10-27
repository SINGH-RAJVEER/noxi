//! Library operations corresponding to CppNix's `nix flake` commands.
//!
//! These functions use a caller-owned evaluator and store. They do not parse
//! command lines, print diagnostics, or choose a host store. Evaluation functions
//! run inside an evaluator generator, using its `GenCo` to force lazy outputs.
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use bstr::ByteSlice;
use noxi_eval::{
	ErrorKind, Value,
	generators::{self, GenCo},
};
use noxi_flakeref::input::{
	Scheme, attrs_to_json, git_rev, maybe_get_bool_attr, maybe_get_str_attr,
};
use noxi_flakeref::lockfile::{Edge, NodeId, ROOT};
use noxi_flakeref::registry::UseRegistries;
use noxi_glue::EvaluatorStore;
use noxi_glue::fetch::flake::{fetch_source, parse_reference};
use serde_json::{Value as Json, json};
use sha2::{Digest, Sha256};

pub use noxi_glue::fetch::flake::{LockOptions, LockedFlake};
mod outputs;
pub use outputs::{CheckOptions, CheckReport, ShowOptions, ShowResult, check, show};

fn error(message: impl std::fmt::Display) -> ErrorKind {
	ErrorKind::EvalError(message.to_string())
}

/// Compute locks without changing the source checkout. Persist explicitly with
/// `LockedFlake::write_lock_file` after deciding the caller's write policy.
pub async fn lock(
	store: &EvaluatorStore,
	co: &GenCo,
	reference: &str,
	options: &LockOptions,
) -> Result<LockedFlake, ErrorKind> {
	noxi_glue::fetch::flake::lock_flake(store, co, reference, options).await
}

/// Update selected paths, or all inputs when `update_inputs` is empty.
pub async fn update(
	store: &EvaluatorStore,
	co: &GenCo,
	reference: &str,
	options: &LockOptions,
) -> Result<LockedFlake, ErrorKind> {
	let mut options = options.clone();
	options.recreate |= options.update_inputs.is_empty();
	lock(store, co, reference, &options).await
}

fn source_path(flake: &LockedFlake) -> Result<String, ErrorKind> {
	Ok(flake
		.source_info()
		.to_attrs()?
		.select_required("outPath")?
		.to_contextful_str()?
		.to_str()?
		.to_owned())
}

/// CppNix's metadata JSON. This does not force evaluation of flake outputs.
pub fn metadata(store: &EvaluatorStore, flake: &LockedFlake) -> Result<Json, ErrorKind> {
	let mut result = json!({
		"originalUrl":flake.original_ref.to_string(), "original":attrs_to_json(&flake.original_ref.to_attrs()).map_err(error)?,
		"resolvedUrl":flake.resolved_ref.to_string(), "resolved":attrs_to_json(&flake.resolved_ref.to_attrs()).map_err(error)?,
		"url":flake.locked_ref.to_string(), "locked":attrs_to_json(&flake.locked_ref.to_attrs()).map_err(error)?,
		"path":source_path(flake)?, "locks":flake.lock_file.to_json().map_err(error)?.0,
	});
	if let Some(description) = &flake.description {
		result["description"] = description.clone().into();
	}
	let input = &flake.locked_ref.input;
	if let Some(rev) = input.get_rev().map_err(error)? {
		result["revision"] = git_rev(&rev).into();
	}
	if let Some(rev) = maybe_get_str_attr(&input.attrs, "dirtyRev").map_err(error)? {
		result["dirtyRevision"] = rev.into();
	}
	if let Some(count) = input.get_rev_count().map_err(error)? {
		result["revCount"] = count.into();
	}
	if let Some(time) = input.get_last_modified().map_err(error)? {
		result["lastModified"] = time.into();
	}
	if let Some(fingerprint) = fingerprint(store, flake)? {
		result["fingerprint"] = fingerprint.into();
	}
	Ok(result)
}

/// Stable metadata fingerprint for revision-locked Git/forge and hashed tarball
/// sources. Dirty Git worktree fingerprints are not yet exposed by this library.
pub fn fingerprint(
	store: &EvaluatorStore,
	flake: &LockedFlake,
) -> Result<Option<String>, ErrorKind> {
	if flake
		.lock_file
		.is_unlocked(&store.fetch_settings())
		.map_err(error)?
		.is_some()
	{
		return Ok(None);
	}
	let input = &flake.locked_ref.input;
	let rev = input.get_rev().map_err(error)?.map(|rev| git_rev(&rev));
	let mut fingerprint = match input.scheme {
		Some(Scheme::Path) => {
			let Some(hash) = input.get_nar_hash().map_err(error)? else {
				return Ok(None);
			};
			format!("path:{}", hash.to_sri_string())
		}
		Some(Scheme::Git) => {
			let Some(mut rev) = rev else {
				return Ok(None);
			};
			for (flag, suffix) in [("submodules", ";s"), ("exportIgnore", ";e"), ("lfs", ";l")] {
				if maybe_get_bool_attr(&input.attrs, flag)
					.map_err(error)?
					.unwrap_or(false)
				{
					rev.push_str(suffix);
				}
			}
			rev
		}
		Some(Scheme::GitHub | Scheme::GitLab | Scheme::SourceHut) => {
			let Some(rev) = rev else {
				return Ok(None);
			};
			rev
		}
		Some(Scheme::Tarball | Scheme::File) => {
			if let Some(hash) = input.get_nar_hash().map_err(error)? {
				hash.to_sri_string()
			} else if let Some(rev) = rev {
				rev
			} else {
				return Ok(None);
			}
		}
		_ => return Ok(None),
	};
	fingerprint.push_str(&format!(
		";{};{}",
		flake.locked_ref.subdir,
		flake.lock_file.render().map_err(error)?
	));
	if let Some(count) = input.get_rev_count().map_err(error)? {
		if input.scheme == Some(Scheme::Git)
			&& !flake.resolved_ref.input.attrs.contains_key("revCount")
		{
			fingerprint.push_str(";hasRevCount");
		} else {
			fingerprint.push_str(&format!(";revCount={count}"));
		}
	}
	if let Some(time) = input.get_last_modified().map_err(error)? {
		fingerprint.push_str(&format!(";lastModified={time}"));
	}
	Ok(Some(
		data_encoding::HEXLOWER.encode(&Sha256::digest(fingerprint.as_bytes())),
	))
}

/// Download and copy a source into the caller's writable store, without requiring
/// `flake.nix`. The JSON has the reference's `storePath/hash/original/locked` keys.
pub fn prefetch(store: &EvaluatorStore, reference: &str) -> Result<Json, ErrorKind> {
	if store.store().is_none() {
		return Err(error("flake prefetch requires a writable store"));
	}
	let reference = parse_reference(store, reference)?;
	let source = fetch_source(store, &reference, UseRegistries::All)?;
	let info = source.source_info.to_attrs()?;
	let mut locked = attrs_to_json(&source.fetched_ref.to_attrs()).map_err(error)?;
	locked.remove("__final");
	Ok(
		json!({"storePath":info.select_required("outPath")?.to_contextful_str()?.to_str()?, "hash":info.select_required("narHash")?.to_contextful_str()?.to_str()?, "original":attrs_to_json(&source.resolved_ref.to_attrs()).map_err(error)?, "locked":locked}),
	)
}

/// Archive the source graph into the selected store. A read-only evaluator
/// computes the same JSON and paths without registering them. Follows edges do
/// not duplicate entries; relative nodes have `inputs` but no independent path.
pub fn archive(store: &EvaluatorStore, flake: &LockedFlake) -> Result<Json, ErrorKind> {
	fn visit(
		store: &EvaluatorStore,
		flake: &LockedFlake,
		node: NodeId,
		active: &mut BTreeSet<NodeId>,
		fetched: &mut BTreeMap<NodeId, String>,
	) -> Result<Json, ErrorKind> {
		if !active.insert(node) {
			return Err(error("cannot archive a cyclic lock graph"));
		}
		let mut result = serde_json::Map::new();
		for (name, edge) in &flake.lock_file.nodes[node].inputs {
			let Edge::Node(child) = edge else {
				continue;
			};
			let locked = &flake.lock_file.nodes[*child]
				.locked
				.as_ref()
				.ok_or_else(|| {
					error("lock graph contains a non-root node without source information")
				})?
				.locked_ref;
			let mut value = json!({"inputs":visit(store,flake,*child,active,fetched)?});
			if locked.input.is_relative().is_none() {
				let path = if let Some(path) = fetched.get(child) {
					path.clone()
				} else {
					let source = fetch_source(store, locked, UseRegistries::No)?;
					let path = source
						.source_info
						.to_attrs()?
						.select_required("outPath")?
						.to_contextful_str()?
						.to_str()?
						.to_owned();
					fetched.insert(*child, path.clone());
					path
				};
				value["path"] = path.into();
			}
			result.insert(name.clone(), value);
		}
		active.remove(&node);
		Ok(Json::Object(result))
	}
	Ok(
		json!({"path":source_path(flake)?, "inputs":visit(store,flake,ROOT,&mut BTreeSet::new(),&mut BTreeMap::new())?}),
	)
}

async fn flake_outputs(
	store: &Rc<EvaluatorStore>,
	co: &GenCo,
	flake: &LockedFlake,
) -> Result<Value, ErrorKind> {
	let result = generators::request_force(co, flake.evaluate(store, co).await?).await;
	let value = result.to_attrs()?.select_required("outputs")?.clone();
	Ok(generators::request_force(co, value).await)
}
