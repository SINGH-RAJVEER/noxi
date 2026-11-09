//! Flake input graph resolution and call-flake output evaluation.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::future::Future;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::rc::Rc;

use bstr::ByteSlice;
use noxi_eval::generators::{self, GenCo};
use noxi_eval::{ErrorKind, NixAttrs, Value};
use noxi_flakeref::flake_ref::{FlakeRef, ParseOptions, parse_flake_ref};
use noxi_flakeref::input::{Attr, Attrs, Input};
use noxi_flakeref::lockfile::{
	Edge, InputAttrPath, LockFile, LockedNode, Node, NodeId, ROOT, parse_input_attr_path,
};
use noxi_flakeref::registry::UseRegistries;

use super::builtins::{error, string, tree_attrs};
use crate::store::EvaluatorStore;

#[derive(Clone, Debug, Default)]
pub struct LockOptions {
	/// Re-resolve these input paths, retaining unrelated locked inputs.
	pub update_inputs: BTreeSet<InputAttrPath>,
	/// Explicit command-line replacements; paths are rooted at the top-level flake.
	pub override_inputs: BTreeMap<InputAttrPath, String>,
	/// Override the evaluation mode's policy for creating unlocked entries.
	pub allow_unlocked: Option<bool>,
	pub recreate: bool,
}

/// A resolved graph and the source trees already fetched while resolving it.
pub struct LockedFlake {
	pub lock_file: LockFile,
	pub original_ref: FlakeRef,
	pub resolved_ref: FlakeRef,
	pub locked_ref: FlakeRef,
	pub out_path: PathBuf,
	pub description: Option<String>,
	sources: HashMap<NodeId, (Value, String)>,
}

impl LockedFlake {
	pub fn source_info(&self) -> &Value {
		&self.sources[&ROOT].0
	}
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
		Some(value) => {
			let value = generators::request_force(co, value.clone()).await;
			let inputs = value.to_attrs()?;
			let filtered = Value::attrs(NixAttrs::from_iter(
				inputs
					.iter()
					.filter(|(key, _)| key.as_bytes() != b"self")
					.map(|(key, value)| (key.clone(), value.clone())),
			));
			parse_inputs(store, co, filtered, base, prefix).await?
		}
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

/// A mounted source and the references before and after locking it.
pub struct FetchedSource {
	pub resolved_ref: FlakeRef,
	/// Fetcher attributes before mounting computes and attaches the NAR hash.
	pub fetched_ref: FlakeRef,
	pub locked_ref: FlakeRef,
	/// Logical flake directory, including `dir` when supplied.
	pub out_path: PathBuf,
	pub source_info: Value,
}

pub fn fetch_source(
	store: &EvaluatorStore,
	original: &FlakeRef,
	registries: UseRegistries,
) -> Result<FetchedSource, ErrorKind> {
	let (mount, resolved, mut input, extra) =
		super::get_cached(store, &original.input, registries).map_err(error)?;
	let fetched = input.clone();
	let path = super::mount_input(store, &mut input, &original.input, &mount).map_err(error)?;
	let subdir = if original.subdir.is_empty() {
		noxi_flakeref::input::maybe_get_str_attr(&extra, "dir")
			.map_err(error)?
			.unwrap_or_default()
	} else {
		original.subdir.clone()
	};
	let logical = PathBuf::from(path.to_absolute_path()).join(&subdir);
	let attrs = tree_attrs(&path, &input, false)?;
	Ok(FetchedSource {
		resolved_ref: FlakeRef::new(resolved, &subdir),
		fetched_ref: FlakeRef::new(fetched, &subdir),
		locked_ref: FlakeRef::new(input, subdir),
		out_path: logical,
		source_info: attrs,
	})
}

async fn fetch_flake_source(
	store: &EvaluatorStore,
	co: &GenCo,
	original: &FlakeRef,
	registries: UseRegistries,
) -> Result<FetchedSource, ErrorKind> {
	let source = fetch_source(store, original, registries)?;
	let (locked, base) = (&source.locked_ref, &source.out_path);
	let import = generators::request_builtin(co, "import").await;
	let value =
		generators::request_call_with(co, import, [Value::Path(Box::new(base.join("flake.nix")))])
			.await;
	let value = generators::request_force(co, value).await;
	let attrs = value.to_attrs()?;
	let Some(inputs) = attrs.select("inputs") else {
		return Ok(source);
	};
	let inputs = generators::request_force(co, inputs.clone()).await;
	let inputs = inputs.to_attrs()?;
	let Some(self_attrs) = inputs.select("self") else {
		return Ok(source);
	};
	let self_attrs = generators::request_force(co, self_attrs.clone()).await;
	let self_attrs = self_attrs.to_attrs()?;
	let mut updated = locked.clone();
	for (key, value) in self_attrs.iter_sorted() {
		let key = key.to_str()?;
		if !matches!(key, "submodules" | "lfs") {
			return Err(error(format!(
				"flake 'self' attribute '{key}' is not supported"
			)));
		}
		let value = generators::request_force(co, value.clone()).await;
		updated
			.input
			.attrs
			.insert(key.into(), Attr::Bool(value.as_bool()?));
	}
	if updated == *locked {
		return Ok(source);
	}
	updated.input.attrs.remove("narHash");
	let updated = fetch_source(store, &updated, UseRegistries::No)?;
	Ok(FetchedSource {
		resolved_ref: source.resolved_ref,
		..updated
	})
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
					noxi_flakeref::lockfile::warn_override_for_missing(&prefix, &name);
				}
			}
			for (name, mut spec) in inputs {
				let mut path = prefix.clone();
				path.push(name.clone());
				let declared = spec.reference.clone();
				let cli_override = self.options.override_inputs.get(&path);
				if let Some(replacement) = cli_override {
					let cwd = self.store.fetch_config.borrow().cwd.clone();
					spec.reference = Some(
						parse_flake_ref(
							&self.store.fetch_settings(),
							replacement,
							ParseOptions {
								base_dir: Some(&cwd),
								..ParseOptions::default()
							},
						)
						.map_err(error)?,
					);
					spec.follows = None;
					spec.base = cwd;
					spec.parent = Vec::new();
				}
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
					&& cli_override.is_none()
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
					&& !self
						.options
						.override_inputs
						.keys()
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
				if !reuse
					&& !self
						.options
						.allow_unlocked
						.unwrap_or(!self.store.pure_eval())
					&& !reference.input.is_locked(&settings)
					&& reference.input.is_relative().is_none()
				{
					return Err(error(format!(
						"cannot update unlocked flake input '{}' in pure mode",
						path.join("/")
					)));
				}
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
					let registries = if settings.use_registries {
						if cli_override.is_some() {
							UseRegistries::All
						} else {
							UseRegistries::Limited
						}
					} else {
						UseRegistries::No
					};
					let source = if spec.is_flake {
						fetch_flake_source(self.store, self.co, &reference, registries).await?
					} else {
						fetch_source(self.store, &reference, registries)?
					};
					(source.locked_ref, source.out_path, Some(source.source_info))
				};
				let child = self.lock.add_node(Node {
					inputs: BTreeMap::new(),
					locked: Some(LockedNode {
						locked_ref: locked.clone(),
						original_ref: if cli_override.is_some() {
							declared.unwrap_or(original)
						} else {
							original
						},
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

/// Parse a reference using the caller's fetch settings and working directory.
pub fn parse_reference(store: &EvaluatorStore, reference: &str) -> Result<FlakeRef, ErrorKind> {
	let settings = store.fetch_settings();
	let cwd = store.fetch_config.borrow().cwd.clone();
	let cwd = if cwd.as_os_str().is_empty() {
		std::env::current_dir().map_err(error)?
	} else {
		cwd
	};
	parse_flake_ref(
		&settings,
		reference,
		ParseOptions {
			base_dir: Some(&cwd),
			..ParseOptions::default()
		},
	)
	.map_err(error)
}

/// Resolve without writing; `builtins.getFlake` never writes a lock file.
pub async fn lock_flake(
	store: &EvaluatorStore,
	co: &GenCo,
	reference: &str,
	options: &LockOptions,
) -> Result<LockedFlake, ErrorKind> {
	let settings = store.fetch_settings();
	let original = parse_reference(store, reference)?;
	if store.pure_eval()
		&& options.allow_unlocked != Some(true)
		&& !original.input.is_locked(&settings)
	{
		return Err(error(format!(
			"cannot call 'getFlake' on unlocked flake reference '{reference}', at (none) (use --impure to override)"
		)));
	}
	let source = fetch_flake_source(
		store,
		co,
		&original,
		if settings.use_registries {
			UseRegistries::All
		} else {
			UseRegistries::No
		},
	)
	.await?;
	let FetchedSource {
		resolved_ref,
		locked_ref,
		out_path,
		source_info: source,
		..
	} = source;
	let import = generators::request_builtin(co, "import").await;
	let flake = generators::request_call_with(
		co,
		import,
		[Value::Path(Box::new(out_path.join("flake.nix")))],
	)
	.await;
	let flake = generators::request_force(co, flake).await;
	let description = match flake.to_attrs()?.select("description") {
		Some(value) => Some(string(co, value.clone()).await?),
		None => None,
	};
	let inputs = read_inputs(store, co, &out_path, &[]).await?;
	let old = read_lock(store, &out_path)?;
	let mut resolver = Resolver {
		store,
		co,
		options,
		lock: LockFile::default(),
		sources: [(ROOT, (source, locked_ref.subdir.clone()))].into(),
		parents: vec![original.clone()],
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
		original_ref: original,
		resolved_ref,
		locked_ref,
		out_path,
		description,
		sources: resolver.sources,
	})
}
