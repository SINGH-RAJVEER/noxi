use std::fs;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::rc::Rc;

use nix_rs_flakeref::error::{Error, Result};
use nix_rs_flakeref::input::{Attr, Input, get_str_attr};
use nix_rs_flakeref::url::{parse_url, url_path_to_path};

use crate::mount::Mount;
use crate::store::EvaluatorStore;

pub(crate) fn download_bytes(url: &str) -> Result<Vec<u8>> {
	download_with_headers(url, &[])
}

pub(crate) fn download_with_headers(url: &str, headers: &[(String, String)]) -> Result<Vec<u8>> {
	let parsed = parse_url(url, true)?;
	if parsed.scheme == "file" {
		return Ok(fs::read(url_path_to_path(&parsed.path)?)?);
	}
	if !matches!(parsed.scheme.as_str(), "http" | "https") {
		return Err(Error::new(format!("unsupported download URL '{url}'")));
	}
	let mut request = ureq::get(url).header("User-Agent", "Nix/2.35.2");
	for (name, value) in headers {
		request = request.header(name, value);
	}
	request
		.call()
		.map_err(|e| Error::new(e.to_string()))?
		.body_mut()
		.with_config()
		.limit(1024 * 1024 * 1024)
		.read_to_vec()
		.map_err(|e| Error::new(e.to_string()))
}

pub(crate) fn get_file_accessor(_store: &EvaluatorStore, input: &Input) -> Result<(Mount, Input)> {
	let url = get_str_attr(&input.attrs, "url")?;
	let bytes = download_bytes(&url)?;
	let keep = Rc::new(tempfile::tempdir()?);
	let root = keep.path().join("file");
	fs::write(&root, bytes)?;
	Ok((
		Mount {
			root,
			filter: None,
			keep: Some(keep),
		},
		input.clone(),
	))
}

pub(crate) fn unpack(bytes: Vec<u8>) -> Result<(Mount, u64)> {
	let reader: Box<dyn Read> = if bytes.starts_with(&[0x1f, 0x8b]) {
		Box::new(flate2::read::GzDecoder::new(Cursor::new(bytes)))
	} else if bytes.starts_with(b"BZh") {
		Box::new(bzip2::read::BzDecoder::new(Cursor::new(bytes)))
	} else if bytes.starts_with(&[0xfd, b'7', b'z', b'X', b'Z', 0]) {
		Box::new(lzma_rust2::XzReader::new(Cursor::new(bytes), false))
	} else if bytes.starts_with(&[0x28, 0xb5, 0x2f, 0xfd]) {
		Box::new(zstd::stream::read::Decoder::new(Cursor::new(bytes))?)
	} else {
		Box::new(Cursor::new(bytes))
	};
	let keep = Rc::new(tempfile::tempdir()?);
	let mut archive = tar::Archive::new(reader);
	// Never preserve owner IDs, setuid bits or directory modes from an archive.
	archive.set_preserve_permissions(false);
	let mut last_modified = 0;
	for entry in archive.entries()? {
		let mut entry = entry?;
		last_modified = last_modified.max(entry.header().mtime()?);
		if !entry.unpack_in(keep.path())? {
			return Err(Error::new(
				"tarball contains an entry outside its destination",
			));
		}
	}
	let entries: Vec<PathBuf> = fs::read_dir(keep.path())?
		.map(|e| e.map(|e| e.path()))
		.collect::<std::io::Result<_>>()?;
	if entries.len() != 1 || !entries[0].is_dir() {
		return Err(Error::new(
			"tarball contains an unexpected number of top-level files",
		));
	}
	Ok((
		Mount {
			root: entries[0].clone(),
			filter: None,
			keep: Some(keep),
		},
		last_modified,
	))
}

pub(crate) fn get_tarball_accessor(
	_store: &EvaluatorStore,
	input: &Input,
) -> Result<(Mount, Input)> {
	let url = get_str_attr(&input.attrs, "url")?;
	let (mount, last_modified) = unpack(download_bytes(&url)?)?;
	let mut result = input.clone();
	result
		.attrs
		.entry("lastModified".into())
		.or_insert(Attr::Int(last_modified));
	Ok((mount, result))
}
