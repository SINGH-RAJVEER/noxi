# noxi-store

A local Nix store for the noxi engine that shares CppNix 2.35.2's on-disk format. Release `0.0.1` is an experimental preview with unstable APIs. Use isolated temporary stores; this release is not qualified to replace the owner of a live system store.

```toml
[dependencies]
noxi-store = "0.0.1"
```

`LocalStore::open` creates or opens the same layout as CppNix's `local` store: store directory, `.links`, `profiles`, `gcroots`, `temproots`, the schema-10 SQLite database with its named migrations, `big-lock`, and reserved space. `LocalStoreConfig::with_root` selects a store below a root directory, like `--store ROOT`. Locks use `flock(2)` on the same files as CppNix, so both implementations exclude each other.

Implemented operations: path-info, referrer, deriver, derivation-output and hash-part queries; registration of valid paths with derivation output checks and reference cycle detection; NAR import with hash, size, content-address and signature checks; content-addressed import of flat, NAR and text data; NAR export; temporary GC roots; signature addition; path invalidation; and store verification. Metadata is canonicalised as CppNix does.

`create_temp_dir_in_store` and `ca_hash_of` are public for the separate local builder. Handles in one process share their temporary-root file and remove it after the last handle is dropped.

`binary_cache::BinaryCache` reads local and HTTP(S) caches. `LocalStore::substitute` imports paths and their references after signature, NAR hash, and size checks, with supported compressed formats. Cache uploads, signing, authentication/TLS configuration parity, and complete substitution compatibility remain unqualified.

Not yet implemented: garbage collection, store optimisation, builds, the daemon, the `ca-derivations` schema, and `read-only` store mode beyond opening the database immutably. A store directory other than `/nix/store` is not supported. Compatibility tests against the reference live in `crates/compat-tests/tests/reference_store.rs` and `reference_substitute.rs`.

GPL-3.0-only. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md).
