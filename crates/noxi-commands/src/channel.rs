//! Subscription-file operations of the legacy channel command.
use crate::cli::{ArgStream, Failure, apply_common_flag};
use nix_compat::nixcpp::settings::Settings;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::process::ExitCode;

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
			"--add" | "--remove" | "--list" => operation = arg,
			"--update" | "--rollback" | "--list-generations" => {
				return Err(Failure::Error(format!(
					"nix-channel {arg} is not supported by noxi yet"
				)));
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
	Ok(ExitCode::SUCCESS)
}
