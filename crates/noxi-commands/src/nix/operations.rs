//! Dependency explanations and the modern daemon entry point.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;
use std::process::ExitCode;

use nix_compat::store_path::StorePath;
use noxi_daemon::connection::{Trust, process_connection};

use super::{Parsed, command_store, expression, unsupported};
use crate::cli::Failure;

pub(super) fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	match parsed.command {
		"daemon" => daemon(parsed),
		"why-depends" => why_depends(parsed),
		"config check" => config_check(parsed),
		"bundle" => bundle(parsed),
		"repl" => super::repl::run(parsed),
		command => Err(unsupported(command)),
	}
}

fn bundle(mut parsed: Parsed) -> Result<ExitCode, Failure> {
	let mut options = expression::options(&parsed, true)?;
	let application = options
		.inputs
		.first()
		.cloned()
		.ok_or_else(|| Failure::Usage("one installable is required".into()))?;
	let bundler = parsed.value("bundler").unwrap_or("github:NixOS/bundlers");
	let (reference, fragment) = bundler.split_once('#').unwrap_or((bundler, ""));
	let system = parsed.settings.get("system").unwrap_or("x86_64-linux");
	let candidates = if fragment.is_empty() {
		vec![
			format!("bundlers.{system}.default"),
			format!("defaultBundler.{system}"),
		]
	} else if let Some(exact) = fragment.strip_prefix('.') {
		vec![exact.to_owned()]
	} else {
		vec![format!("bundlers.{system}.{fragment}"), fragment.to_owned()]
	};
	let quote = crate::instantiate::nix_string_literal;
	let reference = quote(reference);
	let candidates = candidates
		.iter()
		.map(|s| quote(s))
		.collect::<Vec<_>>()
		.join(" ");
	// `InstallableFlake::what`: the reference and its first attribute path.
	let first = match fragment {
		"" => format!("bundlers.{system}.default"),
		fragment => fragment.strip_prefix('.').unwrap_or(fragment).to_owned(),
	};
	// A bundle that is not a derivation evaluates to the bundler's label,
	// which is reported below without evaluation frames.
	options.inputs = vec![format!(
		"let bundle = (__noxi_flake_select (__noxi_flake {reference}) [ {candidates} ] (__noxi_flake_label {reference})) ({application}); in if (bundle.type or null) == \"derivation\" && bundle ? drvPath && bundle ? outPath then bundle else __noxi_flake_label {reference} + \"#\" + {}",
		quote(&first)
	)];
	options.strict = false;
	let mut request = super::flakes::request(&parsed, "outputs")?;
	request.no_write = true;
	options.flake = Some(request);
	// The bundler is selected on its own first, so a missing bundler is
	// reported without the frames of the bundle check.
	let mut first_options = options.clone();
	first_options.inputs.insert(
		0,
		format!("__noxi_flake_select (__noxi_flake {reference}) [ {candidates} ] (__noxi_flake_label {reference})"),
	);
	let evaluated = match crate::instantiate::evaluate(&first_options, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	if let Some(noxi_eval::Value::String(label)) = evaluated.results.get(1) {
		return Err(Failure::Error(format!(
			"the bundler '{}' does not produce a derivation",
			String::from_utf8_lossy(label.as_bytes())
		)));
	}
	if !parsed.has("out-link") && !parsed.has("no-link") {
		let Some(noxi_eval::Value::Attrs(attrs)) = evaluated.results.get(1) else {
			return Err(Failure::Error(
				"bundler did not produce an attribute set".into(),
			));
		};
		// Force the name using the same evaluator rather than reading a lazy value.
		let _ = attrs;
		let mut name_options = options.clone();
		name_options.inputs = vec![format!("({}).name", options.inputs[0])];
		name_options.strict = true;
		let name = match crate::instantiate::evaluate(&name_options, &std::env::current_dir()?) {
			Ok(value) => value,
			Err(code) => return Ok(code),
		};
		let Some(noxi_eval::Value::String(name)) = name.results.first() else {
			return Err(Failure::Error("bundle name is not a string".into()));
		};
		let name = std::str::from_utf8(name.as_bytes())
			.map_err(|e| e.to_string())?
			.to_owned();
		parsed.flags.insert("out-link".into(), vec![vec![name]]);
	}
	let mut build_options = options;
	build_options.eval = false;
	let built = match crate::instantiate::evaluate(&build_options, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	let derivations = crate::instantiate::derivations(&built.results)?;
	let store = built
		.store
		.ok_or_else(|| unsupported("bundle on this store"))?;
	let paths: Vec<_> = derivations
		.into_iter()
		.map(|(drv, output)| noxi_store::DerivedPath::Built {
			drv,
			outputs: BTreeSet::from([output]),
		})
		.collect();
	if paths.len() != 1 {
		return Err(Failure::Error(
			"bundler must produce exactly one derivation".into(),
		));
	}
	store.build_paths(&paths)?;
	if !parsed.has("no-link") {
		let link = std::env::current_dir()?.join(parsed.value("out-link").unwrap_or("result"));
		let outputs = expression::outputs(store.as_ref(), &paths[0])?;
		if outputs.len() != 1 {
			return Err(Failure::Error(
				"bundle must have exactly one selected output".into(),
			));
		}
		store.add_perm_root(outputs.values().next().unwrap(), &link)?;
	}
	Ok(ExitCode::SUCCESS)
}

fn config_check(parsed: Parsed) -> Result<ExitCode, Failure> {
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let (store, _) = crate::store::open_local(&uri, &parsed.settings)?
		.ok_or_else(|| unsupported("config check for a non-local store"))?;
	eprintln!("Running checks against store uri: {uri}");
	let mut versions = BTreeSet::new();
	let mut unrooted = BTreeSet::new();
	for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
		if let Ok(binary) = directory.join("nix-env").canonicalize() {
			if let Some(parent) = binary.parent() {
				versions.insert(parent.to_path_buf());
			}
		}
		let Some(profile) = directory.parent() else {
			continue;
		};
		let Ok(resolved) = profile.canonicalize() else {
			continue;
		};
		if resolved.parent() == Some(std::path::Path::new(nix_compat::store_path::store_dir()))
			&& resolved
				.file_name()
				.is_some_and(|name| name.to_string_lossy().ends_with("-user-environment"))
		{
			let mut current = profile.to_path_buf();
			let mut seen = BTreeSet::new();
			while !current
				.components()
				.any(|component| component.as_os_str() == "profiles")
				&& seen.insert(current.clone())
			{
				let Ok(link) = std::fs::read_link(&current) else {
					break;
				};
				current = if link.is_absolute() {
					link
				} else {
					current
						.parent()
						.unwrap_or(std::path::Path::new("."))
						.join(link)
				};
			}
			if !current
				.components()
				.any(|component| component.as_os_str() == "profiles")
			{
				unrooted.insert(directory);
			}
		}
	}
	let mut success = true;
	match versions.len() {
		0 => {
			eprintln!("[FAIL] No nix-env found in PATH.");
			success = false;
		}
		1 => eprintln!("[PASS] PATH contains only one nix version."),
		_ => {
			eprintln!("[FAIL] Multiple versions of nix found in PATH:");
			for directory in versions {
				eprintln!("  {}", directory.display());
			}
			eprintln!();
			success = false;
		}
	}
	if unrooted.is_empty() {
		eprintln!("[PASS] All profiles are gcroots.");
	} else {
		eprintln!(
			"[FAIL] Found profiles outside of {}/profiles.\nThe generation this profile points to might not have a gcroot and could be\ngarbage collected, resulting in broken symlinks.\n",
			store.config().state_dir.display()
		);
		for directory in unrooted {
			eprintln!("  {}", directory.display());
		}
		eprintln!("\n");
		success = false;
	}
	eprintln!("[PASS] Client protocol matches store protocol.");
	eprintln!("[INFO] You are trusted by store uri: {uri}");
	Ok(ExitCode::from(if success { 0 } else { 2 }))
}

fn daemon(parsed: Parsed) -> Result<ExitCode, Failure> {
	if parsed.has("stdio") && parsed.has("socket-path") {
		return Err(Failure::Usage(
			"'--stdio' and '--socket-path' are incompatible".into(),
		));
	}
	// `--process-ops` only stops forwarding to a remote backing store; a
	// local backing store always processes the operations itself.
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let (store, _) = crate::store::open_local(&uri, &parsed.settings)?
		.ok_or_else(|| unsupported("daemon with a non-local backing store"))?;
	let mut settings =
		noxi_daemon::DaemonSettings::from_config(store.config().clone(), &parsed.settings)?;
	for (name, _) in &parsed.sequence {
		match name.as_str() {
			"force-trusted" => settings.force_trust = Some(Trust::Trusted),
			"force-untrusted" => settings.force_trust = Some(Trust::NotTrusted),
			"default-trust" => settings.force_trust = None,
			_ => {}
		}
	}
	let result = if parsed.has("stdio") {
		process_connection(
			std::io::stdin(),
			std::io::stdout(),
			&store,
			&settings,
			settings.force_trust.unwrap_or(Trust::Trusted),
		)
	} else {
		let socket = parsed
			.value("socket-path")
			.map(PathBuf::from)
			.or_else(|| std::env::var_os("NIX_DAEMON_SOCKET_PATH").map(PathBuf::from))
			.unwrap_or_else(|| settings.store.state_dir.join("daemon-socket/socket"));
		let listeners =
			noxi_daemon::server::listeners(&socket).map_err(|e| Failure::Error(e.to_string()))?;
		noxi_daemon::server::serve_listeners(listeners, &settings)
	};
	result.map_err(|e| Failure::Error(e.to_string()))?;
	Ok(ExitCode::SUCCESS)
}

/// `nix why-depends`, following `src/nix/why-depends.cc`. Distances come
/// from a breadth-first search, which the reference's pointer-ordered
/// priority queue matches whenever the shortest paths are unambiguous.
fn why_depends(parsed: Parsed) -> Result<ExitCode, Failure> {
	let target = match expression::targets(&parsed)? {
		Ok(target) => target,
		Err(code) => return Ok(code),
	};
	if target.paths.len() != 2 {
		return Err(Failure::Usage(
			"exactly two installables are required".into(),
		));
	}
	let resolve = |index: usize| -> Result<StorePath, Failure> {
		if parsed.has("derivation") {
			return Ok(match &target.paths[index] {
				noxi_store::DerivedPath::Opaque(path)
				| noxi_store::DerivedPath::Built { drv: path, .. } => path.clone(),
			});
		}
		let outputs = expression::outputs(target.store.as_ref(), &target.paths[index])?;
		if outputs.len() != 1 {
			return Err(Failure::Error(
				"installable must resolve to exactly one store path".into(),
			));
		}
		Ok(outputs.into_values().next().unwrap())
	};
	if !parsed.has("derivation") {
		target.store.build_paths(&target.paths[..1])?;
	}
	let package = resolve(0)?;
	let dependency = resolve(1)?;
	let store = target.store.as_ref();
	let closure = store.compute_closure(&BTreeSet::from([package.clone()]))?;
	if !closure.contains(&dependency) {
		eprintln!(
			"'{}' does not depend on '{}'",
			what(&parsed.operands[0], &package),
			what(&parsed.operands[1], &dependency)
		);
		return Ok(ExitCode::SUCCESS);
	}
	let mut graph: BTreeMap<StorePath, WhyNode> = BTreeMap::new();
	for path in &closure {
		graph.insert(
			path.clone(),
			WhyNode {
				refs: command_store::info(store, path)?.references,
				rrefs: BTreeSet::new(),
				dist: if *path == dependency { 0 } else { usize::MAX },
				visited: false,
			},
		);
	}
	let edges: Vec<(StorePath, StorePath)> = graph
		.iter()
		.flat_map(|(path, node)| node.refs.iter().map(|r| (r.clone(), path.clone())))
		.collect();
	for (reference, referrer) in edges {
		if let Some(node) = graph.get_mut(&reference) {
			node.rrefs.insert(referrer);
		}
	}
	let mut queue = VecDeque::from([dependency.clone()]);
	while let Some(path) = queue.pop_front() {
		let dist = graph[&path].dist + 1;
		for rref in graph[&path].rrefs.clone() {
			let node = graph.get_mut(&rref).expect("closure member");
			if dist < node.dist {
				node.dist = dist;
				queue.push_back(rref);
			}
		}
	}
	let mut printer = WhyPrinter {
		store,
		graph,
		package: package.clone(),
		dependency: dependency.clone(),
		all: parsed.has("all"),
		precise: parsed.has("precise"),
		out: std::io::stdout().lock(),
	};
	if !printer.precise {
		use std::io::Write;
		writeln!(printer.out, "{}", package.to_absolute_path())?;
	}
	match printer.print_node(&package, "", "") {
		Ok(()) | Err(WhyStop::BailOut) => Ok(ExitCode::SUCCESS),
		Err(WhyStop::Failed(failure)) => Err(failure),
	}
}

/// `Installable::what` for the store-path installables noxi resolves.
fn what(operand: &str, path: &StorePath) -> String {
	if operand.starts_with('/') || operand.starts_with('.') {
		path.to_absolute_path()
	} else {
		operand.to_owned()
	}
}

struct WhyNode {
	refs: BTreeSet<StorePath>,
	rrefs: BTreeSet<StorePath>,
	dist: usize,
	visited: bool,
}

enum WhyStop {
	BailOut,
	Failed(Failure),
}

impl From<Failure> for WhyStop {
	fn from(error: Failure) -> Self {
		Self::Failed(error)
	}
}

impl From<std::io::Error> for WhyStop {
	fn from(error: std::io::Error) -> Self {
		Self::Failed(error.into())
	}
}

const TREE_CONN: &str = "\u{251c}\u{2500}\u{2500}\u{2500}";
const TREE_LAST: &str = "\u{2514}\u{2500}\u{2500}\u{2500}";
const TREE_LINE: &str = "\u{2502}   ";
const TREE_NULL: &str = "    ";
const ANSI_NORMAL: &str = "\x1b[0m";
const ANSI_GREY: &str = "\x1b[38;5;244m";

struct WhyPrinter<'a> {
	store: &'a dyn noxi_store::Store,
	graph: BTreeMap<StorePath, WhyNode>,
	package: StorePath,
	dependency: StorePath,
	all: bool,
	precise: bool,
	out: std::io::StdoutLock<'a>,
}

fn hash_part(path: &StorePath) -> String {
	path.to_string()[..32].to_owned()
}

/// `hilite`: colour `len` bytes of `s` at `pos`.
fn hilite(s: &[u8], pos: usize, len: usize, colour: &str) -> String {
	let end = (pos + len).min(s.len());
	format!(
		"{}{colour}{}{ANSI_NORMAL}{}",
		String::from_utf8_lossy(&s[..pos]),
		String::from_utf8_lossy(&s[pos..end]),
		String::from_utf8_lossy(&s[end..])
	)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
	haystack.windows(needle.len()).position(|w| w == needle)
}

impl WhyPrinter<'_> {
	/// `scanForReferencesDeep` with the hits `why-depends --precise` prints.
	fn scan(
		&self,
		root: &std::path::Path,
		rel: &str,
		refs: &BTreeSet<StorePath>,
		hits: &mut BTreeMap<String, Vec<String>>,
	) -> Result<(), Failure> {
		let physical = if rel.is_empty() {
			root.to_owned()
		} else {
			root.join(rel)
		};
		let meta = std::fs::symlink_metadata(&physical)?;
		let shown = if rel.is_empty() { "/" } else { rel };
		let colour = |hash: &str| {
			if *hash == hash_part(&self.dependency) {
				"\x1b[32;1m"
			} else {
				"\x1b[34;1m"
			}
		};
		if meta.is_dir() {
			let mut names: Vec<_> = std::fs::read_dir(&physical)?
				.map(|entry| entry.map(|e| e.file_name()))
				.collect::<Result<_, _>>()?;
			names.sort();
			for name in names {
				let child = if rel.is_empty() {
					name.to_string_lossy().into_owned()
				} else {
					format!("{rel}/{}", name.to_string_lossy())
				};
				self.scan(root, &child, refs, hits)?;
			}
		} else if meta.file_type().is_symlink() {
			use std::os::unix::ffi::OsStrExt;
			let target = std::fs::read_link(&physical)?;
			let target = target.as_os_str().as_bytes();
			for reference in refs {
				let hash = hash_part(reference);
				if let Some(pos) = find(target, hash.as_bytes()) {
					hits.entry(hash.clone()).or_default().push(format!(
						"{shown} -> {}",
						hilite(target, pos, 32, colour(&hash))
					));
				}
			}
		} else {
			let contents = std::fs::read(&physical)?;
			for reference in refs {
				let hash = hash_part(reference);
				if let Some(pos) = find(&contents, hash.as_bytes()) {
					let margin = 32;
					let start = pos.saturating_sub(margin);
					let end = (pos + hash.len() + margin).min(contents.len());
					let printable: Vec<u8> = contents[start..end]
						.iter()
						.map(|&c| if (0x20..0x7f).contains(&c) { c } else { b'.' })
						.collect();
					hits.entry(hash.clone()).or_default().push(format!(
						"{shown}: \u{2026}{}\u{2026}",
						hilite(&printable, pos - start, 32, colour(&hash))
					));
				}
			}
		}
		Ok(())
	}

	fn print_node(
		&mut self,
		path: &StorePath,
		first_pad: &str,
		tail_pad: &str,
	) -> Result<(), WhyStop> {
		use std::io::Write;
		if self.precise {
			writeln!(
				self.out,
				"{first_pad}{}{}{}{ANSI_NORMAL}",
				if self.graph[path].visited {
					ANSI_GREY
				} else {
					""
				},
				if first_pad.is_empty() {
					""
				} else {
					"\u{2192} "
				},
				path.to_absolute_path()
			)?;
		}
		if *path == self.dependency && !self.all && self.package != self.dependency {
			return Err(WhyStop::BailOut);
		}
		if self.graph[path].visited {
			return Ok(());
		}
		if self.precise {
			self.graph.get_mut(path).expect("closure member").visited = true;
		}
		let mut refs: Vec<(usize, StorePath)> = Vec::new();
		let mut ref_paths = BTreeSet::new();
		for reference in &self.graph[path].refs {
			if reference == path && self.package != self.dependency {
				continue;
			}
			let dist = self.graph[reference].dist;
			if dist == usize::MAX {
				continue;
			}
			refs.push((dist, reference.clone()));
			ref_paths.insert(reference.clone());
		}
		refs.sort_by_key(|(dist, _)| *dist);
		let mut hits = BTreeMap::new();
		if self.precise {
			let root = self.store.real_path(path);
			self.scan(&root, "", &ref_paths, &mut hits)?;
		}
		let count = refs.len();
		for (index, (_, reference)) in refs.into_iter().enumerate() {
			let hash = hash_part(&reference);
			let last = if self.all { index + 1 == count } else { true };
			let found = hits.get(&hash).cloned().unwrap_or_default();
			for (position, hit) in found.iter().enumerate() {
				let first = *hit == found[0];
				let _ = position;
				writeln!(
					self.out,
					"{tail_pad}{}{hit}",
					match (first, last) {
						(true, true) => TREE_LAST,
						(true, false) => TREE_CONN,
						(false, true) => TREE_NULL,
						(false, false) => TREE_LINE,
					}
				)?;
				if !self.all {
					break;
				}
			}
			if !self.precise {
				writeln!(
					self.out,
					"{first_pad}{}{}{}{ANSI_NORMAL}",
					if self.graph[&reference].visited {
						ANSI_GREY
					} else {
						""
					},
					if last { TREE_LAST } else { TREE_CONN },
					reference.to_absolute_path()
				)?;
				self.graph.get_mut(path).expect("closure member").visited = true;
			}
			let pad = format!("{tail_pad}{}", if last { TREE_NULL } else { TREE_LINE });
			self.print_node(&reference, &pad, &pad)?;
		}
		Ok(())
	}
}
