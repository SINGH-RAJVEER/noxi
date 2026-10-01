# nix-rs-compat-derive provenance

The derive macros, attribute parsing, and code generation were imported on 2026-09-25 from `snix/nix-compat-derive/` in [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24.

Authorship remains with the Snix and earlier upstream contributors recorded in source history. The upstream manifest supplied no author list. [LICENSE](LICENSE) reproduces the upstream `snix/LICENSE`; GPL-3.0-only is preserved. No MIT-licensed Snix protocol buffer definitions are included.

Local source and release history are at https://github.com/SINGH-RAJVEER/nix-rs under `crates/nix-rs/nix-compat-derive/`. Initial release `0.1.0` renames the package to `nix-rs-compat-derive`, preserves Rust library name `nix_compat_derive`, and retains the generated `nix_compat` paths. Other local changes include formatting and dependency wiring. Compiler diagnostic tests also exist in the separate, unpublished `nix-compat-derive-tests` package.

CppNix 2.35.2, revision `2c73b59da29606068c0c98db015dd3a66955525d` at https://github.com/NixOS/nix, is the protocol reference used by the consuming compatibility library, not the origin of these Rust macros. This preview makes no daemon-pairing or full protocol qualification claim. Nightly/Polonius repository settings and compiler-specific snapshots do not establish a stable-toolchain packaging result.
