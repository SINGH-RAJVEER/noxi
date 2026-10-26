//! Git LFS v1 batch downloads and CppNix's path/oid cache layout.
use super::http::{Request, Response};
use crate::store::EvaluatorStore;
use nix_rs_flakeref::error::{Error, Result, warn};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::Write;
use std::path::Path;

fn hex(bytes: &[u8]) -> String {
	data_encoding::HEXLOWER.encode(bytes)
}

fn response(store: &EvaluatorStore, request: &Request) -> Result<Response> {
	let response = store.http_request(request)?;
	if !(200..300).contains(&response.status) {
		return Err(Error::new(format!(
			"unable to download '{}': HTTP error {}",
			request.url, response.status
		)));
	}
	Ok(response)
}

fn endpoint(repo: &Path) -> Result<(String, Vec<(String, String)>)> {
	let origin = super::cache::git(repo, &["config", "--get", "remote.origin.url"], &[])?;
	let mut url = nix_rs_flakeref::url::fix_git_url(&origin)?;
	if url.scheme == "ssh" {
		let authority = url
			.authority
			.as_ref()
			.ok_or_else(|| Error::new("SSH LFS URL has no authority"))?;
		let mut command = std::process::Command::new("ssh");
		if let Some(port) = authority.port {
			command.arg("-p").arg(port.to_string());
		}
		let host = if let Some(user) = &authority.user {
			format!("{user}@{}", authority.host)
		} else {
			authority.host.clone()
		};
		let output = command
			.arg(host)
			.args(["--", "git-lfs-authenticate", &url.render_path(), "download"])
			.output()?;
		if !output.status.success() {
			return Err(Error::new(
				String::from_utf8_lossy(&output.stderr).into_owned(),
			));
		}
		let json: serde_json::Value =
			serde_json::from_slice(&output.stdout).map_err(|e| Error::new(e.to_string()))?;
		let href = json["href"]
			.as_str()
			.ok_or_else(|| Error::new("no href in git-lfs-authenticate response"))?;
		let auth = json["header"]["Authorization"]
			.as_str()
			.ok_or_else(|| Error::new("no Authorization in git-lfs-authenticate response"))?;
		return Ok((href.into(), vec![("Authorization".into(), auth.into())]));
	}
	while url.path.last().is_some_and(|s| s.is_empty()) {
		url.path.pop();
	}
	if let Some(name) = url.path.last_mut()
		&& !name.ends_with(".git")
	{
		name.push_str(".git");
	}
	url.path.extend(["info".into(), "lfs".into()]);
	Ok((url.to_string(), Vec::new()))
}

pub(super) fn smudge(
	store: &EvaluatorStore,
	repo: &Path,
	revision: &str,
	root: &Path,
) -> Result<()> {
	let mut pending = vec![root.to_owned()];
	while let Some(dir) = pending.pop() {
		for entry in fs::read_dir(dir)? {
			let entry = entry?;
			let path = entry.path();
			let meta = fs::symlink_metadata(&path)?;
			if meta.is_dir() {
				pending.push(path);
				continue;
			}
			if !meta.is_file() || meta.len() >= 1024 {
				continue;
			}
			let bytes = fs::read(&path)?;
			if !bytes.starts_with(b"version https://git-lfs.github.com/spec/v1") {
				continue;
			}
			let relative = path.strip_prefix(root).unwrap().to_string_lossy();
			let attr = super::cache::git(
				repo,
				&[
					"check-attr",
					"--source",
					revision,
					"filter",
					"--",
					&relative,
				],
				&[],
			)?;
			if !attr.ends_with(": lfs") {
				continue;
			}
			let text = String::from_utf8_lossy(&bytes);
			let oid = text
				.lines()
				.find_map(|line| line.strip_prefix("oid sha256:"));
			let size = text
				.lines()
				.find_map(|line| line.strip_prefix("size "))
				.and_then(|s| s.parse::<u64>().ok());
			let (Some(oid), Some(size)) = (
				oid.filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit())),
				size,
			) else {
				warn(&format!(
					"encountered file '/{relative}' that should have been a git-lfs pointer, but is invalid"
				));
				continue;
			};
			let cache = store
				.fetch_cache_dir()
				.join("git-lfs")
				.join(hex(&Sha256::digest(relative.as_bytes())))
				.join(oid);
			let content = if cache.is_file() {
				fs::read(&cache)?
			} else {
				let (base, mut headers) = endpoint(repo)?;
				headers.extend([
					("Content-Type".into(), "application/vnd.git-lfs+json".into()),
					("Accept".into(), "application/vnd.git-lfs+json".into()),
				]);
				let request = Request {
					method: "POST".into(),
					url: format!("{}/objects/batch", base.trim_end_matches('/')),
					headers,
					body: serde_json::to_vec(
						&serde_json::json!({"operation": "download", "objects": [{"oid": oid, "size": size}]}),
					)
					.unwrap(),
				};
				let json: serde_json::Value =
					serde_json::from_slice(&response(store, &request)?.body)
						.map_err(|e| Error::new(e.to_string()))?;
				let object = &json["objects"][0];
				if object["oid"].as_str() != Some(oid) || object["size"].as_u64() != Some(size) {
					return Err(Error::new(format!(
						"LFS server returned mismatched oid/size for '/{relative}' (got oid={} size={}, expected oid={oid} size={size})",
						object["oid"].as_str().unwrap_or_default(),
						object["size"].as_u64().unwrap_or_default()
					)));
				}
				let download = &object["actions"]["download"];
				let href = download["href"].as_str().ok_or_else(|| {
					Error::new("bad json from /info/lfs/objects/batch: missing download href")
				})?;
				let mut request = Request::get(href);
				if let Some(auth) = download["header"]["Authorization"].as_str() {
					request.headers.push(("Authorization".into(), auth.into()));
				}
				let content = response(store, &request)?.body;
				if content.len() as u64 != size {
					return Err(Error::new(format!(
						"size mismatch while fetching {href}: expected {size} but got {}",
						content.len()
					)));
				}
				let actual = hex(&Sha256::digest(&content));
				if actual != oid {
					return Err(Error::new(format!(
						"hash mismatch while fetching {href}: expected sha256:{oid} but got sha256:{actual}"
					)));
				}
				fs::create_dir_all(cache.parent().unwrap())?;
				let mut file = tempfile::NamedTempFile::new_in(cache.parent().unwrap())?;
				file.write_all(&content)?;
				file.persist(cache).map_err(|e| e.error)?;
				content
			};
			fs::write(path, content)?;
		}
	}
	Ok(())
}
