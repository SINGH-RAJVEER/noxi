//! Process setup shared by the legacy command executables.

use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;

/// CppNix raises its stack limit to 64 MiB so that deep evaluations do not
/// overflow; running the command on a thread of that size does the same.
const STACK_SIZE: usize = 64 * 1024 * 1024;

/// A failure of a command: a usage error gets CppNix's `--help` hint.
#[derive(Debug)]
pub enum Failure {
	Usage(String),
	Error(String),
}

impl<E: std::fmt::Display> From<E> for Failure {
	fn from(error: E) -> Self {
		Failure::Error(error.to_string())
	}
}

/// Run `command` with the settings from the configuration files and the
/// arguments after the program name, reporting a failure as CppNix does.
pub fn main(
	default_name: &'static str,
	command: fn(Settings, Vec<String>) -> Result<ExitCode, Failure>,
) -> ExitCode {
	std::thread::Builder::new()
		.stack_size(STACK_SIZE)
		.spawn(move || {
			let program = std::env::args()
				.next()
				.unwrap_or_else(|| default_name.to_owned());
			let result = match Settings::load(|name| std::env::var(name).ok()) {
				Ok(settings) => command(settings, std::env::args().skip(1).collect()),
				Err(error) if error.is_usage() => Err(Failure::Usage(error.to_string())),
				Err(error) => Err(Failure::Error(error.to_string())),
			};
			match result {
				Ok(code) => code,
				Err(Failure::Usage(message)) => {
					eprintln!("error: {message}\nTry '{program} --help' for more information.");
					ExitCode::FAILURE
				}
				Err(Failure::Error(message)) => {
					eprintln!("error: {message}");
					ExitCode::FAILURE
				}
			}
		})
		.expect("start the command thread")
		.join()
		.unwrap_or(ExitCode::FAILURE)
}

/// Print the warnings collected while reading settings.
pub fn print_setting_warnings(settings: &mut Settings) {
	for warning in settings.finish() {
		eprintln!("warning: {warning}");
	}
}
