//! Cache compressions that CppNix 2.35.2 hands to libarchive and libbrotli.
//!
//! `ArchiveCompressionSink` compresses with libarchive's write filters
//! (`compress`, `grzip`, `lrzip`, `lz4`, `lzip`, `lzop` here), writing a
//! raw archive of one file, and `ArchiveDecompressionSource` reads them
//! back through a raw-format reader with the one filter enabled.
//! `BrotliCompressionSink` drives libbrotlienc. Calling the same libraries
//! gives the reference's bytes and its errors, such as `couldn't
//! initialize compression (Using external lz4 program)` from a libarchive
//! built without liblz4, as nixpkgs' is.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::io::{self, BufRead, Read, Write};

#[repr(C)]
struct Archive {
	_private: [u8; 0],
}

#[repr(C)]
struct ArchiveEntry {
	_private: [u8; 0],
}

type OpenCallback = unsafe extern "C" fn(*mut Archive, *mut c_void) -> c_int;
type WriteCallback = unsafe extern "C" fn(*mut Archive, *mut c_void, *const c_void, usize) -> isize;
type ReadCallback = unsafe extern "C" fn(*mut Archive, *mut c_void, *mut *const c_void) -> isize;
type CloseCallback = unsafe extern "C" fn(*mut Archive, *mut c_void) -> c_int;

const ARCHIVE_OK: c_int = 0;
const ARCHIVE_EOF: c_int = 1;
const ARCHIVE_WARN: c_int = -20;
/// `AE_IFREG`.
const REGULAR_FILE: c_uint = 0o100000;
const EIO: c_int = 5;

unsafe extern "C" {
	fn archive_write_new() -> *mut Archive;
	fn archive_write_add_filter_compress(archive: *mut Archive) -> c_int;
	fn archive_write_add_filter_gzip(archive: *mut Archive) -> c_int;
	fn archive_write_add_filter_grzip(archive: *mut Archive) -> c_int;
	fn archive_write_add_filter_lrzip(archive: *mut Archive) -> c_int;
	fn archive_write_add_filter_lz4(archive: *mut Archive) -> c_int;
	fn archive_write_add_filter_lzip(archive: *mut Archive) -> c_int;
	fn archive_write_add_filter_lzop(archive: *mut Archive) -> c_int;
	fn archive_write_set_format_raw(archive: *mut Archive) -> c_int;
	fn archive_write_set_filter_option(
		archive: *mut Archive,
		module: *const c_char,
		option: *const c_char,
		value: *const c_char,
	) -> c_int;
	fn archive_write_set_bytes_per_block(archive: *mut Archive, bytes: c_int) -> c_int;
	fn archive_write_set_bytes_in_last_block(archive: *mut Archive, bytes: c_int) -> c_int;
	fn archive_write_open(
		archive: *mut Archive,
		client: *mut c_void,
		open: Option<OpenCallback>,
		write: Option<WriteCallback>,
		close: Option<CloseCallback>,
	) -> c_int;
	fn archive_write_header(archive: *mut Archive, entry: *mut ArchiveEntry) -> c_int;
	fn archive_write_data(archive: *mut Archive, data: *const c_void, size: usize) -> isize;
	fn archive_write_close(archive: *mut Archive) -> c_int;
	fn archive_write_free(archive: *mut Archive) -> c_int;

	fn archive_entry_new() -> *mut ArchiveEntry;
	fn archive_entry_set_filetype(entry: *mut ArchiveEntry, kind: c_uint);
	fn archive_entry_free(entry: *mut ArchiveEntry);

	fn archive_read_new() -> *mut Archive;
	fn archive_read_support_filter_by_code(archive: *mut Archive, code: c_int) -> c_int;
	fn archive_read_support_format_raw(archive: *mut Archive) -> c_int;
	fn archive_read_support_format_empty(archive: *mut Archive) -> c_int;
	fn archive_read_set_option(
		archive: *mut Archive,
		module: *const c_char,
		option: *const c_char,
		value: *const c_char,
	) -> c_int;
	fn archive_read_open(
		archive: *mut Archive,
		client: *mut c_void,
		open: Option<OpenCallback>,
		read: Option<ReadCallback>,
		close: Option<CloseCallback>,
	) -> c_int;
	fn archive_read_next_header(archive: *mut Archive, entry: *mut *mut ArchiveEntry) -> c_int;
	fn archive_read_data(archive: *mut Archive, data: *mut c_void, size: usize) -> isize;
	fn archive_filter_count(archive: *mut Archive) -> c_int;
	fn archive_read_free(archive: *mut Archive) -> c_int;

	fn archive_error_string(archive: *mut Archive) -> *const c_char;
	fn archive_set_error(archive: *mut Archive, code: c_int, format: *const c_char, ...);

	fn BrotliEncoderCreateInstance(
		alloc: *const c_void,
		free: *const c_void,
		opaque: *mut c_void,
	) -> *mut c_void;
	fn BrotliEncoderCompressStream(
		state: *mut c_void,
		op: c_int,
		available_in: *mut usize,
		next_in: *mut *const u8,
		available_out: *mut usize,
		next_out: *mut *mut u8,
		total_out: *mut usize,
	) -> c_int;
	fn BrotliEncoderIsFinished(state: *mut c_void) -> c_int;
	fn BrotliEncoderDestroyInstance(state: *mut c_void);
}

/// The methods decompressed here, and compressed here with `gzip`, whose
/// stream libarchive makes with zlib.
pub(crate) fn handles(method: &str) -> bool {
	matches!(
		method,
		"compress" | "grzip" | "lrzip" | "lz4" | "lzip" | "lzop"
	)
}

/// `archive_error_string`, or the empty string.
fn error_string(archive: *mut Archive) -> String {
	// SAFETY: `archive` is live; the string, if any, is NUL-terminated and
	// owned by it.
	let text = unsafe { archive_error_string(archive) };
	if text.is_null() {
		String::new()
	} else {
		unsafe { CStr::from_ptr(text) }
			.to_string_lossy()
			.into_owned()
	}
}

/// `ArchiveCompressionSink::check`.
fn check(archive: *mut Archive, status: c_int, reason: &str) -> io::Result<()> {
	match status {
		ARCHIVE_OK => Ok(()),
		ARCHIVE_EOF => Err(io::Error::other("reached end of archive")),
		_ => Err(io::Error::other(
			reason.replace("%s", &error_string(archive)),
		)),
	}
}

struct WriteState<'a> {
	writer: &'a mut dyn Write,
	error: Option<io::Error>,
}

unsafe extern "C" fn write_callback(
	_archive: *mut Archive,
	client: *mut c_void,
	buffer: *const c_void,
	length: usize,
) -> isize {
	// SAFETY: `client` is the `WriteState` that `compress` keeps alive while
	// the archive is open, and `buffer` holds `length` bytes.
	let state = unsafe { &mut *(client as *mut WriteState<'_>) };
	let data = unsafe { std::slice::from_raw_parts(buffer as *const u8, length) };
	match state.writer.write_all(data) {
		Ok(()) => length as isize,
		Err(e) => {
			state.error.get_or_insert(e);
			-1
		}
	}
}

/// An archive being written, freed on drop.
struct Writer(*mut Archive);

impl Drop for Writer {
	fn drop(&mut self) {
		// SAFETY: the archive was created by `archive_write_new`.
		unsafe { archive_write_free(self.0) };
	}
}

/// Set up libarchive's filter for `method` as `ArchiveCompressionSink`'s
/// constructor does, before it is opened. `level` is `None` for the
/// filter's default.
fn writer(method: &str, level: Option<i32>, parallel: bool) -> io::Result<Writer> {
	// SAFETY: plain libarchive calls on an archive this function owns.
	let archive = unsafe { archive_write_new() };
	if archive.is_null() {
		return Err(io::Error::other("failed to initialize libarchive"));
	}
	let writer = Writer(archive);
	let add: unsafe extern "C" fn(*mut Archive) -> c_int = match method {
		"compress" => archive_write_add_filter_compress,
		"gzip" => archive_write_add_filter_gzip,
		"grzip" => archive_write_add_filter_grzip,
		"lrzip" => archive_write_add_filter_lrzip,
		"lz4" => archive_write_add_filter_lz4,
		"lzip" => archive_write_add_filter_lzip,
		"lzop" => archive_write_add_filter_lzop,
		_ => return Err(io::Error::other("unsupported cache upload compression")),
	};
	let name = CString::new(method).expect("no NUL");
	unsafe {
		check(
			archive,
			add(archive),
			"couldn't initialize compression (%s)",
		)?;
		check(
			archive,
			archive_write_set_format_raw(archive),
			"failed to compress (%s)",
		)?;
		if parallel {
			check(
				archive,
				archive_write_set_filter_option(
					archive,
					name.as_ptr(),
					c"threads".as_ptr(),
					c"0".as_ptr(),
				),
				"failed to compress (%s)",
			)?;
		}
		if let Some(level) = level {
			let level = CString::new(level.to_string()).expect("no NUL");
			check(
				archive,
				archive_write_set_filter_option(
					archive,
					name.as_ptr(),
					c"compression-level".as_ptr(),
					level.as_ptr(),
				),
				"failed to compress (%s)",
			)?;
		}
		check(
			archive,
			archive_write_set_bytes_per_block(archive, 0),
			"failed to compress (%s)",
		)?;
		check(
			archive,
			archive_write_set_bytes_in_last_block(archive, 1),
			"failed to compress (%s)",
		)?;
	}
	Ok(writer)
}

/// Whether libarchive can compress with `method`: the setup
/// `ArchiveCompressionSink` does before anything is written, with its
/// error.
pub(crate) fn check_method(method: &str, level: Option<i32>, parallel: bool) -> io::Result<()> {
	writer(method, level, parallel).map(drop)
}

/// Compress all of `reader` into `writer` as `ArchiveCompressionSink` does.
pub(crate) fn compress(
	method: &str,
	level: Option<i32>,
	parallel: bool,
	reader: &mut dyn Read,
	writer_: &mut dyn Write,
) -> io::Result<()> {
	let writer = writer(method, level, parallel)?;
	let archive = writer.0;
	let mut state = WriteState {
		writer: writer_,
		error: None,
	};
	let take_error = |state: &mut WriteState<'_>, e: io::Error| state.error.take().unwrap_or(e);
	// SAFETY: `state` outlives the archive's use of it: the archive is
	// closed below, or freed when `writer` drops, before `state` goes.
	unsafe {
		let status = archive_write_open(
			archive,
			&mut state as *mut WriteState<'_> as *mut c_void,
			None,
			Some(write_callback),
			None,
		);
		check(archive, status, "failed to compress (%s)").map_err(|e| take_error(&mut state, e))?;
		let entry = archive_entry_new();
		archive_entry_set_filetype(entry, REGULAR_FILE);
		let status = archive_write_header(archive, entry);
		archive_entry_free(entry);
		check(archive, status, "failed to compress (%s)").map_err(|e| take_error(&mut state, e))?;
		let mut buffer = vec![0u8; 32 * 1024];
		loop {
			let n = match reader.read(&mut buffer) {
				Ok(0) => break,
				Ok(n) => n,
				Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
				Err(e) => return Err(e),
			};
			let written = archive_write_data(archive, buffer.as_ptr() as *const c_void, n);
			if written <= 0 {
				check(archive, written as c_int, "failed to compress (%s)")
					.map_err(|e| take_error(&mut state, e))?;
			}
		}
		let status = archive_write_close(archive);
		check(archive, status, "failed to compress (%s)").map_err(|e| take_error(&mut state, e))?;
	}
	drop(writer);
	match state.error {
		Some(e) => Err(e),
		None => Ok(()),
	}
}

/// `ARCHIVE_FILTER_*` codes.
fn filter_code(method: &str) -> Option<c_int> {
	Some(match method {
		"compress" => 3,
		"lzip" => 9,
		"lrzip" => 10,
		"lzop" => 11,
		"grzip" => 12,
		"lz4" => 13,
		_ => return None,
	})
}

struct ReadState {
	reader: Box<dyn BufRead + Send>,
	buffer: Vec<u8>,
}

unsafe extern "C" fn read_callback(
	archive: *mut Archive,
	client: *mut c_void,
	buffer: *mut *const c_void,
) -> isize {
	// SAFETY: `client` is the boxed `ReadState` the `Decompressor` owns.
	let state = unsafe { &mut *(client as *mut ReadState) };
	unsafe { *buffer = state.buffer.as_ptr() as *const c_void };
	loop {
		match state.reader.read(&mut state.buffer) {
			Ok(n) => return n as isize,
			Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
			Err(e) => {
				let message =
					CString::new(format!("Source threw exception: {e}")).unwrap_or_default();
				unsafe { archive_set_error(archive, EIO, c"%s".as_ptr(), message.as_ptr()) };
				return -1;
			}
		}
	}
}

/// `ArchiveDecompressionSource`: the decompressed contents of a raw
/// archive compressed with one filter.
pub(crate) struct Decompressor {
	archive: *mut Archive,
	/// Read through `archive`'s callback; boxed so that its address is
	/// stable.
	_state: Box<ReadState>,
	started: bool,
}

// SAFETY: the archive and its client state are owned by this value and
// used only through `&mut self`.
unsafe impl Send for Decompressor {}

impl Drop for Decompressor {
	fn drop(&mut self) {
		// SAFETY: the archive was created by `archive_read_new`.
		unsafe { archive_read_free(self.archive) };
	}
}

impl Decompressor {
	/// `TarArchive(source, raw = true, method)`.
	pub(crate) fn new(method: &str, reader: Box<dyn BufRead + Send>) -> io::Result<Self> {
		let code = filter_code(method)
			.ok_or_else(|| io::Error::other(format!("unknown compression method '{method}'")))?;
		// SAFETY: plain libarchive calls on an archive this value owns.
		let archive = unsafe { archive_read_new() };
		if archive.is_null() {
			return Err(io::Error::other("failed to initialize libarchive"));
		}
		let mut state = Box::new(ReadState {
			reader,
			buffer: vec![0; 65536],
		});
		let mut this = Self {
			archive,
			_state: Box::new(ReadState {
				reader: Box::new(io::empty()),
				buffer: Vec::new(),
			}),
			started: false,
		};
		unsafe {
			let status = archive_read_support_filter_by_code(archive, code);
			if status == ARCHIVE_WARN {
				// `TarArchive::check` without fatal warnings.
				if nix_compat::log::log_enabled(nix_compat::log::VerbosityLevel::Warn) {
					eprintln!(
						"warning: enabling libarchive decompression filter ({})",
						error_string(archive)
					);
				}
			} else {
				check(
					archive,
					status,
					"enabling libarchive decompression filter (%s)",
				)?;
			}
			archive_read_support_format_raw(archive);
			archive_read_support_format_empty(archive);
			archive_read_set_option(
				archive,
				std::ptr::null(),
				c"mac-ext".as_ptr(),
				std::ptr::null(),
			);
			let client = &mut *state as *mut ReadState as *mut c_void;
			this._state = state;
			check(
				archive,
				archive_read_open(archive, client, None, Some(read_callback), None),
				"Failed to open archive (%s)",
			)?;
		}
		Ok(this)
	}
}

impl Read for Decompressor {
	fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
		// SAFETY: the archive is open and owned by `self`.
		unsafe {
			if !self.started {
				self.started = true;
				let mut entry = std::ptr::null_mut();
				check(
					self.archive,
					archive_read_next_header(self.archive, &mut entry),
					"failed to read header (%s)",
				)?;
				if archive_filter_count(self.archive) < 2 {
					return Err(io::Error::other("input compression not recognized"));
				}
			}
			if buf.is_empty() {
				return Ok(0);
			}
			let n = archive_read_data(self.archive, buf.as_mut_ptr() as *mut c_void, buf.len());
			if n >= 0 {
				return Ok(n as usize);
			}
			check(
				self.archive,
				n as c_int,
				"failed to read compressed data (%s)",
			)?;
			Ok(0)
		}
	}
}

/// `BrotliCompressionSink`: libbrotlienc's defaults, fed at most 128 KiB
/// at a time, then finished.
pub(crate) fn compress_brotli(reader: &mut dyn Read, writer: &mut dyn Write) -> io::Result<()> {
	struct State(*mut c_void);
	impl Drop for State {
		fn drop(&mut self) {
			// SAFETY: created by `BrotliEncoderCreateInstance`.
			unsafe { BrotliEncoderDestroyInstance(self.0) };
		}
	}
	// SAFETY: default allocator.
	let state = State(unsafe {
		BrotliEncoderCreateInstance(std::ptr::null(), std::ptr::null(), std::ptr::null_mut())
	});
	if state.0.is_null() {
		return Err(io::Error::other("unable to initialise brotli encoder"));
	}
	const PROCESS: c_int = 0;
	const FINISH: c_int = 2;
	let mut input = vec![0u8; 128 * 1024];
	// `BUFSIZ`.
	let mut output = vec![0u8; 8192];
	let mut finished = false;
	let mut feed = |data: Option<&[u8]>, finished: &mut bool| -> io::Result<()> {
		let mut next_in = data.map_or(std::ptr::null(), <[u8]>::as_ptr);
		let mut avail_in = data.map_or(0, <[u8]>::len);
		let mut next_out = output.as_mut_ptr();
		let mut avail_out = output.len();
		while !*finished && (data.is_none() || avail_in != 0) {
			// SAFETY: the pointers and counts describe `data` and `output`.
			let ok = unsafe {
				BrotliEncoderCompressStream(
					state.0,
					if data.is_some() { PROCESS } else { FINISH },
					&mut avail_in,
					&mut next_in,
					&mut avail_out,
					&mut next_out,
					std::ptr::null_mut(),
				)
			};
			if ok == 0 {
				return Err(io::Error::other(
					"error while compressing brotli compression",
				));
			}
			if avail_out < output.len() || avail_in == 0 {
				writer.write_all(&output[..output.len() - avail_out])?;
				next_out = output.as_mut_ptr();
				avail_out = output.len();
			}
			// SAFETY: the encoder is live.
			*finished = unsafe { BrotliEncoderIsFinished(state.0) } != 0;
		}
		Ok(())
	};
	loop {
		let mut filled = 0;
		while filled < input.len() {
			match reader.read(&mut input[filled..]) {
				Ok(0) => break,
				Ok(n) => filled += n,
				Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
				Err(e) => return Err(e),
			}
		}
		if filled == 0 {
			break;
		}
		feed(Some(&input[..filled]), &mut finished)?;
	}
	feed(None, &mut finished)
}
