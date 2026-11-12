//! Upgrade a legacy Nix profile using the native downloader and profile installer.
use super::Parsed;
use crate::{
	cli::Failure,
	instantiate::{self, Options},
	user_env::DrvName,
};
use bstr::ByteSlice;
use nix_compat::store_path::StorePath;
use noxi_glue::fetch::http::{Client, Native, Request};
use std::{
	fs,
	path::{Path, PathBuf},
	process::{Command, ExitCode},
};
fn info(text: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
		eprintln!("{text}");
	}
}
pub fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	let profile = match parsed.value("profile") {
		Some(profile) => PathBuf::from(profile),
		None => discover_profile(&parsed)?,
	};
	info(&format!("upgrading Nix in profile {:?}", profile));
	info("querying latest Nix version...");
	let url = parsed.value("nix-store-paths-url").or_else(|| parsed.settings.get("upgrade-nix-store-path-url")).unwrap_or("https://github.com/NixOS/nixpkgs/raw/master/nixos/modules/installer/tools/nix-fallback-paths.nix");
	let url = url::Url::parse(url).map_err(|e| e.to_string())?;
	let bytes = if url.scheme() == "file" {
		fs::read(
			url.to_file_path()
				.map_err(|_| "invalid file URL".to_owned())?,
		)?
	} else {
		let response = Native
			.request(&Request::get(url.as_str()))
			.map_err(|e| e.to_string())?;
		if !(200..300).contains(&response.status) {
			return Err(Failure::Error(
				noxi_glue::fetch::http::download_error(url.as_str(), &response).to_string(),
			));
		}
		response.body
	};
	let body = String::from_utf8(bytes).map_err(|e| e.to_string())?;
	let system = parsed.settings.get("system").unwrap_or("x86_64-linux");
	let options = Options {
		settings: parsed.settings.clone(),
		store: parsed.value("store").map(str::to_owned),
		eval: true,
		modern_pure: true,
		from_args: true,
		strict: true,
		inputs: vec![format!(
			"({body}).{}",
			crate::instantiate::nix_string_literal(system)
		)],
		..Options::default()
	};
	let evaluated = match instantiate::evaluate(&options, Path::new("/no-such-path")) {
		Ok(value) => value,
		Err(code) => return Ok(code),
	};
	let path = evaluated.results[0]
		.to_contextful_str()
		.map_err(|e| e.to_string())?
		.to_str()
		.map_err(|e| e.to_string())?
		.to_owned();
	let path = StorePath::from_absolute_path(path.as_bytes()).map_err(|e| e.to_string())?;
	let version = DrvName::new(path.name()).version;
	if parsed.has("dry-run") {
		super::warn(&format!("would upgrade to version {version}"));
		return Ok(ExitCode::SUCCESS);
	}
	let opened = super::command_store::open(&parsed)?;
	info(&format!("downloading '{}'...", path.to_absolute_path()));
	opened.store.ensure_path(&path)?;
	info(&format!(
		"verifying that '{}' works...",
		path.to_absolute_path()
	));
	let logical = format!("{}/bin/nix-env", path.to_absolute_path());
	let executable = opened.store.real_path(&path).join("bin/nix-env");
	let output = Command::new(&executable).arg("--version").output()?;
	if !output.status.success() || !String::from_utf8_lossy(&output.stdout).contains("Nix") {
		return Err(Failure::Error(format!(
			"could not verify that '{logical}' works"
		)));
	}
	info(&format!(
		"installing '{}' into profile {:?}...",
		path.to_absolute_path(),
		profile
	));
	let mut args = vec![
		"--profile".into(),
		profile.to_string_lossy().into_owned(),
		"-i".into(),
		path.to_absolute_path(),
		"--no-sandbox".into(),
	];
	if let Some(store) = parsed.value("store") {
		args.extend(["--store".into(), store.into()]);
	}
	let code = crate::nix_env::main(parsed.settings, args)?;
	if code == ExitCode::SUCCESS {
		info(&format!(
			"\x1b[32;1mupgrade to version {version} done\x1b[0m"
		));
	}
	Ok(code)
}
fn discover_profile(parsed: &Parsed) -> Result<PathBuf, Failure> {
	let path = std::env::var_os("PATH").unwrap_or_default();
	let executable = std::env::split_paths(&path)
		.map(|dir| dir.join("nix-env"))
		.find(|file| file.is_file())
		.ok_or_else(|| {
			Failure::Error("couldn't figure out how Nix is installed, so I can't upgrade it".into())
		})?;
	let bin = executable.parent().unwrap();
	info(&format!("found Nix in {:?}", bin));
	if bin.starts_with("/run/current-system") {
		return Err(Failure::Error(
			"Nix on NixOS must be upgraded via 'nixos-rebuild'".into(),
		));
	}
	let profile = bin.parent().unwrap().to_path_buf();
	let mut resolved = profile.clone();
	for _ in 0..64 {
		if resolved.components().any(|c| c.as_os_str() == "profiles") || !resolved.is_symlink() {
			break;
		}
		resolved = resolved.parent().unwrap().join(fs::read_link(&resolved)?);
	}
	info(&format!("found profile {:?}", resolved));
	if profile.join("manifest.json").exists() {
		return Err(Failure::Error(format!(
			"directory {:?} is managed by 'nix profile' and currently cannot be upgraded by 'nix upgrade-nix'",
			profile
		)));
	}
	if !profile.join("manifest.nix").exists() {
		return Err(Failure::Error(format!(
			"directory {:?} does not appear to be part of a Nix profile",
			profile
		)));
	}
	let user_env = crate::paths::follow_links_to_store_path(
		&std::env::current_dir()?,
		&profile.to_string_lossy(),
	)?;
	if !super::command_store::open(parsed)?
		.store
		.is_valid_path(&user_env)?
	{
		return Err(Failure::Error(format!(
			"directory {:?} is not in the Nix store",
			profile
		)));
	}
	Ok(profile)
}
