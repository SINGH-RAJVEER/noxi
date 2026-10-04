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
	/// An error with an exit status other than 1, such as a failed build.
	Exit(u8, String),
}

impl From<String> for Failure {
	fn from(error: String) -> Self {
		Failure::Error(error)
	}
}

impl From<std::io::Error> for Failure {
	fn from(error: std::io::Error) -> Self {
		Failure::Error(error.to_string())
	}
}

impl From<nix_rs_store::Error> for Failure {
	fn from(error: nix_rs_store::Error) -> Self {
		match error {
			nix_rs_store::Error::Build {
				exit_status,
				message,
			} => Failure::Exit(exit_status, message),
			error => Failure::Error(error.to_string()),
		}
	}
}

/// Print an error as CppNix does: continuation lines line up after the
/// `error: ` prefix.
pub fn print_error(message: &str) {
	eprintln!("{}", nix_compat::log::format_error(message));
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
					print_error(&message);
					ExitCode::FAILURE
				}
				Err(Failure::Exit(status, message)) => {
					print_error(&message);
					ExitCode::from(status)
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
