//! `nix-store`, following CppNix 2.35.2's `src/nix/nix-store/nix-store.cc`.
//!
//! Supported operations: `--realise` (with `--dry-run` and
//! `--ignore-unknown`) and `--query` with `--outputs`, `--requisites`
//! (`--include-outputs`), `--references`, `--referrers`,
//! `--referrers-closure`, `--deriver`, `--valid-derivers`, `--binding`,
//! `--hash` and `--size`, plus `--use-output` and `--force-realise`.
//! Maintenance: `--gc` with live/dead/root listings and `--max-freed`,
//! `--delete`, `--query --roots`, and `--optimise`/`--optimize`, locally
//! or through the owning daemon.
//! General flags: `--add-root`, `--indirect`, `--no-output`,
//! `--no-gc-warning`, `--stdin` and settings flags. Other operations fail
//! explicitly.

use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixbase32;
use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use nix_rs_store::gc::{GcAction, GcOptions, render_size};
use nix_rs_store::{DerivedPath, Store};

use crate::cli::{Failure, print_setting_warnings};
use crate::paths::{print_missing, topo_sort};
use crate::store;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Operation {
	Realise,
	Query,
	Gc,
	Delete,
	Optimise,
}

/// State of one `nix-store` invocation.
struct NixStore<'a> {
	store: &'a dyn Store,
	gc_root: Option<PathBuf>,
	root_nr: Cell<usize>,
	no_output: bool,
	gc_warning: bool,
	warned: Cell<bool>,
	cwd: PathBuf,
}

/// Run `nix-store` with the arguments after the program name.
pub fn main(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	if args
		.iter()
		.take_while(|arg| arg.as_str() != "--")
		.any(|arg| matches!(arg.as_str(), "--dump" | "--verify"))
	{
		let args = args.into_iter().map(|arg| {
			if arg == "--optimize" {
				"--optimise".into()
			} else {
				arg
			}
		});
		let mut options = crate::maintenance::parse_args(args, settings).map_err(Failure::Usage)?;
		print_setting_warnings(&mut options.settings);
		return Ok(crate::maintenance::run(
			&options,
			&mut io::stdout().lock(),
			&mut io::stderr().lock(),
		));
	}
	let mut op = None;
	let mut op_flags = Vec::new();
	let mut op_args = Vec::new();
	let mut gc_root = None;
	let mut no_output = false;
	let mut gc_warning = true;
	let mut read_stdin = false;
	let mut store_uri = None;
	let cwd = std::env::current_dir()?;

	let mut args = crate::cli::ArgStream::new(args);
	while let Some(arg) = args.next_arg() {
		if let Some(result) = crate::cli::apply_common_flag(&arg, &mut args, &mut settings) {
			result?;
			continue;
		}
		let previous = op;
		match arg.as_str() {
			"--version" => {
				println!("nix-store (Nix) 2.35.2");
				return Ok(ExitCode::SUCCESS);
			}
			"--realise" | "--realize" | "-r" => op = Some(Operation::Realise),
			"--query" | "-q" => op = Some(Operation::Query),
			"--gc" => op = Some(Operation::Gc),
			"--delete" => op = Some(Operation::Delete),
			"--optimise" | "--optimize" => op = Some(Operation::Optimise),
			"--add"
			| "-A"
			| "--add-fixed"
			| "--print-fixed-path"
			| "--print-env"
			| "--read-log"
			| "-l"
			| "--dump-db"
			| "--load-db"
			| "--register-validity"
			| "--check-validity"
			| "--dump"
			| "--restore"
			| "--export"
			| "--import"
			| "--init"
			| "--verify"
			| "--verify-path"
			| "--repair-path"
			| "--serve"
			| "--generate-binary-cache-key"
			| "--help" => {
				return Err(Failure::Error(format!(
					"nix-store {arg} is not supported by nix-rs yet"
				)));
			}
			"--add-root" => {
				let root = args
					.next()
					.ok_or_else(|| Failure::Usage(format!("flag '{arg}' requires 1 argument")))?;
				gc_root = Some(absolute(&cwd, Path::new(&root)));
			}
			"--stdin" => read_stdin = !io::stdin().is_terminal(),
			"--indirect" => {}
			"--no-output" => no_output = true,
			"--no-gc-warning" => gc_warning = false,
			"--" => op_args.extend(args.by_ref()),
			"--store" => {
				store_uri =
					Some(args.next().ok_or_else(|| {
						Failure::Usage(format!("flag '{arg}' requires 1 argument"))
					})?)
			}
			flag if flag.starts_with('-') && flag.len() > 1 => {
				match settings.apply_flag(flag, || args.next()) {
					Some(result) => result.map_err(|e| Failure::Usage(e.to_string()))?,
					None => {
						op_flags.push(arg.clone());
						if matches!(flag, "--max-freed" | "--max-links" | "--max-atime") {
							op_flags.extend(args.next());
						}
					}
				}
			}
			_ => op_args.push(arg),
		}
		if previous.is_some() && previous != op {
			return Err(Failure::Usage(
				"only one operation may be specified".to_owned(),
			));
		}
	}
	print_setting_warnings(&mut settings);
	if read_stdin {
		for line in io::stdin().lock().lines() {
			op_args.extend(line?.split_whitespace().map(str::to_owned));
		}
	}
	let op = op.ok_or_else(|| Failure::Usage("no operation specified".to_owned()))?;

	let uri = store::store_uri(store_uri.as_deref(), &settings);
	let opened = store::open(&uri, &settings)?
		.ok_or_else(|| Failure::Error(format!("store '{uri}' cannot hold store paths")))?;
	let command = NixStore {
		store: opened.store.as_ref(),
		gc_root,
		root_nr: Cell::new(0),
		no_output,
		gc_warning,
		warned: Cell::new(false),
		cwd,
	};
	let mut stdout = io::stdout().lock();
	match op {
		Operation::Realise => command.realise(&op_flags, &op_args, &mut stdout)?,
		Operation::Query => command.query(&op_flags, op_args, &settings, &mut stdout)?,
		Operation::Gc | Operation::Delete => {
			command.collect_garbage(op, &op_flags, &op_args, &settings, &mut stdout)?;
		}
		Operation::Optimise => {
			if !op_flags.is_empty() || !op_args.is_empty() {
				return Err(Failure::Usage("no arguments expected".into()));
			}
			command.store.optimise_store(&mut |message| {
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
					eprintln!("{message}");
				}
			})?;
		}
	}
	Ok(ExitCode::SUCCESS)
}

fn absolute(cwd: &Path, path: &Path) -> PathBuf {
	let joined = cwd.join(path);
	// CppNix's `absPath` canonicalises lexically.
	let mut out = PathBuf::new();
	for component in joined.components() {
		match component {
			std::path::Component::ParentDir => {
				out.pop();
			}
			std::path::Component::CurDir => {}
			c => out.push(c),
		}
	}
	out
}

fn print(path: &StorePath) -> String {
	path.to_absolute_path()
}

/// The paths of a derivation's outputs, which must be known in advance.
fn output_paths(
	store: &dyn Store,
	drv_path: &StorePath,
) -> Result<BTreeMap<String, StorePath>, Failure> {
	let drv = store.read_derivation(drv_path)?;
	if drv.outputs().is_deferred() {
		return Err(Failure::Usage(
			"Cannot use output path of floating content-addressing derivation until we know what it is (e.g. by building it)".to_owned(),
		));
	}
	Ok(drv
		.outputs()
		.iter()
		.map(|(name, path)| (name.to_string(), path.clone()))
		.collect())
}

impl NixStore<'_> {
	fn collect_garbage(
		&self,
		op: Operation,
		flags: &[String],
		args: &[String],
		settings: &Settings,
		stdout: &mut dyn Write,
	) -> Result<(), Failure> {
		let mut options = GcOptions::new(if op == Operation::Delete {
			GcAction::DeleteSpecific
		} else {
			GcAction::DeleteDead
		});
		options.keep_outputs = settings.get_bool("keep-outputs", false);
		options.keep_derivations = settings.get_bool("keep-derivations", true);
		let mut print_roots = false;
		let mut flags = flags.iter();
		while let Some(flag) = flags.next() {
			match (op, flag.as_str()) {
				(Operation::Delete, "--ignore-liveness") => options.ignore_liveness = true,
				(Operation::Delete, _) => {
					return Err(Failure::Usage(format!("unknown flag '{flag}'")));
				}
				(_, "--print-roots") => print_roots = true,
				(_, "--print-live") => options.action = GcAction::ReturnLive,
				(_, "--print-dead") => options.action = GcAction::ReturnDead,
				(_, "--max-freed") => {
					let value = flags
						.next()
						.ok_or_else(|| Failure::Usage(format!("'{flag}' requires an argument")))?;
					options.max_freed = crate::cli::parse_int_with_unit(value)
						.map_err(Failure::Usage)?
						.max(0) as u64;
				}
				_ => return Err(Failure::Usage(format!("bad sub-operation '{flag}' in GC"))),
			}
		}
		if op == Operation::Delete {
			options.paths = Some(
				args.iter()
					.map(|arg| self.follow_links(arg))
					.collect::<Result<_, _>>()?,
			);
		} else if !args.is_empty() {
			return Err(Failure::Usage("no arguments expected".into()));
		}
		if options.max_freed != u64::MAX
			&& (matches!(options.action, GcAction::ReturnLive | GcAction::ReturnDead)
				|| print_roots)
		{
			return Err(Failure::Usage("option --max-freed cannot be combined with --print-live, --print-dead, or --print-roots".into()));
		}
		let mut log = |message: &str| {
			if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
				eprintln!("{message}");
			}
		};
		if print_roots {
			let roots = self.store.find_roots(false, &mut log)?;
			let mut sorted = BTreeSet::new();
			for (path, links) in roots {
				for link in links {
					sorted.insert((link, path.clone()));
				}
			}
			for (link, path) in sorted {
				// C++ filesystem paths print quoted, escaping quotes and backslashes.
				let link = link.replace('\\', "\\\\").replace('"', "\\\"");
				writeln!(stdout, "\"{link}\" -> {}", path.to_absolute_path())?;
			}
		} else {
			let result = self.store.collect_garbage(&options, &mut log);
			// CppNix's finalizer prints the summary even if collection fails.
			let results = result.as_ref().ok().cloned().unwrap_or_default();
			if matches!(options.action, GcAction::ReturnLive | GcAction::ReturnDead) {
				for path in results.paths {
					writeln!(stdout, "{path}")?;
				}
			} else {
				writeln!(
					stdout,
					"{} store paths deleted, {} freed",
					results.paths.len(),
					render_size(results.bytes_freed as i64, false)
				)?;
			}
			result?;
		}
		Ok(())
	}

	fn follow_links(&self, arg: &str) -> Result<StorePath, Failure> {
		crate::paths::follow_links_to_store_path(&self.cwd, arg)
	}

	fn gc_warning(&self) {
		if self.gc_warning
			&& !self.warned.replace(true)
			&& nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn)
		{
			eprintln!(
				"warning: you did not specify '--add-root'; the result might be removed by the garbage collector"
			);
		}
	}

	fn root_name(&self, output: Option<&str>) -> Option<PathBuf> {
		let mut name = self.gc_root.clone()?.into_os_string();
		let nr = self.root_nr.get();
		if nr > 1 {
			name.push(format!("-{nr}"));
		}
		if let Some(output) = output.filter(|o| *o != "out") {
			name.push(format!("-{output}"));
		}
		Some(PathBuf::from(name))
	}

	/// CppNix's `realisePath`: realise (if `build`) and return the output
	/// paths, or the GC roots made for them with `--add-root`.
	fn realise_path(
		&self,
		path: &StorePath,
		outputs: &BTreeSet<String>,
		build: bool,
	) -> Result<BTreeSet<PathBuf>, Failure> {
		if path.name().ends_with(".drv") {
			if build {
				self.store.build_paths(&[DerivedPath::Built {
					drv: path.clone(),
					outputs: outputs.clone(),
				}])?;
			}
			let output_paths = self.store.query_derivation_outputs(path)?;
			let all = output_paths_or_derivation(self.store, path, output_paths)?;
			self.root_nr.set(self.root_nr.get() + 1);
			let wanted: BTreeSet<String> = if outputs.is_empty() {
				all.keys().cloned().collect()
			} else {
				outputs.clone()
			};
			let mut result = BTreeSet::new();
			for name in &wanted {
				let out_path = all.get(name).ok_or_else(|| {
					Failure::Error(format!(
						"derivation '{}' does not have an output named '{name}'",
						print(path)
					))
				})?;
				match self.root_name(Some(name)) {
					Some(root) => {
						result.insert(self.store.add_perm_root(out_path, &root)?);
					}
					None => {
						self.gc_warning();
						result.insert(PathBuf::from(print(out_path)));
					}
				}
			}
			Ok(result)
		} else {
			if build {
				self.store.ensure_path(path)?;
			} else if !self.store.is_valid_path(path)? {
				return Err(Failure::Error(format!(
					"path '{}' does not exist and cannot be created",
					print(path)
				)));
			}
			if self.gc_root.is_some() {
				self.root_nr.set(self.root_nr.get() + 1);
				let root = self.root_name(None).expect("gc root");
				return Ok([self.store.add_perm_root(path, &root)?].into());
			}
			self.gc_warning();
			Ok([PathBuf::from(print(path))].into())
		}
	}

	fn realise(
		&self,
		flags: &[String],
		args: &[String],
		out: &mut dyn Write,
	) -> Result<(), Failure> {
		let mut dry_run = false;
		let mut ignore_unknown = false;
		for flag in flags {
			match flag.as_str() {
				"--dry-run" => dry_run = true,
				"--ignore-unknown" => ignore_unknown = true,
				"--repair" | "--check" => {
					return Err(Failure::Error(format!(
						"nix-store --realise {flag} is not supported by nix-rs yet"
					)));
				}
				_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
			}
		}
		let mut paths = Vec::new();
		for arg in args {
			let (path, outputs) = match arg.split_once('!') {
				Some((path, outputs)) => (
					path,
					outputs
						.split(',')
						.filter(|o| !o.is_empty())
						.map(str::to_owned)
						.collect(),
				),
				None => (arg.as_str(), BTreeSet::new()),
			};
			paths.push((self.follow_links(path)?, outputs));
		}
		let derived = |paths: &[(StorePath, BTreeSet<String>)]| -> Vec<DerivedPath> {
			paths
				.iter()
				.map(|(path, outputs)| {
					if path.name().ends_with(".drv") {
						DerivedPath::Built {
							drv: path.clone(),
							outputs: outputs.clone(),
						}
					} else {
						DerivedPath::Opaque(path.clone())
					}
				})
				.collect()
		};
		let mut missing = self.store.query_missing(&derived(&paths))?;
		if ignore_unknown {
			paths.retain(|(path, _)| !missing.unknown.contains(path));
			missing.unknown.clear();
		}
		print_missing(self.store, &missing)?;
		if dry_run {
			return Ok(());
		}
		self.store.build_paths(&derived(&paths))?;
		if !ignore_unknown {
			for (path, outputs) in &paths {
				for result in self.realise_path(path, outputs, false)? {
					if !self.no_output {
						writeln!(out, "{}", result.display())?;
					}
				}
			}
		}
		Ok(())
	}

	/// CppNix's `maybeUseOutputs`.
	fn maybe_use_outputs(
		&self,
		path: StorePath,
		use_output: bool,
		force_realise: bool,
	) -> Result<BTreeSet<StorePath>, Failure> {
		if force_realise {
			self.realise_path(&path, &BTreeSet::new(), true)?;
		}
		if use_output && path.name().ends_with(".drv") {
			if force_realise {
				return Ok(self
					.store
					.query_derivation_outputs(&path)?
					.into_values()
					.collect());
			}
			return Ok(output_paths(self.store, &path)?.into_values().collect());
		}
		Ok([path].into())
	}

	fn query(
		&self,
		flags: &[String],
		mut args: Vec<String>,
		settings: &Settings,
		out: &mut dyn Write,
	) -> Result<(), Failure> {
		#[derive(Clone, Copy, PartialEq, Eq)]
		enum Query {
			Outputs,
			Requisites,
			References,
			Referrers,
			ReferrersClosure,
			Deriver,
			ValidDerivers,
			Binding,
			Hash,
			Size,
			Roots,
		}
		let mut query = None;
		let mut use_output = false;
		let mut include_outputs = false;
		let mut force_realise = false;
		let mut binding = String::new();
		for flag in flags {
			let previous = query;
			match flag.as_str() {
				"--outputs" => query = Some(Query::Outputs),
				"--requisites" | "-R" => query = Some(Query::Requisites),
				"--references" => query = Some(Query::References),
				"--referrers" | "--referers" => query = Some(Query::Referrers),
				"--referrers-closure" | "--referers-closure" => {
					query = Some(Query::ReferrersClosure)
				}
				"--deriver" | "-d" => query = Some(Query::Deriver),
				"--valid-derivers" => query = Some(Query::ValidDerivers),
				"--binding" | "-b" => {
					if args.is_empty() {
						return Err(Failure::Usage("expected binding name".to_owned()));
					}
					binding = args.remove(0);
					query = Some(Query::Binding);
				}
				"--hash" => query = Some(Query::Hash),
				"--size" => query = Some(Query::Size),
				"--roots" => query = Some(Query::Roots),
				"--tree" | "--graph" | "--graphml" | "--resolve" => {
					return Err(Failure::Error(format!(
						"nix-store --query {flag} is not supported by nix-rs yet"
					)));
				}
				"--use-output" | "-u" => use_output = true,
				"--force-realise" | "--force-realize" | "-f" => force_realise = true,
				"--include-outputs" => include_outputs = true,
				_ => return Err(Failure::Usage(format!("unknown flag '{flag}'"))),
			}
			if previous.is_some() && previous != query {
				return Err(Failure::Usage(format!(
					"query type '{flag}' conflicts with earlier flag"
				)));
			}
		}
		let print_sorted =
			|paths: BTreeSet<StorePath>, out: &mut dyn Write| -> Result<(), Failure> {
				for path in topo_sort(self.store, &paths)?.iter().rev() {
					writeln!(out, "{}", print(path))?;
				}
				Ok(())
			};

		match query.unwrap_or(Query::Outputs) {
			Query::Outputs => {
				for arg in &args {
					let path = self.follow_links(arg)?;
					for output in self.maybe_use_outputs(path, true, force_realise)? {
						writeln!(out, "{}", print(&output))?;
					}
				}
			}
			query @ (Query::Requisites
			| Query::References
			| Query::Referrers
			| Query::ReferrersClosure) => {
				let mut paths = BTreeSet::new();
				for arg in &args {
					let path = self.follow_links(arg)?;
					for path in self.maybe_use_outputs(path, use_output, force_realise)? {
						match query {
							Query::Requisites => {
								paths.extend(self.closure(&path, false, include_outputs, false)?)
							}
							Query::References => paths.extend(self.info(&path)?.references),
							Query::Referrers => paths.extend(self.referrers(&path)?),
							_ => paths.extend(self.closure(&path, true, false, false)?),
						}
					}
				}
				print_sorted(paths, out)?;
			}
			Query::Roots => {
				let mut referrers = BTreeSet::new();
				for arg in &args {
					let path = self.follow_links(arg)?;
					for path in self.maybe_use_outputs(path, use_output, force_realise)? {
						referrers.extend(self.closure(
							&path,
							true,
							settings.get_bool("keep-outputs", false),
							settings.get_bool("keep-derivations", true),
						)?);
					}
				}
				let roots = self.store.find_roots(false, &mut |message| {
					if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
						eprintln!("{message}");
					}
				})?;
				for (path, links) in roots {
					if referrers.contains(&path) {
						for link in links {
							writeln!(out, "{link} -> {}", print(&path))?;
						}
					}
				}
			}
			Query::Deriver => {
				for arg in &args {
					let info = self.info(&self.follow_links(arg)?)?;
					match info.deriver {
						Some(deriver) => writeln!(out, "{}", print(&deriver))?,
						None => writeln!(out, "unknown-deriver")?,
					}
				}
			}
			Query::ValidDerivers => {
				let mut result = BTreeSet::new();
				for arg in &args {
					let path = self.follow_links(arg)?;
					result.extend(self.valid_derivers(&path)?);
				}
				print_sorted(result, out)?;
			}
			Query::Binding => {
				for arg in &args {
					let mut path = self.follow_links(arg)?;
					if !path.name().ends_with(".drv") {
						path = self.info(&path)?.deriver.ok_or_else(|| {
							Failure::Error(format!(
								"deriver of path '{}' is not known",
								print(&path)
							))
						})?;
					}
					let drv = self.store.read_derivation(&path)?;
					let value = drv.environment().get(binding.as_bytes()).ok_or_else(|| {
						Failure::Error(format!(
							"derivation '{}' has no environment binding named '{binding}'",
							print(&path)
						))
					})?;
					out.write_all(value)?;
					out.write_all(b"\n")?;
				}
			}
			query @ (Query::Hash | Query::Size) => {
				for arg in &args {
					let path = self.follow_links(arg)?;
					for path in self.maybe_use_outputs(path, use_output, force_realise)? {
						let info = self.info(&path)?;
						if query == Query::Hash {
							writeln!(out, "sha256:{}", nixbase32::encode(&info.nar_hash))?;
						} else {
							writeln!(out, "{}", info.nar_size)?;
						}
					}
				}
			}
		}
		Ok(())
	}

	fn info(&self, path: &StorePath) -> Result<nix_rs_store::ValidPathInfo, Failure> {
		self.store
			.query_path_info(path)?
			.ok_or_else(|| Failure::Error(format!("path '{}' is not valid", print(path))))
	}

	fn referrers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>, Failure> {
		self.store
			.query_referrers(path)
			.map_err(|e| Failure::Error(e.to_string()))
	}

	fn valid_derivers(&self, path: &StorePath) -> Result<BTreeSet<StorePath>, Failure> {
		self.store
			.query_valid_derivers(path)
			.map_err(|e| Failure::Error(e.to_string()))
	}

	/// CppNix's `computeFSClosure`, following referrers when `flip`, and
	/// adding the valid outputs of derivations with `include_outputs`.
	fn closure(
		&self,
		start: &StorePath,
		flip: bool,
		include_outputs: bool,
		include_derivers: bool,
	) -> Result<BTreeSet<StorePath>, Failure> {
		let mut done = BTreeSet::new();
		let mut pending = vec![start.clone()];
		while let Some(path) = pending.pop() {
			if !done.insert(path.clone()) {
				continue;
			}
			let mut next: Vec<StorePath> = if flip {
				self.referrers(&path)?.into_iter().collect()
			} else {
				self.info(&path)?.references.into_iter().collect()
			};
			if include_outputs && !flip && path.name().ends_with(".drv") {
				for output in self.store.query_derivation_outputs(&path)?.into_values() {
					if self.store.is_valid_path(&output)? {
						next.push(output);
					}
				}
			}
			if include_outputs && flip {
				next.extend(self.valid_derivers(&path)?);
			}
			if include_derivers && flip && path.name().ends_with(".drv") {
				for output in self.store.query_derivation_outputs(&path)?.into_values() {
					if self.store.is_valid_path(&output)? {
						next.push(output);
					}
				}
			}
			pending.extend(next.into_iter().filter(|p| !done.contains(p)));
		}
		Ok(done)
	}
}

/// The output paths of a derivation from the database, or from the
/// derivation itself for outputs the database does not list.
fn output_paths_or_derivation(
	store: &dyn Store,
	drv_path: &StorePath,
	registered: BTreeMap<String, StorePath>,
) -> Result<BTreeMap<String, StorePath>, Failure> {
	let mut all = match output_paths(store, drv_path) {
		Ok(paths) => paths,
		Err(_) if !registered.is_empty() => BTreeMap::new(),
		Err(e) => return Err(e),
	};
	all.extend(registered);
	Ok(all)
}
