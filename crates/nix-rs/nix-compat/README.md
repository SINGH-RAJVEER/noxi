# nix-rs-compat

Nix formats and protocol types for the nix-rs engine, imported and adapted from Snix's `nix-compat`. Release `0.1.1` is an incomplete preview with unstable APIs. The package name is `nix-rs-compat`; its Rust library name remains `nix_compat`.

```toml
[dependencies]
nix-compat = { package = "nix-rs-compat", version = "0.1.1" }
# Add this only when using the wire derive macros directly.
nix-compat-derive = { package = "nix-rs-compat-derive", version = "0.1.0" }
```

The crate includes store paths, ATerm derivations, NAR readers and writers, narinfo, realisations, `nix.conf` parsing, and wire types. Default features are `async`, `hashbrown`, and `wire`. Optional `daemon`, `serde`, and `flakeref` features expose additional modules. Hash and flake reference modules reexport `nix-rs-hash` and `nix-rs-flakeref`.

The reference is CppNix 2.35.2. `cppnix_json` exposes reference JSON formats separately from the imported Serde implementations. `cppnix_serve` supplies value codecs and handshakes, not a qualified serve command. Experimental derivation modes remain incomplete. This crate provides no writable store, builder, source fetcher, or running daemon; format and fixture checks are not full protocol or package-manager qualification.

Release `0.1.1` adds `store_path::downstream_placeholder` for unknown content-addressed outputs. It also gates `cppnix_serve` on `daemon`, so `serde` and `wire` can be enabled without the daemon implementations.

Repository builds use a pinned nightly and local `.cargo/config.toml` with `-Zpolonius=next`. Those are repository-local settings; they do not demonstrate stable support for the packaged crate. Stable compilation of its feature combinations has not been verified here.

The imported source retains GPL-3.0-only. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md).
