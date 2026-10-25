use nix_rs_flakeref::error::{Error, Result};
use nix_rs_flakeref::input::{
	Attr, FetchSettings, Input, Scheme, get_str_attr, git_rev, maybe_get_str_attr,
};

use crate::mount::Mount;
use crate::store::EvaluatorStore;

/// Fetch locked forge archives. Resolving moving forge refs is a separate API operation.
pub(crate) fn get_accessor(
	_store: &EvaluatorStore,
	_settings: &FetchSettings,
	scheme: Scheme,
	input: &Input,
) -> Result<(Mount, Input)> {
	let rev = input
		.get_rev()?
		.ok_or_else(|| Error::new("resolving unlocked forge references is not implemented"))?;
	let rev = git_rev(&rev);
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
	let url = match scheme {
		Scheme::GitHub if host == "github.com" => {
			format!("https://api.github.com/repos/{owner}/{repo}/tarball/{rev}")
		}
		Scheme::GitHub => format!("https://{host}/api/v3/repos/{owner}/{repo}/tarball/{rev}"),
		Scheme::GitLab => format!(
			"https://{host}/api/v4/projects/{}%2F{repo}/repository/archive.tar.gz?sha={rev}",
			owner.replace('/', "%2F")
		),
		_ => format!("https://{host}/{owner}/{repo}/archive/{rev}.tar.gz"),
	};
	let (mount, modified) = super::tarball::unpack(super::tarball::download_bytes(&url)?)?;
	let mut result = input.clone();
	result.attrs.remove("ref");
	result
		.attrs
		.entry("lastModified".into())
		.or_insert(Attr::Int(modified));
	Ok((mount, result))
}
