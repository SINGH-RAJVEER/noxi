//! Profiles, following CppNix 2.35.2's `profiles.cc`: a profile `NAME` is a
//! symlink to one of its generations, the links `NAME-N-link` beside it.

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use nix_compat::store_path::StorePath;
use path_clean::PathClean;

use crate::gc::replace_symlink;
use crate::lock::PathLocks;
use crate::{Error, LocalStore, Result};

/// One generation link of a profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Generation {
	pub number: u64,
	pub path: PathBuf,
	/// The link's modification time, in seconds since the epoch.
	pub creation_time: i64,
}

/// `parseName`: the number of a `PROFILE-N-link` name.
fn parse_name(profile_name: &str, name: &str) -> Option<u64> {
	let rest = name.strip_prefix(profile_name)?.strip_prefix('-')?;
	let number = &rest[..rest.find("-link")?];
	if number.starts_with('+') {
		return None;
	}
	number.parse().ok()
}

fn name_of(path: &Path) -> String {
	path.file_name()
		.map(|name| name.to_string_lossy().into_owned())
		.unwrap_or_default()
}

/// `findGenerations`: the profile's generations in number order, and the
/// current one.
pub fn find_generations(profile: &Path) -> Result<(Vec<Generation>, Option<u64>)> {
	let dir = profile.parent().unwrap_or(Path::new("/"));
	let profile_name = name_of(profile);
	let mut generations = Vec::new();
	let entries = fs::read_dir(dir)
		.map_err(|e| Error::io(format!("reading directory \"{}\"", dir.display()), e))?;
	for entry in entries {
		let entry =
			entry.map_err(|e| Error::io(format!("reading directory \"{}\"", dir.display()), e))?;
		let name = entry.file_name().to_string_lossy().into_owned();
		if let Some(number) = parse_name(&profile_name, &name) {
			let path = entry.path();
			let metadata = fs::symlink_metadata(&path)
				.map_err(|e| Error::io(format!("getting status of \"{}\"", path.display()), e))?;
			generations.push(Generation {
				number,
				path,
				creation_time: metadata.mtime(),
			});
		}
	}
	generations.sort_by_key(|generation| generation.number);
	let current = match fs::symlink_metadata(profile) {
		Ok(_) => {
			let target = fs::read_link(profile).map_err(|e| {
				Error::io(
					format!("reading symbolic link \"{}\"", profile.display()),
					e,
				)
			})?;
			parse_name(&profile_name, &target.to_string_lossy())
		}
		Err(_) => None,
	};
	Ok((generations, current))
}

/// `makeName`: the link of generation `number`.
fn make_name(profile: &Path, number: u64) -> PathBuf {
	PathBuf::from(format!("{}-{number}-link", profile.display()))
}

/// `lockProfile`: hold `PROFILE.lock`, removed again on release.
pub fn lock_profile(profile: &Path) -> Result<PathLocks> {
	let mut lock = PathLocks::lock([profile.to_owned()])
		.map_err(|e| Error::io(format!("locking profile \"{}\"", profile.display()), e))?;
	lock.set_deletion(true);
	Ok(lock)
}

/// `createGeneration`: a new generation link to `out_path`, or the last
/// one if it already points there.
pub fn create_generation(
	store: &LocalStore,
	profile: &Path,
	out_path: &StorePath,
) -> Result<PathBuf> {
	let (generations, _) = find_generations(profile)?;
	let number = match generations.last() {
		Some(last) => {
			if fs::read_link(&last.path)
				.is_ok_and(|target| target == Path::new(&out_path.to_absolute_path()))
			{
				return Ok(last.path.clone());
			}
			last.number
		}
		None => 0,
	};
	let generation = make_name(profile, number + 1);
	store.add_perm_root(out_path, &generation)
}

/// `deleteGeneration`.
pub fn delete_generation(profile: &Path, number: u64) -> Result<()> {
	let generation = make_name(profile, number);
	match fs::remove_file(&generation) {
		Ok(()) => Ok(()),
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
		Err(e) => Err(Error::io(
			format!("removing file \"{}\"", generation.display()),
			e,
		)),
	}
}

/// `deleteGeneration2`: delete with a notice, or only report it.
fn delete_generation_logged(
	profile: &Path,
	number: u64,
	dry_run: bool,
	log: &mut dyn FnMut(&str),
) -> Result<()> {
	if dry_run {
		log(&format!("would remove profile version {number}"));
		Ok(())
	} else {
		log(&format!("removing profile version {number}"));
		delete_generation(profile, number)
	}
}

/// `deleteGenerations`: delete the listed generations, never the current.
pub fn delete_generations(
	profile: &Path,
	numbers: &BTreeSet<u64>,
	dry_run: bool,
	log: &mut dyn FnMut(&str),
) -> Result<()> {
	let _lock = lock_profile(profile)?;
	let (generations, current) = find_generations(profile)?;
	if current.is_some_and(|current| numbers.contains(&current)) {
		return Err(Error::Other(format!(
			"cannot delete current version of profile \"{}\"",
			profile.display()
		)));
	}
	for generation in generations.iter().filter(|g| numbers.contains(&g.number)) {
		delete_generation_logged(profile, generation.number, dry_run, log)?;
	}
	Ok(())
}

/// `deleteGenerationsGreaterThan`: keep the current generation and the
/// `max - 1` before it, as `--delete-generations +N` does.
pub fn delete_generations_greater_than(
	profile: &Path,
	max: u64,
	dry_run: bool,
	log: &mut dyn FnMut(&str),
) -> Result<()> {
	if max == 0 {
		return Err(Error::Other(
			"Must keep at least one generation, otherwise the current one would be deleted".into(),
		));
	}
	let _lock = lock_profile(profile)?;
	let (generations, current) = find_generations(profile)?;
	let older = generations
		.iter()
		.rev()
		.skip_while(|g| Some(g.number) != current)
		.skip(max as usize);
	for generation in older {
		delete_generation_logged(profile, generation.number, dry_run, log)?;
	}
	Ok(())
}

/// `deleteOldGenerations`: delete every generation but the current one.
pub fn delete_old_generations(
	profile: &Path,
	dry_run: bool,
	log: &mut dyn FnMut(&str),
) -> Result<()> {
	let _lock = lock_profile(profile)?;
	let (generations, current) = find_generations(profile)?;
	for generation in generations.iter().filter(|g| Some(g.number) != current) {
		delete_generation_logged(profile, generation.number, dry_run, log)?;
	}
	Ok(())
}

/// `deleteGenerationsOlderThan`: delete generations created before `time`,
/// except the newest of them, which was current at that time, and the
/// current one.
pub fn delete_generations_older_than(
	profile: &Path,
	time: i64,
	dry_run: bool,
	log: &mut dyn FnMut(&str),
) -> Result<()> {
	let _lock = lock_profile(profile)?;
	let (generations, current) = find_generations(profile)?;
	let older = generations
		.iter()
		.rev()
		.skip_while(|g| g.creation_time >= time)
		.skip(1);
	for generation in older.filter(|g| Some(g.number) != current) {
		delete_generation_logged(profile, generation.number, dry_run, log)?;
	}
	Ok(())
}

/// `parseOlderThanTimeSpec`: the time `Nd` days before `now`. The error is
/// a usage error.
pub fn parse_older_than_time_spec(spec: &str, now: i64) -> std::result::Result<i64, String> {
	let Some(days) = spec.strip_suffix('d') else {
		return Err(format!(
			"invalid number of days specifier '{spec}', expected something like '14d'"
		));
	};
	match days.parse::<i32>() {
		Ok(days) if days >= 1 && !spec.starts_with('+') => Ok(now - i64::from(days) * 24 * 3600),
		_ => Err(format!("invalid number of days specifier '{spec}'")),
	}
}

/// `switchLink`: point `link` at `target`, relatively if they share a
/// directory.
pub fn switch_link(link: &Path, target: &Path) -> Result<()> {
	let target = if target.parent() == link.parent() {
		Path::new(target.file_name().unwrap_or_default())
	} else {
		target
	};
	replace_symlink(target, link)
}

/// `switchGeneration`: make generation `number` current, or with `None`
/// the newest one older than the current.
pub fn switch_generation(
	profile: &Path,
	number: Option<u64>,
	dry_run: bool,
	log: &mut dyn FnMut(&str),
) -> Result<()> {
	let _lock = lock_profile(profile)?;
	let (generations, current) = find_generations(profile)?;
	let target = generations
		.iter()
		.filter(|g| match number {
			Some(number) => g.number == number,
			None => current.is_some_and(|current| g.number < current),
		})
		.next_back();
	let Some(target) = target else {
		return Err(Error::Other(match number {
			Some(number) => format!("profile version {number} does not exist"),
			None => format!(
				"no profile version older than the current ({}) exists",
				current.unwrap_or(0)
			),
		}));
	};
	log(&format!(
		"switching profile from version {} to {}",
		current.unwrap_or(0),
		target.number
	));
	if dry_run {
		return Ok(());
	}
	switch_link(profile, &target.path)
}

fn is_root_user() -> bool {
	// SAFETY: `getuid` has no preconditions.
	unsafe { libc::getuid() == 0 }
}

fn env_path(name: &str) -> Option<PathBuf> {
	std::env::var_os(name)
		.filter(|value| !value.is_empty())
		.map(PathBuf::from)
}

/// `getHome`.
pub fn home_dir() -> PathBuf {
	env_path("HOME").unwrap_or_else(|| PathBuf::from("/"))
}

/// `createNixStateDir`: the per-user state directory, created.
pub fn create_nix_state_dir() -> Result<PathBuf> {
	let dir = env_path("NIX_STATE_HOME").unwrap_or_else(|| {
		env_path("XDG_STATE_HOME")
			.unwrap_or_else(|| home_dir().join(".local/state"))
			.join("nix")
	});
	fs::create_dir_all(&dir)
		.map_err(|e| Error::io(format!("creating directory \"{}\"", dir.display()), e))?;
	Ok(dir)
}

/// `rootProfilesDir`.
pub fn root_profiles_dir(state_dir: &Path) -> PathBuf {
	state_dir.join("profiles/per-user/root")
}

/// `profilesDir`: root's profiles under the state directory, or the user's
/// under their own state directory. The directory is created.
pub fn profiles_dir(state_dir: &Path) -> Result<PathBuf> {
	let dir = if is_root_user() {
		root_profiles_dir(state_dir)
	} else {
		create_nix_state_dir()?.join("profiles")
	};
	fs::create_dir_all(&dir)
		.map_err(|e| Error::io(format!("creating directory \"{}\"", dir.display()), e))?;
	Ok(dir)
}

/// `getDefaultProfile`: the profile `~/.nix-profile` (or the XDG `profile`
/// link) names, creating that link, and root's `profiles/default`, if
/// missing.
pub fn default_profile(state_dir: &Path, use_xdg_base_directories: bool) -> PathBuf {
	let link = if use_xdg_base_directories {
		match create_nix_state_dir() {
			Ok(dir) => dir.join("profile"),
			Err(_) => return env_path("HOME").unwrap_or_default().join(".nix-profile"),
		}
	} else {
		home_dir().join(".nix-profile")
	};
	let resolve = || -> Result<PathBuf> {
		let profile = profiles_dir(state_dir)?.join("profile");
		if fs::symlink_metadata(&link).is_err() {
			replace_symlink(&profile, &link)?;
		}
		let global = state_dir.join("profiles/default");
		if is_root_user() && fs::symlink_metadata(&global).is_err() {
			replace_symlink(&profile, &global)?;
		}
		let target = fs::read_link(&link)
			.map_err(|e| Error::io(format!("reading symbolic link \"{}\"", link.display()), e))?;
		Ok(link.parent().unwrap_or(Path::new("/")).join(target).clean())
	};
	resolve().unwrap_or(link)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_generation_names() {
		assert_eq!(parse_name("system", "system-12-link"), Some(12));
		assert_eq!(parse_name("system", "system-12-link-x"), Some(12));
		assert_eq!(parse_name("system", "system-+1-link"), None);
		assert_eq!(parse_name("system", "systemd-1-link"), None);
		assert_eq!(parse_name("system", "system"), None);
	}

	#[test]
	fn deletes_and_switches_generations() {
		let dir = tempfile::tempdir().unwrap();
		let profile = dir.path().join("p");
		for n in 1..=4 {
			std::os::unix::fs::symlink(format!("/nix/store/{n}"), make_name(&profile, n)).unwrap();
		}
		std::os::unix::fs::symlink("p-3-link", &profile).unwrap();
		let mut log = Vec::new();
		switch_generation(&profile, None, false, &mut |m| log.push(m.to_owned())).unwrap();
		assert_eq!(fs::read_link(&profile).unwrap(), Path::new("p-2-link"));
		delete_old_generations(&profile, false, &mut |m| log.push(m.to_owned())).unwrap();
		let (generations, current) = find_generations(&profile).unwrap();
		assert_eq!(current, Some(2));
		assert_eq!(
			generations.iter().map(|g| g.number).collect::<Vec<_>>(),
			[2]
		);
		assert_eq!(
			log,
			[
				"switching profile from version 3 to 2",
				"removing profile version 1",
				"removing profile version 3",
				"removing profile version 4",
			]
		);
		assert!(!dir.path().join("p.lock").exists());
	}

	#[test]
	fn parses_day_specifiers() {
		assert_eq!(
			parse_older_than_time_spec("2d", 1_000_000),
			Ok(1_000_000 - 2 * 86400)
		);
		assert!(parse_older_than_time_spec("0d", 0).is_err());
		assert!(
			parse_older_than_time_spec("14", 0)
				.unwrap_err()
				.contains("expected something like '14d'")
		);
	}
}
