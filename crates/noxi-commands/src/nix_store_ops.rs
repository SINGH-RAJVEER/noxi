//! The `nix-store` operations that add, import, export, register and check
//! store paths, following `src/nix/nix-store/nix-store.cc` and
//! `libstore/export-import.cc` in CppNix 2.35.2.
use std::collections::BTreeSet;
use std::fs;
use std::io::{self, BufRead, BufReader, Read, Seek, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::hash::{self as cpphash, HashFormat};
use nix_compat::nixcpp::settings::Settings;
use nix_compat::nixhash::{HashAlgo, NixHash};
use nix_compat::store_path::StorePath;
use noxi_store::{FileIngestionMethod, LocalStore, Store, ValidPathInfo};

use crate::cli::Failure;

/// The operations this module implements.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
	Add,
	AddFixed,
	PrintFixedPath,
	PrintEnv,
	ReadLog,
	DumpDb,
	LoadDb,
	RegisterValidity,
	CheckValidity,
	Restore,
	Export,
	Import,
	VerifyPath,
	RepairPath,
	GenerateBinaryCacheKey,
	Dump,
}

impl Op {
	pub fn parse(arg: &str) -> Option<Self> {
		Some(match arg {
			"--add" | "-A" => Self::Add,
			"--add-fixed" => Self::AddFixed,
			"--print-fixed-path" => Self::PrintFixedPath,
			"--print-env" => Self::PrintEnv,
			"--read-log" | "-l" => Self::ReadLog,
			"--dump-db" => Self::DumpDb,
			"--load-db" => Self::LoadDb,
			"--register-validity" => Self::RegisterValidity,
			"--check-validity" => Self::CheckValidity,
			"--restore" => Self::Restore,
			"--export" => Self::Export,
			"--import" => Self::Import,
			"--verify-path" => Self::VerifyPath,
			"--repair-path" => Self::RepairPath,
			"--generate-binary-cache-key" => Self::GenerateBinaryCacheKey,
			"--dump" => Self::Dump,
			_ => return None,
		})
	}

	/// Whether `--stdin` adds words from standard input as arguments; the
	/// operations that read standard input themselves keep it.
	pub fn reads_stdin_words(self) -> bool {
		!matches!(self, Self::Import | Self::Restore)
	}
}

fn usage(message: impl Into<String>) -> Failure {
	Failure::Usage(message.into())
}

fn unknown_flag(flags: &[String]) -> Result<(), Failure> {
	match flags.first() {
		Some(flag) => Err(usage(format!("unknown flag '{flag}'"))),
		None => Ok(()),
	}
}

fn no_flags(flags: &[String], message: &str) -> Result<(), Failure> {
	if flags.is_empty() {
		Ok(())
	} else {
		Err(usage(message))
	}
}

/// `baseNameOf`: the last component, ignoring one trailing slash.
fn base_name(path: &str) -> &str {
	let trimmed = path.strip_suffix('/').unwrap_or(path);
	trimmed.rsplit('/').next().unwrap_or(trimmed)
}

fn print(path: &StorePath) -> String {
	path.to_absolute_path()
}

fn log(level: nix_compat::log::VerbosityLevel, message: &str) {
	if nix_compat::log::log_enabled(level) {
		eprintln!("{message}");
	}
}

fn local_store(uri: &str, settings: &Settings) -> Result<LocalStore, Failure> {
	crate::store::open_local(uri, settings)?
		.map(|(store, _)| store)
		.ok_or_else(|| Failure::Error(format!("store '{uri}' cannot hold store paths")))
}

fn parse_hash(s: &str, algo: HashAlgo) -> Result<NixHash, Failure> {
	let hash = cpphash::parse_any(s, Some(algo), true).map_err(|e| match e {
		cpphash::Error::Usage(m) => Failure::Usage(m),
		cpphash::Error::Bad(m) => Failure::Error(m),
	})?;
	NixHash::from_str(&hash.to_string(HashFormat::Sri, true), None)
		.map_err(|e| e.to_string().into())
}

fn parse_algo(name: &str) -> Result<HashAlgo, Failure> {
	cpphash::parse_algo(name, false).map_err(|e| match e {
		cpphash::Error::Usage(m) => Failure::Usage(m),
		cpphash::Error::Bad(m) => Failure::Error(m),
	})
}

/// Serialise `path` (made absolute) into a temporary file for import.
fn dump(cwd: &Path, path: &str, flat: bool) -> Result<fs::File, Failure> {
	let source = crate::instantiate::absolute_path(cwd, Path::new(path));
	check_dumpable(&source)?;
	let mut file = tempfile::tempfile()?;
	if flat {
		io::copy(&mut fs::File::open(&source)?, &mut file)?;
	} else {
		noxi_store::nar::dump_path(&mut file, &source)?;
	}
	file.rewind()?;
	Ok(file)
}

/// `makeValidityRegistration(paths, showDerivers, showHash)`.
fn validity_registration(
	store: &dyn Store,
	paths: &BTreeSet<StorePath>,
	show_derivers: bool,
	show_hash: bool,
) -> Result<String, Failure> {
	let mut s = String::new();
	for path in paths {
		s += &format!("{}\n", print(path));
		let info = store
			.query_path_info(path)?
			.ok_or_else(|| Failure::Error(format!("path '{}' is not valid", print(path))))?;
		if show_hash {
			s += &format!("{}\n{}\n", hex(&info.nar_hash), info.nar_size);
		}
		let deriver = match (&info.deriver, show_derivers) {
			(Some(deriver), true) => print(deriver),
			_ => String::new(),
		};
		s += &format!("{deriver}\n{}\n", info.references.len());
		for reference in &info.references {
			s += &format!("{}\n", print(reference));
		}
	}
	Ok(s)
}

fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `parseStorePath`, with CppNix's message.
fn parse_store_path(s: &str) -> Result<StorePath, Failure> {
	nix_compat::nixcpp::narinfo::parse_store_path(s).map_err(Failure::Error)
}

/// `decodeValidPathInfo`: one entry of a validity registration, or `None`
/// at the end of the input.
fn decode_valid_path_info(
	lines: &mut impl Iterator<Item = io::Result<String>>,
	hash_given: bool,
) -> Result<Option<(ValidPathInfo, bool)>, Failure> {
	let Some(path) = lines.next().transpose()? else {
		return Ok(None);
	};
	let mut next = || -> Result<String, Failure> {
		lines
			.next()
			.transpose()?
			.ok_or_else(|| Failure::Error("missing input".into()))
	};
	let mut hash = [0u8; 32];
	let mut size = 0;
	if hash_given {
		let NixHash::Sha256(digest) = parse_hash(&next()?, HashAlgo::Sha256)? else {
			unreachable!("SHA-256 was requested");
		};
		hash = digest;
		size = next()?
			.parse::<u64>()
			.map_err(|_| Failure::Error("number expected".into()))?;
	}
	let mut info = ValidPathInfo::new(parse_store_path(&path)?, hash);
	info.nar_size = size;
	let deriver = next()?;
	if !deriver.is_empty() {
		info.deriver = Some(parse_store_path(&deriver)?);
	}
	let count = next()?
		.parse::<i32>()
		.map_err(|_| Failure::Error("number expected".into()))?;
	for _ in 0..count.max(0) {
		info.references.insert(parse_store_path(&next()?)?);
	}
	Ok(Some((info, hash_given)))
}

/// `registerValidity`.
fn register_validity(
	store: &LocalStore,
	reregister: bool,
	hash_given: bool,
	canonicalise: bool,
) -> Result<(), Failure> {
	let stdin = io::stdin();
	let mut lines = stdin.lock().lines();
	let mut infos: Vec<ValidPathInfo> = Vec::new();
	while let Some((mut info, _)) = decode_valid_path_info(&mut lines, hash_given)? {
		if store.is_valid_path(&info.path)? && !reregister {
			continue;
		}
		let real = store.real_path(&info.path);
		if canonicalise {
			noxi_store::canonicalise::canonicalise_path_metadata(&real, None)?;
		}
		if !hash_given {
			let (hash, size) = noxi_store::nar::hash_path(&real)?;
			info.nar_hash = hash;
			info.nar_size = size;
		}
		infos.retain(|i| i.path != info.path);
		infos.push(info);
	}
	store.register_valid_paths(&infos)?;
	Ok(())
}

/// The build log of `path` or of its deriver, plain or bzip2-compressed.
fn build_log(store: &LocalStore, path: &StorePath) -> Result<Option<Vec<u8>>, Failure> {
	let drv = if path.name().ends_with(".drv") {
		path.clone()
	} else {
		match store.query_path_info(path)?.and_then(|info| info.deriver) {
			Some(deriver) => deriver,
			None => return Ok(None),
		}
	};
	let name = drv.to_string();
	let base = store.config().log_dir.join("drvs").join(&name[..2]);
	for (file, compressed) in [
		(base.join(&name[2..]), false),
		(base.join(format!("{}.bz2", &name[2..])), true),
	] {
		match fs::File::open(&file) {
			Ok(file) => {
				let mut reader = noxi_store::binary_cache::decompress(
					compressed.then_some("bzip2"),
					Box::new(BufReader::new(file)),
				)?;
				let mut out = Vec::new();
				reader.read_to_end(&mut out)?;
				return Ok(Some(out));
			}
			Err(e) if e.kind() == io::ErrorKind::NotFound => {}
			Err(e) => return Err(e.into()),
		}
	}
	Ok(None)
}

/// `escapeShellArgAlways`.
fn shell_quote(s: &[u8]) -> Vec<u8> {
	let mut out = vec![b'\''];
	for &b in s {
		if b == b'\'' {
			out.extend_from_slice(b"'\\''");
		} else {
			out.push(b);
		}
	}
	out.push(b'\'');
	out
}

fn write_u64(out: &mut impl Write, n: u64) -> io::Result<()> {
	out.write_all(&n.to_le_bytes())
}

fn write_string(out: &mut impl Write, s: &[u8]) -> io::Result<()> {
	write_u64(out, s.len() as u64)?;
	out.write_all(s)?;
	let pad = (8 - s.len() % 8) % 8;
	out.write_all(&[0u8; 8][..pad])
}

fn read_u64(input: &mut impl Read) -> Result<u64, Failure> {
	let mut buf = [0u8; 8];
	input
		.read_exact(&mut buf)
		.map_err(|_| Failure::Error("unexpected end-of-file".into()))?;
	Ok(u64::from_le_bytes(buf))
}

fn read_string(input: &mut impl Read) -> Result<String, Failure> {
	let len = read_u64(input)? as usize;
	let mut buf = vec![0u8; len];
	input
		.read_exact(&mut buf)
		.map_err(|_| Failure::Error("unexpected end-of-file".into()))?;
	let mut pad = vec![0u8; (8 - len % 8) % 8];
	input
		.read_exact(&mut pad)
		.map_err(|_| Failure::Error("unexpected end-of-file".into()))?;
	String::from_utf8(buf).map_err(|e| Failure::Error(e.to_string()))
}

const EXPORT_MAGIC: u64 = 0x4558_494e;

/// A reader that keeps a copy of everything read through it.
struct Tee<'a, R> {
	inner: &'a mut R,
	saved: Vec<u8>,
}

impl<R: Read> Read for Tee<'_, R> {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		let n = self.inner.read(buf)?;
		self.saved.extend_from_slice(&buf[..n]);
		Ok(n)
	}
}

/// Run one of this module's operations.
pub fn run(
	op: Op,
	flags: &[String],
	args: &[String],
	uri: &str,
	settings: &Settings,
	cwd: &Path,
) -> Result<ExitCode, Failure> {
	let follow = |arg: &str| crate::paths::follow_links_to_store_path(cwd, arg);
	if op == Op::Dump {
		no_flags(flags, "unknown flag")?;
		if args.len() != 1 {
			return Err(usage("only one argument allowed"));
		}
		let path = crate::instantiate::absolute_path(cwd, Path::new(&args[0]));
		check_dumpable(&path)?;
		let mut out = io::BufWriter::new(io::stdout().lock());
		noxi_store::nar::dump_path(&mut out, &path)?;
		out.flush()?;
		return Ok(ExitCode::SUCCESS);
	}
	if op == Op::Restore {
		no_flags(flags, "unknown flag")?;
		if args.len() != 1 {
			return Err(usage("only one argument allowed"));
		}
		let dest = crate::instantiate::absolute_path(cwd, Path::new(&args[0]));
		let mut input = io::stdin();
		let prefix;
		// Inspect the root node while retaining every buffered byte for the
		// store restorer. This gives the command's filesystem diagnostic its
		// operation context without treating malformed NARs as path errors.
		let directory = {
			let mut reader = BufReader::new(Tee {
				inner: &mut input,
				saved: Vec::new(),
			});
			let directory = matches!(
				nix_compat::nar::reader::open(&mut reader)?,
				nix_compat::nar::reader::Node::Directory(_)
			);
			prefix = reader.into_inner().saved;
			directory
		};
		let mut reader = BufReader::new(prefix.as_slice().chain(input));
		noxi_store::nar::restore_path(&dest, &mut reader).map_err(|error| {
			if directory && error.kind() == io::ErrorKind::AlreadyExists && dest.exists() {
				Failure::Error(format!("creating directory {:?}: File exists", args[0]))
			} else {
				Failure::from(error)
			}
		})?;
		return Ok(ExitCode::SUCCESS);
	}
	let store = local_store(uri, settings)?;
	let mut out = io::stdout().lock();
	match op {
		Op::Add => {
			no_flags(flags, "unknown flag")?;
			for arg in args {
				let mut file = dump(cwd, arg, false)?;
				let path = Store::add_to_store_from_dump(
					&store,
					&mut file,
					base_name(arg),
					FileIngestionMethod::NixArchive,
					FileIngestionMethod::NixArchive,
					HashAlgo::Sha256,
					&BTreeSet::new(),
				)?;
				writeln!(out, "{}", print(&path))?;
			}
		}
		Op::AddFixed => {
			let mut method = FileIngestionMethod::Flat;
			for flag in flags {
				if flag == "--recursive" {
					method = FileIngestionMethod::NixArchive;
				} else {
					return Err(usage(format!("unknown flag '{flag}'")));
				}
			}
			let Some((algo, paths)) = args.split_first() else {
				return Err(usage("first argument must be hash algorithm"));
			};
			let algo = parse_algo(algo)?;
			for arg in paths {
				let mut file = dump(cwd, arg, method == FileIngestionMethod::Flat)?;
				let path = Store::add_to_store_from_dump(
					&store,
					&mut file,
					base_name(arg),
					method,
					method,
					algo,
					&BTreeSet::new(),
				)?;
				writeln!(out, "{}", print(&path))?;
			}
		}
		Op::PrintFixedPath => {
			let mut recursive = false;
			for flag in flags {
				if flag == "--recursive" {
					recursive = true;
				} else {
					return Err(usage(format!("unknown flag '{flag}'")));
				}
			}
			let [algo, hash, name] = args else {
				return Err(usage("'--print-fixed-path' requires three arguments"));
			};
			let algo = parse_algo(algo)?;
			let hash = parse_hash(hash, algo)?;
			let path = nix_compat::store_path::build_ca_path(name, recursive, &hash, [], false)
				.map_err(|e| e.to_string())?;
			writeln!(out, "{}", path.to_absolute_path())?;
		}
		Op::PrintEnv => {
			no_flags(flags, "unknown flag")?;
			let [drv] = args else {
				return Err(usage("'--print-env' requires one derivation store path"));
			};
			let drv = parse_store_path(drv)?;
			if !store.is_valid_path(&drv)? {
				return Err(Failure::Error(format!(
					"path '{}' is required, but there is no substituter that can build it",
					print(&drv)
				)));
			}
			let drv = store.read_derivation(&drv)?;
			for (name, value) in drv.environment() {
				out.write_all(b"export ")?;
				out.write_all(name)?;
				out.write_all(b"; ")?;
				out.write_all(name)?;
				out.write_all(b"=")?;
				out.write_all(&shell_quote(value))?;
				out.write_all(b"\n\n")?;
			}
			let args: Vec<&[u8]> = drv.arguments().iter().map(|a| a.as_slice()).collect();
			out.write_all(b"export _args; _args=")?;
			out.write_all(&shell_quote(&args.join(&b' ')))?;
			out.write_all(b"\n")?;
		}
		Op::ReadLog => {
			no_flags(flags, "unknown flag")?;
			for arg in args {
				let path = follow(arg)?;
				let log = build_log(&store, &path)?.ok_or_else(|| {
					Failure::Error(format!(
						"build log of derivation '{}' is not available",
						print(&path)
					))
				})?;
				out.write_all(&log)?;
			}
		}
		Op::DumpDb => {
			no_flags(flags, "unknown flag")?;
			if args.is_empty() {
				for path in store.query_all_valid_paths()? {
					out.write_all(
						validity_registration(&store, &BTreeSet::from([path]), true, true)?
							.as_bytes(),
					)?;
				}
			} else {
				for arg in args {
					let path = follow(arg)?;
					out.write_all(
						validity_registration(&store, &BTreeSet::from([path]), true, true)?
							.as_bytes(),
					)?;
				}
			}
		}
		Op::LoadDb => {
			no_flags(flags, "unknown flag")?;
			if !args.is_empty() {
				return Err(usage("no arguments expected"));
			}
			register_validity(&store, true, true, false)?;
		}
		Op::RegisterValidity => {
			let (mut reregister, mut hash_given) = (false, false);
			for flag in flags {
				match flag.as_str() {
					"--reregister" => reregister = true,
					"--hash-given" => hash_given = true,
					_ => return Err(usage(format!("unknown flag '{flag}'"))),
				}
			}
			if !args.is_empty() {
				return Err(usage("no arguments expected"));
			}
			register_validity(&store, reregister, hash_given, true)?;
		}
		Op::CheckValidity => {
			let mut print_invalid = false;
			for flag in flags {
				if flag == "--print-invalid" {
					print_invalid = true;
				} else {
					return Err(usage(format!("unknown flag '{flag}'")));
				}
			}
			for arg in args {
				let path = follow(arg)?;
				if !store.is_valid_path(&path)? {
					if print_invalid {
						writeln!(out, "{}", print(&path))?;
					} else {
						return Err(Failure::Error(format!(
							"path '{}' is not valid",
							print(&path)
						)));
					}
				}
			}
		}
		Op::Export => {
			unknown_flag(flags)?;
			let mut paths = BTreeSet::new();
			for arg in args {
				paths.insert(follow(arg)?);
			}
			let mut sink = io::BufWriter::new(out);
			for path in crate::paths::topo_sort(&store, &paths)?.into_iter().rev() {
				let info = store.query_path_info(&path)?.ok_or_else(|| {
					Failure::Error(format!("path '{}' is not valid", print(&path)))
				})?;
				write_u64(&mut sink, 1)?;
				let mut nar = Vec::new();
				store.nar_from_path(&path, &mut nar)?;
				let (hash, _) = noxi_store::nar::hash_path(&store.real_path(&path))?;
				if hash != info.nar_hash && info.nar_hash != [0; 32] {
					return Err(Failure::Error(format!(
						"hash of path '{}' has changed from '{}' to 'sha256:{}'!",
						print(&path),
						info.nar_hash_nix32(),
						nix_compat::nixbase32::encode(&hash)
					)));
				}
				sink.write_all(&nar)?;
				write_u64(&mut sink, EXPORT_MAGIC)?;
				write_string(&mut sink, print(&path).as_bytes())?;
				write_u64(&mut sink, info.references.len() as u64)?;
				for reference in &info.references {
					write_string(&mut sink, print(reference).as_bytes())?;
				}
				let deriver = info.deriver.as_ref().map(print).unwrap_or_default();
				write_string(&mut sink, deriver.as_bytes())?;
				write_u64(&mut sink, 0)?;
			}
			write_u64(&mut sink, 0)?;
			sink.flush()?;
		}
		Op::Import => {
			unknown_flag(flags)?;
			if !args.is_empty() {
				return Err(usage("no arguments expected"));
			}
			let stdin = io::stdin();
			let mut input = BufReader::new(stdin.lock());
			loop {
				let n = read_u64(&mut input)?;
				if n == 0 {
					break;
				}
				if n != 1 {
					return Err(Failure::Error(
						"input doesn't look like something created by 'nix-store --export'".into(),
					));
				}
				let mut tee = Tee {
					inner: &mut input,
					saved: Vec::new(),
				};
				noxi_store::nar_listing::parse_listing(&mut tee).map_err(Failure::Error)?;
				let nar = tee.saved;
				let magic = read_u64(&mut input)? & 0xffff_ffff;
				if magic != EXPORT_MAGIC {
					return Err(Failure::Error(
						"Nix archive cannot be imported; wrong format".into(),
					));
				}
				let path = parse_store_path(&read_string(&mut input)?)?;
				let count = read_u64(&mut input)?;
				let mut references = BTreeSet::new();
				for _ in 0..count {
					references.insert(parse_store_path(&read_string(&mut input)?)?);
				}
				let deriver = read_string(&mut input)?;
				let mut digester = nix_compat::nixhash::NixHashDigester::new(HashAlgo::Sha256);
				digester.update(&nar);
				let NixHash::Sha256(digest) = digester.finalize() else {
					unreachable!("SHA-256 was requested");
				};
				let mut info = ValidPathInfo::new(path.clone(), digest);
				if !deriver.is_empty() {
					info.deriver = Some(parse_store_path(&deriver)?);
				}
				info.references = references;
				info.nar_size = nar.len() as u64;
				if read_u64(&mut input)? == 1 {
					read_string(&mut input)?;
				}
				store.add_to_store(&info, &mut nar.as_slice(), false, false)?;
				writeln!(out, "{}", print(&path))?;
				out.flush()?;
			}
		}
		Op::VerifyPath => {
			no_flags(flags, "no flags expected")?;
			let mut status = 0;
			for arg in args {
				let path = follow(arg)?;
				log(
					nix_compat::log::VerbosityLevel::Talkative,
					&format!("checking path '{}'...", print(&path)),
				);
				let info = store.query_path_info(&path)?.ok_or_else(|| {
					Failure::Error(format!("path '{}' is not valid", print(&path)))
				})?;
				let (hash, _) = noxi_store::nar::hash_path(&store.real_path(&path))?;
				if hash != info.nar_hash {
					log(
						nix_compat::log::VerbosityLevel::Error,
						&format!(
							"path '{}' was modified! expected hash '{}', got 'sha256:{}'",
							print(&path),
							info.nar_hash_nix32(),
							nix_compat::nixbase32::encode(&hash)
						),
					);
					status = 1;
				}
			}
			return Ok(ExitCode::from(status));
		}
		Op::RepairPath => {
			no_flags(flags, "no flags expected")?;
			for arg in args {
				let path = follow(arg)?;
				repair_path(&store, settings, &path)?;
			}
		}
		Op::GenerateBinaryCacheKey => {
			unknown_flag(flags)?;
			let [name, secret_file, public_file] = args else {
				return Err(usage("three arguments expected"));
			};
			let key = noxi_store::keys::SecretKey::generate(name)?;
			write_file(public_file, cwd, key.to_public_text().as_bytes(), 0o666)?;
			write_file(secret_file, cwd, key.to_text().as_bytes(), 0o600)?;
		}
		Op::Restore | Op::Dump => unreachable!("handled above"),
	}
	Ok(ExitCode::SUCCESS)
}

/// `writeFile` with a mode and fsync.
fn write_file(path: &str, cwd: &Path, contents: &[u8], mode: u32) -> Result<(), Failure> {
	let path: PathBuf = crate::instantiate::absolute_path(cwd, Path::new(path));
	let mut file = fs::OpenOptions::new()
		.write(true)
		.create(true)
		.truncate(true)
		.mode(mode)
		.open(&path)
		.map_err(|e| {
			Failure::Error(format!(
				"opening file \"{}\": {}",
				path.display(),
				crate::nix::hash::strerror(&e)
			))
		})?;
	file.write_all(contents)?;
	file.sync_all()?;
	Ok(())
}

/// `Store::repairPath`: substitute the path again from a binary cache, else
/// fail; rebuilding from the deriver is not available.
pub fn repair_path(
	store: &LocalStore,
	settings: &Settings,
	path: &StorePath,
) -> Result<(), Failure> {
	let caches = if settings.get_bool("substitute", true) {
		settings
			.get_list("substituters", &["https://cache.nixos.org/"])
			.iter()
			.map(|uri| noxi_store::binary_cache::BinaryCache::open(uri))
			.collect::<Result<Vec<_>, _>>()?
	} else {
		Vec::new()
	};
	for cache in &caches {
		if let Some(cached) = cache.query_path_info(path)? {
			let mut nar = cache.nar(&cached)?;
			store.add_to_store(&cached.info, &mut nar, true, true)?;
			return Ok(());
		}
	}
	let info = store
		.query_path_info(path)?
		.ok_or_else(|| Failure::Error(format!("path '{}' is not valid", print(path))))?;
	if let Some(deriver) = &info.deriver
		&& store.is_valid_path(deriver)?
	{
		return Err(Failure::Error(
			"nix-store --repair-path by rebuilding the deriver is not supported by noxi yet".into(),
		));
	}
	Err(Failure::Error(format!(
		"cannot repair path '{}'",
		print(path)
	)))
}

/// `dumpPath`'s errors for a missing or unsupported file.
fn check_dumpable(path: &Path) -> Result<(), Failure> {
	let meta = fs::symlink_metadata(path).map_err(|e| {
		Failure::Error(format!(
			"opening file \"{}\": {}",
			path.display(),
			crate::nix::hash::strerror(&e)
		))
	})?;
	let kind = meta.file_type();
	if !(kind.is_file() || kind.is_dir() || kind.is_symlink()) {
		return Err(Failure::Error(format!(
			"file \"{}\" has an unsupported type",
			path.display()
		)));
	}
	Ok(())
}
