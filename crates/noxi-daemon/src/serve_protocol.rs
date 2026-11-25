//! The serve protocol's server, CppNix 2.35.2's `nix-store --serve`
//! (`opServe` in `nix-store.cc`, `serve-protocol.cc`).

use std::collections::BTreeSet;
use std::io::{BufReader, Read, Write};
use std::os::fd::RawFd;

use nix_compat::log::{VerbosityLevel, set_verbosity};
use nix_compat::store_path::StorePath;
use noxi_build::backend::LocalBackend;
use noxi_store::build_derivation::OutputFeatures;
use noxi_store::{DerivedPath, Store, ValidPathInfo};

use crate::client::DaemonStore;
use crate::connection::{HangupMonitor, read_basic_derivation};
use crate::error::{Error, Result};
use crate::legacy_ssh::{
	BuildOptions, SERVE_MAGIC_1, SERVE_MAGIC_2, SERVE_VERSION, command, parse_nar_hash,
};
use crate::nar_stream::with_nar;
use crate::peer::{BuildResult, Peer, build_locally};
use crate::wire::{Reader, Version, Writer, parse_content_address, parse_store_path, version};

/// `exportMagic`.
const EXPORT_MAGIC: u64 = 0x4558494e;

/// A store `nix-store --serve` can serve.
pub trait Served: Peer + Store {
	/// `getBuildSettings`: the client's options for the next build. They
	/// have no effect on a store that a daemon owns.
	fn set_build_options(&mut self, options: &BuildOptions);

	/// [`Peer::build_derivation`], cancelled when the client on `fd` hangs
	/// up.
	fn build_for(
		&self,
		fd: RawFd,
		drv_path: &StorePath,
		drv: &noxi_store::build_derivation::BuildDerivation,
	) -> Result<BuildResult>;
}

impl Served for LocalBackend {
	fn set_build_options(&mut self, options: &BuildOptions) {
		let settings = &mut self.settings;
		settings.keep_build_log = false;
		settings.use_substitutes = false;
		settings.run_diff_hook = true;
		settings.max_silent_time = options.max_silent_time;
		settings.timeout = options.build_timeout;
		settings.keep_failed = options.keep_failed;
		for (name, value) in [
			("keep-build-log", "false".to_owned()),
			("run-diff-hook", "true".to_owned()),
			("timeout", options.build_timeout.to_string()),
			("max-build-log-size", options.max_log_size.to_string()),
		] {
			let _ = settings.config.set(name, &value);
		}
	}

	fn build_for(
		&self,
		fd: RawFd,
		drv_path: &StorePath,
		drv: &noxi_store::build_derivation::BuildDerivation,
	) -> Result<BuildResult> {
		let monitor = HangupMonitor::new(fd)?;
		let cancelled = monitor.cancelled();
		Ok(build_locally(
			&self.store,
			&self.settings,
			self.open_substituters(),
			Some(&cancelled),
			drv_path,
			drv,
		))
	}
}

impl Served for DaemonStore {
	fn set_build_options(&mut self, _options: &BuildOptions) {}

	fn build_for(
		&self,
		_fd: RawFd,
		drv_path: &StorePath,
		drv: &noxi_store::build_derivation::BuildDerivation,
	) -> Result<BuildResult> {
		Ok(self.build_derivation(drv_path, drv)?)
	}
}

/// Serve `store` until the client closes its end. `fd` is the descriptor of
/// `input`.
pub fn serve<S: Served>(
	store: &mut S,
	features: OutputFeatures,
	input: impl Read,
	output: impl Write,
	fd: RawFd,
	write: bool,
) -> Result<()> {
	let mut from = Reader::new(BufReader::new(input));
	let mut to = Writer::new(output);
	if from.read_int()? != SERVE_MAGIC_1 {
		return Err(Error::Nix("protocol mismatch".into()));
	}
	to.u64(SERVE_MAGIC_2).u64(SERVE_VERSION);
	to.flush()?;
	let version = crate::wire::from_wire(from.read_int()?).min(SERVE_VERSION);
	loop {
		let request = match from.read_int() {
			Err(Error::EndOfFile) => return Ok(()),
			request => request?,
		};
		match request {
			command::QUERY_VALID_PATHS => {
				let lock = from.read_int()? != 0;
				let substitute = from.read_int()? != 0;
				let paths = from.read_store_paths()?;
				if lock && write {
					for path in &paths {
						store.add_temp_root(path)?;
					}
				}
				if substitute && write {
					substitute_paths(store, &paths)?;
				}
				let valid = Store::query_valid_paths(store, &paths)?;
				to.store_paths(valid.iter());
			}
			command::QUERY_PATH_INFOS => {
				for path in &from.read_store_paths()? {
					if let Some(info) = store.query_path_info(path)? {
						write_path_info(&mut to, version, &info);
					}
				}
				to.string("");
			}
			command::DUMP_STORE_PATH => {
				let path = from.read_store_path()?;
				if !Store::is_valid_path(store, &path)? {
					return Err(Error::Nix(format!(
						"path '{}' is not valid",
						path.to_absolute_path()
					)));
				}
				to.flush()?;
				store.nar(&path, to.inner_mut())?;
			}
			command::IMPORT_PATHS => {
				allowed(write, "importing paths")?;
				import_paths(store, &mut from)?;
				to.u64(1);
			}
			command::BUILD_PATHS => {
				allowed(write, "building paths")?;
				let mut paths = Vec::new();
				for text in from.read_strings()? {
					paths.push(parse_path_with_outputs(&text)?);
				}
				let options = read_build_options(&mut from, version)?;
				store.set_build_options(&options);
				match Store::build_paths(store, &paths) {
					Ok(()) => {
						to.u64(0);
					}
					Err(noxi_store::Error::Build {
						exit_status,
						message,
					}) => {
						to.u64(exit_status.into()).string(&message);
					}
					Err(error) => {
						to.u64(1).string(&error.to_string());
					}
				}
			}
			command::BUILD_DERIVATION => {
				allowed(write, "building paths")?;
				let drv_path = from.read_store_path()?;
				if !drv_path.name().ends_with(".drv") {
					return Err(Error::Nix(format!(
						"path '{}' is not a derivation",
						drv_path.to_absolute_path()
					)));
				}
				let drv = read_basic_derivation(&mut from, &drv_path, features)?;
				let options = read_build_options(&mut from, version)?;
				store.set_build_options(&options);
				let result = store.build_for(fd, &drv_path, &drv)?;
				write_build_result(&mut to, version, &result);
			}
			command::QUERY_CLOSURE => {
				let include_outputs = from.read_int()? != 0;
				let paths = from.read_store_paths()?;
				let closure = closure(store, paths, include_outputs)?;
				to.store_paths(closure.iter());
			}
			command::ADD_TO_STORE_NAR => {
				allowed(write, "importing paths")?;
				add_to_store_nar(store, &mut from)?;
				to.u64(1);
			}
			request => return Err(Error::Nix(format!("unknown serve command {request}"))),
		}
		to.flush()?;
	}
}

fn allowed(write: bool, what: &str) -> Result<()> {
	if write {
		Ok(())
	} else {
		Err(Error::Nix(format!("{what} is not allowed")))
	}
}

/// `Store::substitutePaths`: a failure is only a warning.
fn substitute_paths<S: Served>(store: &S, paths: &BTreeSet<StorePath>) -> Result<()> {
	let wanted: Vec<DerivedPath> = paths
		.iter()
		.filter(|path| !path.name().ends_with(".drv"))
		.cloned()
		.map(DerivedPath::Opaque)
		.collect();
	let missing = store.query_missing(&wanted)?.will_substitute;
	if !missing.is_empty() {
		let missing: Vec<DerivedPath> = missing.into_iter().map(DerivedPath::Opaque).collect();
		if let Err(error) = Store::build_paths(store, &missing) {
			nix_compat::log::logger::warn(&error.to_string());
		}
	}
	Ok(())
}

/// `computeFSClosure`, with the outputs of the derivations in it if asked.
fn closure<S: Served>(
	store: &S,
	paths: BTreeSet<StorePath>,
	include_outputs: bool,
) -> Result<BTreeSet<StorePath>> {
	let mut closure = store.compute_closure(&paths)?;
	while include_outputs {
		let mut outputs = BTreeSet::new();
		for drv in closure.iter().filter(|path| path.name().ends_with(".drv")) {
			for path in store
				.query_partial_derivation_output_map(drv)?
				.into_values()
			{
				if let Some(path) = path
					&& !closure.contains(&path)
					&& Store::is_valid_path(store, &path)?
				{
					outputs.insert(path);
				}
			}
		}
		if outputs.is_empty() {
			break;
		}
		closure.extend(store.compute_closure(&outputs)?);
	}
	Ok(closure)
}

/// `ServeProto::Serialise<UnkeyedValidPathInfo>::write`, after the path.
fn write_path_info<W: Write>(to: &mut Writer<W>, version: Version, info: &ValidPathInfo) {
	to.store_path(&info.path)
		.opt_store_path(info.deriver.as_ref())
		.store_paths(info.references.iter())
		.u64(info.nar_size)
		.u64(info.nar_size);
	if version >= self::version(2, 4) {
		to.string(&info.nar_hash_nix32())
			.string(
				&info
					.ca
					.as_ref()
					.map(ToString::to_string)
					.unwrap_or_default(),
			)
			.strings(info.sigs.iter().map(String::as_str));
	}
}

/// `ServeProto::Serialise<BuildOptions>::read`. The verbosity of a build is
/// not the client's.
fn read_build_options<R: Read>(from: &mut Reader<R>, version: Version) -> Result<BuildOptions> {
	set_verbosity(VerbosityLevel::Error as u64);
	let mut options = BuildOptions {
		max_silent_time: from.read_int()?,
		build_timeout: from.read_int()?,
		..BuildOptions::default()
	};
	if version >= self::version(2, 2) {
		options.max_log_size = from.read_u64()?;
	}
	if version >= self::version(2, 3) {
		let repeats = from.read_int()?;
		from.read_int()?; // whether to enforce determinism
		if repeats != 0 {
			return Err(Error::Nix(
				"client requested repeating builds, but this is not currently implemented".into(),
			));
		}
	}
	if version >= self::version(2, 7) {
		options.keep_failed = from.read_int()? != 0;
	}
	Ok(options)
}

/// `ServeProto::Serialise<BuildResult>::write`.
fn write_build_result<W: Write>(to: &mut Writer<W>, version: Version, result: &BuildResult) {
	to.u64(result.status).string(&result.message);
	if version >= self::version(2, 3) {
		to.u64(result.times_built)
			.bool(result.non_deterministic)
			.u64(result.start)
			.u64(result.stop);
	}
	if version >= self::version(2, 8) {
		to.u64(result.outputs.len() as u64);
		for (name, path) in &result.outputs {
			to.string(name).store_path(path).u64(0);
		}
	} else if version >= self::version(2, 6) {
		let dummy = format!("sha256:{}", "0".repeat(64));
		to.u64(result.outputs.len() as u64);
		for (name, path) in &result.outputs {
			let id = format!("{dummy}!{name}");
			to.string(&id)
				.string(&format!("{{\"id\":\"{id}\",\"outPath\":\"{path}\"}}"));
		}
	}
}

/// `parsePathWithOutputs` and `toDerivedPath`.
fn parse_path_with_outputs(text: &str) -> Result<DerivedPath> {
	let (path, outputs) = text.split_once('!').unwrap_or((text, ""));
	let path = parse_store_path(path)?;
	let outputs: BTreeSet<String> = outputs
		.split(',')
		.filter(|name| !name.is_empty())
		.map(str::to_owned)
		.collect();
	Ok(if !outputs.is_empty() || path.name().ends_with(".drv") {
		DerivedPath::Built { drv: path, outputs }
	} else {
		DerivedPath::Opaque(path)
	})
}

fn add_to_store_nar<S: Served, R: Read>(store: &S, from: &mut Reader<R>) -> Result<()> {
	let path = from.read_store_path()?;
	let deriver = from.read_string()?;
	let mut info = ValidPathInfo::new(path, parse_nar_hash(&from.read_string()?)?);
	info.deriver = (!deriver.is_empty())
		.then(|| parse_store_path(&deriver))
		.transpose()?;
	info.references = from.read_store_paths()?;
	info.registration_time = i64::try_from(from.read_u64()?).unwrap_or(0);
	info.nar_size = from.read_u64()?;
	info.ultimate = from.read_bool()?;
	info.sigs = from.read_signatures()?.into_iter().collect();
	let ca = from.read_string()?;
	info.ca = (!ca.is_empty())
		.then(|| parse_content_address(&ca))
		.transpose()?;
	if info.nar_size == 0 {
		return Err(Error::Nix(
			"narInfo is too old and missing the narSize field".into(),
		));
	}
	// Exactly the declared size is read, whatever it holds.
	let mut nar = tempfile::tempfile()?;
	let mut sized = from.get_mut().take(info.nar_size);
	if std::io::copy(&mut sized, &mut nar)? != info.nar_size {
		return Err(Error::EndOfFile);
	}
	std::io::Seek::rewind(&mut nar)?;
	store.add_paths(&[info], &mut |_, sink| {
		std::io::copy(&mut nar, sink).map(drop).map_err(Into::into)
	})?;
	Ok(())
}

/// `importPaths` without signature checks.
fn import_paths<S: Served, R: Read>(store: &S, from: &mut Reader<R>) -> Result<()> {
	loop {
		match from.read_u64()? {
			0 => return Ok(()),
			1 => {}
			_ => {
				return Err(Error::Nix(
					"input doesn't look like something created by 'nix-store --export'".into(),
				));
			}
		}
		let (nar, hashed) = with_nar(from.get_mut(), |file| {
			use std::io::Seek;
			let hashed = noxi_store::nar::HashingReader::new(BufReader::new(&*file));
			let hashed = drain(hashed);
			file.rewind()
				.and(hashed)
				.and_then(|hashed| Ok((file.try_clone()?, hashed)))
		})??;
		if from.read_int()? != EXPORT_MAGIC {
			return Err(Error::Nix(
				"Nix archive cannot be imported; wrong format".into(),
			));
		}
		let path = from.read_store_path()?;
		let mut info = ValidPathInfo::new(path, hashed.0);
		info.nar_size = hashed.1;
		info.references = from.read_store_paths()?;
		let deriver = from.read_string()?;
		info.deriver = (!deriver.is_empty())
			.then(|| parse_store_path(&deriver))
			.transpose()?;
		// An optional legacy signature.
		if from.read_int()? == 1 {
			from.read_string()?;
		}
		let mut nar = nar;
		store.add_paths(&[info], &mut |_, sink| {
			std::io::copy(&mut nar, sink).map(drop).map_err(Into::into)
		})?;
	}
}

fn drain<R: std::io::BufRead + Send>(
	mut reader: noxi_store::nar::HashingReader<R>,
) -> std::io::Result<([u8; 32], u64)> {
	std::io::copy(&mut reader, &mut std::io::sink())?;
	Ok(reader.finish())
}
