use std::fs;
use std::io::{Cursor, Read};
use std::path::PathBuf;
use std::rc::Rc;

use nix_compat::store_path::StorePath;
use noxi_flakeref::error::{Error, Result};
use noxi_flakeref::input::{Attr, Attrs, Input, get_str_attr};
use noxi_flakeref::url::{parse_url, url_path_to_path};

use crate::mount::Mount;
use crate::store::EvaluatorStore;

pub(crate) fn download_bytes(store: &EvaluatorStore, url: &str) -> Result<Vec<u8>> {
	download_with_headers(store, url, &[])
}

pub(crate) fn download_with_headers(
	store: &EvaluatorStore,
	url: &str,
	headers: &[(String, String)],
) -> Result<Vec<u8>> {
	let (mount, _) = download_file(store, url, "source", headers)?;
	Ok(fs::read(mount.root)?)
}

fn request(
	store: &EvaluatorStore,
	url: &str,
	headers: &[(String, String)],
	etag: Option<&str>,
) -> Result<super::http::Response> {
	let parsed = parse_url(url, true)?;
	if parsed.scheme == "file" {
		return Ok(super::http::Response {
			status: 200,
			effective_url: url.into(),
			body: fs::read(url_path_to_path(&parsed.path)?)?,
			..Default::default()
		});
	}
	if !matches!(parsed.scheme.as_str(), "http" | "https") {
		return Err(Error::new(format!("unsupported download URL '{url}'")));
	}
	let mut request = super::http::Request::get(url);
	request.headers = headers.to_vec();
	if let Some(etag) = etag.filter(|s| !s.is_empty()) {
		request.headers.push(("If-None-Match".into(), etag.into()));
	}
	let response = store.http_request(&request)?;
	if response.status != 304 && !(200..300).contains(&response.status) {
		return Err(super::http::download_error(url, &response));
	}
	Ok(response)
}

/// Download and cache a flat file, retaining its effective redirect URL.
pub fn download_file(
	store: &EvaluatorStore,
	url: &str,
	name: &str,
	headers: &[(String, String)],
) -> Result<(Mount, String)> {
	let cache = store.fetch_cache()?;
	let key: Attrs = [
		("url".into(), url.into()),
		("name".into(), name.into()),
		("store".into(), nix_compat::store_path::store_dir().into()),
	]
	.into();
	let cached = cache.lookup_expired("file", &key, store.fetch_settings().tarball_ttl)?;
	let cached_mount = cached.as_ref().and_then(|entry| {
		let path = get_str_attr(&entry.value, "storePath").ok()?;
		let path = StorePath::from_bytes(path.as_bytes()).ok()?;
		store.valid_store_path(&path).map(Mount::new).or_else(|| {
			let path = store
				.fetch_cache_dir()
				.join("noxi-downloads")
				.join(path.to_string());
			path.is_file().then(|| Mount::new(path))
		})
	});
	if let Some(mount) = &cached_mount
		&& (store.fetch_config.borrow().offline || !cached.as_ref().unwrap().expired)
	{
		return Ok((
			mount.clone(),
			get_str_attr(&cached.as_ref().unwrap().value, "url")?,
		));
	}
	let etag = cached
		.as_ref()
		.and_then(|c| get_str_attr(&c.value, "etag").ok());
	let response = match request(store, url, headers, etag.as_deref()) {
		Ok(response) => response,
		Err(e) => {
			if let Some(mount) = cached_mount {
				noxi_flakeref::warn(&format!("{e}; using cached version"));
				return Ok((mount, get_str_attr(&cached.as_ref().unwrap().value, "url")?));
			} else {
				return Err(e);
			}
		}
	};
	if response.status == 304 {
		let mount =
			cached_mount.ok_or_else(|| Error::new("HTTP 304 without a valid cached file"))?;
		cache.upsert("file", &key, &cached.as_ref().unwrap().value)?;
		return Ok((mount, get_str_attr(&cached.as_ref().unwrap().value, "url")?));
	}
	let keep = Rc::new(tempfile::tempdir()?);
	let root = keep.path().join("file");
	fs::write(&root, response.body)?;
	let captured = store
		.capture_import(&root, name, false, None)
		.map_err(|e| Error::new(e.to_string()))?;
	let path = store
		.import_captured(captured, None)
		.map_err(|e| Error::new(e.to_string()))?;
	let physical = if let Some(physical) = store.valid_store_path(&path) {
		physical
	} else {
		let dir = store.fetch_cache_dir().join("noxi-downloads");
		fs::create_dir_all(&dir)?;
		let target = dir.join(path.to_string());
		let mut file = tempfile::NamedTempFile::new_in(&dir)?;
		std::io::copy(&mut fs::File::open(&root)?, &mut file)?;
		file.persist(&target).map_err(|e| e.error)?;
		target
	};
	let value: Attrs = [
		("storePath".into(), path.to_string().into()),
		("url".into(), response.effective_url.clone().into()),
		(
			"etag".into(),
			response
				.headers
				.get("etag")
				.cloned()
				.unwrap_or_default()
				.into(),
		),
	]
	.into();
	cache.upsert("file", &key, &value)?;
	if response.effective_url != url {
		let mut key = key;
		key.insert("url".into(), response.effective_url.clone().into());
		cache.upsert("file", &key, &value)?;
	}
	Ok((Mount::new(physical), response.effective_url))
}

pub(crate) fn get_file_accessor(store: &EvaluatorStore, input: &Input) -> Result<(Mount, Input)> {
	let url = get_str_attr(&input.attrs, "url")?;
	Ok((
		download_file(store, &url, &input.get_name(), &[])?.0,
		input.clone(),
	))
}

pub fn unpack(bytes: Vec<u8>) -> Result<(Mount, u64)> {
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
	store: &EvaluatorStore,
	input: &Input,
) -> Result<(Mount, Input)> {
	let url = get_str_attr(&input.attrs, "url")?;
	let (mount, last_modified) = download_tarball(store, &url, &[], &input.get_name())?;
	let mut result = input.clone();
	result
		.attrs
		.entry("lastModified".into())
		.or_insert(Attr::Int(last_modified));
	Ok((mount, result))
}

pub(super) fn download_tarball(
	store: &EvaluatorStore,
	url: &str,
	headers: &[(String, String)],
	name: &str,
) -> Result<(Mount, u64)> {
	let cache = store.fetch_cache()?;
	let key: Attrs = [("url".into(), url.into())].into();
	let cached = cache.lookup_expired("tarball", &key, store.fetch_settings().tarball_ttl)?;
	let fresh = cached
		.as_ref()
		.is_some_and(|entry| store.fetch_config.borrow().offline || !entry.expired);
	if fresh {
		let value = &cached.as_ref().unwrap().value;
		let tree = super::tree_hash_entry(get_str_attr(value, "treeHash")?);
		if let Some(mount) = super::stored_source(store, &cache, &tree, name)? {
			return Ok((
				mount,
				noxi_flakeref::input::maybe_get_int_attr(value, "lastModified")?.unwrap_or(0),
			));
		}
	}
	let trees = super::cache::TreeCache::open(&store.fetch_cache_dir())?;
	let mount = match &cached {
		Some(entry) => {
			let tree_hash = get_str_attr(&entry.value, "treeHash")?;
			let mount = trees.get(&tree_hash)?;
			if let Some(mount) = &mount {
				store.record_nar_hash(&mount.root, super::tree_hash_entry(tree_hash));
			}
			mount
		}
		None => None,
	};
	if let Some(mount) = &mount
		&& fresh
	{
		return Ok((
			mount.clone(),
			noxi_flakeref::input::maybe_get_int_attr(
				&cached.as_ref().unwrap().value,
				"lastModified",
			)?
			.unwrap_or(0),
		));
	}
	let etag = cached
		.as_ref()
		.filter(|_| mount.is_some())
		.and_then(|c| get_str_attr(&c.value, "etag").ok());
	let response = request(store, url, headers, etag.as_deref())?;
	if response.status == 304 {
		let mount = mount.ok_or_else(|| Error::new("HTTP 304 without a valid cached tarball"))?;
		let value = cached.unwrap().value;
		cache.upsert("tarball", &key, &value)?;
		return Ok((
			mount,
			noxi_flakeref::input::maybe_get_int_attr(&value, "lastModified")?.unwrap_or(0),
		));
	}
	let (mount, modified) = unpack(response.body)?;
	let tree_hash = trees.insert(&mount.root)?;
	store.record_nar_hash(&mount.root, super::tree_hash_entry(tree_hash.clone()));
	let value: Attrs = [
		("treeHash".into(), tree_hash.into()),
		("lastModified".into(), modified.into()),
		(
			"etag".into(),
			response
				.headers
				.get("etag")
				.cloned()
				.unwrap_or_default()
				.into(),
		),
	]
	.into();
	cache.upsert("tarball", &key, &value)?;
	if response.effective_url != url {
		let mut key = key;
		key.insert("url".into(), response.effective_url.into());
		cache.upsert("tarball", &key, &value)?;
	}
	Ok((mount, modified))
}
