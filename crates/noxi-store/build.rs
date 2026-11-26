//! Link the system liblzma, which `src/xz.rs` uses to compress cache
//! uploads as CppNix's libarchive does, and libarchive and libbrotlienc,
//! which `src/archive.rs` uses for the compressions CppNix hands to them.

fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	for (library, version, name) in [
		("liblzma", "5.2", "lzma"),
		("libarchive", "3.4", "archive"),
		("libbrotlienc", "1.0", "brotlienc"),
	] {
		if pkg_config::Config::new()
			.atleast_version(version)
			.probe(library)
			.is_err()
		{
			println!("cargo:rustc-link-lib=dylib={name}");
		}
	}
}
