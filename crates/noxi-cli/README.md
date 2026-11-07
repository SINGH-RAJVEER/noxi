# noxi-cli

The `noxi` native command for the noxi engine. Release `0.0.1` is an incomplete preview that evaluates pure Nix expressions. APIs and command behavior may change.

```sh
cargo install noxi-cli --version 0.0.1 --root "$HOME/.local/noxi-preview"
"$HOME/.local/noxi-preview/bin/noxi" eval --expr '1 + 2'
"$HOME/.local/noxi-preview/bin/noxi" eval --file ./example.nix
"$HOME/.local/noxi-preview/bin/noxi" --version
```

Evaluation is strict and prints an evaluator value. The command supports only the forms above. Reading `--file` loads the expression text; it does not enable impure evaluation or read-only glue. There are no builds, store operations, source fetchers, flake commands, or Lua configuration support yet. Complete Nixpkgs/NixOS evaluation and reference compatibility remain unqualified. Install alongside system Nix; this preview cannot replace it.

The CLI's own source is Apache-2.0 in [LICENSE](LICENSE). The binary links the GPL-3.0-only evaluator, so binary distribution must meet the GPL terms and preserve dependency notices. See [PROVENANCE.md](PROVENANCE.md).
