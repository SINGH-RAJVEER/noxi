//! `builtin:buildenv`, following CppNix 2.35.2's `builtins/buildenv.cc`:
//! the symlink tree of a `nix-env` user environment.
//!
//! The builder sees store paths as the build does, under `/nix/store`;
//! `real` maps such a path to where this process reads and writes it.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};

/// One output to link into the environment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
	pub path: PathBuf,
	pub active: bool,
	pub priority: i64,
}

struct State<'a> {
	priorities: BTreeMap<PathBuf, i64>,
	real: &'a dyn Fn(&Path) -> PathBuf,
	warn: &'a mut dyn FnMut(&str),
}

/// A builder failure, with CppNix's message.
#[derive(Debug)]
pub struct Error(pub String);

impl From<io::Error> for Error {
	fn from(e: io::Error) -> Self {
		Error(e.to_string())
	}
}

fn quoted(path: &Path) -> String {
	format!("\"{}\"", path.display())
}

impl State<'_> {
	/// `canonPath(path, true)` in the builder's view: the path with every
	/// symlink resolved.
	fn resolve(&self, path: &Path) -> io::Result<PathBuf> {
		let mut pending: Vec<PathBuf> = path
			.components()
			.rev()
			.filter_map(|c| match c {
				Component::Normal(name) => Some(PathBuf::from(name)),
				Component::ParentDir => Some(PathBuf::from("..")),
				_ => None,
			})
			.collect();
		let mut resolved = PathBuf::from("/");
		let mut follows = 0;
		while let Some(name) = pending.pop() {
			if name == Path::new("..") {
				resolved.pop();
				continue;
			}
			let next = resolved.join(&name);
			match fs::symlink_metadata((self.real)(&next)) {
				Ok(metadata) if metadata.file_type().is_symlink() => {
					follows += 1;
					if follows > 1024 {
						return Err(io::Error::other("too many symbolic links"));
					}
					let target = fs::read_link((self.real)(&next))?;
					if target.is_absolute() {
						resolved = PathBuf::from("/");
					}
					for component in target.components().rev() {
						match component {
							Component::Normal(name) => pending.push(PathBuf::from(name)),
							Component::ParentDir => pending.push(PathBuf::from("..")),
							_ => {}
						}
					}
				}
				_ => resolved = next,
			}
		}
		Ok(resolved)
	}

	fn stat(&self, path: &Path) -> Option<fs::Metadata> {
		fs::metadata((self.real)(&self.resolve(path).ok()?)).ok()
	}

	/// `createLinks`: link the entries of `src_dir` into `dst_dir`.
	fn create_links(&mut self, src_dir: &Path, dst_dir: &Path, priority: i64) -> Result<(), Error> {
		let real_src = (self.real)(&self.resolve(src_dir)?);
		let entries = match fs::read_dir(&real_src) {
			Ok(entries) => entries,
			Err(e) if e.raw_os_error() == Some(libc::ENOTDIR) => {
				(self.warn)(&format!(
					"not including {} in the user environment because it's not a directory",
					quoted(src_dir)
				));
				return Ok(());
			}
			Err(e) => return Err(Error(format!("opening directory {}: {e}", quoted(src_dir)))),
		};
		for entry in entries {
			let name = entry?.file_name();
			if name.as_encoded_bytes().first() == Some(&b'.') {
				continue;
			}
			let src_file = src_dir.join(&name);
			let dst_file = dst_dir.join(&name);
			let real_dst = (self.real)(&dst_file);
			let Some(src_metadata) = self.stat(&src_file) else {
				(self.warn)(&format!(
					"skipping dangling symlink '{}'",
					dst_file.display()
				));
				continue;
			};
			let src_text = src_file.to_string_lossy();
			if [
				"/propagated-build-inputs",
				"/nix-support",
				"/perllocal.pod",
				"/info/dir",
				"/log",
				"/manifest.nix",
				"/manifest.json",
			]
			.iter()
			.any(|suffix| src_text.ends_with(suffix))
			{
				continue;
			}
			let dst_metadata = fs::symlink_metadata(&real_dst).ok();
			if src_metadata.is_dir() {
				if let Some(dst_metadata) = &dst_metadata {
					if dst_metadata.is_dir() {
						self.create_links(&src_file, &dst_file, priority)?;
						continue;
					} else if dst_metadata.file_type().is_symlink() {
						let target = self.resolve(&dst_file)?;
						let target_is_dir =
							fs::symlink_metadata((self.real)(&target)).is_ok_and(|m| m.is_dir());
						if !target_is_dir {
							return Err(Error(format!(
								"collision between {} and non-directory {}",
								quoted(&src_file),
								quoted(&target)
							)));
						}
						fs::remove_file(&real_dst)?;
						fs::DirBuilder::new()
							.mode(0o755)
							.create(&real_dst)
							.map_err(|e| {
								Error(format!("creating directory '{}': {e}", dst_file.display()))
							})?;
						let previous = self.priorities.get(&dst_file).copied().unwrap_or(0);
						self.create_links(&target, &dst_file, previous)?;
						self.create_links(&src_file, &dst_file, priority)?;
						continue;
					}
				}
			} else if let Some(dst_metadata) = &dst_metadata {
				if dst_metadata.file_type().is_symlink() {
					let previous = self.priorities.get(&dst_file).copied().unwrap_or(0);
					if previous == priority {
						let existing = fs::read_link(&real_dst)?;
						return Err(Error(format!(
							"Unable to build profile. There is a conflict for the following files:\n\n  {}\n  {}",
							quoted(&existing),
							quoted(&src_file)
						)));
					}
					if previous < priority {
						continue;
					}
					fs::remove_file(&real_dst)?;
				} else if dst_metadata.is_dir() {
					return Err(Error(format!(
						"collision between non-directory '{}' and directory '{}'",
						src_file.display(),
						dst_file.display()
					)));
				}
			}
			std::os::unix::fs::symlink(&src_file, &real_dst).map_err(|e| {
				Error(format!(
					"creating symlink \"{}\" -> \"{}\": {e}",
					dst_file.display(),
					src_file.display()
				))
			})?;
			self.priorities.insert(dst_file, priority);
		}
		Ok(())
	}
}

use std::os::unix::fs::DirBuilderExt;

/// `buildProfile`: link `packages` into `out`, the active ones in
/// priority order, then the packages they propagate.
pub fn build_profile(
	out: &Path,
	mut packages: Vec<Package>,
	real: &dyn Fn(&Path) -> PathBuf,
	warn: &mut dyn FnMut(&str),
) -> Result<(), Error> {
	let mut state = State {
		priorities: BTreeMap::new(),
		real,
		warn,
	};
	let mut done: BTreeSet<PathBuf> = BTreeSet::new();
	let mut postponed: BTreeSet<PathBuf> = BTreeSet::new();
	let add = |state: &mut State<'_>,
	           done: &mut BTreeSet<PathBuf>,
	           postponed: &mut BTreeSet<PathBuf>,
	           dir: &Path,
	           priority: i64|
	 -> Result<(), Error> {
		if !done.insert(dir.to_owned()) {
			return Ok(());
		}
		state.create_links(dir, out, priority)?;
		let file = dir.join("nix-support/propagated-user-env-packages");
		let contents = match state
			.resolve(&file)
			.and_then(|p| fs::read((state.real)(&p)))
		{
			Ok(contents) => contents,
			Err(e)
				if matches!(e.raw_os_error(), Some(libc::ENOENT) | Some(libc::ENOTDIR))
					|| e.kind() == io::ErrorKind::NotFound =>
			{
				return Ok(());
			}
			Err(e) => return Err(e.into()),
		};
		for word in String::from_utf8_lossy(&contents).split([' ', '\n']) {
			if !word.is_empty() && !done.contains(Path::new(word)) {
				postponed.insert(PathBuf::from(word));
			}
		}
		Ok(())
	};
	packages.sort_by(|a, b| {
		a.priority
			.cmp(&b.priority)
			.then_with(|| a.path.cmp(&b.path))
	});
	for package in packages.iter().filter(|p| p.active) {
		add(
			&mut state,
			&mut done,
			&mut postponed,
			&package.path,
			package.priority,
		)?;
	}
	let mut priority = 1000;
	while !postponed.is_empty() {
		for dir in std::mem::take(&mut postponed) {
			add(&mut state, &mut done, &mut postponed, &dir, priority)?;
			priority += 1;
		}
	}
	Ok(())
}

/// `builtinBuildenv`: the environment for the `derivations` and
/// `manifest` attributes of a derivation, at `out`.
pub fn builtin_buildenv(
	env: &BTreeMap<String, String>,
	out: &Path,
	real: &dyn Fn(&Path) -> PathBuf,
	warn: &mut dyn FnMut(&str),
) -> Result<(), Error> {
	let attr = |name: &str| {
		env.get(name)
			.ok_or_else(|| Error(format!("attribute '{name}' missing")))
	};
	fs::create_dir_all(real(out))?;
	let words: Vec<&str> = attr("derivations")?
		.split([' ', '\t', '\n', '\r'])
		.filter(|w| !w.is_empty())
		.collect();
	let mut packages = Vec::new();
	let mut words = words.into_iter();
	let bad = || Error("invalid 'derivations' attribute".to_owned());
	while let Some(active) = words.next() {
		let active = active != "false";
		let priority = words.next().and_then(|w| w.parse().ok()).ok_or_else(bad)?;
		let outputs: usize = words.next().and_then(|w| w.parse().ok()).ok_or_else(bad)?;
		for _ in 0..outputs {
			let path = words.next().ok_or_else(bad)?;
			packages.push(Package {
				path: PathBuf::from(path),
				active,
				priority,
			});
		}
	}
	build_profile(out, packages, real, warn)?;
	std::os::unix::fs::symlink(attr("manifest")?, real(&out.join("manifest.nix")))?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn links_packages_by_priority_and_reports_conflicts() {
		let dir = tempfile::tempdir().unwrap();
		let root = dir.path().to_owned();
		let real = move |path: &Path| root.join(path.strip_prefix("/").unwrap());
		for (pkg, file) in [
			("a", "bin/x"),
			("b", "bin/x"),
			("b", "share/y"),
			("c", "bin/x"),
		] {
			let path = real(&Path::new("/s").join(pkg).join(file));
			fs::create_dir_all(path.parent().unwrap()).unwrap();
			fs::write(path, pkg).unwrap();
		}
		let env = BTreeMap::from([
			(
				"derivations".to_owned(),
				"true 5 1 /s/a 1 4 1 /s/b".to_owned(),
			),
			("manifest".to_owned(), "/s/manifest".to_owned()),
		]);
		builtin_buildenv(&env, Path::new("/out"), &real, &mut |_| {}).unwrap();
		assert_eq!(
			fs::read_link(real(Path::new("/out/bin/x"))).unwrap(),
			Path::new("/s/b/bin/x")
		);
		assert_eq!(
			fs::read_link(real(Path::new("/out/share"))).unwrap(),
			Path::new("/s/b/share")
		);
		assert_eq!(
			fs::read_link(real(Path::new("/out/manifest.nix"))).unwrap(),
			Path::new("/s/manifest")
		);

		let env = BTreeMap::from([
			(
				"derivations".to_owned(),
				"true 5 1 /s/a true 5 1 /s/c".to_owned(),
			),
			("manifest".to_owned(), "/s/manifest".to_owned()),
		]);
		let error = builtin_buildenv(&env, Path::new("/out2"), &real, &mut |_| {}).unwrap_err();
		assert_eq!(
			error.0,
			"Unable to build profile. There is a conflict for the following files:\n\n  \"/s/a/bin/x\"\n  \"/s/c/bin/x\""
		);
	}
}
