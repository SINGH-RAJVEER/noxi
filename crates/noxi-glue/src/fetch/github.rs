use noxi_flakeref::error::{Error, Result};
use noxi_flakeref::input::{
	Attr, FetchSettings, Input, Scheme, get_str_attr, git_rev, maybe_get_str_attr, parse_rev,
};

use crate::mount::Mount;
use crate::store::EvaluatorStore;

fn api_rev(scheme: Scheme, bytes: &[u8]) -> Result<String> {
	let json: serde_json::Value =
		serde_json::from_slice(bytes).map_err(|e| Error::new(e.to_string()))?;
	let rev = match scheme {
		Scheme::GitHub => json.get("sha").and_then(|v| v.as_str()),
		Scheme::GitLab => {
			if json.as_array().is_some_and(|a| a.is_empty()) {
				return Err(Error::new(
					"No commits returned by GitLab API -- does the git ref really exist?",
				));
			}
			json.get(0)
				.and_then(|v| v.get("id"))
				.and_then(|v| v.as_str())
		}
		_ => None,
	}
	.ok_or_else(|| Error::new("Unexpected response received from forge API"))?;
	Ok(git_rev(&parse_rev(rev)?))
}

fn auth(
	settings: &FetchSettings,
	scheme: Scheme,
	host: &str,
	owner: &str,
	repo: &str,
) -> Vec<(String, String)> {
	let path = format!("{host}/{owner}/{repo}");
	let token = settings
		.access_tokens
		.iter()
		.filter(|(key, _)| {
			path.starts_with(key.as_str())
				&& (key.len() == path.len() || path.as_bytes()[key.len()] == b'/')
		})
		.max_by_key(|(key, _)| key.len())
		.map(|(_, token)| token);
	token
		.map(|token| match scheme {
			Scheme::GitHub => ("Authorization".into(), format!("token {token}")),
			Scheme::GitLab => {
				let (kind, token) = token.split_once(':').unwrap_or((token, token));
				match kind {
					"OAuth2" => ("Authorization".into(), format!("Bearer {token}")),
					"PAT" => ("Private-token".into(), token.into()),
					other => {
						noxi_flakeref::warn(&format!("Unrecognized GitLab token type {other}"));
						(other.into(), token.into())
					}
				}
			}
			_ => ("Authorization".into(), format!("Bearer {token}")),
		})
		.into_iter()
		.collect()
}

fn api_url(scheme: Scheme, host: &str, owner: &str, repo: &str, reference: &str) -> String {
	match scheme {
		Scheme::GitHub if host == "github.com" => {
			format!("https://api.{host}/repos/{owner}/{repo}/commits/{reference}")
		}
		Scheme::GitHub => format!("https://{host}/api/v3/repos/{owner}/{repo}/commits/{reference}"),
		Scheme::GitLab => format!(
			"https://{host}/api/v4/projects/{owner}%2F{repo}/repository/commits?ref_name={reference}"
		),
		_ => format!("https://{host}/{owner}/{repo}/info/refs"),
	}
}

fn archive_url(
	scheme: Scheme,
	host: &str,
	owner: &str,
	repo: &str,
	rev: &str,
	authenticated: bool,
) -> String {
	match scheme {
		Scheme::GitHub if host == "github.com" && !authenticated => {
			format!("https://{host}/{owner}/{repo}/archive/{rev}.tar.gz")
		}
		Scheme::GitHub if host == "github.com" => {
			format!("https://api.{host}/repos/{owner}/{repo}/tarball/{rev}")
		}
		Scheme::GitHub => format!("https://{host}/api/v3/repos/{owner}/{repo}/tarball/{rev}"),
		Scheme::GitLab => format!(
			"https://{host}/api/v4/projects/{owner}%2F{repo}/repository/archive.tar.gz?sha={rev}"
		),
		_ => format!("https://{host}/{owner}/{repo}/archive/{rev}.tar.gz"),
	}
}

pub(crate) fn get_accessor(
	store: &EvaluatorStore,
	settings: &FetchSettings,
	scheme: Scheme,
	input: &Input,
) -> Result<(Mount, Input)> {
	let owner = get_str_attr(&input.attrs, "owner")?;
	let repo = get_str_attr(&input.attrs, "repo")?;
	let host = maybe_get_str_attr(&input.attrs, "host")?.unwrap_or_else(|| {
		match scheme {
			Scheme::GitHub => "github.com",
			Scheme::GitLab => "gitlab.com",
			_ => "git.sr.ht",
		}
		.into()
	});
	let headers = auth(settings, scheme, &host, &owner, &repo);
	let rev = match input.get_rev()? {
		Some(rev) => git_rev(&rev),
		None => {
			let reference = input.get_ref()?.unwrap_or_else(|| "HEAD".into());
			let bytes = super::tarball::download_with_headers(
				store,
				&api_url(scheme, &host, &owner, &repo, &reference),
				&headers,
			)?;
			if scheme == Scheme::SourceHut {
				let target = if reference == "HEAD" {
					let head = super::tarball::download_with_headers(
						store,
						&format!("https://{host}/{owner}/{repo}/HEAD"),
						&headers,
					)?;
					String::from_utf8_lossy(&head)
						.trim()
						.strip_prefix("ref: ")
						.ok_or_else(|| Error::bad_url("couldn't resolve SourceHut HEAD"))?
						.to_owned()
				} else {
					reference.clone()
				};
				let rev = String::from_utf8_lossy(&bytes)
					.lines()
					.find_map(|line| {
						let (rev, name) = line.split_once('\t')?;
						(name == target
							|| name == format!("refs/heads/{target}")
							|| name == format!("refs/tags/{target}"))
						.then(|| rev.to_owned())
					})
					.ok_or_else(|| {
						Error::bad_url(format!("couldn't find SourceHut ref '{reference}'"))
					})?;
				git_rev(&parse_rev(&rev)?)
			} else {
				api_rev(scheme, &bytes)?
			}
		}
	};
	let url = archive_url(scheme, &host, &owner, &repo, &rev, !headers.is_empty());
	let (mount, modified) = super::tarball::download_tarball(store, &url, &headers)?;
	let mut result = input.clone();
	result.attrs.remove("ref");
	result.attrs.insert("rev".into(), rev.into());
	result
		.attrs
		.insert("lastModified".into(), Attr::Int(modified));
	Ok((mount, result))
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn forge_requests_and_revision_responses_follow_cppnix() {
		assert_eq!(
			api_url(Scheme::GitHub, "github.com", "owner", "repo", "main"),
			"https://api.github.com/repos/owner/repo/commits/main"
		);
		assert_eq!(
			archive_url(Scheme::GitHub, "github.com", "owner", "repo", "rev", false),
			"https://github.com/owner/repo/archive/rev.tar.gz"
		);
		assert_eq!(
			archive_url(Scheme::GitHub, "github.com", "owner", "repo", "rev", true),
			"https://api.github.com/repos/owner/repo/tarball/rev"
		);
		let rev = "0123456789abcdef0123456789abcdef01234567";
		assert_eq!(
			api_rev(Scheme::GitHub, format!("{{\"sha\":\"{rev}\"}}").as_bytes()).unwrap(),
			rev
		);
		assert_eq!(
			api_rev(Scheme::GitLab, format!("[{{\"id\":\"{rev}\"}}]").as_bytes()).unwrap(),
			rev
		);
		assert!(api_rev(Scheme::GitLab, b"[]").is_err());
		let mut settings = FetchSettings::default();
		settings
			.access_tokens
			.insert("github.com".into(), "host".into());
		settings
			.access_tokens
			.insert("github.com/owner/repo".into(), "repo".into());
		settings
			.access_tokens
			.insert("github.com/owner/repository".into(), "wrong".into());
		assert_eq!(
			auth(&settings, Scheme::GitHub, "github.com", "owner", "repo"),
			[("Authorization".into(), "token repo".into())]
		);
	}
}
