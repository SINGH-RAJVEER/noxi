# noxi-glue provenance

## Snix source

This package adapts parts of [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24. Borrowed scopes include derivation builtin/coercion and context handling from `snix/glue/src/builtins/derivation.rs`, the known-derivation tracking design from `snix/build-glue/src/known_paths.rs`, and the derivation wrapper from `snix/glue/src/builtins/derivation.nix`.

The upstream contributors retain authorship as recorded in source history. Snix-derived Rust code remains GPL-3.0-only; [LICENSE](LICENSE) reproduces `snix/LICENSE`. No Snix MIT-licensed protocol buffer definitions are included. The inspected revision uses depot `default.nix` build definitions, not a `flake.nix`; package metadata does not override the per-file notice on the Nix-derived wrapper.

## Bundled Nix expressions and reference

- `src/derivation.nix` matches the wrapper in the recorded Snix revision. Its existing header says LGPL-2.1-or-later and cites https://github.com/NixOS/nix/blob/master/src/libexpr/primops/derivation.nix. Snix did not pin the original Nix revision in that notice. The pinned Snix copy identifies the exact imported content; it is not the current 2.35.2 wrapper.
- `src/corepkgs/fetchurl.nix` is a verbatim copy of `src/libexpr/fetchurl.nix` from [CppNix 2.35.2](https://github.com/NixOS/nix/tree/2c73b59da29606068c0c98db015dd3a66955525d), revision `2c73b59da29606068c0c98db015dd3a66955525d`. It retains Nix's LGPL-2.1-or-later terms and its Nix authors' authorship. That revision's `packaging/components.nix` defaults the component license to `lib.licenses.lgpl21Plus`.
- `src/corepkgs/call-flake.nix` copies `src/libflake/call-flake.nix` from the same CppNix 2.35.2 revision, with leading indentation changed to tabs and a source/license notice added. It retains LGPL-2.1-or-later. It constructs recursive flake outputs and resolves lock graph inputs; Rust supplies the graph, fetched source overrides, and private final-tree callback.
- [COPYING](COPYING) reproduces the LGPL 2.1 text from repository-root `tests/nix/COPYING`, taken from that CppNix revision. These notices preserve the Nix expressions' licenses; the GPL notice for Snix Rust source does not relicense them.

CppNix 2.35.2 also supplies the read-only evaluation reference, especially `src/libexpr/primops.cc`, `src/libexpr/eval.cc`, and `src/libstore/derivations.cc`, under LGPL-2.1-or-later. The copied project reference fixture tree is outside this package.

## Adaptation and limits

Local source and release history are at https://github.com/SINGH-RAJVEER/nix-rs under `crates/noxi-glue/`. Initial release `0.1.0` adapts the upstream logic to read-only store-path calculation, in-memory derivation/text tracking, local imports, path filtering, NAR hashing, and core-package lookup. It changes coercion, structured attributes, context handling, and derivation construction for the 2.35.2 reference.

Snix's storage services, build execution, and fetch tracking are excluded. Nothing writes to the store, fetches sources, substitutes paths, or realises derivation outputs. Local I/O is not sandboxed. Printed paths do not establish the existence of their contents, and partial Nixpkgs evaluation is not full Nixpkgs/NixOS or store compatibility qualification.

Release `0.1.1` corrects catchable derivation-attribute errors and preserves an existing caller-supplied search path. Its dependency on evaluator `0.2.1` supplies the new search-path inspection method; the imported source and license scopes remain unchanged.

The current workspace extends the preview with optional store-owner registration, registered derivation imports and context realization. The shared build/store interface supplies output realization; Snix storage services and fetching remain excluded. The evaluator owns the bundled CppNix imported-derivation helper and records its LGPL source notice in its package-local provenance. This development evidence does not change the published release claims above.
