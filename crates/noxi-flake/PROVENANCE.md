# noxi-flake provenance

`noxi-flake` is project-owned source at https://github.com/SINGH-RAJVEER/nix-rs under `crates/noxi-flake/`. It implements the library side of the `nix flake` commands (metadata, lock graphs, outputs, checks) on top of the workspace's evaluator, flake reference, glue and store crates. It copies no CppNix or Snix source. The behavior it follows, including the metadata JSON and the app and template attribute checks, is that of CppNix 2.35.2, revision `2c73b59da29606068c0c98db015dd3a66955525d` (`src/libflake` and `src/nix/flake.cc`), which is LGPL-2.1-or-later.

The flake evaluation helper `call-flake.nix` copied from CppNix lives in `noxi-glue`, whose provenance and `COPYING` record it.

[LICENSE](LICENSE) supplies GPLv3 for this crate. The notices of the linked evaluator, glue and store crates remain applicable to distributed binaries.
