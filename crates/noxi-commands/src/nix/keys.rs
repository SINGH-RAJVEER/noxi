//! `nix key generate-secret` and `nix key convert-secret-to-public`,
//! following `src/nix/sigs.cc`. Neither opens a store.
use std::io::{self, Read, Write};
use std::process::ExitCode;

use noxi_store::keys::SecretKey;

use super::{Parsed, unsupported};
use crate::cli::Failure;

pub fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	let text = match parsed.command {
		"key generate-secret" => {
			// CppNix checks required flags while parsing the command line.
			let name = parsed.value("key-name").ok_or_else(|| {
				Failure::Usage("required argument '--key-name' is missing".into())
			})?;
			SecretKey::generate(name)?.to_text()
		}
		"key convert-secret-to-public" => {
			let mut input = Vec::new();
			io::stdin().lock().read_to_end(&mut input)?;
			SecretKey::parse(&String::from_utf8_lossy(&input))?.to_public_text()
		}
		command => return Err(unsupported(command)),
	};
	let mut out = io::stdout().lock();
	out.write_all(text.as_bytes())?;
	out.flush()?;
	Ok(ExitCode::SUCCESS)
}
