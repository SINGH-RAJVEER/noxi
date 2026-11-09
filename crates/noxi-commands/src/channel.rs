//! Native legacy channel subscriptions and profile lifecycle.
use crate::cli::{ArgStream, Failure, apply_common_flag};
use nix_compat::nixcpp::settings::Settings;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::process::ExitCode;

fn channel_profile() -> Result<std::path::PathBuf, Failure> {
	Ok(
		noxi_store::profiles::profiles_dir(&noxi_store::LocalStoreConfig::system().state_dir)?
			.join("channels"),
	)
}

fn download(url: &str) -> Result<(Vec<u8>, String), Failure> {
	use noxi_glue::fetch::http::{Client, Native, Request};
	if let Some(path) = url.strip_prefix("file://") {
		return Ok((
			if std::path::Path::new(path).is_dir() {
				Vec::new()
			} else {
				fs::read(path)?
			},
			url.into(),
		));
	}
	let response = Native
		.request(&Request::get(url))
		.map_err(|e| e.to_string())?;
	if !(200..300).contains(&response.status) {
		return Err(Failure::Error(format!(
			"unable to download '{url}': HTTP error {}",
			response.status
		)));
	}
	Ok((response.body, response.effective_url))
}

fn lifecycle(
	settings: &Settings,
	operation: &str,
	operands: &[String],
	channels: &BTreeMap<String, String>,
) -> Result<ExitCode, Failure> {
	use crate::{
		instantiate::{self, Options, nix_string_literal as quote},
		user_env,
	};
	use noxi_store::{FileIngestionMethod, profiles};
	let profile = channel_profile()?;
	if operation != "--update" {
		let mut args = vec!["--profile".into(), profile.to_string_lossy().into_owned()];
		if operation == "--rollback" && operands.len() == 1 {
			args.extend(["--switch-generation".into(), operands[0].clone()]);
		} else if operands.is_empty() {
			args.push(operation.into());
		} else {
			return Err(Failure::Usage(format!(
				"invalid arguments for nix-channel {operation}"
			)));
		}
		return crate::nix_env::main(settings.clone(), args);
	}
	let uri = crate::store::store_uri(None, settings);
	let opened = crate::store::open(&uri, settings)?
		.ok_or_else(|| Failure::Error("channel update requires a writable store".into()))?;
	let store = opened.store.as_ref();
	let cwd = std::env::current_dir()?;
	let mut elements = Vec::new();
	for (name, url) in channels {
		let version = regex::Regex::new(r"(-\d.*)$").unwrap();
		let base = url.rsplit('/').next().unwrap_or("");
		let cname = format!("{name}{}", version.find(base).map_or("", |m| m.as_str()));
		if !operands.is_empty() && !operands.contains(name) {
			let generation = fs::read_link(&profile)?;
			let generation = if generation.is_absolute() {
				generation
			} else {
				profile.parent().unwrap().join(generation)
			};
			let output = fs::read_link(generation)?;
			let output = nix_compat::store_path::StorePath::from_absolute_path(
				output.to_string_lossy().as_bytes(),
			)
			.map_err(|e| e.to_string())?;
			let target = fs::read_link(store.real_path(&output).join(name))?;
			let path = nix_compat::store_path::StorePath::from_absolute_path(
				target.parent().unwrap().to_string_lossy().as_bytes(),
			)
			.map_err(|e| e.to_string())?;
			let mut element = user_env::Element::from_store_path(store, path)?;
			element.name = cname;
			element.system = "builtin".into();
			elements.push(element);
			continue;
		}
		let (mut bytes, effective) = download(url)?;
		let direct = [".tar.gz", ".tar.bz2", ".tar.xz"]
			.iter()
			.any(|ext| effective.ends_with(ext));
		let mut filename = effective.rsplit('/').next().unwrap_or("").to_owned();
		if ![".tar.gz", ".tar.bz2", ".tar.xz"]
			.iter()
			.any(|ext| filename.ends_with(ext))
		{
			let base = effective.trim_end_matches('/');
			match download(&format!("{base}/nixexprs.tar.xz")) {
				Ok((body, _)) => {
					bytes = body;
					filename = "nixexprs.tar.xz".into();
				}
				Err(_) => {
					bytes = download(&format!("{base}/nixexprs.tar.bz2"))?.0;
					filename = "nixexprs.tar.bz2".into();
				}
			}
		}
		let src = store.add_to_store_from_dump(
			&mut bytes.as_slice(),
			&filename,
			FileIngestionMethod::Flat,
			FileIngestionMethod::Flat,
			nix_compat::nixhash::HashAlgo::Sha256,
			&Default::default(),
		)?;
		let expr = format!(
			"derivation {{ builder = \"builtin:unpack-channel\"; system = \"builtin\"; name = {}; channelName = {}; src = builtins.storePath {}; preferLocalBuild = true; }}",
			quote(&cname),
			quote(name),
			quote(&src.to_absolute_path())
		);
		let options = Options {
			from_args: true,
			inputs: vec![expr],
			settings: settings.clone(),
			store: Some(uri.clone()),
			gc_warning: false,
			..Default::default()
		};
		let evaluated = match instantiate::evaluate(&options, &cwd) {
			Ok(v) => v,
			Err(code) => return Ok(code),
		};
		let drvs = instantiate::derivations(&evaluated.results)?;
		if direct {
			let targets = [noxi_store::DerivedPath::Built {
				drv: drvs[0].0.clone(),
				outputs: Default::default(),
			}];
			crate::paths::print_missing(store, &store.query_missing(&targets)?)?;
			store.build_paths(&targets)?;
		}
		let mut element = user_env::Element::from_store_path(store, drvs[0].0.clone())?;
		element.name = cname;
		element.system = "builtin".into();
		elements.push(element);
	}
	std::eprintln!("unpacking {} channels...", elements.len());
	loop {
		let token = user_env::lock_token(&profile);
		let verbosity = nix_compat::log::verbosity();
		nix_compat::log::set_verbosity(0);
		let result = user_env::create_user_env(
			store,
			settings,
			Some(&uri),
			&cwd,
			&elements,
			&profile,
			settings.get_bool("keep-env-derivations", false),
			&token,
		);
		nix_compat::log::set_verbosity(verbosity);
		match result? {
			Ok(true) => break,
			Ok(false) => continue,
			Err(code) => return Ok(code),
		}
	}
	let dir = if settings.get_bool("use-xdg-base-directories", false) {
		profiles::create_nix_state_dir()?.join("defexpr")
	} else {
		profiles::home_dir().join(".nix-defexpr")
	};
	if fs::symlink_metadata(&dir).is_ok_and(|m| m.file_type().is_symlink()) {
		fs::remove_file(&dir)?;
	}
	fs::create_dir_all(&dir)?;
	profiles::switch_link(&dir.join("channels"), &profile)?;
	Ok(ExitCode::SUCCESS)
}

pub fn run(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	let mut args = ArgStream::new(args);
	let mut operation = String::new();
	let mut operands = Vec::new();
	while let Some(arg) = args.next_arg() {
		if let Some(result) = apply_common_flag(&arg, &mut args, &mut settings) {
			result?;
			continue;
		}
		match arg.as_str() {
			"--version" => {
				println!("nix-channel (Nix) 2.35.2");
				return Ok(ExitCode::SUCCESS);
			}
			"--add" | "--remove" | "--list" | "--update" | "--rollback" | "--list-generations" => {
				operation = arg
			}
			"--" => operands.extend(args.by_ref()),
			flag if flag.starts_with('-') => {
				settings
					.apply_flag(flag, || args.next())
					.ok_or_else(|| Failure::Usage(format!("unrecognised flag '{flag}'")))?
					.map_err(|e| e.to_string())?;
			}
			_ => operands.push(arg),
		}
	}
	let file = if settings.get_bool("use-xdg-base-directories", false) {
		noxi_store::profiles::create_nix_state_dir()?.join("channels")
	} else {
		noxi_store::profiles::home_dir().join(".nix-channels")
	};
	let text = match fs::read_to_string(&file) {
		Ok(text) => text,
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
		Err(e) => return Err(e.into()),
	};
	let mut channels = BTreeMap::new();
	for line in text.lines() {
		let mut words = line.split_whitespace();
		if let (Some(url), Some(name)) = (words.next(), words.next()) {
			channels.insert(name.to_owned(), url.trim_end_matches('/').to_owned());
		}
	}
	if matches!(
		operation.as_str(),
		"--update" | "--rollback" | "--list-generations"
	) {
		return lifecycle(&settings, &operation, &operands, &channels);
	}
	match operation.as_str() {
		"--list" if operands.is_empty() => {
			for (name, url) in channels {
				println!("{name} {url}");
			}
			return Ok(ExitCode::SUCCESS);
		}
		"--add" if (1..=2).contains(&operands.len()) => {
			let url = &operands[0];
			let parsed = url::Url::parse(url)
				.map_err(|_| Failure::Error(format!("invalid channel URL '{url}'")))?;
			if !matches!(parsed.scheme(), "http" | "https" | "file") {
				return Err(Failure::Error(format!("invalid channel URL '{url}'")));
			}
			let name = operands.get(1).cloned().unwrap_or_else(|| {
				parsed
					.path()
					.trim_end_matches('/')
					.rsplit('/')
					.next()
					.unwrap_or("")
					.to_owned()
			});
			if name.is_empty()
				|| !name
					.chars()
					.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
				|| name.starts_with('.')
			{
				return Err(Failure::Error(format!(
					"invalid channel identifier '{name}'"
				)));
			}
			channels.insert(name, url.clone());
		}
		"--remove" if operands.len() == 1 => {
			channels.remove(&operands[0]);
		}
		"" => return Err(Failure::Usage("no operation specified".into())),
		_ => {
			return Err(Failure::Usage(format!(
				"invalid arguments for nix-channel {operation}"
			)));
		}
	}
	let parent = file.parent().unwrap();
	fs::create_dir_all(parent)?;
	let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
	for (name, url) in channels {
		writeln!(temporary, "{url} {name}")?;
	}
	temporary.persist(&file).map_err(|e| e.error)?;
	if operation == "--remove" {
		let profile = channel_profile()?;
		return crate::nix_env::main(
			settings,
			vec![
				"--profile".into(),
				profile.to_string_lossy().into_owned(),
				"--uninstall".into(),
				operands[0].clone(),
			],
		);
	}
	Ok(ExitCode::SUCCESS)
}
