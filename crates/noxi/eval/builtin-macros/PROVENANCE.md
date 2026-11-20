# noxi-eval-builtin-macros provenance

The procedural macros that define evaluator builtins were imported on 2026-09-25 from `snix/eval/builtin-macros/` in [Snix](https://git.snix.dev/snix/snix), revision `22ca63bdb71c221001526304eca934385fb47c26` dated 2026-09-24, together with `noxi-eval` (see [../PROVENANCE.md](../PROVENANCE.md)). The upstream package was `snix-eval-builtin-macros`.

Upstream authors keep their authorship as recorded in Snix's history. The code remains GPL-3.0-only; [LICENSE](LICENSE) reproduces `snix/LICENSE`. Local changes since the import are in this repository's history under `crates/noxi/eval/builtin-macros/` and `crates/nix-rs/eval/builtin-macros/`.

The macros run at build time and are not linked into the `noxi` executables, but the code they generate is, so the release notices list this crate.
