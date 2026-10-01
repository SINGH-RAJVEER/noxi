# nix-rs-glue

Read-only store-aware builtins for `nix-rs-eval`, adapted from Snix's glue crates. Initial release `0.1.0` is an incomplete preview with unstable APIs.

```toml
[dependencies]
nix-rs-glue = "0.1.0"
```

`configure` adds derivation builtins and a `ReadOnlyStore` to an evaluation builder. It computes paths for derivations, imported local paths, and `builtins.toFile`, and keeps derivations in memory. `StoreIO` reads local files. The implementation uses Unix filesystem APIs.

The target behavior is CppNix 2.35.2 read-only evaluation, as used by `nix-instantiate --eval`. Computing a `.drv` or output path does not create it in `/nix/store`. There is no writable store/database, builder, substitution, network fetching, or import-from-derivation realisation. Fetcher builtins fail when called. The bundled `<nix/fetchurl.nix>` describes a derivation; it does not download a URL. Partial local Nixpkgs evaluation is possible, but complete Nixpkgs/NixOS evaluation and reference compatibility remain unqualified.

Snix-derived Rust source retains GPL-3.0-only in [LICENSE](LICENSE). Bundled Nix expressions retain LGPL-2.1-or-later in [COPYING](COPYING). See [PROVENANCE.md](PROVENANCE.md) for source scopes and revisions.
