# noxi-glue

Read-only store-aware builtins for `noxi-eval`, adapted from Snix's glue crates. Release `0.0.1` is an incomplete preview with unstable APIs.

```toml
[dependencies]
noxi-glue = "0.0.1"
```

`configure` adds derivation builtins and a `ReadOnlyStore` to an evaluation builder. It computes paths for derivations, imported local paths, and `builtins.toFile`, and keeps derivations in memory. `StoreIO` reads local files. The implementation uses Unix filesystem APIs.

Release `0.0.1` preserves a caller-supplied Nix search path when adding the built-in files and propagates catchable errors from derivation attributes. It requires evaluator `0.0.1`.

The target behavior is CppNix 2.35.2 read-only evaluation, as used by `nix-instantiate --eval`. Computing a `.drv` or output path does not create it in `/nix/store`. There is no writable store/database, builder, substitution, network fetching, or import-from-derivation realisation. Fetcher builtins fail when called. The bundled `<nix/fetchurl.nix>` describes a derivation; it does not download a URL. Partial local Nixpkgs evaluation is possible, but complete Nixpkgs/NixOS evaluation and reference compatibility remain unqualified.

In the current workspace, `EvaluatorStore` adds opt-in writable registration backed by `noxi-store`, while `ReadOnlyStore` remains a compatibility alias for the read-only default. `EvaluatorStore::writable(root)` requires an existing diverted root and materializes text, source paths, and derivations. `EvaluatorStore::read_only(root)` initially maps diverted files without opening a database; registered derivation/context operations open the selected owner lazily. A configured owner can also read registered derivations and realize input-addressed IFD contexts through the shared store interface. Read-only evaluation reuses valid outputs and rejects missing ones; writable evaluation can dispatch builds. IFD permission/trace settings, deferred-output gates and stored-file reference filtering have focused local/daemon comparisons. Fetching, lazy read-only source mounts and broader IFD qualification remain open. See [verification](../../docs/verification.md#rust-core-completion-work) for tests and limits.

Snix-derived Rust source retains GPL-3.0-only in [LICENSE](LICENSE). Bundled Nix expressions retain LGPL-2.1-or-later in [COPYING](COPYING). See [PROVENANCE.md](PROVENANCE.md) for source scopes and revisions.
