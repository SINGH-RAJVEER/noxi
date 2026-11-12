//! `nix nar ls`, `nar cat`, `nar pack` and `nar dump-path`, following
//! `src/nix/ls.cc`, `cat.cc` and `dump-path.cc`. NAR files are indexed with
//! `noxi_store::nar_listing`, whose offsets `nar cat` reads back.
use std::fs;
use std::io::{self, IsTerminal, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use noxi_store::nar_listing::{Listing, parse_listing};
use serde_json::{Value, json};

use super::{Parsed, hash::strerror, unsupported};
use crate::cli::Failure;

/// `CanonPath`: an absolute path without `.`, `..` or empty components.
fn canon(path: &str) -> Vec<String> {
	let mut parts: Vec<String> = Vec::new();
	for part in path.split('/') {
		match part {
			"" | "." => {}
			".." => {
				parts.pop();
			}
			part => parts.push(part.to_owned()),
		}
	}
	parts
}

fn show(parts: &[String]) -> String {
	format!("/{}", parts.join("/"))
}

fn open_nar(path: &str) -> Result<fs::File, Failure> {
	fs::File::open(path).map_err(|error| {
		Failure::Error(format!("opening NAR file \"{path}\": {}", strerror(&error)))
	})
}

fn listing(path: &str) -> Result<(fs::File, Listing), Failure> {
	let mut file = open_nar(path)?;
	let listing = parse_listing(&mut io::BufReader::new(&mut file)).map_err(Failure::Error)?;
	Ok((file, listing))
}

/// `SourceAccessor::lstat` of the NAR accessor, whose paths display with
/// the default `«unknown»` prefix.
fn lstat<'a>(root: &'a Listing, parts: &[String]) -> Result<&'a Listing, Failure> {
	let components: Vec<&[u8]> = parts.iter().map(|p| p.as_bytes()).collect();
	root.find(&components).ok_or_else(|| {
		Failure::Error(format!(
			"path '\u{ab}unknown\u{bb}{}' does not exist",
			show(parts)
		))
	})
}

fn regular_json(executable: bool, size: u64, offset: u64) -> Value {
	let mut value = json!({"type": "regular", "size": size, "executable": executable});
	if offset != 0 {
		value["narOffset"] = json!(offset);
	}
	value
}

/// `listNarDeep` and `listNarShallow`.
fn list_json(node: &Listing, deep: bool) -> Value {
	match node {
		Listing::Regular {
			executable,
			size,
			offset,
		} => regular_json(*executable, *size, *offset),
		Listing::Symlink(target) => {
			json!({"type": "symlink", "target": String::from_utf8_lossy(target)})
		}
		Listing::Directory(entries) => {
			let entries: serde_json::Map<String, Value> = entries
				.iter()
				.map(|(name, child)| {
					(
						String::from_utf8_lossy(name).into_owned(),
						if deep {
							list_json(child, true)
						} else {
							json!({})
						},
					)
				})
				.collect();
			json!({"type": "directory", "entries": entries})
		}
	}
}

struct Text<'a> {
	long: bool,
	recursive: bool,
	out: io::StdoutLock<'a>,
}

impl Text<'_> {
	fn show_file(&mut self, node: &Listing, rel: &str) -> io::Result<()> {
		if self.long {
			let (kind, size) = match node {
				Listing::Regular {
					executable, size, ..
				} => (
					if *executable {
						"-r-xr-xr-x"
					} else {
						"-r--r--r--"
					},
					*size,
				),
				Listing::Symlink(_) => ("lrwxrwxrwx", 0),
				Listing::Directory(_) => ("dr-xr-xr-x", 0),
			};
			let mut line = format!("{kind} {size:20} {rel}");
			if let Listing::Symlink(target) = node {
				line.push_str(" -> ");
				line.push_str(&String::from_utf8_lossy(target));
			}
			writeln!(self.out, "{line}")?;
		} else {
			writeln!(self.out, "{rel}")?;
		}
		if self.recursive && matches!(node, Listing::Directory(_)) {
			self.do_path(node, rel, false)?;
		}
		Ok(())
	}

	fn do_path(&mut self, node: &Listing, rel: &str, show_directory: bool) -> io::Result<()> {
		match node {
			Listing::Directory(entries) if !show_directory => {
				for (name, child) in entries {
					self.show_file(child, &format!("{rel}/{}", String::from_utf8_lossy(name)))?;
				}
				Ok(())
			}
			_ => self.show_file(node, rel),
		}
	}
}

fn ls(parsed: &Parsed) -> Result<ExitCode, Failure> {
	let (nar, path) = (&parsed.operands[0], &parsed.operands[1]);
	let (_, root) = listing(nar)?;
	let parts = canon(path);
	if parsed.has("json") {
		if parsed.has("directory") {
			return Err(Failure::Usage(
				"'--directory' is useless with '--json'".into(),
			));
		}
		let node = lstat(&root, &parts)?;
		println!("{}", list_json(node, parsed.has("recursive")));
		return Ok(ExitCode::SUCCESS);
	}
	let node = lstat(&root, &parts)?;
	let label = if matches!(node, Listing::Directory(_)) {
		".".to_owned()
	} else {
		parts.last().cloned().unwrap_or_default()
	};
	Text {
		long: parsed.has("long"),
		recursive: parsed.has("recursive"),
		out: io::stdout().lock(),
	}
	.do_path(node, &label, parsed.has("directory"))?;
	Ok(ExitCode::SUCCESS)
}

fn cat(parsed: &Parsed) -> Result<ExitCode, Failure> {
	let (nar, path) = (&parsed.operands[0], &parsed.operands[1]);
	// `nix nar cat` is a store command: it opens the selected store first.
	super::ensure_diverted_root(parsed)?;
	let uri = crate::store::store_uri(parsed.value("store"), &parsed.settings);
	crate::store::open(&uri, &parsed.settings)?;
	let (mut file, root) = listing(nar)?;
	let parts = canon(path);
	let components: Vec<&[u8]> = parts.iter().map(|p| p.as_bytes()).collect();
	let Some(Listing::Regular { size, offset, .. }) = root.find(&components) else {
		return Err(Failure::Error(format!(
			"NAR does not contain regular file '{path}'"
		)));
	};
	file.seek(SeekFrom::Start(*offset))?;
	io::copy(&mut file.take(*size), &mut io::stdout().lock())?;
	Ok(ExitCode::SUCCESS)
}

fn pack(parsed: &Parsed) -> Result<ExitCode, Failure> {
	if parsed.command == "nar dump-path"
		&& nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn)
	{
		eprintln!("warning: 'nix nar dump-path' is a deprecated alias for 'nix nar pack'");
	}
	if io::stdout().is_terminal() {
		return Err(Failure::Usage("refusing to write NAR to a terminal".into()));
	}
	let path: PathBuf = crate::instantiate::absolute_path(
		&std::env::current_dir()?,
		Path::new(&parsed.operands[0]),
	);
	let meta = fs::symlink_metadata(&path).map_err(|error| {
		Failure::Error(format!(
			"opening file \"{}\": {}",
			path.display(),
			strerror(&error)
		))
	})?;
	let kind = meta.file_type();
	if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
		return Err(Failure::Error(format!(
			"file \"{}\" has an unsupported type",
			path.display()
		)));
	}
	let mut out = io::BufWriter::new(io::stdout().lock());
	noxi_store::nar::dump_path(&mut out, &path)?;
	out.flush()?;
	Ok(ExitCode::SUCCESS)
}

pub fn run(parsed: Parsed) -> Result<ExitCode, Failure> {
	match parsed.command {
		"nar ls" => ls(&parsed),
		"nar cat" => cat(&parsed),
		"nar pack" | "nar dump-path" => pack(&parsed),
		command => Err(unsupported(command)),
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn canonical_paths_drop_dots_and_empty_components() {
		assert_eq!(super::canon("//sub/../a/./"), ["a"]);
		assert_eq!(super::canon("a"), ["a"]);
		assert!(super::canon("/").is_empty());
	}
}
