# noxi-eval provenance

## Source and authorship

The compiler, bytecode VM, builtins, value types, evaluation I/O abstractions, diagnostics, and evaluator tests were imported on 2026-09-25 from `snix/eval/` in [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24. The upstream package was `snix-eval`. Parsing uses the separately distributed `rnix` dependency; this package does not vendor its parser source.

The imported source retains its Snix and earlier upstream contributors' authorship as recorded in source history. [LICENSE](LICENSE) reproduces `snix/LICENSE`; GPL-3.0-only applies to the imported evaluator. No MIT-licensed Snix protocol buffer definitions are included. The README retains the upstream dedication to rnix-parser author jD91mZM2.

`noxi-eval-builtin-macros` is a separate dependency with its own unchanged `0.0.1` release. Its source originated in `snix/eval/builtin-macros/`; this package does not republish that crate.

## Bundled fixture notices

`src/tests/nix_tests/README.md` records https://github.com/NixOS/nix/tree/2.3.16/tests/lang as the source of the imported Nix language cases, under Nix's LGPL-2.1-or-later terms. They arrived through the Snix revision above; the import record specifies the Nix tag, not an exact original Nix commit or an unmodified snapshot. Nix authors and contributors retain authorship. [COPYING](COPYING) supplies the LGPL 2.1 text, copied verbatim from this repository's CppNix 2.35.2 `tests/nix/COPYING`.

`src/tests/noxi_tests/` contains upstream evaluator-specific cases and local additions. Its local name does not imply that noxi authored the imported cases. The separate project reference fixtures under repository-root `tests/nix/` are not this older suite and are outside this package.

`src/compiler/imported-drv-to-derivation.nix` adapts `src/libexpr/imported-drv-to-derivation.nix` from CppNix 2.35.2, revision `2c73b59da29606068c0c98db015dd3a66955525d`. Only layout and the source notice changed. Its Nix authors retain authorship and LGPL-2.1-or-later terms; [COPYING](COPYING) supplies the license text. It constructs recursive derivation output values after a configured I/O integration reads a registered derivation.

## Local changes and limits

Local source and release history are at https://github.com/SINGH-RAJVEER/nix-rs under `crates/noxi/eval/`. Release `0.2.0` includes the package/import/test-path renaming, formatting, and checked integer arithmetic, negation, and division-by-zero behavior. The reference for these changes is [CppNix 2.35.2](https://github.com/NixOS/nix/tree/2c73b59da29606068c0c98db015dd3a66955525d), revision `2c73b59da29606068c0c98db015dd3a66955525d`, especially `src/libexpr/primops.cc`, under LGPL-2.1-or-later.

This preview retains unsupported cases and diagnostic differences. It can evaluate the pure language and provide configured local I/O, but does not itself implement store-aware derivation builtins, a writable store, builds, or fetching. The separate read-only glue does not establish complete Nixpkgs/NixOS or reference-release qualification.

Release `0.2.1` adds explicit-directory evaluation for fileless expressions and search-path inspection for callers. It retains the source and license scopes above.
