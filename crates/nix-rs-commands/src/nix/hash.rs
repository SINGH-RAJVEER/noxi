//! `nix hash` and the legacy `nix-hash`, following `src/nix/hash.cc`.
//!
//! Flag values are checked when the flag is read, as CppNix's handlers
//! parse them, so a bad algorithm or format stops parsing and BLAKE3 is
//! accepted only if `blake3-hashes` is enabled by then. `--mode git` needs
//! Git tree hashing, which nix-rs does not provide; it fails explicitly.

use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::process::ExitCode;

use nix_compat::nixcpp::hash::{self, Hash, HashFormat};
use nix_compat::nixcpp::settings::Settings;
use nix_compat::nixhash::{HashAlgo, NixHashDigester};

use super::Parsed;
use crate::cli::{Failure, apply_common_flag, print_setting_warnings};

fn warn(message: &str) {
	if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
		eprintln!("warning: {message}");
	}
}

fn failure(error: hash::Error) -> Failure {
	match error {
		hash::Error::Usage(message) => Failure::Usage(message),
		hash::Error::Bad(message) => Failure::Error(message),
	}
}

fn blake3_enabled(settings: &Settings) -> bool {
	settings.experimental_features().contains("blake3-hashes")
}

fn parse_algo(name: &str, settings: &Settings) -> Result<HashAlgo, Failure> {
	hash::parse_algo(name, blake3_enabled(settings)).map_err(failure)
}

/// `parseHashFormat`, printing the deprecation warning for `base32`.
fn parse_format(name: &str) -> Result<HashFormat, Failure> {
	let (format, warning) = HashFormat::parse(name).map_err(failure)?;
	if let Some(warning) = warning {
		warn(warning);
	}
	Ok(format)
}

/// `parseFileIngestionMethod`.
fn parse_mode(name: &str) -> Result<Mode, Failure> {
	match name {
		"flat" => Ok(Mode::Flat),
		"nar" => Ok(Mode::Nar),
		"git" => Ok(Mode::Git),
		_ => Err(Failure::Usage(format!(
			"Unknown file ingestion method '{name}', expect `flat`, `nar`, or `git`"
		))),
	}
}

/// Handle a flag of a `hash` subcommand while the command line is read.
/// Returns the value the command keeps for it, already checked.
pub(super) fn check_flag(
	settings: &Settings,
	command: &str,
	name: &str,
	values: &[String],
) -> Result<(), Failure> {
	if !command.starts_with("hash ") {
		return Ok(());
	}
	match name {
		"type" | "algo" | "hash-algo" => parse_algo(&values[0], settings).map(drop),
		"to" | "from" | "format" => parse_format(&values[0]).map(drop),
		"mode" => parse_mode(&values[0]).map(drop),
		_ => Ok(()),
	}
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
	Flat,
	Nar,
	Git,
}

/// Run `nix hash SUBCOMMAND` with flags that [`check_flag`] accepted.
pub(super) fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	let blake3 = blake3_enabled(&parsed.settings);
	// The values were checked when they were read; reading them again
	// cannot fail or warn.
	let algo = |name: &str| hash::parse_algo(name, true).expect("checked algorithm");
	let format = |name: &str| HashFormat::parse(name).expect("checked format").0;
	let mut out = io::stdout().lock();
	match parsed.command {
		"hash convert" => {
			let (mut from, mut to, mut want) = (None, HashFormat::Sri, None);
			for (name, values) in &parsed.sequence {
				match name.as_str() {
					"from" => from = Some(format(&values[0])),
					"to" => to = format(&values[0]),
					"hash-algo" => want = Some(algo(&values[0])),
					_ => {}
				}
			}
			for s in &parsed.operands {
				let (hash, parsed_format) =
					hash::parse_any_returning_format(s, want, blake3).map_err(failure)?;
				if let Some(from) = from
					&& from != parsed_format
				{
					return Err(Failure::Error(format!(
						"input hash '{s}' has format '{}', but '--from {}' was specified",
						parsed_format.name(),
						from.name()
					)));
				}
				writeln!(out, "{}", hash.to_string(to, to == HashFormat::Sri))?;
			}
		}
		"hash path" | "hash file" => {
			let mut options = HashOptions {
				mode: if parsed.command == "hash path" {
					Mode::Nar
				} else {
					Mode::Flat
				},
				format: HashFormat::Sri,
				truncate: false,
				algo: HashAlgo::Sha256,
			};
			for (name, values) in &parsed.sequence {
				match name.as_str() {
					"sri" => options.format = HashFormat::Sri,
					"base64" => options.format = HashFormat::Base64,
					"base32" => options.format = HashFormat::Nix32,
					"base16" => options.format = HashFormat::Base16,
					"format" => options.format = format(&values[0]),
					"type" | "algo" => options.algo = algo(&values[0]),
					"mode" => options.mode = parse_mode(&values[0])?,
					_ => {}
				}
			}
			hash_paths(&options, &parsed.settings, &parsed.operands, &mut out)?;
		}
		command => {
			let format = match command {
				"hash to-base16" => HashFormat::Base16,
				"hash to-base32" => HashFormat::Nix32,
				"hash to-base64" => HashFormat::Base64,
				_ => HashFormat::Sri,
			};
			let want = parsed
				.sequence
				.iter()
				.rfind(|(name, _)| name == "type")
				.map(|(_, values)| algo(&values[0]));
			warn(
				"The old format conversion subcommands of `nix hash` were deprecated in favor of `nix hash convert`.",
			);
			to_base(&parsed.operands, want, format, blake3, &mut out)?;
		}
	}
	Ok(ExitCode::SUCCESS)
}

/// `CmdToBase::run`.
fn to_base(
	hashes: &[String],
	want: Option<HashAlgo>,
	format: HashFormat,
	blake3: bool,
	out: &mut impl Write,
) -> Result<(), Failure> {
	for s in hashes {
		let hash = hash::parse_any(s, want, blake3).map_err(failure)?;
		writeln!(out, "{}", hash.to_string(format, format == HashFormat::Sri))?;
	}
	Ok(())
}

struct HashOptions {
	mode: Mode,
	format: HashFormat,
	truncate: bool,
	algo: HashAlgo,
}

/// A sink that hashes what is written to it.
struct Digester(NixHashDigester);

impl Write for Digester {
	fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
		self.0.update(buf);
		Ok(buf.len())
	}

	fn flush(&mut self) -> io::Result<()> {
		Ok(())
	}
}

/// The C library's message for an I/O error, without Rust's suffix.
pub(crate) fn strerror(error: &io::Error) -> String {
	match error.raw_os_error() {
		// SAFETY: strerror returns a valid C string for any error number.
		Some(code) => unsafe { std::ffi::CStr::from_ptr(libc::strerror(code)) }
			.to_string_lossy()
			.into_owned(),
		None => error.to_string(),
	}
}

/// `CmdHashBase::run`: print the hash of each path.
fn hash_paths(
	options: &HashOptions,
	settings: &Settings,
	paths: &[String],
	out: &mut impl Write,
) -> Result<(), Failure> {
	let cwd = std::env::current_dir()?;
	for path in paths {
		let mut digester = Digester(NixHashDigester::new(options.algo));
		match options.mode {
			Mode::Flat => {
				// `readFile` reads the path as given, so FIFOs work.
				let mut file = File::open(path).map_err(|error| {
					Failure::Error(format!("opening file \"{path}\": {}", strerror(&error)))
				})?;
				let mut buffer = vec![0; 65536];
				loop {
					let n = file.read(&mut buffer).map_err(|error| {
						Failure::Error(format!("read of 65536 bytes: {}", strerror(&error)))
					})?;
					if n == 0 {
						break;
					}
					digester.0.update(&buffer[..n]);
				}
			}
			Mode::Nar => {
				let path = crate::instantiate::absolute_path(&cwd, Path::new(path));
				if let Err(error) = std::fs::symlink_metadata(&path) {
					return Err(Failure::Error(format!(
						"opening file \"{}\": {}",
						path.display(),
						strerror(&error)
					)));
				}
				nix_rs_store::nar::dump_path(&mut digester, &path)?;
			}
			Mode::Git => {
				if !settings.experimental_features().contains("git-hashing") {
					return Err(Failure::Error(hash::missing_feature("git-hashing")));
				}
				return Err(Failure::Error(
					"nix hash path --mode git is not supported by nix-rs yet".into(),
				));
			}
		}
		let digest = digester.0.finalize();
		let mut hash = Hash {
			algo: options.algo,
			digest: digest.digest_as_bytes().to_vec(),
		};
		if options.truncate && hash.digest.len() > 20 {
			hash = hash.compress(20);
		}
		writeln!(
			out,
			"{}",
			hash.to_string(options.format, options.format == HashFormat::Sri)
		)?;
	}
	Ok(())
}

/// The legacy `nix-hash` command: `compatNixHash`.
pub fn legacy(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	let mut algo = None;
	let mut flat = false;
	let mut format = HashFormat::Base16;
	let mut truncate = false;
	let mut to = false;
	let mut operands = Vec::new();
	let mut args = crate::cli::ArgStream::new(args);
	let mut dash_dash = false;
	while let Some(arg) = args.next_arg() {
		if dash_dash || !arg.starts_with('-') {
			if dash_dash && arg.starts_with('-') {
				return Err(Failure::Usage(format!("unexpected argument '{arg}'")));
			}
			operands.push(arg);
			continue;
		}
		if arg == "--" {
			dash_dash = true;
			continue;
		}
		if let Some(result) = apply_legacy_flag(&arg, &mut args, &mut settings) {
			result?;
			continue;
		}
		match arg.as_str() {
			"--help" => {
				return Err(Failure::Error(
					"nix-hash --help is not supported by nix-rs yet".into(),
				));
			}
			"--version" => {
				println!("nix-hash (Nix) 2.35.2");
				return Ok(ExitCode::SUCCESS);
			}
			"--flat" => flat = true,
			"--base16" => format = HashFormat::Base16,
			"--base32" => format = HashFormat::Nix32,
			"--base64" => format = HashFormat::Base64,
			"--sri" => format = HashFormat::Sri,
			"--truncate" => truncate = true,
			"--type" => {
				let name = args
					.next()
					.ok_or_else(|| Failure::Usage("'--type' requires an argument".into()))?;
				algo = Some(parse_algo(&name, &settings)?);
			}
			"--to-base16" | "--to-base32" | "--to-base64" | "--to-sri" => {
				to = true;
				format = match arg.as_str() {
					"--to-base16" => HashFormat::Base16,
					"--to-base32" => HashFormat::Nix32,
					"--to-base64" => HashFormat::Base64,
					_ => HashFormat::Sri,
				};
			}
			_ => return Err(Failure::Usage(format!("unrecognised flag '{arg}'"))),
		}
	}
	print_setting_warnings(&mut settings);
	let mut out = io::stdout().lock();
	if to {
		to_base(&operands, algo, format, blake3_enabled(&settings), &mut out)?;
	} else {
		let options = HashOptions {
			mode: if flat { Mode::Flat } else { Mode::Nar },
			format,
			truncate,
			algo: algo.unwrap_or(HashAlgo::Md5),
		};
		hash_paths(&options, &settings, &operands, &mut out)?;
	}
	Ok(ExitCode::SUCCESS)
}

/// The flags `LegacyArgs` handles before the command's own: the common
/// flags, `--option`, `--no-gc-warning` and the settings flags, with
/// CppNix's messages for missing values. `None` means `arg` is none of them.
fn apply_legacy_flag(
	arg: &str,
	args: &mut crate::cli::ArgStream,
	settings: &mut Settings,
) -> Option<Result<(), Failure>> {
	if let Some(result) = apply_common_flag(arg, args, settings) {
		return Some(result);
	}
	if arg == "--no-gc-warning" {
		return Some(Ok(()));
	}
	let flag = super::args::find_flag(super::table::GLOBAL_FLAGS, arg)
		.filter(|flag| !super::NIX_ONLY_FLAGS.contains(&flag.name))?;
	let arity = flag.arity.unwrap_or(0);
	let values: Vec<String> = args.by_ref().take(arity).collect();
	if values.len() < arity {
		return Some(Err(Failure::Usage(format!(
			"flag '{arg}' requires {arity} argument(s), but only {} were given",
			values.len()
		))));
	}
	Some(super::apply_setting_flag(settings, flag, values))
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn rejects_unknown_ingestion_methods_as_usage_errors() {
		assert!(matches!(parse_mode("text"), Err(Failure::Usage(_))));
		assert!(parse_mode("git").is_ok());
	}
}
