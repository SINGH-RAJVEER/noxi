use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_rs_commands::instantiate;

fn main() -> ExitCode {
	let program = std::env::args()
		.next()
		.unwrap_or_else(|| "nix-instantiate".to_owned());
	let usage = |message: &dyn std::fmt::Display| {
		eprintln!("error: {message}\nTry '{program} --help' for more information.");
		ExitCode::FAILURE
	};
	let settings = match Settings::load(|name| std::env::var(name).ok()) {
		Ok(settings) => settings,
		Err(error) if error.is_usage() => return usage(&error),
		Err(error) => {
			eprintln!("error: {error}");
			return ExitCode::FAILURE;
		}
	};
	let mut options = match instantiate::parse_args(std::env::args().skip(1), settings) {
		Ok(options) => options,
		Err(message) => return usage(&message),
	};
	for warning in options.settings.finish() {
		eprintln!("warning: {warning}");
	}
	let cwd = match std::env::current_dir() {
		Ok(cwd) => cwd,
		Err(error) => {
			eprintln!("error: cannot determine the current directory: {error}");
			return ExitCode::FAILURE;
		}
	};
	instantiate::run(&options, &cwd, &mut std::io::stdout().lock())
}
