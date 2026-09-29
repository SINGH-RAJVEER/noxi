# nix-rs evaluator

This crate contains the imported Nix language parser, compiler, and bytecode evaluator used as a starting point for nix-rs. It currently supports pure evaluation but lacks the store-aware operations needed to evaluate Nixpkgs into derivations. Its source revision and license are recorded in [PROVENANCE.md](../PROVENANCE.md).

The crate retains two local language fixture groups under `src/tests/`: `nix_tests/` contains cases derived from the C++ Nix language suite, and `nix_rs_tests/` contains evaluator-specific cases. Each has a `notyetpassing/` directory for behavior the current evaluator does not support. The separate reference snapshot for this project lives under `tests/nix/` at the repository root.

This evaluator uses [rnix-parser](https://github.com/nix-community/rnix-parser). The upstream work was written in memory of jD91mZM2, the parser's author.
