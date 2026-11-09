//! Legacy fixed-output downloads, using the native HTTP and store APIs.
use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::ExitCode;

use crate::cli::{ArgStream, Failure, apply_common_flag};
use nix_compat::nixcpp::{
	hash::{self, Hash, HashFormat},
	settings::Settings,
};
use nix_compat::nixhash::{HashAlgo, NixHashDigester};
use noxi_glue::fetch::http::{Client, Native, Request};
use noxi_store::FileIngestionMethod;

pub fn run(mut settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	let mut args = ArgStream::new(args);
	let (mut unpack, mut executable, mut print_path) = (false, false, false);
	let (mut name, mut store_uri) = (None, None);
	let mut algo = HashAlgo::Sha256;
	let mut operands = Vec::new();
	while let Some(arg) = args.next_arg() {
		if let Some(result) = apply_common_flag(&arg, &mut args, &mut settings) {
			result?;
			continue;
		}
		let value = |args: &mut ArgStream| {
			args.next()
				.ok_or_else(|| Failure::Usage(format!("'{arg}' requires an argument")))
		};
		match arg.as_str() {
			"--version" => {
				println!("nix-prefetch-url (Nix) 2.35.2");
				return Ok(ExitCode::SUCCESS);
			}
			"--unpack" => unpack = true,
			"--executable" => executable = true,
			"--print-path" => print_path = true,
			"--name" => name = Some(value(&mut args)?),
			"--store" => store_uri = Some(value(&mut args)?),
			"--type" => {
				algo = hash::parse_algo(
					&value(&mut args)?,
					settings.experimental_features().contains("blake3-hashes"),
				)
				.map_err(|e| e.to_string())?
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
	if operands.is_empty() {
		return Err(Failure::Usage("you must specify a URL".into()));
	}
	if operands.len() > 2 {
		return Err(Failure::Usage("too many arguments".into()));
	}
	let url = url::Url::parse(&operands[0]).map_err(|e| e.to_string())?;
	let name = name.unwrap_or_else(|| {
		url.path_segments()
			.and_then(|mut s| s.next_back())
			.unwrap_or("")
			.to_owned()
	});
	let expected = operands
		.get(1)
		.filter(|s| !s.is_empty())
		.map(|s| hash::parse_any(s, Some(algo), true).map_err(|e| e.to_string()))
		.transpose()?;
	let uri = crate::store::store_uri(store_uri.as_deref(), &settings);
	let store = crate::store::open(&uri, &settings)?
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
			print_result(expected, &path.to_absolute_path(), print_path);
			return Ok(ExitCode::SUCCESS);
		}
	}
	let bytes = if url.scheme() == "file" {
		fs::read(
			url.to_file_path()
				.map_err(|_| "invalid file URL".to_owned())?,
		)?
	} else {
		let response = Native
			.request(&Request::get(url.as_str()))
			.map_err(|e| e.to_string())?;
		if !(200..300).contains(&response.status) {
			return Err(Failure::Error(format!(
				"unable to download '{}': HTTP error {}",
				url, response.status
			)));
		}
		response.body
	};
	let mut dump = tempfile::tempfile()?;
	let method = if unpack || executable {
		FileIngestionMethod::NixArchive
	} else {
		FileIngestionMethod::Flat
	};
	if unpack {
		let (mount, _) = noxi_glue::fetch::unpack_tarball(bytes).map_err(|e| e.to_string())?;
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
			"hash mismatch for '{}':\n  specified: {}\n     got:    {}",
			url,
			expected.to_string(HashFormat::Sri, true),
			actual.to_string(HashFormat::Sri, true)
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
	print_result(&actual, &path, print_path);
	Ok(ExitCode::SUCCESS)
}

fn print_result(actual: &Hash, path: &str, print_path: bool) {
	let algo = actual.algo;
	if !print_path && nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Info) {
		eprintln!("path is '{path}'");
	}
	println!(
		"{}",
		actual.to_string(
			if algo == HashAlgo::Md5 {
				HashFormat::Base16
			} else {
				HashFormat::Nix32
			},
			false
		)
	);
	if print_path {
		println!("{path}");
	}
}
