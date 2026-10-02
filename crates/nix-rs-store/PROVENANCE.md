# nix-rs-store provenance

## Sources

The Rust code in this package was written for nix-rs; it is not copied from another Rust project. Its behavior follows [CppNix 2.35.2](https://github.com/NixOS/nix/tree/2c73b59da29606068c0c98db015dd3a66955525d), revision `2c73b59da29606068c0c98db015dd3a66955525d`, licensed LGPL-2.1-or-later. The reference sources are `src/libstore/local-store.cc`, `gc.cc` (temporary roots), `path-info.cc`, `posix-fs-canonicalise.cc`, `sqlite.cc`, `unix/pathlocks.cc`, and `derivations.cc`. Error messages reproduce the reference's wording.

`src/schema.sql` is a verbatim copy of `src/libstore/schema.sql` from that revision, retaining its LGPL-2.1-or-later terms and the Nix authors' authorship. [COPYING](COPYING) reproduces the LGPL 2.1 text from that revision.

The package depends on `nix-rs-compat` for store paths, hashes, derivations and NAR encoding, and on `rusqlite` with SQLite bundled.

The binary-cache implementation follows the reference's `BinaryCacheStore`, `LocalBinaryCacheStore`, `HttpBinaryCacheStore`, and `PathSubstitutionGoal`, using `ureq` and Rust compression libraries for local and HTTP(S) transport. Signature and content checks pass through the separately distributed compatibility and local-store implementations. Selected reference comparisons do not qualify complete cache, TLS, authentication, or live-store operation.

## License

The package is GPL-3.0-only; [LICENSE](LICENSE) reproduces the license text. The copied schema keeps its own LGPL-2.1-or-later notice. Local source history is at https://github.com/SINGH-RAJVEER/nix-rs under `crates/nix-rs-store/`.
