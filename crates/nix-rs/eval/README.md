# nix-rs evaluator

`nix-rs-eval` contains the Nix language compiler and bytecode evaluator used by the nix-rs engine, with parsing supplied by `rnix`. Release `0.2.1` is an incomplete preview derived from Snix's `snix-eval`. APIs and diagnostics are not stable, and full CppNix 2.35.2 language compatibility has not been qualified.

```toml
[dependencies]
nix-rs-eval = "0.2.1"
```

Use `nix_rs_eval::Evaluation::builder_pure()` for pure evaluation. The optional `impure` feature enables local I/O when configured by the caller; this crate alone does not provide a writable store, builds, source fetching, or store-aware derivation builtins. The separate `nix-rs-glue` crate adds partial read-only evaluation and derivation path calculation. That does not qualify Nixpkgs or NixOS evaluation.

`Evaluation::evaluate_in_dir` resolves relative paths in fileless expressions against an explicit directory. `EvaluationBuilder::get_nix_path` inspects the configured search path without replacing it. These additions support the updated read-only glue and command crates.

The imported source retains GPL-3.0-only. Its source revision, borrowed scopes, and fixture licenses are recorded in the bundled [PROVENANCE.md](PROVENANCE.md). See [LICENSE](LICENSE) for GPLv3 and [COPYING](COPYING) for the LGPL language fixtures.

The crate retains two language fixture groups under `src/tests/`: `nix_tests/` cites the C++ Nix 2.3.16 language suite, and `nix_rs_tests/` contains imported evaluator-specific cases and local additions. Each has a `notyetpassing/` directory for unsupported behavior. The separate CppNix 2.35.2 reference snapshot lives under `tests/nix/` at the repository root, outside this package.

This evaluator uses [rnix-parser](https://github.com/nix-community/rnix-parser). The upstream work was written in memory of jD91mZM2, the parser's author.
