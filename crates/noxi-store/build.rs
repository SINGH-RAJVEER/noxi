//! Link the system liblzma, which `src/xz.rs` uses to compress cache
//! uploads as CppNix's libarchive does.

fn main() {
	println!("cargo:rerun-if-changed=build.rs");
	if pkg_config::Config::new()
		.atleast_version("5.2")
		.probe("liblzma")
		.is_err()
	{
		println!("cargo:rustc-link-lib=dylib=lzma");
	}
}
