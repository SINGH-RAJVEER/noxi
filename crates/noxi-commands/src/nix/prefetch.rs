//! Native modern fixed-output downloads.
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;

use super::Parsed;
use crate::cli::Failure;
use nix_compat::nixcpp::hash::{self, Hash, HashFormat};
use nix_compat::nixhash::NixHashDigester;
use noxi_glue::fetch::http::{Client, Native, Request};
use noxi_store::FileIngestionMethod;

pub fn prefetch_file(parsed: &Parsed) -> Result<ExitCode, Failure> {
	let settings = &parsed.settings;
	let unpack = parsed.has("unpack");
	let executable = parsed.has("executable");
	let name = parsed.value("name").map(str::to_owned);
	let algo = hash::parse_algo(
		parsed.value("hash-type").unwrap_or("sha256"),
		settings.experimental_features().contains("blake3-hashes"),
	)
	.map_err(|e| e.to_string())?;
	let operands = [
		parsed.operands[0].clone(),
		parsed.value("expected-hash").unwrap_or("").to_owned(),
	];
	let url = url::Url::parse(&operands[0]).map_err(|e| e.to_string())?;
	let derived = name.is_none();
	let name = match name {
		Some(name) => name,
		None => url
			.path_segments()
			.and_then(|mut segments| segments.rfind(|s| !s.is_empty()))
			.map(percent_decode)
			.ok_or_else(|| {
				Failure::Error(format!("cannot figure out file name for '{}'", operands[0]))
			})?,
	};
	if let Err(error) = check_name(&name) {
		return Err(Failure::Error(if derived {
			format!(
				"\n\u{2026} file name '{name}' was extracted from URL '{}'\n\nerror: {error}",
				operands[0]
			)
		} else {
			error
		}));
	}
	let expected = operands
		.get(1)
		.filter(|s| !s.is_empty())
		.map(|s| hash::parse_any(s, Some(algo), true).map_err(|e| e.to_string()))
		.transpose()?;
	let uri = crate::store::store_uri(parsed.value("store"), settings);
	let store = crate::store::open(&uri, settings)?
		.ok_or_else(|| Failure::Error("nix-prefetch-url: unsupported store".into()))?;
	if let Some(expected) = &expected {
		let hash = nix_compat::nixhash::NixHash::from_str(
			&expected.to_string(HashFormat::Sri, true),
			None,
		)
		.map_err(|e| e.to_string())?;
		let path =
			nix_compat::store_path::build_ca_path(&name, unpack || executable, &hash, [], false)
				.map_err(|e| e.to_string())?
				.to_owned();
		if store.store.is_valid_path(&path)? {
			print_result(parsed, expected, &path.to_absolute_path())?;
			return Ok(ExitCode::SUCCESS);
		}
	}
	let bytes = if url.scheme() == "file" {
		let file = url
			.to_file_path()
			.map_err(|_| "invalid file URL".to_owned())?;
		fs::read(&file).map_err(|_| {
			Failure::Error(format!(
				"unable to download '{}': Could not read a file:// file (37) Could not open file {}",
				operands[0],
				file.display()
			))
		})?
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
	let temporary = tempfile::Builder::new()
		.prefix(&format!("nix-{}-", std::process::id()))
		.tempdir()?;
	let downloaded = temporary.path().join("tmp");
	fs::write(&downloaded, &bytes)?;
	let mut mismatch_path = downloaded.clone();
	let mut dump = tempfile::tempfile()?;
	let method = if unpack || executable {
		FileIngestionMethod::NixArchive
	} else {
		FileIngestionMethod::Flat
	};
	if unpack {
		let (mount, _) = noxi_glue::fetch::unpack_tarball(bytes).map_err(|e| e.to_string())?;
		mismatch_path = mount.root.clone();
		noxi_store::nar::dump_path(&mut dump, &mount.root)?;
	} else if executable {
		let mut file = tempfile::NamedTempFile::new()?;
		file.write_all(&bytes)?;
		file.as_file()
			.set_permissions(fs::Permissions::from_mode(0o755))?;
		noxi_store::nar::dump_path(&mut dump, file.path())?;
	} else {
		dump.write_all(&bytes)?;
	}
	dump.rewind()?;
	let mut digester = NixHashDigester::new(algo);
	let mut buffer = [0; 65536];
	loop {
		let count = dump.read(&mut buffer)?;
		if count == 0 {
			break;
		}
		digester.update(&buffer[..count]);
	}
	let digest = digester.finalize();
	let actual = Hash {
		algo,
		digest: digest.digest_as_bytes().to_vec(),
	};
	if let Some(expected) = expected
		&& actual != expected
	{
		return Err(Failure::Error(format!(
			"hash mismatch for '{}'",
			mismatch_path.display()
		)));
	}
	dump.rewind()?;
	let path = store.store.add_to_store_from_dump(
		&mut dump,
		&name,
		method,
		method,
		algo,
		&BTreeSet::new(),
	)?;
	let path = path.to_absolute_path();
	print_result(parsed, &actual, &path)?;
	Ok(ExitCode::SUCCESS)
}

fn print_result(parsed: &Parsed, actual: &Hash, path: &str) -> Result<(), Failure> {
	let hash = actual.to_string(HashFormat::Sri, true);
	if parsed.has("json") {
		let value = serde_json::json!({"storePath": path, "hash": hash});
		if parsed.has("pretty") {
			println!(
				"{}",
				serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
			);
		} else {
			println!("{value}");
		}
	} else {
		if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Notice) {
			eprintln!(
				"Downloaded '{}' to '{}' (hash '{}').",
				parsed.operands[0], path, hash
			);
		}
	}
	Ok(())
}

/// Decode `%XX` escapes in a URL path segment.
fn percent_decode(segment: &str) -> String {
	let bytes = segment.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		if bytes[i] == b'%'
			&& i + 2 < bytes.len()
			&& let Ok(value) = u8::from_str_radix(&segment[i + 1..i + 3], 16)
		{
			out.push(value);
			i += 3;
		} else {
			out.push(bytes[i]);
			i += 1;
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

/// `checkName` for store path names.
fn check_name(name: &str) -> Result<(), String> {
	if name.is_empty() {
		return Err("name must not be empty".into());
	}
	if name.len() > 211 {
		return Err(format!(
			"name '{name}' must be no longer than 211 characters"
		));
	}
	if name == "." || name == ".." {
		return Err(format!("name '{name}' is not valid"));
	}
	for dots in ["..", "."] {
		if name.starts_with(&format!("{dots}-")) {
			return Err(format!(
				"name '{name}' is not valid: first dash-separated component must not be '{dots}'"
			));
		}
	}
	if let Some(c) = name
		.chars()
		.find(|c| !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_' | '?' | '=')))
	{
		return Err(format!("name '{name}' contains illegal character '{c}'"));
	}
	Ok(())
}
