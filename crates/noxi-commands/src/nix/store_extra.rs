//! `nix store` subcommands beyond inspection and maintenance: signing,
//! signature import, hash-part lookup, closure diffs, content-addressed
//! rewriting, build-log copies, repair, the roots daemon and build traces.
//! They follow `sigs.cc`, `path-from-hash-part.cc`, `diff-closures.cc`,
//! `make-content-addressed.cc`, `store-copy-log.cc`, `store-repair.cc`,
//! `unix/store-roots-daemon.cc` and `build-trace.cc` in CppNix 2.35.2.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, BufReader, Write};
use std::process::ExitCode;

use nix_compat::nixhash::{CAHash, HashAlgo, NixHash};
use nix_compat::store_path::{StorePath, build_ca_path};
use noxi_store::keys::SecretKey;
use noxi_store::{LocalStore, Store, ValidPathInfo};
use serde_json::json;

use super::command_store::{info, paths};
use super::{Parsed, hash::strerror, unsupported};
use crate::cli::Failure;
use crate::user_env::DrvName;

const ANSI_NORMAL: &str = "\x1b[0m";
const ANSI_RED: &str = "\x1b[31;1m";
const ANSI_GREEN: &str = "\x1b[32;1m";

fn info_log(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
		eprintln!("{message}");
	}
}

fn notice(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Notice) {
		eprintln!("{message}");
	}
}

/// Run the subcommands this module implements; `None` for the others.
pub fn run(parsed: &Parsed) -> Option<Result<ExitCode, Failure>> {
	Some(match parsed.command {
		"store sign" => sign(parsed),
		"store copy-sigs" => copy_sigs(parsed),
		"store path-from-hash-part" => path_from_hash_part(parsed),
		"store prefetch-file" => super::prefetch::prefetch_file(parsed),
		"store diff-closures" => diff_closures(parsed),
		"store make-content-addressed" => make_content_addressed(parsed),
		"store copy-log" => copy_log(parsed),
		"store repair" => repair(parsed),
		"store roots-daemon" => roots_daemon(parsed),
		"store build-trace info" => build_trace_info(parsed),
		_ => return None,
	})
}

/// The selected store, which these commands need to be a local one.
fn local(parsed: &Parsed, uri: Option<&str>, what: &str) -> Result<LocalStore, Failure> {
	let default = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	if let Some(root) = uri
		.and_then(|u| u.split_once('?'))
		.and_then(|(_, query)| query.split('&').find_map(|p| p.strip_prefix("root=")))
		&& !root.starts_with('/')
	{
		return Err(Failure::Error(format!("not an absolute path: \"{root}\"")));
	}
	let uri = uri.unwrap_or(&default);
	crate::store::open_local(uri, &parsed.settings)
		.map_err(|error| {
			if error.starts_with("operations through the Nix daemon") {
				unsupported(&format!("{what} through the Nix daemon"))
			} else {
				Failure::Error(error)
			}
		})?
		.map(|(store, _)| store)
		.ok_or_else(|| unsupported(&format!("{what} on this store")))
}

fn sign(parsed: &Parsed) -> Result<ExitCode, Failure> {
	// CppNix checks required flags while parsing the command line.
	let key_file = parsed
		.value("key-file")
		.ok_or_else(|| Failure::Usage("required argument '--key-file' is missing".into()))?;
	let store = local(parsed, None, "store sign")?;
	let paths = paths(parsed, &store)?;
	let text = fs::read(key_file).map_err(|error| {
		Failure::Error(format!("opening file \"{key_file}\": {}", strerror(&error)))
	})?;
	let key = SecretKey::parse(&String::from_utf8_lossy(&text))?;
	let mut added = 0;
	for path in &paths {
		let info = info(&store, path)?;
		let signature = info.signature(&key)?;
		if !info.sigs.contains(&signature) {
			store.add_signatures(path, &BTreeSet::from([signature]))?;
			added += 1;
		}
	}
	info_log(&format!("added {added} signatures"));
	Ok(ExitCode::SUCCESS)
}

/// A store `copy-sigs` reads signatures from.
enum Substituter {
	Local(LocalStore),
	Cache(noxi_store::binary_cache::BinaryCache),
}

impl Substituter {
	fn query(&self, path: &StorePath) -> Result<Option<ValidPathInfo>, Failure> {
		Ok(match self {
			Self::Local(store) => store.query_path_info(path)?,
			Self::Cache(cache) => cache.query_path_info(path)?.map(|cached| cached.info),
		})
	}
}

fn copy_sigs(parsed: &Parsed) -> Result<ExitCode, Failure> {
	let store = local(parsed, None, "store copy-sigs")?;
	let paths = paths(parsed, &store)?;
	let uris: Vec<&str> = parsed
		.flags
		.get("substituter")
		.map(|values| values.iter().map(|v| v[0].as_str()).collect())
		.unwrap_or_default();
	if uris.is_empty() {
		return Err(Failure::Usage(
			"you must specify at least one substituter using '-s'".into(),
		));
	}
	let substituters = uris
		.iter()
		.map(|uri| {
			if uri.starts_with("file://")
				|| uri.starts_with("http://")
				|| uri.starts_with("https://")
			{
				Ok(Substituter::Cache(
					noxi_store::binary_cache::BinaryCache::open(uri)?,
				))
			} else {
				local(parsed, Some(uri), "store copy-sigs from this substituter")
					.map(Substituter::Local)
			}
		})
		.collect::<Result<Vec<_>, Failure>>()?;
	let mut added = 0;
	for path in &paths {
		let info = info(&store, path)?;
		let mut new = BTreeSet::new();
		for substituter in &substituters {
			let Some(other) = substituter.query(path)? else {
				continue;
			};
			// Signatures for different contents do not apply.
			if info.nar_hash != other.nar_hash
				|| info.nar_size != other.nar_size
				|| info.references != other.references
			{
				continue;
			}
			new.extend(
				other
					.sigs
					.into_iter()
					.filter(|sig| !info.sigs.contains(sig)),
			);
		}
		if !new.is_empty() {
			store.add_signatures(path, &new)?;
			added += new.len();
		}
	}
	info_log(&format!("imported {added} signatures"));
	Ok(ExitCode::SUCCESS)
}

fn path_from_hash_part(parsed: &Parsed) -> Result<ExitCode, Failure> {
	let store = local(parsed, None, "store path-from-hash-part")?;
	let hash_part = &parsed.operands[0];
	match store.query_path_from_hash_part(hash_part)? {
		Some(path) => println!("{}", path.to_absolute_path()),
		None => {
			return Err(Failure::Error(format!(
				"there is no store path corresponding to '{hash_part}'"
			)));
		}
	}
	Ok(ExitCode::SUCCESS)
}

/// A store-path installable: a store path or a symlink to one.
fn store_path_installable(store: &dyn Store, operand: &str) -> Result<StorePath, Failure> {
	if operand.contains('#') || operand.starts_with('.') && !operand.contains('/') {
		return Err(unsupported("installables other than store paths"));
	}
	let path = crate::paths::follow_links_to_store_path(&std::env::current_dir()?, operand)
		.map_err(|_| unsupported("installables other than store paths"))?;
	if !store.is_valid_path(&path)? {
		info_log("don't know how to build these paths:");
		info_log(&format!("  {}", path.to_absolute_path()));
		return Err(Failure::Error(format!(
			"path '{}' is not valid",
			path.to_absolute_path()
		)));
	}
	Ok(path)
}

type Grouped = BTreeMap<String, BTreeMap<String, BTreeSet<StorePath>>>;

/// `getClosureInfo`: the closure grouped by package name and version.
fn closure_info(store: &dyn Store, top: &StorePath) -> Result<Grouped, Failure> {
	let output = regex::Regex::new("^(.*)-([a-z]+|lib32|lib64)$").expect("valid regex");
	let mut grouped = Grouped::new();
	for path in store.compute_closure(&BTreeSet::from([top.clone()]))? {
		let name = output
			.captures(path.name())
			.map_or_else(|| path.name().to_owned(), |c| c[1].to_owned());
		let name = DrvName::new(&name);
		grouped
			.entry(name.name)
			.or_default()
			.entry(name.version)
			.or_default()
			.insert(path);
	}
	Ok(grouped)
}

fn show_versions(versions: &BTreeSet<&String>) -> String {
	if versions.is_empty() {
		return "\u{2205}".into();
	}
	let shown: BTreeSet<&str> = versions
		.iter()
		.map(|v| if v.is_empty() { "\u{3b5}" } else { v.as_str() })
		.collect();
	shown.into_iter().collect::<Vec<_>>().join(", ")
}

/// `printClosureDiff`.
pub(super) fn print_closure_diff(
	store: &dyn Store,
	before: &StorePath,
	after: &StorePath,
	indent: &str,
) -> Result<(), Failure> {
	let before = closure_info(store, before)?;
	let after = closure_info(store, after)?;
	let names: BTreeSet<&String> = before.keys().chain(after.keys()).collect();
	let empty = BTreeMap::new();
	let mut out = io::stdout().lock();
	for name in names {
		let old = before.get(name).unwrap_or(&empty);
		let new = after.get(name).unwrap_or(&empty);
		let total = |versions: &BTreeMap<String, BTreeSet<StorePath>>| -> Result<u64, Failure> {
			let mut sum = 0;
			for paths in versions.values() {
				for path in paths {
					sum += info(store, path)?.nar_size;
				}
			}
			Ok(sum)
		};
		let delta = total(new)? as i64 - total(old)? as i64;
		let show_delta = delta.unsigned_abs() >= 8 * 1024;
		let removed: BTreeSet<&String> = old.keys().filter(|v| !new.contains_key(*v)).collect();
		let added: BTreeSet<&String> = new.keys().filter(|v| !old.contains_key(*v)).collect();
		if show_delta || !removed.is_empty() || !added.is_empty() {
			let mut items = Vec::new();
			if !removed.is_empty() || !added.is_empty() {
				items.push(format!(
					"{} \u{2192} {}",
					show_versions(&removed),
					show_versions(&added)
				));
			}
			if show_delta {
				items.push(format!(
					"{}{}{ANSI_NORMAL}",
					if delta > 0 { ANSI_RED } else { ANSI_GREEN },
					noxi_store::gc::render_size(delta, false)
				));
			}
			writeln!(out, "{indent}{name}: {}", items.join(", "))?;
		}
	}
	Ok(())
}

fn diff_closures(parsed: &Parsed) -> Result<ExitCode, Failure> {
	if parsed.has("expr") || parsed.has("file") {
		return Err(unsupported(
			"store diff-closures of expression installables",
		));
	}
	let opened = super::command_store::open(parsed)?;
	let store = opened.store.as_ref();
	let before = store_path_installable(store, &parsed.operands[0])?;
	let after = store_path_installable(store, &parsed.operands[1])?;
	print_closure_diff(store, &before, &after, "")?;
	Ok(ExitCode::SUCCESS)
}

/// `makeContentAddressed`: copy the closure of `roots` from `source` to
/// `destination` as NAR-hashed content-addressed paths, rewriting the
/// references between them. Returns the old and new path of each member.
fn content_address(
	source: &LocalStore,
	destination: &LocalStore,
	roots: &BTreeSet<StorePath>,
) -> Result<BTreeMap<StorePath, StorePath>, Failure> {
	let closure = source.compute_closure(roots)?;
	let mut remappings = BTreeMap::new();
	for path in crate::paths::topo_sort(source, &closure)?.into_iter().rev() {
		let old = info(source, &path)?;
		let old_hash = path.to_string()[..32].to_owned();
		let mut nar = Vec::new();
		source.nar_from_path(&path, &mut nar)?;
		let mut references = BTreeSet::new();
		let mut self_reference = false;
		let mut rewrites = BTreeMap::new();
		for reference in &old.references {
			if *reference == path {
				self_reference = true;
				continue;
			}
			let replacement = remappings
				.get(reference)
				.cloned()
				.unwrap_or_else(|| reference.clone());
			if replacement != *reference {
				rewrites.insert(reference.to_absolute_path(), replacement.to_absolute_path());
			}
			references.insert(replacement);
		}
		// `rewriteStrings`: each rewrite in turn, over the whole NAR.
		for (from, to) in &rewrites {
			nar = replace_all(&nar, from.as_bytes(), to.as_bytes());
		}
		let NixHash::Sha256(digest) =
			noxi_store::ca::hash_modulo_bytes(&nar, HashAlgo::Sha256, old_hash.as_bytes())
		else {
			unreachable!("SHA-256 was requested");
		};
		let hash = NixHash::Sha256(digest);
		let new_path = build_ca_path(
			path.name(),
			true,
			&hash,
			references.iter().map(StorePath::as_ref),
			self_reference,
		)
		.map_err(|e| e.to_string())?
		.to_owned();
		info_log(&format!(
			"rewriting '{}' to '{}'",
			path.to_absolute_path(),
			new_path.to_absolute_path()
		));
		let new_hash = new_path.to_string()[..32].to_owned();
		let rewritten =
			noxi_store::ca::rewrite_bytes(&nar, old_hash.as_bytes(), new_hash.as_bytes());
		let mut digester = nix_compat::nixhash::NixHashDigester::new(HashAlgo::Sha256);
		digester.update(&rewritten);
		let NixHash::Sha256(nar_hash) = digester.finalize() else {
			unreachable!()
		};
		let mut new_info = ValidPathInfo::new(new_path.clone(), nar_hash);
		new_info.nar_size = nar.len() as u64;
		new_info.ca = Some(CAHash::Nar(hash));
		new_info.references = references;
		if self_reference {
			new_info.references.insert(new_path.clone());
		}
		if !destination.is_valid_path(&new_path)? {
			destination.add_to_store(
				&new_info,
				&mut BufReader::new(rewritten.as_slice()),
				false,
				true,
			)?;
		}
		remappings.insert(path, new_path);
	}
	Ok(remappings)
}

fn replace_all(data: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
	if from.is_empty() {
		return data.to_vec();
	}
	let mut out = Vec::with_capacity(data.len());
	let mut rest = data;
	while let Some(at) = memfind(rest, from) {
		out.extend_from_slice(&rest[..at]);
		out.extend_from_slice(to);
		rest = &rest[at + from.len()..];
	}
	out.extend_from_slice(rest);
	out
}

fn memfind(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|w| w == needle)
}

fn make_content_addressed(parsed: &Parsed) -> Result<ExitCode, Failure> {
	let source = local(parsed, parsed.value("from"), "store make-content-addressed")?;
	let destination = local(parsed, parsed.value("to"), "store make-content-addressed")?;
	let roots = paths(parsed, &source)?;
	let remappings = content_address(&source, &destination, &roots)?;
	if parsed.has("json") {
		let rewrites: serde_json::Map<String, serde_json::Value> = roots
			.iter()
			.map(|path| {
				(
					path.to_absolute_path(),
					json!(remappings[path].to_absolute_path()),
				)
			})
			.collect();
		let value = json!({ "rewrites": rewrites });
		if parsed.has("pretty") {
			println!(
				"{}",
				serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
			);
		} else {
			println!("{value}");
		}
	} else {
		for path in &roots {
			notice(&format!(
				"rewrote '{}' to '{}'",
				path.to_absolute_path(),
				remappings[path].to_absolute_path()
			));
		}
	}
	Ok(ExitCode::SUCCESS)
}

/// `LocalFSStore::getBuildLog`: the plain or bzip2-compressed log.
fn build_log(store: &LocalStore, drv: &StorePath) -> Result<Option<Vec<u8>>, Failure> {
	let name = drv.to_string();
	let base = store.config().log_dir.join("drvs").join(&name[..2]);
	for (path, compressed) in [
		(base.join(&name[2..]), false),
		(base.join(format!("{}.bz2", &name[2..])), true),
	] {
		match fs::File::open(&path) {
			Ok(file) => {
				let mut reader = noxi_store::binary_cache::decompress(
					compressed.then_some("bzip2"),
					Box::new(BufReader::new(file)),
				)?;
				let mut log = Vec::new();
				io::Read::read_to_end(&mut reader, &mut log)?;
				return Ok(Some(log));
			}
			Err(error) if error.kind() == io::ErrorKind::NotFound => {}
			Err(error) => return Err(error.into()),
		}
	}
	Ok(None)
}

fn copy_log(parsed: &Parsed) -> Result<ExitCode, Failure> {
	if parsed.has("expr") || parsed.has("file") {
		return Err(unsupported("store copy-log of expression installables"));
	}
	let source = local(parsed, parsed.value("from"), "store copy-log")?;
	if !parsed.has("from") && !parsed.has("to") {
		return Err(Failure::Usage(
			"you must pass '--from' and/or '--to'".into(),
		));
	}
	let destination = local(parsed, parsed.value("to"), "store copy-log")?;
	let mut derivations = Vec::new();
	for operand in &parsed.operands {
		let path = store_path_installable(&source, operand)?;
		if path.name().ends_with(".drv") {
			derivations.push(path);
		} else {
			derivations.push(info(&source, &path)?.deriver.ok_or_else(|| {
				Failure::Error(format!(
					"'{}' does not have a known deriver",
					path.to_absolute_path()
				))
			})?);
		}
	}
	if parsed.operands.is_empty() {
		return Err(unsupported(
			"implicit flake installables; provide store paths",
		));
	}
	for drv in derivations {
		let Some(log) = build_log(&source, &drv)? else {
			return Err(Failure::Error(format!(
				"build log for '{}' is not available",
				drv.to_absolute_path()
			)));
		};
		destination.add_build_log(&drv, &mut log.as_slice())?;
	}
	Ok(ExitCode::SUCCESS)
}

fn repair(parsed: &Parsed) -> Result<ExitCode, Failure> {
	let store = local(parsed, None, "store repair")?;
	let caches = if parsed.settings.get_bool("substitute", true) {
		parsed
			.settings
			.get_list("substituters", &["https://cache.nixos.org/"])
			.iter()
			.map(|uri| noxi_store::binary_cache::BinaryCache::open(uri))
			.collect::<Result<Vec<_>, _>>()?
	} else {
		Vec::new()
	};
	for path in paths(parsed, &store)? {
		let mut restored = false;
		for cache in &caches {
			let Some(cached) = cache.query_path_info(&path)? else {
				continue;
			};
			// The store import verifies the archive hash and trust before replacing
			// the path. Importing with repair=true restores even registered paths.
			let mut nar = cache.nar(&cached)?;
			store.add_to_store(&cached.info, &mut nar, true, true)?;
			restored = true;
			break;
		}
		if restored {
			continue;
		}
		if let Some(deriver) = info(&store, &path)?.deriver
			&& store.is_valid_path(&deriver)?
		{
			return Err(unsupported(
				"store repair by rebuilding the deriver (repairing builds are unavailable)",
			));
		}
		return Err(Failure::Error(format!(
			"cannot repair path '{}'",
			path.to_absolute_path()
		)));
	}
	Ok(ExitCode::SUCCESS)
}

fn roots_daemon(parsed: &Parsed) -> Result<ExitCode, Failure> {
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let store = match crate::store::open_local(&uri, &parsed.settings) {
		Ok(Some((store, _))) => store,
		_ => {
			let shown = uri.split('?').next().unwrap_or(&uri);
			return Err(Failure::Usage(format!(
				"Roots daemon only functions with a local store, not '{shown}'"
			)));
		}
	};
	if std::env::var_os("LISTEN_FDS").is_some() {
		return Err(unsupported(
			"store roots-daemon with systemd socket activation",
		));
	}
	let socket = store.config().state_dir.join("gc-roots-socket/socket");
	let parent = socket.parent().expect("socket has a parent");
	fs::create_dir_all(parent)?;
	let _ = fs::remove_file(&socket);
	let listener = std::os::unix::net::UnixListener::bind(&socket)?;
	fs::set_permissions(&socket, std::os::unix::fs::PermissionsExt::from_mode(0o666))?;
	for connection in listener.incoming() {
		let Ok(mut connection) = connection else {
			continue;
		};
		std::thread::spawn(move || {
			let mut buffer = Vec::new();
			for root in noxi_store::gc::runtime_roots_unchecked() {
				buffer.extend_from_slice(root.to_absolute_path().as_bytes());
				buffer.push(0);
			}
			let _ = connection.write_all(&buffer);
		});
	}
	Ok(ExitCode::SUCCESS)
}

fn build_trace_info(parsed: &Parsed) -> Result<ExitCode, Failure> {
	super::gate(&parsed.settings, "ca-derivations")?;
	let opened = super::command_store::open(parsed)?;
	let store = opened.store.as_ref();
	if parsed.operands.is_empty() {
		return Err(unsupported(
			"implicit flake installables; provide store paths",
		));
	}
	let mut paths = BTreeSet::new();
	for operand in &parsed.operands {
		let path = store_path_installable(store, operand)?;
		if path.name().ends_with(".drv") || operand.contains('^') {
			return Err(unsupported("store build-trace info of derivation outputs"));
		}
		paths.insert(path);
	}
	if parsed.has("json") {
		let value: Vec<_> = paths
			.iter()
			.map(|path| json!({ "opaquePath": path.to_absolute_path() }))
			.collect();
		println!("{}", serde_json::Value::Array(value));
	} else {
		for path in paths {
			println!("{}", path.to_absolute_path());
		}
	}
	Ok(ExitCode::SUCCESS)
}
