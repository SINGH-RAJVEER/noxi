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

impl From<noxi_store::Error> for Failure {
	fn from(error: noxi_store::Error) -> Self {
		match error {
			noxi_store::Error::Build {
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
					eprintln!(
						"{}\nTry '{program} --help' for more information.",
						nix_compat::log::format_error(&message)
					);
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

/// Command-line arguments read as CppNix's `Args::parseCmdline` reads
/// them: a compound short flag such as `-qR` or `-j4` is expanded when it
/// is read as a flag, not when it is the value of the flag before it, and
/// after `--` nothing is expanded. Values are taken with
/// [`Iterator::next`].
pub struct ArgStream {
	args: std::collections::VecDeque<String>,
	dash_dash: bool,
}

impl ArgStream {
	pub fn new(args: impl IntoIterator<Item = String>) -> Self {
		Self {
			args: args.into_iter().collect(),
			dash_dash: false,
		}
	}

	/// The next argument where a flag or operand is expected.
	pub fn next_arg(&mut self) -> Option<String> {
		let arg = self.args.pop_front()?;
		if self.dash_dash {
			return Some(arg);
		}
		if arg == "--" {
			self.dash_dash = true;
			return Some(arg);
		}
		let bytes = arg.as_bytes();
		if bytes.len() > 2 && bytes[0] == b'-' && bytes[1] != b'-' && bytes[1].is_ascii_alphabetic()
		{
			let mut rest = Vec::new();
			for (i, c) in arg.char_indices().skip(2) {
				if c.is_ascii_alphabetic() {
					rest.push(format!("-{c}"));
				} else {
					rest.push(arg[i..].to_owned());
					break;
				}
			}
			for part in rest.into_iter().rev() {
				self.args.push_front(part);
			}
			return Some(arg[..2].to_owned());
		}
		Some(arg)
	}
}

impl Iterator for ArgStream {
	type Item = String;

	fn next(&mut self) -> Option<String> {
		self.args.pop_front()
	}
}

/// `string2IntWithUnitPrefix`: an integer with an optional `K`, `M`, `G`
/// or `T` suffix. The error is a usage error's message.
pub fn parse_int_with_unit(s: &str) -> Result<i64, String> {
	let (digits, multiplier) = split_unit(s)?;
	match digits.parse::<i64>() {
		Ok(n) => Ok(n.wrapping_mul(multiplier as i64)),
		_ => Err(format!("'{digits}' is not an integer")),
	}
}

/// The unsigned form used by legacy build settings.
fn parse_uint_with_unit(s: &str) -> Result<u64, String> {
	let (digits, multiplier) = split_unit(s)?;
	match digits.parse::<u64>() {
		Ok(n) => Ok(n.wrapping_mul(multiplier)),
		_ => Err(format!("'{digits}' is not an integer")),
	}
}

fn split_unit(s: &str) -> Result<(&str, u64), String> {
	let (digits, multiplier) = match s.chars().last().map(|c| c.to_ascii_uppercase()) {
		Some(unit) if unit.is_ascii_alphabetic() => {
			let shift = match unit {
				'K' => 10,
				'M' => 20,
				'G' => 30,
				'T' => 40,
				_ => return Err(format!("invalid unit specifier '{unit}'")),
			};
			(&s[..s.len() - 1], 1u64 << shift)
		}
		_ => (s, 1),
	};
	Ok((digits, multiplier))
}

/// The flags of CppNix's `MixCommonArgs` and `LegacyArgs` that every
/// legacy command accepts, other than `--option`, the settings flags,
/// `--store` and `--no-gc-warning`, which commands handle themselves.
/// `None` means `flag` is not one of them.
pub fn apply_common_flag(
	flag: &str,
	args: &mut dyn Iterator<Item = String>,
	settings: &mut Settings,
) -> Option<Result<(), Failure>> {
	use nix_compat::log::{set_print_build_logs, set_verbosity, verbosity};
	const COMMON: &[&str] = &[
		"--verbose",
		"-v",
		"--quiet",
		"--debug",
		"--log-format",
		"--no-build-output",
		"-Q",
		"--keep-failed",
		"-K",
		"--keep-going",
		"-k",
		"--fallback",
		"--max-jobs",
		"-j",
		"--cores",
		"--max-silent-time",
		"--timeout",
		"--readonly-mode",
	];
	if !COMMON.contains(&flag) {
		return None;
	}
	let mut value = || {
		args.next().ok_or_else(|| {
			Failure::Usage(format!(
				"flag '{flag}' requires 1 argument(s), but only 0 were given"
			))
		})
	};
	let mut set = |name: &str, value: &str| {
		settings
			.set(name, value)
			.map(drop)
			.map_err(|error| Failure::Usage(error.to_string()))
	};
	Some((|| match flag {
		"--verbose" | "-v" => {
			set_verbosity((verbosity() + 1).min(7));
			Ok(())
		}
		"--quiet" => {
			set_verbosity(verbosity().saturating_sub(1));
			Ok(())
		}
		"--debug" => {
			set_verbosity(6);
			Ok(())
		}
		"--log-format" => {
			match value()?.as_str() {
				"raw" | "bar" => set_print_build_logs(false),
				"raw-with-logs" | "bar-with-logs" => set_print_build_logs(true),
				"internal-json" => {
					return Err(Failure::Error(
						"--log-format internal-json is not supported by noxi yet".into(),
					));
				}
				format => {
					return Err(Failure::Error(format!(
						"option 'log-format' has an invalid value '{format}'"
					)));
				}
			}
			Ok(())
		}
		"--no-build-output" | "-Q" => {
			set_print_build_logs(false);
			Ok(())
		}
		"--keep-failed" | "-K" => set("keep-failed", "true"),
		"--keep-going" | "-k" => set("keep-going", "true"),
		"--fallback" => set("fallback", "true"),
		"--max-jobs" | "-j" => {
			let jobs = value()?;
			if jobs != "auto" && jobs.parse::<u32>().is_err() {
				return Err(Failure::Usage(
					"configuration setting 'max-jobs' should be 'auto' or an integer".into(),
				));
			}
			set("max-jobs", &jobs)
		}
		"--cores" | "--max-silent-time" | "--timeout" => {
			let n = parse_uint_with_unit(&value()?).map_err(Failure::Usage)?;
			set(&flag[2..], &n.to_string())
		}
		"--readonly-mode" => Err(Failure::Error(
			"--readonly-mode is not supported by noxi yet".into(),
		)),
		_ => Ok(()),
	})())
}

/// Print the warnings collected while reading settings.
pub fn print_setting_warnings(settings: &mut Settings) {
	if !nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
		let _ = settings.finish();
		return;
	}
	for warning in settings.finish() {
		eprintln!("warning: {warning}");
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_integers_with_units() {
		assert_eq!(parse_int_with_unit("1K"), Ok(1024));
		assert_eq!(parse_int_with_unit("2g"), Ok(2 << 30));
		assert_eq!(parse_int_with_unit("-5"), Ok(-5));
		assert_eq!(parse_int_with_unit("+1K"), Ok(1024));
		assert_eq!(
			parse_int_with_unit("x"),
			Err("invalid unit specifier 'X'".into())
		);
		assert_eq!(
			parse_int_with_unit("1.5"),
			Err("'1.5' is not an integer".into())
		);
	}

	#[test]
	fn parses_unsigned_build_settings_with_units() {
		assert_eq!(parse_uint_with_unit("2g"), Ok(2 << 30));
		assert_eq!(parse_uint_with_unit("18446744073709551615"), Ok(u64::MAX));
		assert_eq!(parse_uint_with_unit("18014398509481984K"), Ok(0));
		assert_eq!(
			parse_uint_with_unit("-1K"),
			Err("'-1' is not an integer".into())
		);
		assert_eq!(parse_uint_with_unit("+1"), Ok(1));
	}

	#[test]
	fn rejects_readonly_mode_until_it_can_prevent_writes() {
		let mut settings = Settings::default();
		let mut args = std::iter::empty();
		assert!(matches!(
			apply_common_flag("--readonly-mode", &mut args, &mut settings),
			Some(Err(Failure::Error(_)))
		));
	}

	#[test]
	fn expands_compound_short_flags_only_where_flags_are_read() {
		let mut args = ArgStream::new(
			["-qR", "-j4", "-b", "-xy", "-vb", "--long", "--", "-ab"].map(String::from),
		);
		let mut read = Vec::new();
		while let Some(arg) = args.next_arg() {
			let takes_value = arg == "-b" || arg == "-j";
			read.push(arg);
			if takes_value {
				read.push(args.next().unwrap());
			}
		}
		assert_eq!(
			read,
			[
				"-q", "-R", "-j", "4", "-b", "-xy", "-v", "-b", "--long", "--", "-ab"
			]
		);
	}
}
