# nix-rs-serde provenance

The deserialiser, error handling, and imported tests originate in `snix/serde/` from [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24, imported on 2026-09-25 as upstream package `snix-serde`.

The imported source retains its Snix and earlier upstream contributors' authorship as recorded in source history. The upstream manifest supplied no author list. GPL-3.0-only is preserved; [LICENSE](LICENSE) reproduces `snix/LICENSE`. No Snix MIT-licensed protocol buffer definitions are included.

Local source and release history are at https://github.com/SINGH-RAJVEER/nix-rs under `crates/nix-rs/serde/`. Initial release `0.1.0` includes the package/import renaming and formatting, `from_value`, Rust-to-Nix serialisation in `ser.rs`, `to_value` and `to_string`, and associated local tests.

The evaluator dependency has its own packaged provenance and license. CppNix 2.35.2 at https://github.com/NixOS/nix, revision `2c73b59da29606068c0c98db015dd3a66955525d`, is the project's reference release, not a bundled implementation or fixture source in this package. This preview inherits evaluator gaps and is not a CppNix store JSON codec. Functions, byte values, and arbitrary Rust integer ranges do not map to ordinary Nix data.
