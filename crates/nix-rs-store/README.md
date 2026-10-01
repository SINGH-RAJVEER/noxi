# nix-rs-store

A local Nix store for the nix-rs engine that shares CppNix 2.35.2's on-disk format. It is an unpublished preview with unstable APIs.

`LocalStore::open` creates or opens the same layout as CppNix's `local` store: store directory, `.links`, `profiles`, `gcroots`, `temproots`, the schema-10 SQLite database with its named migrations, `big-lock`, and reserved space. `LocalStoreConfig::with_root` selects a store below a root directory, like `--store ROOT`. Locks use `flock(2)` on the same files as CppNix, so both implementations exclude each other.

Implemented operations: path-info, referrer, deriver, derivation-output and hash-part queries; registration of valid paths with derivation output checks and reference cycle detection; NAR import with hash, size, content-address and signature checks; content-addressed import of flat, NAR and text data; NAR export; temporary GC roots; signature addition; path invalidation; and store verification. Metadata is canonicalised as CppNix does.

Not yet implemented: garbage collection, store optimisation, binary-cache substitution, builds, the daemon, the `ca-derivations` schema, and `read-only` store mode beyond opening the database immutably. A store directory other than `/nix/store` is not supported. Compatibility tests against the reference live in `crates/compat-tests/tests/reference_store.rs`.

GPL-3.0-only. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md).
