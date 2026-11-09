//! Registry commands share the fetcher's parser, resolution and JSON codec.
use super::Parsed;
use crate::cli::Failure;
use noxi_flakeref::{
	flake_ref::{FlakeRef, ParseOptions, parse_flake_ref},
	registry::{Registry, RegistryType, UseRegistries, lookup_in_registries, user_registry_path},
};
use std::{fs, io::Write, path::PathBuf, process::ExitCode};

pub(super) fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	let store = noxi_glue::EvaluatorStore::new();
	let mut config = noxi_glue::fetch::FetchConfig {
		cwd: std::env::current_dir()?,
		..Default::default()
	};
	config.settings.flakes = parsed.settings.experimental_features().contains("flakes");
	config.settings.flake_registry = parsed
		.settings
		.get("flake-registry")
		.unwrap_or("https://channels.nixos.org/flake-registry.json")
		.into();
	store.configure_fetching(config);
	let settings = store.fetch_settings();
	let parse = |s: &str| {
		parse_flake_ref(&settings, s, ParseOptions::default())
			.map_err(|e| Failure::Error(e.to_string()))
	};
	if matches!(parsed.command, "registry list" | "registry resolve") {
		let registries = store.registries().map_err(|e| e.to_string())?;
		if parsed.command == "registry list" {
			for registry in registries.iter() {
				let label = match registry.registry_type {
					RegistryType::Flag => "flags ",
					RegistryType::User => "user  ",
					RegistryType::System => "system",
					_ => "global",
				};
				for entry in &registry.entries {
					let subdir =
						noxi_flakeref::input::maybe_get_str_attr(&entry.extra_attrs, "dir")
							.map_err(|e| e.to_string())?
							.unwrap_or_default();
					println!(
						"{label} {} {}",
						entry.from.display(),
						FlakeRef::new(entry.to.clone(), subdir)
					);
				}
			}
		} else {
			for operand in &parsed.operands {
				let reference = parse(operand)?;
				let (input, extra) =
					lookup_in_registries(&registries, &reference.input, UseRegistries::All)
						.map_err(|e| e.to_string())?;
				let subdir = if reference.subdir.is_empty() {
					noxi_flakeref::input::maybe_get_str_attr(&extra, "dir")
						.map_err(|e| e.to_string())?
						.unwrap_or_default()
				} else {
					reference.subdir
				};
				println!("{}", FlakeRef::new(input, subdir));
			}
		}
		return Ok(ExitCode::SUCCESS);
	}
	let path = parsed
		.value("registry")
		.map(PathBuf::from)
		.unwrap_or_else(user_registry_path);
	let mut registry = Registry::read(&settings, &path, RegistryType::User);
	let from = parse(&parsed.operands[0])?;
	registry.remove(&from.input);
	if parsed.command != "registry remove" {
		let to = parse(parsed.operands.get(1).unwrap_or(&parsed.operands[0]))?;
		let to = if parsed.command == "registry pin" {
			noxi_glue::fetch::flake::fetch_source(&store, &to, UseRegistries::All)
				.map_err(|e| e.to_string())?
				.fetched_ref
		} else {
			to
		};
		if parsed.command == "registry pin" && !to.input.is_locked(&settings) {
			eprintln!("warning: flake '{to}' is not locked");
		}
		let mut extra = Default::default();
		let subdir = if parsed.command == "registry pin" {
			&from.subdir
		} else {
			&to.subdir
		};
		if !subdir.is_empty() {
			extra = [(
				"dir".into(),
				noxi_flakeref::input::Attr::String(subdir.clone()),
			)]
			.into();
		}
		registry.add(from.input, to.input, extra);
	}
	let parent = path
		.parent()
		.filter(|p| !p.as_os_str().is_empty())
		.unwrap_or(std::path::Path::new("."));
	fs::create_dir_all(parent)?;
	let mut file = tempfile::NamedTempFile::new_in(parent)?;
	file.write_all(
		registry
			.to_json_string()
			.map_err(|e| e.to_string())?
			.replace("\"flakes\": []", "\"flakes\": null")
			.as_bytes(),
	)?;
	file.persist(path).map_err(|e| e.error)?;
	Ok(ExitCode::SUCCESS)
}
