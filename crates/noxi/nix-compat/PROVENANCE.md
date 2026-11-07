# noxi-compat provenance

## Source and authorship

This package adapts `snix/nix-compat/` from [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24, imported on 2026-09-25. Borrowed scopes include store paths, ATerm derivations, NAR and narinfo formats, path info, realisations, configuration parsing, Git formats, fetcher declarations, wire codecs, daemon protocol types, and the upstream tests and data.

The original source retains its Snix and earlier upstream contributors' authorship, recorded in upstream source history. Its GPL-3.0-only license is preserved in [LICENSE](LICENSE), copied from `snix/LICENSE`. No Snix MIT-licensed protocol buffer definitions are included. The upstream manifests did not supply an author list; the package rename makes no claim of original authorship.

Local source and release history are at https://github.com/SINGH-RAJVEER/nix-rs under `crates/noxi/nix-compat/`. Initial release `0.1.0` uses package name `noxi-compat` while preserving Rust library name `nix_compat`.

## Adaptation and reference

Local changes include formatting, crate dependency aliases, extracted hash and flake reference modules with reexports, store-path ordering, byte-preserving derivation fields, derivation parsing/hashing, worker-protocol fields, and new `cppnix_json` and `cppnix_serve` modules.

Release `0.1.1` adds the downstream content-addressed output placeholder helper following the reference's `DownstreamPlaceholder::unknownCaOutput` and corrects the serve-codec feature gate. These changes retain the existing GPL-3.0-only package terms and reference-source notices.

The reference source is [CppNix 2.35.2](https://github.com/NixOS/nix/tree/2c73b59da29606068c0c98db015dd3a66955525d), revision `2c73b59da29606068c0c98db015dd3a66955525d`, under LGPL-2.1-or-later. Relevant files include `src/libstore/derivations.cc`, store-path and JSON implementations, `worker-protocol.cc`, and `serve-protocol.cc`. The repository's copied reference fixtures and license live outside this package in `tests/nix/`; they are distinct from the imported Snix test data here.

An early local ATerm parser enforced development-version 2.36 ordering rules. That behavior was superseded on 2026-09-29 by 2.35.2 parsing: collections accept arbitrary order, repeated outputs keep the first entry, repeated input derivations and environment keys keep the last, and set entries collapse. Experimental output and dynamic-derivation behavior remains incomplete.

This package is a preview format/protocol library. It does not implement a writable store or qualify daemon pairings, serve commands, builds, fetchers, or complete reference compatibility. Imported Serde encodings retain their upstream conventions; use `cppnix_json` for the implemented reference JSON forms.
