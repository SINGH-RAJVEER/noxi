# noxi-cli provenance

The `noxi` command is project-owned source at https://github.com/SINGH-RAJVEER/nix-rs under `crates/noxi-cli/`, prepared for preview release `0.1.1`. Its own command parsing, file loading, and evaluation/output driver are Apache-2.0, with text in [LICENSE](LICENSE). This package bundles no imported Snix module tree or copied Nix fixture suite.

The command links `noxi-eval`, derived from `snix/eval/` in [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24. That dependency retains GPL-3.0-only and its original contributors' authorship. Its packaged provenance records borrowed modules, older LGPL Nix language fixtures, and local adaptations. Project ownership of the CLI does not claim authorship of the evaluator. Distributed linked binaries must meet GPL terms; the CLI's Apache license does not override dependency licenses.

CppNix 2.35.2 at https://github.com/NixOS/nix, revision `2c73b59da29606068c0c98db015dd3a66955525d`, is the project's behavior reference. `noxi` has no direct CppNix command counterpart. The preview performs strict pure evaluation only, without read-only glue, store operations, builds, fetching, or flakes. No full language, Nixpkgs/NixOS, or drop-in compatibility qualification is claimed.
