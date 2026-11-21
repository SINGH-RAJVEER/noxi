//! xz and legacy lzma compression through the system liblzma.
//!
//! CppNix 2.35.2 compresses cache uploads with libarchive's xz and lzma
//! filters, which drive liblzma with a preset (6 by default) and a CRC64
//! check. lzma-rust2's encoder makes different, equally valid streams for
//! inputs larger than a few kilobytes, so uploads would not match the
//! reference's `FileHash`, `FileSize` and `URL`. Calling liblzma itself
//! reproduces the reference's bytes.

use std::io::{self, Read, Write};
use std::os::raw::c_void;

/// `lzma_stream` from liblzma 5's `base.h`.
#[repr(C)]
struct LzmaStream {
	next_in: *const u8,
	avail_in: usize,
	total_in: u64,
	next_out: *mut u8,
	avail_out: usize,
	total_out: u64,
	allocator: *const c_void,
	internal: *mut c_void,
	reserved_ptr1: *mut c_void,
	reserved_ptr2: *mut c_void,
	reserved_ptr3: *mut c_void,
	reserved_ptr4: *mut c_void,
	seek_pos: u64,
	reserved_int2: u64,
	reserved_int3: usize,
	reserved_int4: usize,
	reserved_enum1: u32,
	reserved_enum2: u32,
}

/// `lzma_mt` from liblzma 5's `container.h`.
#[repr(C)]
struct LzmaMt {
	flags: u32,
	threads: u32,
	block_size: u64,
	timeout: u32,
	preset: u32,
	filters: *const c_void,
	check: u32,
	reserved_enum1: u32,
	reserved_enum2: u32,
	reserved_enum3: u32,
	reserved_int1: u32,
	reserved_int2: u32,
	reserved_int3: u32,
	reserved_int4: u32,
	memlimit_threading: u64,
	memlimit_stop: u64,
	reserved_int7: u64,
	reserved_int8: u64,
	reserved_ptr1: *mut c_void,
	reserved_ptr2: *mut c_void,
	reserved_ptr3: *mut c_void,
	reserved_ptr4: *mut c_void,
}

/// `lzma_options_lzma` from liblzma 5's `lzma12.h`.
#[repr(C)]
struct LzmaOptions {
	dict_size: u32,
	preset_dict: *const u8,
	preset_dict_size: u32,
	lc: u32,
	lp: u32,
	pb: u32,
	mode: u32,
	nice_len: u32,
	mf: u32,
	depth: u32,
	ext_flags: u32,
	ext_size_low: u32,
	ext_size_high: u32,
	reserved_int4: u32,
	reserved_int5: u32,
	reserved_int6: u32,
	reserved_int7: u32,
	reserved_int8: u32,
	reserved_enum1: u32,
	reserved_enum2: u32,
	reserved_enum3: u32,
	reserved_enum4: u32,
	reserved_ptr1: *mut c_void,
	reserved_ptr2: *mut c_void,
}

const LZMA_OK: u32 = 0;
const LZMA_STREAM_END: u32 = 1;
const LZMA_RUN: u32 = 0;
const LZMA_FINISH: u32 = 3;
const LZMA_CHECK_CRC64: u32 = 4;

unsafe extern "C" {
	fn lzma_easy_encoder(strm: *mut LzmaStream, preset: u32, check: u32) -> u32;
	fn lzma_stream_encoder_mt(strm: *mut LzmaStream, options: *const LzmaMt) -> u32;
	fn lzma_alone_encoder(strm: *mut LzmaStream, options: *const LzmaOptions) -> u32;
	fn lzma_lzma_preset(options: *mut LzmaOptions, preset: u32) -> u8;
	fn lzma_cputhreads() -> u32;
	fn lzma_code(strm: *mut LzmaStream, action: u32) -> u32;
	fn lzma_end(strm: *mut LzmaStream);
}

/// The container to write.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
	/// `.xz`, as libarchive's `xz` filter writes it.
	Xz,
	/// The legacy `.lzma` format of libarchive's `lzma` filter.
	Lzma,
}

/// An initialised encoder; ends the stream when dropped.
struct Encoder(Box<LzmaStream>);

impl Drop for Encoder {
	fn drop(&mut self) {
		// SAFETY: the stream was initialised by an encoder constructor.
		unsafe { lzma_end(&mut *self.0) };
	}
}

fn check(ret: u32, what: &str) -> io::Result<()> {
	if ret == LZMA_OK {
		Ok(())
	} else {
		Err(io::Error::other(format!(
			"{what} failed with liblzma error {ret}"
		)))
	}
}

impl Encoder {
	/// libarchive's settings for `compression-level` (`-1` for the
	/// default) and `threads=0`, which `parallel-compression` sets.
	fn new(format: Format, level: i32, parallel: bool) -> io::Result<Self> {
		let preset = if (0..=9).contains(&level) {
			level as u32
		} else {
			6
		};
		// SAFETY: an all-zero `lzma_stream` is `LZMA_STREAM_INIT`.
		let mut stream: Box<LzmaStream> = Box::new(unsafe { std::mem::zeroed() });
		match format {
			Format::Xz => {
				// libarchive resolves `threads=0` to `lzma_cputhreads()` and
				// only uses the multithreaded encoder for more than one.
				// SAFETY: plain function call.
				let threads = if parallel {
					unsafe { lzma_cputhreads() }
				} else {
					1
				};
				if threads > 1 {
					// SAFETY: an all-zero `lzma_mt` is a valid starting point.
					let mut options: LzmaMt = unsafe { std::mem::zeroed() };
					options.threads = threads;
					options.timeout = 300;
					options.preset = preset;
					options.check = LZMA_CHECK_CRC64;
					// SAFETY: `stream` and `options` are valid for the call.
					check(
						unsafe { lzma_stream_encoder_mt(&mut *stream, &options) },
						"starting xz compression",
					)?;
				} else {
					// SAFETY: `stream` is a valid, initialised stream.
					check(
						unsafe { lzma_easy_encoder(&mut *stream, preset, LZMA_CHECK_CRC64) },
						"starting xz compression",
					)?;
				}
			}
			Format::Lzma => {
				// SAFETY: an all-zero options struct is filled in by the preset.
				let mut options: LzmaOptions = unsafe { std::mem::zeroed() };
				// SAFETY: `options` is valid for writing.
				if unsafe { lzma_lzma_preset(&mut options, preset) } != 0 {
					return Err(io::Error::other("invalid lzma preset"));
				}
				// SAFETY: `stream` and `options` are valid for the call.
				check(
					unsafe { lzma_alone_encoder(&mut *stream, &options) },
					"starting lzma compression",
				)?;
			}
		}
		Ok(Encoder(stream))
	}

	/// Feed `input`, writing all output produced to `writer`.
	fn code(&mut self, input: &[u8], action: u32, writer: &mut dyn Write) -> io::Result<()> {
		let mut out = [0u8; 65536];
		self.0.next_in = input.as_ptr();
		self.0.avail_in = input.len();
		loop {
			self.0.next_out = out.as_mut_ptr();
			self.0.avail_out = out.len();
			// SAFETY: the input and output buffers outlive the call.
			let ret = unsafe { lzma_code(&mut *self.0, action) };
			let produced = out.len() - self.0.avail_out;
			writer.write_all(&out[..produced])?;
			match ret {
				LZMA_STREAM_END => return Ok(()),
				LZMA_OK if action == LZMA_RUN && self.0.avail_in == 0 => return Ok(()),
				LZMA_OK => {}
				error => {
					return Err(io::Error::other(format!(
						"xz compression failed with liblzma error {error}"
					)));
				}
			}
		}
	}
}

/// Compress all of `reader` into `writer`.
pub fn compress(
	format: Format,
	level: i32,
	parallel: bool,
	reader: &mut dyn Read,
	writer: &mut dyn Write,
) -> io::Result<()> {
	let mut encoder = Encoder::new(format, level, parallel)?;
	let mut buffer = vec![0u8; 65536];
	loop {
		let n = match reader.read(&mut buffer) {
			Ok(0) => break,
			Ok(n) => n,
			Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
			Err(e) => return Err(e),
		};
		encoder.code(&buffer[..n], LZMA_RUN, writer)?;
	}
	encoder.code(&[], LZMA_FINISH, writer)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn streams_decode_with_the_existing_readers() {
		let data: Vec<u8> = (0..200_000u32)
			.flat_map(|i| (i % 251).to_le_bytes())
			.collect();
		for (format, method) in [(Format::Xz, "xz"), (Format::Lzma, "lzma")] {
			for parallel in [false, true] {
				let mut compressed = Vec::new();
				compress(format, -1, parallel, &mut &data[..], &mut compressed).unwrap();
				let mut decoded = Vec::new();
				crate::binary_cache::decompress(
					Some(method),
					Box::new(std::io::Cursor::new(compressed)),
				)
				.unwrap()
				.read_to_end(&mut decoded)
				.unwrap();
				assert_eq!(decoded, data, "{method} parallel={parallel}");
			}
		}
	}
}
