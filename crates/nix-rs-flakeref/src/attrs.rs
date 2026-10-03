//! Flake references as fetcher attribute sets, for the evaluator builtins.

use crate::FlakeRef;
use serde_json::{Map, Value};
use url::Url;

pub fn parse(reference: &str) -> Result<Map<String, Value>, String> {
	if reference.contains('#') {
		return Err("unexpected fragment in flake reference".into());
	}
	let mut attrs = Map::new();
	let uri =
		if !reference.contains(':') && !reference.starts_with('/') && !reference.starts_with('.') {
			format!("flake:{reference}")
		} else {
			reference.to_owned()
		};
	let url = Url::parse(&uri).map_err(|error| error.to_string())?;
	if url.scheme() == "flake" {
		attrs.insert("type".into(), "indirect".into());
		let mut parts = url.path().split('/');
		attrs.insert("id".into(), parts.next().unwrap_or_default().into());
		if let Some(selector) = parts.next() {
			attrs.insert(
				if selector.len() == 40 && selector.bytes().all(|b| b.is_ascii_hexdigit()) {
					"rev"
				} else {
					"ref"
				}
				.into(),
				selector.into(),
			);
		}
	} else {
		let parsed: FlakeRef = uri
			.parse()
			.map_err(|error: crate::FlakeRefError| error.to_string())?;
		match parsed {
			FlakeRef::GitHub {
				owner,
				repo,
				host,
				r#ref,
				rev,
				..
			}
			| FlakeRef::GitLab {
				owner,
				repo,
				host,
				r#ref,
				rev,
				..
			}
			| FlakeRef::SourceHut {
				owner,
				repo,
				host,
				r#ref,
				rev,
				..
			} => {
				attrs.insert("type".into(), url.scheme().into());
				attrs.insert("owner".into(), owner.into());
				attrs.insert("repo".into(), repo.into());
				for (key, value) in [("host", host), ("ref", r#ref), ("rev", rev)] {
					if let Some(value) = value {
						attrs.insert(key.into(), value.into());
					}
				}
			}
			FlakeRef::Git {
				url, r#ref, rev, ..
			}
			| FlakeRef::Mercurial { url, r#ref, rev } => {
				attrs.insert(
					"type".into(),
					if uri.starts_with("hg+") {
						"mercurial"
					} else {
						"git"
					}
					.into(),
				);
				attrs.insert("url".into(), url.to_string().into());
				for (key, value) in [("ref", r#ref), ("rev", rev)] {
					if let Some(value) = value {
						attrs.insert(key.into(), value.into());
					}
				}
			}
			FlakeRef::Path {
				path,
				nar_hash,
				last_modified,
				rev,
				rev_count,
			} => {
				attrs.insert("type".into(), "path".into());
				attrs.insert("path".into(), path.to_string_lossy().as_ref().into());
				common(&mut attrs, nar_hash, last_modified, rev, rev_count);
			}
			FlakeRef::File {
				url,
				nar_hash,
				last_modified,
				rev,
				rev_count,
			}
			| FlakeRef::Tarball {
				url,
				nar_hash,
				last_modified,
				rev,
				rev_count,
			} => {
				attrs.insert(
					"type".into(),
					if uri.starts_with("tarball+")
						|| [".tar", ".tar.gz", ".tgz", ".tar.xz", ".tar.bz2", ".tar.zst"]
							.iter()
							.any(|suffix| url.path().ends_with(suffix))
					{
						"tarball"
					} else {
						"file"
					}
					.into(),
				);
				attrs.insert("url".into(), url.to_string().into());
				common(&mut attrs, nar_hash, last_modified, rev, rev_count);
			}
			FlakeRef::Indirect { id, r#ref, rev } => {
				attrs.insert("type".into(), "indirect".into());
				attrs.insert("id".into(), id.into());
				for (key, value) in [("ref", r#ref), ("rev", rev)] {
					if let Some(value) = value {
						attrs.insert(key.into(), value.into());
					}
				}
			}
		}
	}
	let mut query = std::collections::BTreeMap::new();
	for (key, value) in url.query_pairs() {
		query.entry(key).or_insert(value);
	}
	for (key, value) in query {
		match key.as_ref() {
			"dir" | "ref" | "rev" | "narHash" | "host" | "keytype" | "publicKey" | "publicKeys" => {
				attrs.insert(key.into_owned(), value.into_owned().into());
			}
			"lastModified" | "revCount" => {
				attrs.insert(
					key.into_owned(),
					Value::from(value.parse::<u64>().map_err(|error| error.to_string())?),
				);
			}
			"shallow" | "submodules" | "allRefs" | "exportIgnore" | "verifyCommit" => {
				attrs.insert(key.into_owned(), (value == "1").into());
			}
			_ => {}
		}
	}
	Ok(attrs)
}

fn common(
	attrs: &mut Map<String, Value>,
	nar_hash: Option<String>,
	last_modified: Option<u64>,
	rev: Option<String>,
	rev_count: Option<u64>,
) {
	for (key, value) in [("narHash", nar_hash), ("rev", rev)] {
		if let Some(value) = value {
			attrs.insert(key.into(), value.into());
		}
	}
	for (key, value) in [("lastModified", last_modified), ("revCount", rev_count)] {
		if let Some(value) = value {
			attrs.insert(key.into(), value.into());
		}
	}
}

pub fn to_string(attrs: &Map<String, Value>) -> Result<String, String> {
	let string = |key: &str| {
		attrs
			.get(key)
			.and_then(Value::as_str)
			.ok_or_else(|| format!("flake reference requires string attribute '{key}'"))
	};
	let kind = string("type")?;
	let mut consumed = vec!["type"];
	let base = match kind {
		"github" | "gitlab" | "sourcehut" => {
			consumed.extend(["owner", "repo", "ref", "rev"]);
			let selector = attrs
				.get("rev")
				.or_else(|| attrs.get("ref"))
				.map(|value| value.as_str().ok_or("flake selector must be a string"))
				.transpose()?;
			format!(
				"{kind}:{}/{}{}",
				string("owner")?,
				string("repo")?,
				selector.map(|s| format!("/{s}")).unwrap_or_default()
			)
		}
		"path" => {
			consumed.push("path");
			format!("path:{}", string("path")?)
		}
		"indirect" => {
			consumed.push("id");
			format!("flake:{}", string("id")?)
		}
		"git" => {
			consumed.push("url");
			let url = string("url")?;
			if url.starts_with("git:") {
				url.to_owned()
			} else {
				format!("git+{url}")
			}
		}
		"mercurial" => {
			consumed.push("url");
			format!("hg+{}", string("url")?)
		}
		"file" | "tarball" => {
			consumed.push("url");
			let url = string("url")?;
			if kind == "file" {
				url.to_owned()
			} else {
				format!("tarball+{url}")
			}
		}
		_ => return Err(format!("unsupported flake input type '{kind}'")),
	};
	let mut url = Url::parse(&base).map_err(|error| error.to_string())?;
	for (key, value) in attrs {
		if consumed.contains(&key.as_str()) {
			continue;
		}
		let value = match value {
			Value::String(value) => value.clone(),
			Value::Bool(value) => if *value { "1" } else { "0" }.into(),
			Value::Number(value) if value.as_u64().is_some() => value.to_string(),
			_ => return Err(format!("invalid flake reference attribute '{key}'")),
		};
		url.query_pairs_mut().append_pair(key, &value);
	}
	let result = url.to_string();
	parse(&result)?;
	Ok(result)
}
