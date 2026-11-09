//! Modern profile manifests and content-addressed symlink environments.
use super::{Parsed, expression};
use crate::{cli::Failure, user_env::DrvName};
use nix_compat::{nixhash::HashAlgo, store_path::StorePath};
use noxi_store::{FileIngestionMethod, Store, profiles};
use serde_json::{Value, json};
use std::{
	collections::BTreeSet,
	fs,
	path::{Path, PathBuf},
	process::ExitCode,
};

fn manifest(store: &dyn Store, profile: &Path) -> Result<Value, Failure> {
	let mut link = profile.to_owned();
	for _ in 0..100 {
		if let Ok(path) = StorePath::from_absolute_path(link.to_string_lossy().as_bytes()) {
			return match fs::read(store.real_path(&path).join("manifest.json")) {
				Ok(bytes) => normalize_manifest(
					serde_json::from_slice(&bytes).map_err(|e| Failure::Error(e.to_string()))?,
					&link,
				),
				Err(e)
					if e.kind() == std::io::ErrorKind::NotFound
						&& fs::symlink_metadata(store.real_path(&path).join("manifest.nix"))
							.is_ok() =>
				{
					let settings = nix_compat::nixcpp::settings::Settings::default();
					let manifest_path = store.real_path(&path).join("manifest.nix");
					let target = fs::read_link(&manifest_path).unwrap_or(manifest_path.clone());
					let target = StorePath::from_absolute_path(target.to_string_lossy().as_bytes())
						.map(|p| store.real_path(&p))
						.unwrap_or(target);
					let elements = crate::user_env::query_manifest(
						&settings,
						&std::env::current_dir()?,
						&target,
					)?
					.map_err(|_| Failure::Error("cannot read legacy profile manifest".into()))?;
					let mut result = serde_json::Map::new();
					for element in elements {
						let name = DrvName::new(element.out_path.name()).name;
						let mut key = name.clone();
						let mut suffix = 1;
						while result.contains_key(&key) {
							key = format!("{name}-{suffix}");
							suffix += 1;
						}
						result.insert(
							key,
							json!({"active":true,"priority":5,"storePaths":[element.out_path.to_absolute_path()]}),
						);
					}
					Ok(json!({"version":3,"elements":result}))
				}
				Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
					Ok(json!({"version":3,"elements":{}}))
				}
				Err(e) => Err(e.into()),
			};
		}
		match fs::read_link(&link) {
			Ok(next) => {
				link = if next.is_absolute() {
					next
				} else {
					link.parent().unwrap().join(next)
				};
			}
			Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
				return Ok(json!({"version":3,"elements":{}}));
			}
			Err(e) => return Err(e.into()),
		}
	}
	Err(Failure::Error("too many symbolic links in profile".into()))
}

fn normalize_manifest(mut value: Value, profile: &Path) -> Result<Value, Failure> {
	let version = value["version"].as_u64().unwrap_or(0);
	if !(1..=3).contains(&version) {
		return Err(Failure::Error(format!(
			"profile manifest {} has unsupported version {version}",
			profile.join("manifest.json").display()
		)));
	}
	let mut elements = serde_json::Map::new();
	let old = value["elements"].take();
	let entries: Vec<_> = match old {
		Value::Object(o) => o.into_iter().map(|(k, v)| (Some(k), v)).collect(),
		Value::Array(a) => a.into_iter().map(|v| (None, v)).collect(),
		_ => return Err(Failure::Error("invalid profile manifest".into())),
	};
	for (name, mut e) in entries {
		if version == 1 {
			if let Some(v) = e.get("uri").cloned() {
				e["url"] = v;
			}
			if let Some(v) = e.get("originalUri").cloned() {
				e["originalUrl"] = v;
			}
			e.as_object_mut().unwrap().remove("uri");
			e.as_object_mut().unwrap().remove("originalUri");
		}
		if e.get("priority").is_none() {
			e["priority"] = json!(5);
		}
		if let Some(paths) = e["storePaths"].as_array_mut() {
			paths.sort_by(|a, b| a.as_str().cmp(&b.as_str()));
			paths.dedup();
		}
		let name = name.unwrap_or_else(|| {
			e["attrPath"]
				.as_str()
				.and_then(|s| s.rsplit('.').next())
				.map(str::to_owned)
				.unwrap_or_else(|| identifier(&e))
		});
		let mut key = name.clone();
		let mut suffix = 1;
		while elements.contains_key(&key) {
			key = format!("{name}-{suffix}");
			suffix += 1;
		}
		elements.insert(key, e);
	}
	value["elements"] = Value::Object(elements);
	value["version"] = json!(3);
	Ok(value)
}

fn build(store: &dyn Store, manifest: &Value, diagnose: bool) -> Result<StorePath, Failure> {
	let temp = tempfile::tempdir()?;
	let mut packages = Vec::new();
	let mut references = BTreeSet::new();
	for element in manifest["elements"]
		.as_object()
		.ok_or_else(|| Failure::Error("invalid profile manifest".into()))?
		.values()
	{
		for path in element["storePaths"]
			.as_array()
			.ok_or_else(|| Failure::Error("invalid profile store paths".into()))?
		{
			let path = path
				.as_str()
				.ok_or_else(|| Failure::Error("invalid profile store path".into()))?;
			let parsed =
				StorePath::from_absolute_path(path.as_bytes()).map_err(|e| e.to_string())?;
			store.ensure_path(&parsed)?;
			references.insert(parsed);
			packages.push(noxi_build::buildenv::Package {
				path: path.into(),
				active: element["active"].as_bool().unwrap_or(true),
				priority: element["priority"].as_i64().unwrap_or(5),
			});
		}
	}
	let real = |path: &Path| {
		StorePath::from_absolute_path(path.to_string_lossy().as_bytes())
			.map(|p| store.real_path(&p))
			.unwrap_or_else(|_| {
				let Some(rest) = path.strip_prefix("/nix/store").ok() else {
					return path.to_owned();
				};
				let mut parts = rest.components();
				let Some(first) = parts.next() else {
					return path.to_owned();
				};
				StorePath::from_bytes(first.as_os_str().as_encoded_bytes())
					.map(|p| store.real_path(&p).join(parts.as_path()))
					.unwrap_or_else(|_| path.to_owned())
			})
	};
	noxi_build::buildenv::build_profile(temp.path(), packages, &real, &mut |warning| {
		eprintln!("warning: {warning}")
	})
	.map_err(|e| {
		if diagnose {
			conflict_message(&e.0, manifest)
		} else {
			e.0
		}
	})?;
	fs::write(temp.path().join("manifest.json"), manifest.to_string())?;
	let mut nar = tempfile::tempfile()?;
	noxi_store::nar::dump_path(&mut nar, temp.path())?;
	use std::io::Seek;
	nar.rewind()?;
	Ok(store.add_to_store_from_dump(
		&mut nar,
		"profile",
		FileIngestionMethod::NixArchive,
		FileIngestionMethod::NixArchive,
		HashAlgo::Sha256,
		&references,
	)?)
}

fn conflict_message(error: &str, manifest: &Value) -> String {
	if !error.starts_with("Unable to build profile. There is a conflict for the following files:") {
		return error.into();
	}
	let files: Vec<String> = error
		.lines()
		.skip(1)
		.filter_map(|line| serde_json::from_str::<String>(line.trim()).ok())
		.collect();
	if files.len() != 2 {
		return error.into();
	}
	let entries = manifest["elements"].as_object().unwrap();
	let find = |name: &String, element: &Value| {
		element["storePaths"]
			.as_array()
			.into_iter()
			.flatten()
			.filter_map(|p| p.as_str())
			.find_map(|path| {
				files
					.iter()
					.find(|file| file.starts_with(&format!("{path}/")))
					.map(|file| (name.clone(), element.clone(), file.clone()))
			})
	};
	let Some((old_name, old, old_file)) = entries.iter().find_map(|(n, e)| find(n, e)) else {
		return error.into();
	};
	let Some((_, new, new_file)) = entries.iter().rev().find_map(|(n, e)| find(n, e)) else {
		return error.into();
	};
	let installable = if new.get("originalUrl").is_some() {
		identifier(&new)
	} else {
		new["storePaths"]
			.as_array()
			.unwrap()
			.iter()
			.filter_map(|p| p.as_str())
			.collect::<Vec<_>>()
			.join(" ")
	};
	let priority = old["priority"].as_i64().unwrap_or(5);
	format!(
		"An existing package already provides the following file:\n\n  \"{old_file}\"\n\nThis is the conflicting file from the new package:\n\n  \"{new_file}\"\n\nTo remove the existing package:\n\n  nix profile remove {old_name}\n\nThe new package can also be added next to the existing one by assigning a different priority.\nThe conflicting packages have a priority of {priority}.\nTo prioritise the new package:\n\n  nix profile add {installable} --priority {}\n\nTo prioritise the existing package:\n\n  nix profile add {installable} --priority {}\n",
		priority - 1,
		priority + 1
	)
}

fn single(parsed: &Parsed, operand: &str) -> Parsed {
	Parsed {
		command: parsed.command,
		settings: parsed.settings.clone(),
		flags: parsed.flags.clone(),
		sequence: parsed.sequence.clone(),
		operands: vec![operand.to_owned()],
	}
}

fn output_suffix(element: &Value) -> String {
	element["outputs"]
		.as_array()
		.map(|names| {
			format!(
				"^{}",
				names
					.iter()
					.filter_map(|n| n.as_str())
					.collect::<Vec<_>>()
					.join(",")
			)
		})
		.unwrap_or_default()
}

fn identifier(element: &Value) -> String {
	if let Some(url) = element["originalUrl"].as_str() {
		format!(
			"{url}#{}{}",
			element["attrPath"].as_str().unwrap_or(""),
			output_suffix(element)
		)
	} else {
		element["storePaths"]
			.as_array()
			.into_iter()
			.flatten()
			.filter_map(|p| p.as_str())
			.filter_map(|p| StorePath::from_absolute_path(p.as_bytes()).ok())
			.map(|p| DrvName::new(p.name()).name)
			.collect::<BTreeSet<_>>()
			.into_iter()
			.collect::<Vec<_>>()
			.join(", ")
	}
}

fn matching(
	parsed: &Parsed,
	elements: &serde_json::Map<String, Value>,
) -> Result<BTreeSet<String>, Failure> {
	let patterns = parsed.flags.get("regex").cloned().unwrap_or_default();
	if parsed.has("all") && (!parsed.operands.is_empty() || !patterns.is_empty()) {
		return Err(Failure::Usage(
			"--all cannot be used with package names or regular expressions.".into(),
		));
	}
	if !parsed.has("all") && parsed.operands.is_empty() && patterns.is_empty() {
		return Err(Failure::Usage("No packages specified.".into()));
	}
	if elements.is_empty() {
		eprintln!("warning: There are no packages in the profile.");
		return Ok(BTreeSet::new());
	}
	let mut names = BTreeSet::new();
	for operand in &parsed.operands {
		if let Ok(index) = operand.parse::<usize>() {
			return Err(Failure::Error(format!(
				"'nix profile' no longer supports indices ('{index}')"
			)));
		}
		let path = StorePath::from_absolute_path(operand.as_bytes()).is_ok();
		let mut found = false;
		for (name, e) in elements {
			if if path {
				e["storePaths"]
					.as_array()
					.is_some_and(|p| p.contains(&json!(operand)))
			} else {
				name == operand
			} {
				names.insert(name.clone());
				found = true;
			}
		}
		if !found {
			eprintln!(
				"warning: {} '{operand}' does not match any packages in the profile.",
				if path { "Store path" } else { "Package name" }
			);
		}
	}
	for pattern in patterns {
		let pattern = &pattern[0];
		let re = regex::RegexBuilder::new(&format!("^(?:{pattern})$"))
			.case_insensitive(true)
			.build()
			.map_err(|e| Failure::Error(e.to_string()))?;
		let mut found = false;
		for (name, e) in elements {
			if re.is_match(&identifier(e)) {
				names.insert(name.clone());
				found = true;
			}
		}
		if !found {
			eprintln!("warning: Regex '{pattern}' does not match any packages in the profile.");
		}
	}
	if parsed.has("all") {
		names.extend(elements.keys().cloned());
	}
	Ok(names)
}

fn source_info(parsed: &Parsed, operand: &str) -> Result<Result<Value, ExitCode>, Failure> {
	let (reference, attr) = operand.split_once('#').unwrap_or((operand, ""));
	let reference = if reference.is_empty() { "." } else { reference };
	let mut options = expression::options(&single(parsed, operand), false)?;
	options.eval = true;
	let system = parsed.settings.get("system").unwrap_or("x86_64-linux");
	let selected = if attr.is_empty() { "default" } else { attr };
	let candidates = [
		format!("packages.{system}.{selected}"),
		format!("legacyPackages.{system}.{selected}"),
		selected.to_owned(),
	];
	let mut selection = "throw \"flake attribute missing\"".to_owned();
	for candidate in candidates.iter().rev() {
		let path = crate::instantiate::parse_attr_path(candidate)
			.map_err(Failure::Usage)?
			.iter()
			.map(|a| crate::instantiate::nix_string_literal(a))
			.collect::<Vec<_>>()
			.join(".");
		selection = format!(
			"if f ? {path} then {} else ({selection})",
			crate::instantiate::nix_string_literal(candidate)
		);
	}
	options.inputs = vec![format!(
		"let f = __noxi_flake {}; in {selection}",
		crate::instantiate::nix_string_literal(reference)
	)];
	let evaluated = match crate::instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(v) => v,
		Err(c) => return Ok(Err(c)),
	};
	let attr = evaluated.results[0].to_str().map_err(|e| e.to_string())?;
	let attr = String::from_utf8_lossy(attr.as_bytes()).into_owned();
	options
		.flake
		.as_mut()
		.ok_or_else(|| Failure::Error("flake source missing".into()))?
		.operation = "metadata".into();
	options.inputs = vec![format!(
		"__noxi_flake {}",
		crate::instantiate::nix_string_literal(reference)
	)];
	let evaluated = match crate::instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(v) => v,
		Err(c) => return Ok(Err(c)),
	};
	let metadata = evaluated.results[0].to_str().map_err(|e| e.to_string())?;
	let metadata: Value = serde_json::from_slice(metadata.as_bytes()).map_err(|e| e.to_string())?;
	Ok(Ok(
		json!({"originalUrl":metadata["originalUrl"],"url":metadata["url"],"attrPath":attr,"outputs":null}),
	))
}

fn add_element(
	parsed: &Parsed,
	operand: &str,
) -> Result<Result<(String, Value), ExitCode>, Failure> {
	let (operand, output_spec) = operand
		.split_once('^')
		.map_or((operand, None), |(a, b)| (a, Some(b)));
	let source =
		if !parsed.has("expr") && !parsed.has("file") && !operand.starts_with("/nix/store/") {
			Some(match source_info(parsed, operand)? {
				Ok(v) => v,
				Err(c) => return Ok(Err(c)),
			})
		} else {
			None
		};
	let locked_operand = source.as_ref().map(|source| {
		format!(
			"{}#{}",
			source["url"].as_str().unwrap_or(""),
			source["attrPath"].as_str().unwrap_or("")
		)
	});
	let target_operand = locked_operand.as_deref().unwrap_or(operand);
	let mut targets = match expression::targets(&single(parsed, target_operand))? {
		Ok(v) => v,
		Err(c) => return Ok(Err(c)),
	};
	if output_spec.is_none()
		&& !parsed.has("expr")
		&& !parsed.has("file")
		&& !operand.starts_with("/nix/store/")
	{
		let mut options = expression::options(&single(parsed, target_operand), false)?;
		let root = options.inputs.first().cloned().unwrap_or_default();
		options.eval = true;
		options.inputs = vec![format!(
			"let d = ({root}); in d.meta.outputsToInstall or [ (d.outputName or \"out\") ]"
		)];
		let evaluated = match crate::instantiate::evaluate(&options, &std::env::current_dir()?) {
			Ok(v) => v,
			Err(c) => return Ok(Err(c)),
		};
		let outputs = evaluated.results[0]
			.to_list()
			.map_err(|e| e.to_string())?
			.iter()
			.map(|v| {
				v.to_str()
					.map(|s| String::from_utf8_lossy(s.as_bytes()).into_owned())
					.map_err(|e| Failure::Error(e.to_string()))
			})
			.collect::<Result<BTreeSet<_>, _>>()?;
		for path in &mut targets.paths {
			if let noxi_store::DerivedPath::Built {
				outputs: selected, ..
			} = path
			{
				*selected = outputs.clone();
			}
		}
	}
	if let Some(spec) = output_spec {
		for path in &mut targets.paths {
			if let noxi_store::DerivedPath::Built { outputs, .. } = path {
				*outputs = if spec == "*" {
					BTreeSet::new()
				} else {
					spec.split(',').map(str::to_owned).collect()
				};
			}
		}
	}
	let paths = targets
		.paths
		.iter()
		.map(|target| {
			expression::outputs(targets.store.as_ref(), target)
				.map(|o| o.into_values().collect::<Vec<_>>())
		})
		.collect::<Result<Vec<_>, _>>()?
		.into_iter()
		.flatten()
		.collect::<BTreeSet<_>>();
	let mut element = json!({"active":true,"priority":parsed.value("priority").map(|s| s.parse::<i64>().map_err(|e| Failure::Usage(e.to_string()))).transpose()?.unwrap_or(5),"storePaths":paths.iter().map(|p| p.to_absolute_path()).collect::<Vec<_>>()});
	if !parsed.has("priority") && !operand.starts_with("/nix/store/") {
		let mut options = expression::options(&single(parsed, target_operand), false)?;
		let root = options.inputs.first().cloned().unwrap_or_default();
		options.eval = true;
		options.inputs = vec![format!("({root}).meta.priority or 5")];
		let evaluated = match crate::instantiate::evaluate(&options, &std::env::current_dir()?) {
			Ok(v) => v,
			Err(c) => return Ok(Err(c)),
		};
		if let noxi_eval::Value::Integer(priority) = evaluated.results[0] {
			element["priority"] = json!(priority);
		}
	}
	let mut name = identifier(&element);
	if !parsed.has("expr") && !parsed.has("file") && !operand.starts_with("/nix/store/") {
		let source = source
			.as_ref()
			.expect("flake source was resolved before evaluation");
		for (k, v) in source.as_object().unwrap() {
			element[k] = v.clone();
		}
		if let Some(spec) = output_spec {
			element["outputs"] = json!(spec.split(',').collect::<BTreeSet<_>>());
		}
		// URL fragments determine the package name, falling back to the source basename for default outputs.
		let attr = operand.split_once('#').map(|(_, a)| a).unwrap_or("");
		name = if !attr.is_empty() {
			attr.rsplit('.')
				.next()
				.unwrap_or(attr)
				.trim_matches('"')
				.into()
		} else {
			operand
				.trim_end_matches('/')
				.rsplit('/')
				.next()
				.unwrap_or(operand)
				.trim_start_matches("path:")
				.into()
		};
	}
	if parsed.command != "profile upgrade" {
		crate::paths::print_missing(
			targets.store.as_ref(),
			&targets.store.query_missing(&targets.paths)?,
		)?;
		if !parsed.has("dry-run") {
			targets.store.build_paths(&targets.paths)?;
		}
	}
	Ok(Ok((name, element)))
}

fn versions(element: &Value) -> String {
	let versions: BTreeSet<_> = element["storePaths"]
		.as_array()
		.into_iter()
		.flatten()
		.filter_map(|p| p.as_str())
		.filter_map(|p| StorePath::from_absolute_path(p.as_bytes()).ok())
		.map(|p| DrvName::new(p.name()).version)
		.collect();
	if versions.len() == 1 && versions.contains("") {
		"ε".into()
	} else {
		versions
			.into_iter()
			.map(|s| if s.is_empty() { "ε".into() } else { s })
			.collect::<Vec<_>>()
			.join(", ")
	}
}
fn history(store: &dyn Store, profile: &Path) -> Result<(), Failure> {
	let (generations, current) = profiles::find_generations(profile)?;
	let mut previous = json!({"version":3,"elements":{}});
	let mut number = None;
	for generation in generations {
		let next = manifest(store, &generation.path)?;
		if number.is_some() {
			println!();
		}
		let time = generation.creation_time as libc::time_t;
		let mut tm: libc::tm = unsafe { std::mem::zeroed() };
		// SAFETY: both pointers reference live values for the duration of gmtime_r.
		unsafe {
			libc::gmtime_r(&time, &mut tm);
		}
		println!(
			"Version {}{}\x1b[0m ({:04}-{:02}-{:02}){}:",
			if Some(generation.number) == current {
				"\x1b[32;1m"
			} else {
				"\x1b[1m"
			},
			generation.number,
			tm.tm_year + 1900,
			tm.tm_mon + 1,
			tm.tm_mday,
			number.map(|n| format!(" <- {n}")).unwrap_or_default()
		);
		let old = previous["elements"].as_object().unwrap();
		let new = next["elements"].as_object().unwrap();
		let names: BTreeSet<_> = old.keys().chain(new.keys()).collect();
		let mut changes = false;
		for name in names {
			let (id, before, after) = match (old.get(name), new.get(name)) {
				(None, Some(e)) => (identifier(e), "∅".into(), versions(e)),
				(Some(e), None) => (identifier(e), versions(e), "∅".into()),
				(Some(a), Some(b)) => (identifier(a), versions(a), versions(b)),
				_ => unreachable!(),
			};
			if before != after {
				println!("  {id}: {before} -> {after}");
				changes = true;
			}
		}
		if !changes {
			println!("  No changes.");
		}
		number = Some(generation.number);
		previous = next;
	}
	Ok(())
}

fn diff_closures(store: &dyn Store, profile: &Path) -> Result<(), Failure> {
	type Groups = std::collections::BTreeMap<String, std::collections::BTreeMap<String, u64>>;
	fn closure(store: &dyn Store, profile: &Path) -> Result<Groups, Failure> {
		let mut path = profile.to_owned();
		let root = loop {
			if let Ok(p) = StorePath::from_absolute_path(path.to_string_lossy().as_bytes()) {
				break p;
			}
			let next = fs::read_link(&path)?;
			path = if next.is_absolute() {
				next
			} else {
				path.parent().unwrap().join(next)
			};
		};
		let mut seen = BTreeSet::new();
		let mut pending = vec![root];
		let mut result = Groups::new();
		let output = regex::Regex::new("^(.*)-([a-z]+|lib32|lib64)$").unwrap();
		while let Some(path) = pending.pop() {
			if !seen.insert(path.clone()) {
				continue;
			}
			let info = store.query_path_info(&path)?.ok_or_else(|| {
				Failure::Error(format!("path '{}' is not valid", path.to_absolute_path()))
			})?;
			let name = output
				.captures(path.name())
				.map(|c| c[1].to_owned())
				.unwrap_or_else(|| path.name().into());
			let name = DrvName::new(&name);
			*result
				.entry(name.name)
				.or_default()
				.entry(name.version)
				.or_default() += info.nar_size;
			pending.extend(info.references);
		}
		Ok(result)
	}
	let (generations, _) = profiles::find_generations(profile)?;
	let first_generation = generations.first().map_or(0, |g| g.number);
	let mut previous: Option<(u64, Groups)> = None;
	for generation in generations {
		let next = closure(store, &generation.path)?;
		if let Some((number, old)) = previous {
			if number != first_generation {
				println!();
			}
			println!("Version {number} -> {}:", generation.number);
			let names: BTreeSet<_> = old.keys().chain(next.keys()).collect();
			for name in names {
				let empty = std::collections::BTreeMap::new();
				let a = old.get(name).unwrap_or(&empty);
				let b = next.get(name).unwrap_or(&empty);
				let removed: Vec<_> = a.keys().filter(|v| !b.contains_key(*v)).cloned().collect();
				let added: Vec<_> = b.keys().filter(|v| !a.contains_key(*v)).cloned().collect();
				let delta = b.values().sum::<u64>() as i64 - a.values().sum::<u64>() as i64;
				let mut items = Vec::new();
				let show = |v: Vec<String>| {
					if v.is_empty() {
						"∅".into()
					} else {
						v.into_iter()
							.map(|v| if v.is_empty() { "ε".into() } else { v })
							.collect::<BTreeSet<_>>()
							.into_iter()
							.collect::<Vec<_>>()
							.join(", ")
					}
				};
				if !removed.is_empty() || !added.is_empty() {
					items.push(format!("{} → {}", show(removed), show(added)));
				}
				if delta.unsigned_abs() >= 8192 {
					items.push(format!(
						"{}{}\x1b[0m",
						if delta > 0 {
							"\x1b[31;1m"
						} else {
							"\x1b[32;1m"
						},
						noxi_store::gc::render_size(delta, false)
					));
				}
				if !items.is_empty() {
					println!("  {name}: {}", items.join(", "));
				}
			}
		}
		previous = Some((generation.number, next));
	}
	Ok(())
}
pub(super) fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	let opened = crate::store::open(&uri, &parsed.settings)?
		.ok_or_else(|| Failure::Error("profile requires a writable store".into()))?;
	let store = opened.store.as_ref();
	let profile = parsed
		.value("profile")
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			profiles::default_profile(
				&noxi_store::LocalStoreConfig::system().state_dir,
				parsed.settings.get_bool("use-xdg-base-directories", false),
			)
		});
	let profile = crate::instantiate::absolute_path(&std::env::current_dir()?, &profile);
	if parsed.command == "profile rollback" {
		let to = parsed
			.value("to")
			.map(|v| {
				v.parse::<u64>()
					.map_err(|_| Failure::Usage(format!("invalid integer '{v}'")))
			})
			.transpose()?;
		profiles::switch_generation(&profile, to, parsed.has("dry-run"), &mut |s| {
			eprintln!("{s}")
		})?;
		return Ok(ExitCode::SUCCESS);
	}
	if parsed.command == "profile wipe-history" {
		let dry = parsed.has("dry-run");
		if let Some(age) = parsed.value("older-than") {
			let now = std::time::SystemTime::now()
				.duration_since(std::time::UNIX_EPOCH)
				.map(|d| d.as_secs() as i64)
				.unwrap_or(0);
			let time = profiles::parse_older_than_time_spec(age, now).map_err(Failure::Usage)?;
			profiles::delete_generations_older_than(&profile, time, dry, &mut |s| {
				eprintln!("{s}")
			})?;
		} else {
			profiles::delete_old_generations(&profile, dry, &mut |s| eprintln!("{s}"))?;
		}
		return Ok(ExitCode::SUCCESS);
	}
	if parsed.command == "profile diff-closures" {
		diff_closures(store, &profile)?;
		return Ok(ExitCode::SUCCESS);
	}
	if parsed.command == "profile history" {
		history(store, &profile)?;
		return Ok(ExitCode::SUCCESS);
	}
	let token = crate::user_env::lock_token(&profile);
	let mut current = manifest(store, &profile)?;
	if parsed.command == "profile list" {
		if parsed.has("json") {
			if parsed.has("pretty") && !parsed.has("no-pretty") {
				println!(
					"{}",
					serde_json::to_string_pretty(&current).map_err(|e| e.to_string())?
				);
			} else {
				println!("{current}");
			}
		} else {
			for (i, (name, element)) in current["elements"]
				.as_object()
				.ok_or_else(|| Failure::Error("invalid manifest".into()))?
				.iter()
				.enumerate()
			{
				if i > 0 {
					println!();
				}
				println!(
					"Name:               \x1b[1m{name}\x1b[0m{}",
					if element["active"] == false {
						"  \x1b[31m(inactive)\x1b[0m"
					} else {
						""
					}
				);
				if element.get("originalUrl").is_some() {
					println!(
						"Flake attribute:    {}{}",
						element["attrPath"].as_str().unwrap_or(""),
						output_suffix(element)
					);
					println!(
						"Original flake URL: {}",
						element["originalUrl"].as_str().unwrap_or("")
					);
					println!(
						"Locked flake URL:   {}",
						element["url"].as_str().unwrap_or("")
					);
				}
				println!(
					"Store paths:        {}",
					element["storePaths"]
						.as_array()
						.unwrap()
						.iter()
						.map(|s| s.as_str().unwrap())
						.collect::<Vec<_>>()
						.join(" ")
				);
			}
		}
		return Ok(ExitCode::SUCCESS);
	}
	let elements = current["elements"]
		.as_object_mut()
		.ok_or_else(|| Failure::Error("invalid profile manifest".into()))?;
	if parsed.command == "profile remove" {
		let names = matching(&parsed, elements)?;
		if names.is_empty() {
			eprintln!(
				"warning: No packages to remove. Use 'nix profile list' to see the current profile."
			);
			return Ok(ExitCode::SUCCESS);
		}
		let count = names.len();
		for name in names {
			eprintln!("removing '{}'", identifier(&elements[&name]));
			elements.remove(&name);
		}
		eprintln!("removed {count} packages, kept {} packages", elements.len());
	} else if parsed.command == "profile add" {
		let operands = if parsed.operands.is_empty() {
			vec![if parsed.has("expr") || parsed.has("file") {
				"".to_owned()
			} else {
				".".to_owned()
			}]
		} else {
			parsed.operands.clone()
		};
		for operand in operands {
			let (name, element) = match add_element(&parsed, &operand)? {
				Ok(v) => v,
				Err(c) => return Ok(c),
			};
			if elements.values().any(|e| {
				e.get("originalUrl").is_some()
					&& e["originalUrl"] == element["originalUrl"]
					&& e["attrPath"] == element["attrPath"]
					&& e["priority"] == element["priority"]
			}) {
				eprintln!("warning: '{name}' is already added");
				continue;
			}
			let mut key = name.clone();
			let mut suffix = 1;
			while elements.contains_key(&key) {
				key = format!("{name}-{suffix}");
				suffix += 1;
			}
			elements.insert(key, element);
		}
	} else if parsed.command == "profile upgrade" {
		let names = matching(&parsed, elements)?;
		let mut count = 0;
		for name in names {
			let old = &elements[&name];
			let Some(url) = old["originalUrl"].as_str() else {
				eprintln!(
					"warning: Found package '{}', but it was not added from a flake, so it can't be checked for upgrades!",
					identifier(old)
				);
				continue;
			};
			if url.contains("narHash=") || url.contains("rev=") {
				eprintln!(
					"warning: Found package '{}', but it was added from a locked flake reference so it can't be upgraded!",
					identifier(old)
				);
				continue;
			}
			count += 1;
			let operand = format!(
				"{url}#{}{}",
				old["attrPath"].as_str().unwrap_or(""),
				output_suffix(old)
			);
			let (_, mut element) = match add_element(&parsed, &operand)? {
				Ok(v) => v,
				Err(c) => return Ok(c),
			};
			element["priority"] = old["priority"].clone();
			element["active"] = old["active"].clone();
			if old["url"] != element["url"] {
				eprintln!(
					"upgrading '{}' from flake '{}' to '{}'",
					old["attrPath"].as_str().unwrap_or(""),
					old["url"].as_str().unwrap_or(""),
					element["url"].as_str().unwrap_or("")
				);
			}
			if !parsed.has("dry-run") {
				let target_operand = format!(
					"{}#{}",
					element["url"].as_str().unwrap_or(""),
					element["attrPath"].as_str().unwrap_or("")
				);
				let mut targets = match expression::targets(&single(&parsed, &target_operand))? {
					Ok(v) => v,
					Err(c) => return Ok(c),
				};
				for path in &mut targets.paths {
					if let noxi_store::DerivedPath::Built { drv, outputs } = path {
						*outputs = targets
							.store
							.query_derivation_outputs(drv)?
							.into_iter()
							.filter(|(_, p)| {
								element["storePaths"].as_array().is_some_and(|paths| {
									paths.contains(&json!(p.to_absolute_path()))
								})
							})
							.map(|(name, _)| name)
							.collect();
					}
				}
				crate::paths::print_missing(
					targets.store.as_ref(),
					&targets.store.query_missing(&targets.paths)?,
				)?;
				targets.store.build_paths(&targets.paths)?;
			}
			elements.insert(name, element);
		}
		if count == 0 {
			eprintln!("warning: Found some packages but none of them could be upgraded.");
			return Ok(ExitCode::SUCCESS);
		}
	} else {
		return Err(super::unsupported(parsed.command));
	}
	if parsed.has("dry-run") {
		return Ok(ExitCode::SUCCESS);
	}
	let output = build(store, &current, parsed.command == "profile add")?;
	fs::create_dir_all(profile.parent().unwrap())?;
	let _lock = profiles::lock_profile(&profile)?;
	if crate::user_env::lock_token(&profile) != token {
		drop(_lock);
		return run(parsed);
	}
	let generation = profiles::create_generation(store, &profile, &output)?;
	profiles::switch_link(&profile, &generation)?;
	Ok(ExitCode::SUCCESS)
}
