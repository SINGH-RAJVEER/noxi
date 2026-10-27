//! Legacy entry points whose required library capabilities remain unavailable.
use crate::cli::{ArgStream, Failure, apply_common_flag, print_setting_warnings};
use nix_compat::nixcpp::settings::Settings;
use std::process::ExitCode;

pub fn copy_closure(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	let mut args = ArgStream::new(args);
	let mut host = None;
	while let Some(arg) = args.next_arg() {
		if let Some(result) = apply_common_flag(&arg, &mut args, &mut settings) {
			result?;
			continue;
		}
		match arg.as_str() {
			"--version" => {
				println!("nix-copy-closure (Nix) 2.35.2");
				return Ok(ExitCode::SUCCESS);
			}
			"--to" | "--from" | "--include-outputs" | "--dry-run" | "--use-substitutes" | "-s"
			| "--gzip" => {}
			"--" => {
				if host.is_none() {
					host = args.next();
				}
				for _ in args.by_ref() {}
			}
			flag if flag.starts_with('-') => {
				if let Some(result) = settings.apply_flag(flag, || args.next()) {
					result.map_err(|e| Failure::Usage(e.to_string()))?;
				} else {
					return Err(Failure::Usage(format!("unrecognised flag '{flag}'")));
				}
			}
			_ if host.is_none() => host = Some(arg),
			_ => {}
		}
	}
	print_setting_warnings(&mut settings);
	if host.is_none() {
		return Err(Failure::Usage("no host name specified".into()));
	}
	Err(Failure::Error("nix-copy-closure local mode is not supported by noxi yet: CppNix 2.35.2 uses SSH for this command and the shared library has no local copy-closure transport; use nix copy for supported file:// exports".into()))
}
