//! Store inspection and maintenance through existing Rust store APIs.
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{self, IsTerminal};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use super::{Parsed, unsupported};
use crate::cli::Failure;
use nix_compat::cppnix_json::{PathInfo, PathInfoFormat};
use nix_compat::narinfo::VerifyingKey;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use nix_rs_store::gc::{GcAction, GcOptions, render_size};
use nix_rs_store::{FileIngestionMethod, Store, ValidPathInfo};
use serde_json::{Value, json};

fn open(parsed: &Parsed) -> Result<crate::store::OpenStore, Failure> {
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	crate::store::open(&uri, &parsed.settings)?
		.ok_or_else(|| unsupported("operations on this store"))
}
fn info(store: &dyn Store, path: &StorePath) -> Result<ValidPathInfo, Failure> {
	store
		.query_path_info(path)?
		.ok_or_else(|| Failure::Error(format!("path '{}' is not valid", path.to_absolute_path())))
}

/// Path infos read once per command, as CppNix's path info cache keeps
/// them, so closures do not query the database again for shared members.
struct InfoGraph<'a> {
	store: &'a dyn Store,
	index: HashMap<StorePath, usize>,
	nodes: Vec<InfoNode>,
	epoch: u32,
}

struct InfoNode {
	path: StorePath,
	info: Option<Option<ValidPathInfo>>,
	references: Vec<usize>,
	mark: u32,
}

impl<'a> InfoGraph<'a> {
	fn new(store: &'a dyn Store) -> Self {
		Self {
			store,
			index: HashMap::new(),
			nodes: Vec::new(),
			epoch: 0,
		}
	}

	fn node(&mut self, path: &StorePath) -> usize {
		if let Some(&node) = self.index.get(path) {
			return node;
		}
		self.nodes.push(InfoNode {
			path: path.clone(),
			info: None,
			references: Vec::new(),
			mark: 0,
		});
		self.index.insert(path.clone(), self.nodes.len() - 1);
		self.nodes.len() - 1
	}

	fn load(&mut self, node: usize) -> Result<Option<&ValidPathInfo>, Failure> {
		if self.nodes[node].info.is_none() {
			let info = self.store.query_path_info(&self.nodes[node].path)?;
			let references = info
				.iter()
				.flat_map(|info| &info.references)
				.map(|r| r.clone())
				.collect::<Vec<_>>();
			let references = references.iter().map(|r| self.node(r)).collect();
			self.nodes[node].references = references;
			self.nodes[node].info = Some(info);
		}
		Ok(self.nodes[node].info.as_ref().unwrap().as_ref())
	}

	fn query(&mut self, path: &StorePath) -> Result<Option<ValidPathInfo>, Failure> {
		let node = self.node(path);
		Ok(self.load(node)?.cloned())
	}

	/// The total NAR size of the closure of `path`.
	fn closure_size(&mut self, path: &StorePath) -> Result<u64, Failure> {
		self.epoch += 1;
		let mut size = 0;
		let mut pending = vec![self.node(path)];
		while let Some(node) = pending.pop() {
			if self.nodes[node].mark == self.epoch {
				continue;
			}
			self.nodes[node].mark = self.epoch;
			let Some(info) = self.load(node)? else {
				return Err(nix_rs_store::Error::InvalidPath(
					self.nodes[node].path.to_absolute_path(),
				)
				.into());
			};
			size += info.nar_size;
			let epoch = self.epoch;
			pending.extend(
				self.nodes[node]
					.references
					.iter()
					.filter(|&&r| self.nodes[r].mark != epoch),
			);
		}
		Ok(size)
	}
}
fn paths(parsed: &Parsed, store: &dyn Store) -> Result<BTreeSet<StorePath>, Failure> {
	if parsed.operands.is_empty() {
		return Err(unsupported(
			"implicit flake installables; provide store paths",
		));
	}
	parsed.unsupported(&["all", "derivation"])?;
	if parsed.has("expr") || parsed.has("file") {
		return Err(unsupported("store queries with expression installables"));
	}
	let mut paths = parsed
		.operands
		.iter()
		.map(|p| {
			crate::paths::follow_links_to_store_path(
				&std::env::current_dir().unwrap_or_default(),
				p,
			)
		})
		.collect::<Result<BTreeSet<_>, _>>()?;
	if parsed.has("recursive") {
		paths = store.compute_closure(&paths)?;
	}
	Ok(paths)
}
fn log(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
		eprintln!("{message}");
	}
}

pub fn path_info(parsed: Parsed) -> Result<ExitCode, Failure> {
	let opened = open(&parsed)?;
	let store = opened.store.as_ref();
	let paths = paths(&parsed, store)?;
	let mut graph = InfoGraph::new(store);
	let format = match parsed.value("json-format") {
		None | Some("1") => PathInfoFormat::V1,
		Some("2") => PathInfoFormat::V2,
		Some("3") => PathInfoFormat::V3,
		Some(version) => {
			return Err(Failure::Error(format!(
				"unsupported path info JSON format version {version}"
			)));
		}
	};
	if parsed.has("json") {
		if parsed.value("json-format").is_none() {
			eprintln!(
				"warning: '--json' without '--json-format' is deprecated; please specify '--json-format 1' or '--json-format 2'. This will become an error in a future release."
			);
		}
		let mut objects = serde_json::Map::new();
		for path in &paths {
			let key = if format == PathInfoFormat::V1 {
				path.to_absolute_path()
			} else {
				path.to_string()
			};
			let Some(info) = graph.query(path)? else {
				objects.insert(key, Value::Null);
				continue;
			};
			let mut value = PathInfo {
				store_dir: "/nix/store".into(),
				nar_hash: NixHash::Sha256(info.nar_hash),
				nar_size: info.nar_size,
				references: info.references,
				ca: info.ca,
				deriver: info.deriver,
				registration_time: info.registration_time,
				ultimate: info.ultimate,
				signatures: info
					.sigs
					.iter()
					.map(|s| nix_compat::cppnix_json::Signature::parse(s))
					.collect::<Result<_, _>>()
					.map_err(|e| e.to_string())?,
			}
			.to_json(true, format);
			value["storeDir"] = json!("/nix/store");
			if parsed.has("closure-size") {
				value["closureSize"] = json!(graph.closure_size(path)?);
			}
			objects.insert(key, value);
		}
		let value = if format == PathInfoFormat::V1 {
			Value::Object(objects)
		} else {
			json!({"version": if format == PathInfoFormat::V2 {2} else {3}, "storeDir":"/nix/store", "info": objects})
		};
		println!("{value}");
	} else {
		let width = paths
			.iter()
			.map(|p| p.to_absolute_path().len())
			.max()
			.unwrap_or(0);
		// CppNix lists store paths after the paths they refer to.
		let sorted = crate::paths::topo_sort_by(&paths, |path| {
			Ok(graph
				.query(path)?
				.map(|info| info.references)
				.unwrap_or_default())
		})?;
		for path in sorted.iter().rev() {
			let info = graph.query(path)?.ok_or_else(|| {
				Failure::Error(format!("path '{}' is not valid", path.to_absolute_path()))
			})?;
			let path = path.to_absolute_path();
			let padded = parsed.has("size") || parsed.has("closure-size") || parsed.has("sigs");
			let mut line = if padded {
				format!("{path:width$}")
			} else {
				path
			};
			for enabled in ["size", "closure-size"] {
				if parsed.has(enabled) {
					let size = if enabled == "size" {
						info.nar_size
					} else {
						graph.closure_size(&info.path)?
					};
					line.push_str(&if parsed.has("human-readable") {
						format!("\t{}", render_size(size as i64, true))
					} else {
						format!("\t{size:11}")
					});
				}
			}
			if parsed.has("sigs") {
				let mut sigs = Vec::new();
				if info.ultimate {
					sigs.push("ultimate".into());
				}
				if let Some(ca) = info.ca {
					sigs.push(format!("ca:{ca}"));
				}
				sigs.extend(info.sigs);
				line.push('\t');
				line.push_str(&sigs.join(" "));
			}
			println!("{line}");
		}
	}
	Ok(ExitCode::SUCCESS)
}

/// Resolve a store-relative accessor path without following links outside its object.
/// Store accessors do not follow symlinks. Reject an intermediate symlink explicitly.
fn accessor(store: &dyn Store, operand: &str) -> Result<(PathBuf, String), Failure> {
	let (path, rest) =
		StorePath::from_absolute_path_full(Path::new(operand)).map_err(|e| e.to_string())?;
	info(store, &path)?;
	let mut physical = store.real_path(&path);
	let rest = Path::new(rest);
	let parts: Vec<_> = rest.components().collect();
	for (index, part) in parts.iter().enumerate() {
		match part {
			std::path::Component::Normal(name) => physical.push(name),
			std::path::Component::RootDir | std::path::Component::CurDir => continue,
			_ => return Err(unsupported("store accessor paths containing '..'")),
		}
		if index + 1 < parts.len() && fs::symlink_metadata(&physical)?.file_type().is_symlink() {
			return Err(unsupported("store access through intermediate symlinks"));
		}
	}
	Ok((
		physical,
		format!("/{}", rest.to_string_lossy().trim_start_matches('/')),
	))
}

pub fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	if matches!(
		parsed.command,
		"store ls" | "store cat" | "store dump-path" | "store verify"
	) {
		let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
		if uri != "local" && !uri.starts_with("local?") {
			return Err(unsupported(
				"filesystem access without an explicit local store (remote accessor API unavailable)",
			));
		}
	}
	let opened = open(&parsed)?;
	let store = opened.store.as_ref();
	match parsed.command {
		"store info" => {
			let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
			if !(uri == "local" || uri.starts_with("local?")) {
				return Err(unsupported(
					"store info for non-local stores (connection metadata unavailable)",
				));
			}
			let url = uri.replacen("local", "local://", 1);
			if parsed.has("json") {
				println!("{}", json!({"url":url,"version":"2.35.2","trusted":true}));
			} else {
				log(&format!("Store URL: {url}"));
				log("Version: 2.35.2");
				log("Trusted: 1");
			}
		}
		"store add" | "store add-path" | "store add-file" => {
			parsed.unsupported(&["dry-run"])?;
			let source = Path::new(&parsed.operands[0]);
			let name = parsed.value("name").map(str::to_owned).unwrap_or_else(|| {
				source
					.file_name()
					.unwrap_or_default()
					.to_string_lossy()
					.into_owned()
			});
			let method =
				match parsed
					.value("mode")
					.unwrap_or(if parsed.command == "store add-file" {
						"flat"
					} else {
						"nar"
					}) {
					"flat" => FileIngestionMethod::Flat,
					"nar" => FileIngestionMethod::NixArchive,
					_ => return Err(unsupported("store add with this ingestion method")),
				};
			let algo = parsed
				.value("hash-algo")
				.unwrap_or("sha256")
				.parse::<HashAlgo>()
				.map_err(|e| e.to_string())?;
			let mut dump = tempfile::tempfile()?;
			if method == FileIngestionMethod::Flat {
				io::copy(&mut fs::File::open(source)?, &mut dump)?;
			} else {
				nix_rs_store::nar::dump_path(&mut dump, source)?;
			}
			use std::io::Seek;
			dump.rewind()?;
			let path = store.add_to_store_from_dump(
				&mut dump,
				&name,
				method,
				method,
				algo,
				&BTreeSet::new(),
			)?;
			println!("{}", path.to_absolute_path());
		}
		"store cat" | "store ls" => {
			let (path, rest) = accessor(store, &parsed.operands[0])?;
			if parsed.command == "store cat" {
				if !fs::symlink_metadata(&path)?.is_file() {
					return Err(Failure::Error(format!(
						"path '{rest}' is not a regular file"
					)));
				}
				io::copy(&mut fs::File::open(path)?, &mut io::stdout().lock())?;
			} else if parsed.has("json") {
				if parsed.has("directory") {
					return Err(Failure::Usage(
						"'--directory' is useless with '--json'".into(),
					));
				}
				println!("{}", listing_json(&path, parsed.has("recursive"))?);
			} else {
				let meta = fs::symlink_metadata(&path)?;
				let label = if meta.is_dir() {
					".".into()
				} else {
					path.file_name()
						.unwrap_or_default()
						.to_string_lossy()
						.into_owned()
				};
				listing_text(
					&path,
					&label,
					parsed.has("directory"),
					parsed.has("long"),
					parsed.has("recursive"),
				)?;
			}
		}
		"store dump-path" => {
			if io::stdout().is_terminal() {
				return Err(Failure::Usage("refusing to write NAR to a terminal".into()));
			}
			let paths = paths(&parsed, store)?;
			for path in paths {
				info(store, &path)?;
				nix_rs_store::nar::dump_path(&mut io::stdout().lock(), &store.real_path(&path))?;
			}
		}
		"store gc" | "store delete" => {
			parsed.unsupported(&["also-referrers", "skip-alive", "skip-live"])?;
			let mut options = GcOptions::new(if parsed.command == "store delete" {
				GcAction::DeleteSpecific
			} else if parsed.has("dry-run") {
				GcAction::ReturnDead
			} else {
				GcAction::DeleteDead
			});
			if parsed.has("dry-run") && parsed.has("max") {
				return Err(Failure::Usage(
					"options --max and --dry-run cannot be combined".into(),
				));
			}
			options.keep_outputs = parsed.settings.get_bool("keep-outputs", false);
			options.keep_derivations = parsed.settings.get_bool("keep-derivations", true);
			options.ignore_liveness = parsed.has("ignore-liveness");
			if let Some(max) = parsed.value("max") {
				options.max_freed = crate::cli::parse_int_with_unit(max)
					.map_err(Failure::Usage)?
					.max(0) as u64;
			}
			if parsed.command == "store delete" {
				options.paths = Some(paths(&parsed, store)?);
			}
			let result = store.collect_garbage(&options, &mut log);
			let summary = result.as_ref().ok().cloned().unwrap_or_default();
			if parsed.has("dry-run") {
				println!("{} store paths would be deleted", summary.paths.len());
			} else {
				println!(
					"{} store paths deleted, {} freed",
					summary.paths.len(),
					render_size(summary.bytes_freed as i64, false)
				);
			}
			result?;
		}
		"store optimise" => store.optimise_store(&mut log)?,
		"store verify" => return verify(&parsed, store),
		_ => return Err(unsupported(parsed.command)),
	}
	Ok(ExitCode::SUCCESS)
}

fn listing_text(
	path: &Path,
	label: &str,
	directory: bool,
	long: bool,
	recursive: bool,
) -> Result<(), Failure> {
	let meta = fs::symlink_metadata(path)?;
	if !meta.is_dir() || directory {
		if long {
			let (mode, size) = if meta.is_file() {
				(
					if meta.permissions().mode() & 0o100 != 0 {
						"-r-xr-xr-x"
					} else {
						"-r--r--r--"
					},
					meta.len(),
				)
			} else if meta.file_type().is_symlink() {
				("lrwxrwxrwx", 0)
			} else {
				("dr-xr-xr-x", 0)
			};
			let target = if meta.file_type().is_symlink() {
				format!(" -> {}", fs::read_link(path)?.display())
			} else {
				String::new()
			};
			println!("{mode} {size:20} {label}{target}");
		} else {
			println!("{label}");
		}
		if !(recursive && meta.is_dir()) {
			return Ok(());
		}
	}
	let mut entries = fs::read_dir(path)?.collect::<Result<Vec<_>, _>>()?;
	entries.sort_by_key(|entry| entry.file_name());
	for entry in entries {
		listing_text(
			&entry.path(),
			&format!("{label}/{}", entry.file_name().to_string_lossy()),
			true,
			long,
			recursive,
		)?;
	}
	Ok(())
}
fn listing_json(path: &Path, recursive: bool) -> Result<Value, Failure> {
	let meta = fs::symlink_metadata(path)?;
	Ok(if meta.is_file() {
		json!({"type":"regular", "size":meta.len(), "executable":meta.permissions().mode() & 0o100 != 0})
	} else if meta.file_type().is_symlink() {
		json!({"type":"symlink", "target":fs::read_link(path)?.to_string_lossy()})
	} else {
		let mut entries = serde_json::Map::new();
		for entry in fs::read_dir(path)? {
			let entry = entry?;
			entries.insert(
				entry.file_name().to_string_lossy().into_owned(),
				if recursive {
					listing_json(&entry.path(), true)?
				} else {
					json!({})
				},
			);
		}
		json!({"type":"directory", "entries":entries})
	})
}

fn verify(parsed: &Parsed, store: &dyn Store) -> Result<ExitCode, Failure> {
	parsed.unsupported(&["substituter"])?;
	let paths = paths(parsed, store)?;
	let keys = parsed
		.settings
		.get_list(
			"trusted-public-keys",
			&["cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY="],
		)
		.iter()
		.map(|k| VerifyingKey::parse(k).map_err(|e| e.to_string()))
		.collect::<Result<Vec<_>, _>>()?;
	let needed = parsed
		.value("sigs-needed")
		.unwrap_or("0")
		.parse::<usize>()
		.map_err(|e| e.to_string())?;
	let mut status = 0;
	for path in paths {
		let info = match info(store, &path) {
			Ok(info) => info,
			Err(error) => {
				if let Failure::Error(message) = error {
					crate::cli::print_error(&message);
				}
				status |= 4;
				continue;
			}
		};
		log(&format!("checking '{}'...", path.to_absolute_path()));
		if !parsed.has("no-contents") {
			let (hash, _) = match nix_rs_store::nar::hash_path(&store.real_path(&path)) {
				Ok(value) => value,
				Err(error) => {
					crate::cli::print_error(&error.to_string());
					status |= 4;
					continue;
				}
			};
			if hash != info.nar_hash {
				crate::cli::print_error(&format!(
					"path '{}' was modified! expected hash '{}', got 'sha256:{}'",
					path.to_absolute_path(),
					info.nar_hash_nix32(),
					nix_compat::nixbase32::encode(&hash)
				));
				status |= 1;
			}
		}
		if !parsed.has("no-trust")
			&& !(info.ultimate && needed == 0)
			&& info.check_signatures(&keys) < needed.max(1)
		{
			crate::cli::print_error(&format!("path '{}' is untrusted", path.to_absolute_path()));
			status |= 2;
		}
	}
	Ok(ExitCode::from(status))
}
