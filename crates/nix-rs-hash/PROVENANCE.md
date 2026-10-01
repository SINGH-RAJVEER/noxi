# nix-rs-hash provenance

## Source and authorship

The `nixbase32` and `nixhash` modules, including their tests and `format_sha256!` macro, were extracted on 2026-09-30 from the imported `nix-compat` crate. Their source is [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24, under `snix/nix-compat/src/`. The initial import was on 2026-09-25.

The source remains the work of its Snix contributors and earlier contributors recorded in upstream history. Extraction and package ownership do not transfer authorship. GPL-3.0-only applies; [LICENSE](LICENSE) reproduces `snix/LICENSE` at that revision. No Snix MIT-licensed protocol buffer definitions are included.

Local source and release history are maintained at https://github.com/SINGH-RAJVEER/nix-rs under `crates/nix-rs-hash/`. The initial package release is `0.1.0`.

## Adaptation and limits

Local changes include hard-tab formatting, module extraction with compatibility reexports, BLAKE3 support, uppercase hexadecimal input, and digest/encoding adjustments. The reference is [CppNix 2.35.2](https://github.com/NixOS/nix/tree/2c73b59da29606068c0c98db015dd3a66955525d), especially `src/libutil/hash.cc`, under LGPL-2.1-or-later. Reference-generated hash comparisons live outside this package in the repository's compatibility tests.

This preview implements hash formats and helpers. It does not enforce reference experimental settings, provide a `nix hash` command, or qualify store and cache behavior. Imported Serde forms remain separate from CppNix JSON encodings.
