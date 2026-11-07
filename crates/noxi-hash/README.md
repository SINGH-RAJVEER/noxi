# noxi-hash

Nix hash values, digest helpers, and Nix base32 encoding for the noxi engine. Initial release `0.0.1` is an incomplete preview with unstable APIs.

```toml
[dependencies]
noxi-hash = "0.0.1"
```

Use `noxi_hash::nixbase32` for encoding and decoding, and `noxi_hash::nixhash` for hash parsing, formatting, and digesting. Optional `async` and `serde` features add asynchronous digest helpers and Serde support.

This is a format library, not a hash command or store implementation. BLAKE3 is available without enforcing CppNix's `blake3-hashes` experimental setting. Imported Serde representations do not substitute for the reference JSON formats in `nix_compat::cppnix_json`. No full reference-release compatibility qualification is claimed.

Derived from Snix under GPL-3.0-only. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md) for origins and local changes.
