use std::collections::BTreeSet;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::rc::Rc;

use nix_rs_flakeref::error::{Error, Result, warn};
use nix_rs_flakeref::input::{
	Attr, FetchSettings, Input, get_str_attr, git_rev, maybe_get_bool_attr,
};
use nix_rs_flakeref::url::{fix_git_url, url_path_to_path};

use crate::mount::{Mount, TreeFilter};
use crate::store::EvaluatorStore;

fn run(repo: &Path, args: &[&str]) -> Result<Vec<u8>> {
	let output = Command::new("git")
		.arg("-C")
		.arg(repo)
		.args(args)
		.env("GIT_TERMINAL_PROMPT", "0")
		.output()?;
	if !output.status.success() {
		return Err(Error::new(
			String::from_utf8_lossy(&output.stderr).trim().to_owned(),
		));
	}
	Ok(output.stdout)
}

fn text(repo: &Path, args: &[&str]) -> Result<String> {
	Ok(String::from_utf8_lossy(&run(repo, args)?).trim().to_owned())
}

fn boolean(input: &Input, name: &str) -> Result<bool> {
	Ok(maybe_get_bool_attr(&input.attrs, name)?.unwrap_or(false))
}

/// Fetch into an evaluation-owned bare repository. No shell interpolation,
/// checkout hooks, credentials prompts, or persistent cache are involved.
fn remote_repo(
	url: &str,
	input: &Input,
	directory: &Path,
	ttl: u64,
	offline: bool,
) -> Result<PathBuf> {
	use sha2::{Digest, Sha256};
	let shallow = boolean(input, "shallow")?;
	let hash = nix_compat::nixhash::NixHash::Sha256(Sha256::digest(url.as_bytes()).into())
		.to_nix_nixbase32();
	let name =
		hash.strip_prefix("sha256:").unwrap().to_owned() + if shallow { "-shallow" } else { "" };
	let repo = directory.join("gitv3").join(name);
	fs::create_dir_all(repo.parent().unwrap())?;
	let lock = fs::OpenOptions::new()
		.create(true)
		.truncate(false)
		.write(true)
		.open(repo.with_extension("lock"))?;
	lock.lock()?;
	fs::create_dir_all(&repo)?;
	if !repo.join("config").exists() {
		run(&repo, &["init", "--bare", "--quiet"])?;
	}
	run(&repo, &["config", "remote.origin.url", url])?;
	let mut reference = input.get_ref()?.unwrap_or_else(|| "HEAD".into());
	let revision = input.get_rev()?.map(|rev| git_rev(&rev));
	if revision
		.as_ref()
		.is_some_and(|rev| run(&repo, &["cat-file", "-e", &format!("{rev}^{{commit}}")]).is_ok())
	{
		return Ok(repo);
	}
	if reference == "HEAD" && revision.is_none() {
		if let Ok(head) = text(&repo, &["symbolic-ref", "HEAD"])
			&& run(&repo, &["rev-parse", "--verify", "HEAD"]).is_ok()
		{
			reference = head;
		} else if !offline {
			let head = text(&repo, &["ls-remote", "--symref", "--", url, "HEAD"])?;
			if let Some(target) = head.lines().find_map(|line| {
				line.strip_prefix("ref: ")
					.and_then(|line| line.split_whitespace().next())
			}) {
				reference = target.into();
				run(&repo, &["symbolic-ref", "HEAD", target])?;
			}
		}
	}
	let ref_file = if reference.starts_with("refs/") || reference == "HEAD" {
		repo.join(&reference)
	} else {
		repo.join("refs/heads").join(&reference)
	};
	let valid = run(&repo, &["rev-parse", "--verify", &reference]).is_ok();
	let fresh = fs::metadata(&ref_file)
		.and_then(|m| m.modified())
		.ok()
		.and_then(|m| m.elapsed().ok())
		.is_some_and(|age| age.as_secs() < ttl);
	if revision.is_none() && valid && (offline || fresh && !boolean(input, "allRefs")?) {
		return Ok(repo);
	}
	if offline {
		return Err(Error::new(format!(
			"cannot fetch Git input '{}' in offline mode",
			input.display()
		)));
	}
	let fetch_ref = if boolean(input, "allRefs")? && !shallow {
		"+refs/*:refs/*"
	} else if let Some(revision) = &revision {
		revision
	} else {
		""
	};
	let refspec = if reference.starts_with("refs/") {
		format!("{reference}:{reference}")
	} else if reference == "HEAD" {
		"HEAD:HEAD".into()
	} else {
		format!("refs/heads/{reference}:refs/heads/{reference}")
	};
	let fetch_ref = if fetch_ref.is_empty() {
		&refspec
	} else {
		fetch_ref
	};
	let mut args = vec!["fetch", "--quiet", "--force"];
	if shallow {
		args.push("--depth=1");
	}
	args.extend(["--", url, fetch_ref]);
	if let Err(error) = run(&repo, &args) {
		if !valid || revision.is_some() {
			return Err(error);
		}
		warn(&format!(
			"could not update local clone of Git repository '{url}'; continuing with the most recent version"
		));
	}
	if boolean(input, "allRefs")? && !shallow {
		run(&repo, &["fetch", "--quiet", "--", url, &refspec])?;
	}
	if revision.is_none()
		&& let Ok(file) = fs::OpenOptions::new().write(true).open(ref_file)
	{
		file.set_modified(std::time::SystemTime::now())?;
	}
	Ok(repo)
}

fn ignored(repo: &Path, paths: &BTreeSet<PathBuf>) -> Result<BTreeSet<PathBuf>> {
	use std::io::Write;
	use std::process::Stdio;
	let mut command = Command::new("git")
		.arg("-C")
		.arg(repo)
		.args(["check-attr", "-z", "--stdin", "export-ignore"])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.spawn()?;
	let mut stdin = command.stdin.take().unwrap();
	let mut names = BTreeSet::new();
	for path in paths {
		for parent in path.ancestors().filter(|p| !p.as_os_str().is_empty()) {
			names.insert(parent.to_owned());
		}
	}
	// Feed concurrently with reading stdout so large tracked trees cannot fill both pipes.
	let writer = std::thread::spawn(move || -> std::io::Result<()> {
		for path in names {
			stdin.write_all(path.as_os_str().as_bytes())?;
			stdin.write_all(&[0])?;
		}
		Ok(())
	});
	let output = command.wait_with_output()?;
	writer
		.join()
		.map_err(|_| Error::new("Git attribute writer panicked"))??;
	if !output.status.success() {
		return Err(Error::new(
			String::from_utf8_lossy(&output.stderr).into_owned(),
		));
	}
	let fields: Vec<_> = output.stdout.split(|b| *b == 0).collect();
	Ok(fields
		.chunks_exact(3)
		.filter(|f| f[2] == b"set")
		.map(|f| PathBuf::from(std::ffi::OsStr::from_bytes(f[0])))
		.collect())
}

/// Export immutable Git objects without running checkout filters or copying `.git`.
pub(super) fn export(
	repo: &Path,
	rev: &str,
	destination: &Path,
	submodules: bool,
	export_ignore: bool,
) -> Result<()> {
	fs::create_dir_all(destination)?;
	if export_ignore {
		let bytes = run(repo, &["archive", "--format=tar", rev])?;
		tar::Archive::new(std::io::Cursor::new(bytes)).unpack(destination)?;
		return Ok(());
	}
	let entries = run(repo, &["ls-tree", "-rtz", "--full-tree", rev])?;
	for entry in entries.split(|b| *b == 0).filter(|e| !e.is_empty()) {
		let tab = entry
			.iter()
			.position(|b| *b == b'\t')
			.ok_or_else(|| Error::new("invalid Git tree entry"))?;
		let header = String::from_utf8_lossy(&entry[..tab]);
		let mut fields = header.split(' ');
		let mode = fields.next().unwrap_or_default();
		let _kind = fields.next();
		let object = fields
			.next()
			.ok_or_else(|| Error::new("missing Git object"))?;
		let relative = Path::new(std::ffi::OsStr::from_bytes(&entry[tab + 1..]));
		let path = destination.join(relative);
		fs::create_dir_all(path.parent().unwrap())?;
		if mode == "040000" {
			fs::create_dir_all(&path)?;
		} else if mode == "160000" {
			fs::create_dir_all(&path)?;
			if submodules {
				export(&repo.join(relative), object, &path, true, false)?;
			}
		} else {
			let bytes = run(repo, &["cat-file", "blob", object])?;
			if mode == "120000" {
				symlink(std::ffi::OsStr::from_bytes(&bytes), &path)?;
			} else {
				fs::write(&path, bytes)?;
				fs::set_permissions(
					path,
					fs::Permissions::from_mode(if mode == "100755" { 0o755 } else { 0o644 }),
				)?;
			}
		}
	}
	Ok(())
}

fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
	fs::create_dir_all(destination)?;
	for entry in fs::read_dir(source)? {
		let entry = entry?;
		let from = entry.path();
		let to = destination.join(entry.file_name());
		let meta = fs::symlink_metadata(&from)?;
		if meta.is_dir() {
			copy_tree(&from, &to)?;
		} else if meta.file_type().is_symlink() {
			symlink(fs::read_link(from)?, to)?;
		} else {
			fs::copy(from, to)?;
		}
	}
	Ok(())
}

fn resolve_submodule_url(origin: &str, location: &str) -> Result<String> {
	if !location.starts_with("./") && !location.starts_with("../") {
		return Ok(location.into());
	}
	let mut parsed = fix_git_url(origin)?;
	for elem in location.split('/') {
		match elem {
			"." | "" => {}
			".." => {
				parsed.path.pop();
			}
			value => parsed.path.push(value.into()),
		}
	}
	Ok(parsed.to_string())
}

fn export_submodules(
	store: &EvaluatorStore,
	settings: &FetchSettings,
	input: &Input,
	repo: &Path,
	revision: &str,
	destination: &Path,
) -> Result<()> {
	let blob = format!("{revision}:.gitmodules");
	let Ok(paths) = text(
		repo,
		&[
			"config",
			"--blob",
			&blob,
			"--get-regexp",
			"^submodule\\..*\\.path$",
		],
	) else {
		return Ok(());
	};
	let origin = text(repo, &["config", "--get", "remote.origin.url"])
		.unwrap_or(get_str_attr(&input.attrs, "url")?);
	for line in paths.lines() {
		let Some((key, path)) = line.split_once(char::is_whitespace) else {
			return Err(Error::new("invalid submodule path entry"));
		};
		let name = key.strip_suffix(".path").unwrap();
		let location = text(
			repo,
			&["config", "--blob", &blob, "--get", &format!("{name}.url")],
		)?;
		let entry = text(repo, &["ls-tree", revision, "--", path])?;
		let Some(header) = entry.split_once('\t').map(|e| e.0) else {
			continue;
		};
		let Some(rev) = header.strip_prefix("160000 commit ") else {
			continue;
		};
		let url = resolve_submodule_url(&origin, &location)?;
		let attrs = [
			("type".into(), "git".into()),
			("url".into(), url.into()),
			("rev".into(), rev.into()),
			("allRefs".into(), Attr::Bool(true)),
			("submodules".into(), Attr::Bool(true)),
			("lfs".into(), Attr::Bool(boolean(input, "lfs")?)),
		]
		.into();
		let submodule = Input::from_attrs(settings, attrs)?;
		let (tree, _) = get_accessor(store, settings, &submodule)?;
		copy_tree(&tree.root, &destination.join(path))?;
	}
	Ok(())
}

pub(crate) fn get_accessor(
	store: &EvaluatorStore,
	settings: &FetchSettings,
	input: &Input,
) -> Result<(Mount, Input)> {
	let url = get_str_attr(&input.attrs, "url")?;
	let parsed = fix_git_url(&url)?;
	if parsed.scheme != "file" {
		let repo = remote_repo(
			&url,
			input,
			&store.fetch_cache_dir(),
			settings.tarball_ttl,
			store.fetch_config.borrow().offline,
		)?;
		let mut local = input.clone();
		local
			.attrs
			.insert("url".into(), format!("file://{}", repo.display()).into());
		if local.get_rev()?.is_none() {
			local.attrs.insert(
				"rev".into(),
				text(
					&repo,
					&[
						"rev-parse",
						"--verify",
						input.get_ref()?.as_deref().unwrap_or("HEAD"),
					],
				)?
				.into(),
			);
		}
		let (mount, mut locked) = get_accessor(store, settings, &local)?;
		locked.attrs.insert("url".into(), url.into());
		return Ok((mount, locked));
	}
	let repo = fs::canonicalize(url_path_to_path(&parsed.path)?)?;
	if boolean(input, "verifyCommit")?
		|| input.attrs.contains_key("publicKey")
		|| input.attrs.contains_key("publicKeys")
	{
		return Err(Error::new("verified fetches are not implemented"));
	}
	let submodules = boolean(input, "submodules")?;
	let export_ignore = boolean(input, "exportIgnore")?;
	if submodules && export_ignore {
		return Err(Error::new(
			"exportIgnore and submodules are not supported together yet",
		));
	}
	let shallow = boolean(input, "shallow")?;
	if !shallow && text(&repo, &["rev-parse", "--is-shallow-repository"])? == "true" {
		return Err(Error::new(format!(
			"'{}' is a shallow Git repository, so 'revCount' is not available",
			repo.display()
		)));
	}
	let worktree =
		input.get_ref()?.is_none() && input.get_rev()?.is_none() && repo.join(".git").exists();
	let mut result = input.clone();
	let rev;
	let mount;
	if worktree {
		let head = text(&repo, &["rev-parse", "--verify", "HEAD"]).ok();
		let tracked: BTreeSet<PathBuf> = run(&repo, &["ls-files", "-z", "--cached"])?
			.split(|b| *b == 0)
			.filter(|e| !e.is_empty())
			.map(|e| PathBuf::from(std::ffi::OsStr::from_bytes(e)))
			.collect();
		let mut visible = tracked.clone();
		// Gitlinks are mount points. Without submodules their content is an empty directory.
		let stage = run(&repo, &["ls-files", "--stage", "-z"])?;
		for entry in stage
			.split(|b| *b == 0)
			.filter(|e| e.starts_with(b"160000 "))
		{
			let tab = entry.iter().position(|b| *b == b'\t').unwrap();
			let rel = PathBuf::from(std::ffi::OsStr::from_bytes(&entry[tab + 1..]));
			visible.remove(&rel);
			if submodules && repo.join(&rel).join(".git").exists() {
				let paths = run(
					&repo.join(&rel),
					&["ls-files", "-z", "--recurse-submodules"],
				)?;
				visible.extend(
					paths
						.split(|b| *b == 0)
						.filter(|e| !e.is_empty())
						.map(|e| rel.join(std::ffi::OsStr::from_bytes(e))),
				);
			} else if submodules {
				return Err(Error::new(format!(
					"Failed to fetch git repository 'file://{}'",
					repo.join(&rel).display()
				)));
			}
		}
		let ignored = if export_ignore {
			ignored(&repo, &tracked)?
		} else {
			BTreeSet::new()
		};
		let dirty = head.is_some()
			&& !run(
				&repo,
				&[
					"status",
					"--porcelain",
					"--untracked-files=no",
					if submodules {
						"--ignore-submodules=untracked"
					} else {
						"--ignore-submodules=all"
					},
				],
			)?
			.is_empty();
		if dirty {
			if !settings.allow_dirty {
				return Err(Error::new(format!(
					"Git tree '{}' is dirty",
					repo.display()
				)));
			}
			if settings.warn_dirty {
				warn(&format!("Git tree '{}' is dirty", repo.display()));
			}
			if let Some(head) = &head {
				result
					.attrs
					.insert("dirtyRev".into(), format!("{head}-dirty").into());
				result.attrs.insert(
					"dirtyShortRev".into(),
					format!("{}-dirty", &head[..7]).into(),
				);
			}
		} else {
			let head = head.clone().unwrap_or_else(|| "0".repeat(40));
			result.attrs.insert("rev".into(), head.into());
			if let Ok(branch) = text(&repo, &["symbolic-ref", "HEAD"]) {
				result.attrs.insert("ref".into(), branch.into());
			}
		}
		rev = head;
		// Materialise only visible files so directory listings and hashing agree, including empty gitlinks.
		let keep = Rc::new(tempfile::tempdir()?);
		for rel in &visible {
			if rel.ancestors().any(|p| ignored.contains(p)) {
				continue;
			}
			let from = repo.join(rel);
			let Ok(metadata) = fs::symlink_metadata(&from) else {
				continue;
			};
			let to = keep.path().join(rel);
			fs::create_dir_all(to.parent().unwrap())?;
			if metadata.file_type().is_symlink() {
				symlink(fs::read_link(from)?, to)?;
			} else if metadata.is_file() {
				fs::copy(from, to)?;
			}
		}
		for entry in stage
			.split(|b| *b == 0)
			.filter(|e| submodules && e.starts_with(b"160000 "))
		{
			let tab = entry.iter().position(|b| *b == b'\t').unwrap();
			fs::create_dir_all(
				keep.path()
					.join(std::ffi::OsStr::from_bytes(&entry[tab + 1..])),
			)?;
		}
		mount = Mount {
			root: keep.path().to_owned(),
			filter: Some(Rc::new(TreeFilter {
				tracked,
				ignored,
				repo: repo.clone(),
			})),
			keep: Some(keep),
		};
	} else {
		let requested = input
			.get_rev()?
			.map(|r| git_rev(&r))
			.or(input.get_ref()?)
			.unwrap_or_else(|| "HEAD".into());
		let commit = text(
			&repo,
			&["rev-parse", "--verify", &format!("{requested}^{{commit}}")],
		)?;
		result.attrs.insert("rev".into(), commit.clone().into());
		if result.get_ref()?.is_none() {
			result.attrs.insert("ref".into(), "HEAD".into());
		}
		let keep = Rc::new(tempfile::tempdir()?);
		export(&repo, &commit, keep.path(), false, export_ignore)?;
		if submodules {
			export_submodules(store, settings, input, &repo, &commit, keep.path())?;
		}
		if boolean(input, "lfs")? {
			super::lfs::smudge(store, &repo, &commit, keep.path())?;
		}
		mount = Mount {
			root: keep.path().to_owned(),
			filter: None,
			keep: Some(keep),
		};
		rev = Some(commit);
	}
	let modified = match &rev {
		Some(rev) => text(&repo, &["show", "-s", "--format=%ct", rev])?
			.parse::<u64>()
			.map_err(|e| Error::new(e.to_string()))?,
		None => 0,
	};
	if worktree {
		result
			.attrs
			.insert("lastModified".into(), Attr::Int(modified));
	} else {
		result
			.attrs
			.entry("lastModified".into())
			.or_insert(Attr::Int(modified));
	}
	if !shallow && result.get_rev()?.is_some() {
		let count = match rev {
			Some(rev) => text(&repo, &["rev-list", "--count", &rev])?
				.parse::<u64>()
				.map_err(|e| Error::new(e.to_string()))?,
			None => 0,
		};
		result
			.attrs
			.entry("revCount".into())
			.or_insert(Attr::Int(count));
	}
	Ok((mount, result))
}

#[cfg(test)]
mod tests {
	use super::*;
	#[test]
	fn remote_transport_fetches_refs_and_shallow_revisions_offline() {
		let source = tempfile::tempdir().unwrap();
		run(source.path(), &["init", "--quiet", "-b", "main"]).unwrap();
		for _ in 0..2 {
			run(
				source.path(),
				&[
					"-c",
					"user.name=Fetcher test",
					"-c",
					"user.email=fetcher@example.invalid",
					"commit",
					"--allow-empty",
					"--quiet",
					"-m",
					"commit",
				],
			)
			.unwrap();
		}
		let revision = text(source.path(), &["rev-parse", "HEAD"]).unwrap();
		let url = format!("file://{}", source.path().display());
		let settings = FetchSettings::default();
		for (shallow, all_refs) in [(false, false), (false, true), (true, false)] {
			let input = Input::from_attrs(
				&settings,
				[
					("type".into(), "git".into()),
					("url".into(), url.clone().into()),
					("shallow".into(), Attr::Bool(shallow)),
					("allRefs".into(), Attr::Bool(all_refs)),
				]
				.into(),
			)
			.unwrap();
			let cache = tempfile::tempdir().unwrap();
			let fetched = remote_repo(&url, &input, cache.path(), 3600, false).unwrap();
			assert_eq!(text(&fetched, &["rev-parse", "HEAD"]).unwrap(), revision);
			assert_eq!(
				text(&fetched, &["rev-list", "--count", "HEAD"]).unwrap(),
				if shallow { "1" } else { "2" }
			);
		}
	}
}
