//! Flake input graph resolution and call-flake output evaluation.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use bstr::ByteSlice;
use nix_rs_eval::generators::{self, GenCo};
use nix_rs_eval::{ErrorKind, NixAttrs, Value};
use nix_rs_flakeref::flake_ref::{FlakeRef, ParseOptions, parse_flake_ref};
use nix_rs_flakeref::input::{Attr, Attrs, Input};
use nix_rs_flakeref::lockfile::{
	Edge, InputAttrPath, LockFile, LockedNode, Node, NodeId, ROOT, parse_input_attr_path,
};
use nix_rs_flakeref::registry::UseRegistries;

use super::builtins::{error, string, tree_attrs};
use crate::store::EvaluatorStore;

#[derive(Clone, Debug, Default)]
pub struct LockOptions {
	/// Re-resolve these input paths, retaining unrelated locked inputs.
	pub update_inputs: BTreeSet<InputAttrPath>,
	pub recreate: bool,
}

/// A resolved graph and the source trees already fetched while resolving it.
pub struct LockedFlake {
	pub lock_file: LockFile,
	pub locked_ref: FlakeRef,
	pub out_path: PathBuf,
	sources: HashMap<NodeId, (Value, String)>,
}

impl LockedFlake {
	/// Write CppNix's version-7 bytes, including the final newline, atomically.
	pub fn write_lock_file(&self, path: &Path) -> std::io::Result<()> {
		let contents = self.lock_file.render().map_err(std::io::Error::other)? + "\n";
		if std::fs::read(path).ok().as_deref() == Some(contents.as_bytes()) {
			return Ok(());
		}
		let permissions = std::fs::metadata(path)
			.map(|m| m.permissions())
			.unwrap_or_else(|_| std::fs::Permissions::from_mode(0o666));
		let mut file = tempfile::Builder::new()
			.permissions(permissions)
			.tempfile_in(path.parent().unwrap_or(Path::new(".")))?;
		file.write_all(contents.as_bytes())?;
		file.persist(path).map_err(|e| e.error)?;
		Ok(())
	}

	pub async fn evaluate(
		&self,
		store: &Rc<EvaluatorStore>,
		co: &GenCo,
	) -> Result<Value, ErrorKind> {
		let (lock, keys) = self.lock_file.to_string_with_keys().map_err(error)?;
		let overrides: Vec<(String, Value)> = self
			.sources
			.iter()
			.map(|(node, (source, dir))| {
				(
					keys[node].clone(),
					Value::attrs(NixAttrs::from_iter([
						("sourceInfo", source.clone()),
						("dir", Value::from(dir.clone())),
					])),
				)
			})
			.collect();
		let import = generators::request_builtin(co, "import").await;
		let helper = generators::request_call_with(
			co,
			import,
			[Value::from("/__corepkgs__/call-flake.nix")],
		)
		.await;
		let final_fetch = super::builtins::final_tree_builtin(store);
		Ok(generators::request_call_with(
			co,
			helper,
			[
				Value::from(lock),
				Value::attrs(NixAttrs::from_iter(overrides)),
				final_fetch,
			],
		)
		.await)
	}
}

#[derive(Clone, Default)]
struct InputSpec {
	reference: Option<FlakeRef>,
	follows: Option<InputAttrPath>,
	is_flake: bool,
	overrides: BTreeMap<String, InputSpec>,
	/// Source directory and input path of the flake declaring this override.
	base: PathBuf,
	parent: InputAttrPath,
}

type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ErrorKind>> + 'a>>;

fn parse_inputs<'a>(
	store: &'a EvaluatorStore,
	co: &'a GenCo,
	value: Value,
	base: &'a Path,
	prefix: &'a [String],
) -> LocalFuture<'a, BTreeMap<String, InputSpec>> {
	Box::pin(async move {
		let value = generators::request_force(co, value).await;
		let attrs = value.to_attrs()?;
		let settings = store.fetch_settings();
		let mut inputs = BTreeMap::new();
		for (name, value) in attrs.iter_sorted() {
			let name = name.to_str()?;
			if name == "self" {
				return Err(error("flake input 'self' attributes are not implemented"));
			}
			let value = generators::request_force(co, value.clone()).await;
			let attrs = value.to_attrs()?;
			let mut spec = InputSpec {
				is_flake: true,
				base: base.to_owned(),
				parent: prefix.to_vec(),
				..InputSpec::default()
			};
			let mut url = None;
			let mut fetch_attrs = Attrs::new();
			for (key, value) in attrs.iter_sorted() {
				let value = generators::request_force(co, value.clone()).await;
				match key.to_str()? {
					"url" => {
						url = Some(if let Value::Path(path) = value {
							format!("path:{}", relative_path(base, path.as_ref()))
						} else {
							string(co, value).await?
						});
					}
					"flake" => spec.is_flake = value.as_bool()?,
					"follows" => {
						let mut target = prefix.to_vec();
						target.extend(
							parse_input_attr_path(&string(co, value).await?).map_err(error)?,
						);
						spec.follows = Some(target);
					}
					"inputs" => {
						spec.overrides = parse_inputs(store, co, value, base, prefix).await?
					}
					key => {
						let attr = match value {
							Value::String(value) => Attr::String(value.to_str()?.to_owned()),
							Value::Bool(value) => Attr::Bool(value),
							Value::Integer(value) if value >= 0 => Attr::Int(value as u64),
							_ => {
								return Err(error(format!(
									"invalid flake input attribute '{key}'"
								)));
							}
						};
						fetch_attrs.insert(key.into(), attr);
					}
				}
			}
			if fetch_attrs.contains_key("type") {
				if let Some(url) = url {
					fetch_attrs.insert("url".into(), url.into());
				}
				spec.reference =
					Some(FlakeRef::from_attrs(&settings, &fetch_attrs).map_err(error)?);
			} else if !fetch_attrs.is_empty() {
				return Err(error("unexpected flake input attribute"));
			} else if let Some(url) = url {
				spec.reference = Some(
					parse_flake_ref(
						&settings,
						&url,
						ParseOptions {
							allow_missing: true,
							is_flake: spec.is_flake,
							preserve_relative_paths: true,
							..ParseOptions::default()
						},
					)
					.map_err(error)?,
				);
			}
			if spec.reference.is_some() && spec.follows.is_some() {
				return Err(error(
					"flake input has both a flake reference and a follows attribute",
				));
			}
			inputs.insert(name.to_owned(), spec);
		}
		Ok(inputs)
	})
}

fn relative_path(base: &Path, path: &Path) -> String {
	let mut root = base;
	let mut prefix = String::new();
	while !path.starts_with(root) {
		let Some(parent) = root.parent() else {
			return path.to_string_lossy().into_owned();
		};
		root = parent;
		prefix.push_str("../");
	}
	if prefix.is_empty() {
		prefix.push_str("./");
	}
	format!("{prefix}{}", path.strip_prefix(root).unwrap().display())
}

async fn read_inputs(
	store: &EvaluatorStore,
	co: &GenCo,
	base: &Path,
	prefix: &[String],
) -> Result<BTreeMap<String, InputSpec>, ErrorKind> {
	let import = generators::request_builtin(co, "import").await;
	let value =
		generators::request_call_with(co, import, [Value::Path(Box::new(base.join("flake.nix")))])
			.await;
	let value = generators::request_force(co, value).await;
	let attrs = value.to_attrs()?;
	let mut inputs = match attrs.select("inputs") {
		Some(value) => parse_inputs(store, co, value.clone(), base, prefix).await?,
		None => BTreeMap::new(),
	};
	let outputs = attrs.select_required("outputs")?.clone();
	let function_args = generators::request_builtin(co, "functionArgs").await;
	let formals = generators::request_call_with(co, function_args, [outputs]).await;
	for (name, _) in formals.to_attrs()?.iter_sorted() {
		let name = name.to_str()?;
		if name != "self" {
			inputs.entry(name.into()).or_insert(InputSpec {
				is_flake: true,
				base: base.to_owned(),
				parent: prefix.to_vec(),
				..InputSpec::default()
			});
		}
	}
	Ok(inputs)
}

fn read_lock(store: &EvaluatorStore, base: &Path) -> Result<LockFile, ErrorKind> {
	let path = store.physical_path(&base.join("flake.lock"));
	match std::fs::read_to_string(&path) {
		Ok(contents) => {
			LockFile::parse(&store.fetch_settings(), &contents, &path.to_string_lossy())
				.map_err(error)
		}
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LockFile::default()),
		Err(e) => Err(error(e)),
	}
}

fn fetch_source(
	store: &EvaluatorStore,
	original: &FlakeRef,
	registries: UseRegistries,
) -> Result<(FlakeRef, PathBuf, Value), ErrorKind> {
	let (mount, _, mut input, extra) =
		super::get_cached(store, &original.input, registries).map_err(error)?;
	let path = super::mount_input(store, &mut input, &original.input, &mount).map_err(error)?;
	let subdir = if original.subdir.is_empty() {
		nix_rs_flakeref::input::maybe_get_str_attr(&extra, "dir")
			.map_err(error)?
			.unwrap_or_default()
	} else {
		original.subdir.clone()
	};
	let logical = PathBuf::from(path.to_absolute_path()).join(&subdir);
	let attrs = tree_attrs(&path, &input, false)?;
	Ok((FlakeRef::new(input, subdir), logical, attrs))
}

struct Resolver<'a> {
	store: &'a EvaluatorStore,
	co: &'a GenCo,
	options: &'a LockOptions,
	lock: LockFile,
	sources: HashMap<NodeId, (Value, String)>,
	parents: Vec<FlakeRef>,
}

impl Resolver<'_> {
	fn copy_old(
		&mut self,
		old: &LockFile,
		node: NodeId,
		prefix: &[String],
		copied: &mut HashMap<NodeId, NodeId>,
	) -> NodeId {
		if let Some(node) = copied.get(&node) {
			return *node;
		}
		let mut locked = old.nodes[node].locked.clone();
		if let Some(locked) = &mut locked
			&& let Some(parent) = &mut locked.parent_input_attr_path
		{
			parent.splice(0..0, prefix.iter().cloned());
		}
		let new_node = self.lock.add_node(Node {
			inputs: BTreeMap::new(),
			locked,
		});
		copied.insert(node, new_node);
		for (name, edge) in &old.nodes[node].inputs {
			let edge = match edge {
				Edge::Node(child) => Edge::Node(self.copy_old(old, *child, prefix, copied)),
				Edge::Follows(path) => Edge::Follows(prefix.iter().chain(path).cloned().collect()),
			};
			self.lock.nodes[new_node].inputs.insert(name.clone(), edge);
		}
		new_node
	}

	fn compute<'a>(
		&'a mut self,
		node: NodeId,
		mut inputs: BTreeMap<String, InputSpec>,
		overrides: BTreeMap<String, InputSpec>,
		prefix: InputAttrPath,
		old: LockFile,
		old_node: NodeId,
		old_prefix: InputAttrPath,
	) -> LocalFuture<'a, ()> {
		Box::pin(async move {
			for (name, spec) in overrides {
				if let Some(input) = inputs.get_mut(&name) {
					if spec.reference.is_some() || spec.follows.is_some() {
						input.reference = spec.reference;
						input.follows = spec.follows;
						input.base = spec.base;
						input.parent = spec.parent;
					}
					input.overrides.extend(spec.overrides);
				} else {
					nix_rs_flakeref::lockfile::warn_override_for_missing(&prefix, &name);
				}
			}
			for (name, spec) in inputs {
				let mut path = prefix.clone();
				path.push(name.clone());
				if let Some(follows) = spec.follows {
					self.lock.nodes[node]
						.inputs
						.insert(name, Edge::Follows(follows));
					continue;
				}
				let settings = self.store.fetch_settings();
				let original = match spec.reference {
					Some(reference) => reference,
					None => FlakeRef::new(
						Input::from_attrs(
							&settings,
							[
								("type".into(), "indirect".into()),
								("id".into(), name.clone().into()),
							]
							.into(),
						)
						.map_err(error)?,
						"",
					),
				};
				let parent = original.input.is_relative().map(|_| spec.parent.clone());
				let old_child = match old.nodes[old_node].inputs.get(&name) {
					Some(Edge::Node(child)) => Some(*child),
					_ => None,
				};
				let old_locked = old_child.and_then(|child| old.nodes[child].locked.clone());
				let reuse = !self.options.recreate
					&& !self.options.update_inputs.contains(&path)
					&& old_locked.as_ref().is_some_and(|l| {
						l.original_ref.canonicalize() == original.canonicalize()
							&& l.parent_input_attr_path == parent
					});
				let follows_outside = old_child.is_some_and(|child| {
					let mut pending = vec![child];
					let mut seen = BTreeSet::new();
					while let Some(node) = pending.pop() {
						if !seen.insert(node) {
							continue;
						}
						for edge in old.nodes[node].inputs.values() {
							match edge {
								Edge::Node(child) => pending.push(*child),
								Edge::Follows(target) if !target.starts_with(&path) => return true,
								_ => {}
							}
						}
					}
					false
				});
				if reuse
					&& !follows_outside
					&& spec.overrides.is_empty()
					&& !self
						.options
						.update_inputs
						.iter()
						.any(|update| update.starts_with(&path))
				{
					let child =
						self.copy_old(&old, old_child.unwrap(), &old_prefix, &mut HashMap::new());
					self.lock.nodes[node].inputs.insert(name, Edge::Node(child));
					continue;
				}
				let reference = if reuse {
					old_locked.as_ref().unwrap().locked_ref.clone()
				} else {
					original.clone()
				};
				if self.parents.contains(&original) {
					return Err(error(format!(
						"found circular import of flake '{original}'"
					)));
				}
				self.parents.push(original.clone());
				let (locked, base, source) = if let Some(relative) = reference.input.is_relative() {
					(
						reference.clone(),
						spec.base.join(relative).join(&reference.subdir),
						None,
					)
				} else {
					let (locked, base, source) = fetch_source(
						self.store,
						&reference,
						if settings.use_registries {
							UseRegistries::Limited
						} else {
							UseRegistries::No
						},
					)?;
					(locked, base, Some(source))
				};
				let child = self.lock.add_node(Node {
					inputs: BTreeMap::new(),
					locked: Some(LockedNode {
						locked_ref: locked.clone(),
						original_ref: original,
						is_flake: spec.is_flake,
						parent_input_attr_path: parent,
					}),
				});
				self.lock.nodes[node].inputs.insert(name, Edge::Node(child));
				if let Some(source) = source {
					self.sources.insert(child, (source, locked.subdir));
				}
				if spec.is_flake {
					let child_inputs = read_inputs(self.store, self.co, &base, &path).await?;
					let (child_old, child_old_node, child_old_prefix) = if reuse {
						(old.clone(), old_child.unwrap(), old_prefix.clone())
					} else {
						(read_lock(self.store, &base)?, ROOT, path.clone())
					};
					self.compute(
						child,
						child_inputs,
						spec.overrides,
						path,
						child_old,
						child_old_node,
						child_old_prefix,
					)
					.await?;
				}
				self.parents.pop();
			}
			Ok(())
		})
	}
}

/// Resolve a flake graph without writing a lock file. Callers decide whether to
/// persist it with `write_lock_file`; `builtins.getFlake` never writes one.
pub async fn lock_flake(
	store: &EvaluatorStore,
	co: &GenCo,
	reference: &str,
	options: &LockOptions,
) -> Result<LockedFlake, ErrorKind> {
	let settings = store.fetch_settings();
	let cwd = store.fetch_config.borrow().cwd.clone();
	let cwd = if cwd.as_os_str().is_empty() {
		std::env::current_dir().map_err(error)?
	} else {
		cwd
	};
	let original = parse_flake_ref(
		&settings,
		reference,
		ParseOptions {
			base_dir: Some(&cwd),
			..ParseOptions::default()
		},
	)
	.map_err(error)?;
	if store.pure_eval() && !original.input.is_locked(&settings) {
		return Err(error(format!(
			"cannot call 'getFlake' on unlocked flake reference '{reference}', at (none) (use --impure to override)"
		)));
	}
	let (locked_ref, out_path, source) = fetch_source(
		store,
		&original,
		if settings.use_registries {
			UseRegistries::All
		} else {
			UseRegistries::No
		},
	)?;
	let inputs = read_inputs(store, co, &out_path, &[]).await?;
	let old = read_lock(store, &out_path)?;
	let mut resolver = Resolver {
		store,
		co,
		options,
		lock: LockFile::default(),
		sources: [(ROOT, (source, locked_ref.subdir.clone()))].into(),
		parents: vec![original],
	};
	resolver
		.compute(
			ROOT,
			inputs,
			BTreeMap::new(),
			Vec::new(),
			old,
			ROOT,
			Vec::new(),
		)
		.await?;
	resolver.lock.check().map_err(error)?;
	Ok(LockedFlake {
		lock_file: resolver.lock,
		locked_ref,
		out_path,
		sources: resolver.sources,
	})
}
