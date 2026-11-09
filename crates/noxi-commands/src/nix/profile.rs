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
				Ok(bytes) => {
					serde_json::from_slice(&bytes).map_err(|e| Failure::Error(e.to_string()))
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

fn build(store: &dyn Store, manifest: &Value) -> Result<StorePath, Failure> {
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
	.map_err(|e| e.0)?;
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
	if matches!(parsed.command, "profile rollback" | "profile wipe-history") {
		let operation = match parsed.command {
			"profile rollback" => "--rollback",
			"profile wipe-history" => "--delete-generations",
			_ => "--list-generations",
		};
		let mut args = vec![
			"--profile".into(),
			profile.to_string_lossy().into_owned(),
			operation.into(),
		];
		if parsed.command == "profile wipe-history" {
			args.push(
				parsed
					.value("older-than")
					.map(str::to_owned)
					.unwrap_or_else(|| "old".into()),
			);
		}
		if parsed.has("dry-run") {
			args.push("--dry-run".into());
		}
		return crate::nix_env::main(parsed.settings, args);
	}
	let mut current = manifest(store, &profile)?;
	if parsed.command == "profile list" {
		if parsed.has("json") {
			println!("{current}");
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
		let names: Vec<_> = elements
			.keys()
			.filter(|name| parsed.has("all") || parsed.operands.contains(name))
			.cloned()
			.collect();
		if names.is_empty() {
			eprintln!(
				"warning: No packages to remove. Use 'nix profile list' to see the current profile."
			);
			return Ok(ExitCode::SUCCESS);
		}
		let count = names.len();
		for name in names {
			eprintln!("removing '{name}'");
			elements.remove(&name);
		}
		eprintln!("removed {count} packages, kept {} packages", elements.len());
	} else if parsed.command == "profile add" {
		if !parsed.has("expr")
			&& !parsed.has("file")
			&& parsed
				.operands
				.iter()
				.any(|operand| !operand.starts_with("/nix/store/"))
		{
			return Err(super::unsupported("profile add flake source tracking"));
		}
		let targets = match expression::targets(&parsed)? {
			Ok(v) => v,
			Err(code) => return Ok(code),
		};
		crate::paths::print_missing(
			targets.store.as_ref(),
			&targets.store.query_missing(&targets.paths)?,
		)?;
		targets.store.build_paths(&targets.paths)?;
		for target in &targets.paths {
			let paths: Vec<_> = expression::outputs(targets.store.as_ref(), target)?
				.into_values()
				.collect();
			let name = paths
				.iter()
				.map(|p| DrvName::new(p.name()).name)
				.collect::<BTreeSet<_>>()
				.into_iter()
				.collect::<Vec<_>>()
				.join(", ");
			let mut key = name.clone();
			let mut suffix = 1;
			while elements.contains_key(&key) {
				key = format!("{name}-{suffix}");
				suffix += 1;
			}
			elements.insert(key, json!({"active":true,"priority":parsed.value("priority").and_then(|s| s.parse::<i64>().ok()).unwrap_or(5),"storePaths":paths.iter().map(|p| p.to_absolute_path()).collect::<Vec<_>>()}));
		}
	} else {
		return Err(super::unsupported(parsed.command));
	}
	if parsed.has("dry-run") {
		return Ok(ExitCode::SUCCESS);
	}
	let output = build(store, &current)?;
	fs::create_dir_all(profile.parent().unwrap())?;
	let _lock = profiles::lock_profile(&profile)?;
	let generation = profiles::create_generation(store, &profile, &output)?;
	profiles::switch_link(&profile, &generation)?;
	Ok(ExitCode::SUCCESS)
}
