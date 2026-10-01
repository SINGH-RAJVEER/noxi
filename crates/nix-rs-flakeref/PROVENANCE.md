# nix-rs-flakeref provenance

The parser, formatter, metadata types, and tests were extracted on 2026-09-30 from `nix-compat::flakeref`. The original source is [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24, at `snix/nix-compat/src/flakeref/mod.rs`, imported on 2026-09-25.

The imported code retains its Snix and earlier upstream contributors' authorship as recorded in source history. GPL-3.0-only applies; [LICENSE](LICENSE) reproduces the upstream `snix/LICENSE`. No MIT-licensed Snix protocol buffer definitions are included.

Local source and release history are at https://github.com/SINGH-RAJVEER/nix-rs under `crates/nix-rs-flakeref/`. Initial release `0.1.0` separates the module into a package while `nix-rs-compat` preserves its optional `flakeref` reexport. Local changes include formatting and dependency wiring.

CppNix 2.35.2, revision `2c73b59da29606068c0c98db015dd3a66955525d` at https://github.com/NixOS/nix, is the project's reference release, not the origin of this module. This preview has no fetcher, registry resolution, lock-file management, or flake evaluator. Full parsing and formatting equivalence has not been qualified.
