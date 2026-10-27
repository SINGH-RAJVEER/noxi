# noxi-commands

Partial compatibility commands for the noxi engine. Release `0.0.1` is an incomplete preview targeting CppNix 2.35.2. This package currently provides `nix-instantiate --eval` and its Rust command implementation.

## Isolated installation

`cargo install noxi-commands` installs a binary named `nix-instantiate`. A default Cargo bin directory early in `PATH` can shadow CppNix. Install into an isolated prefix and invoke the absolute path. Never replace system Nix or put this prefix ahead of it in `PATH`.

```sh
cargo install noxi-commands --version 0.0.1 --root "$HOME/.local/noxi-commands-preview"
"$HOME/.local/noxi-commands-preview/bin/nix-instantiate" --eval --strict --expr '1 + 2'
```

## Scope

Implemented arguments include `--eval`, `--strict`, `--json`, `--expr`/`-E`, `--attr`/`-A`, `--arg`, `--argstr`, files, `<lookup>` paths, and `-` for standard input. This is partial evaluation support; the general CppNix option set, help/version behavior, diagnostics, and full language semantics are incomplete.

Release `0.0.1` resolves relative paths in library `--expr` evaluation against the supplied directory. It requires evaluator `0.0.1` and glue `0.0.1`; the executable continues to use its current directory.

Read-only glue computes derivation and imported-path names without writing them to the store. Instantiation without `--eval` is rejected. There are no builds, downloads, substitution, daemon operation, or writable-store maintenance. Partial local Nixpkgs evaluation does not qualify a system rebuild or a drop-in replacement.

The current workspace, beyond the published preview described above, supports single-derivation instantiation with an explicit `--store 'local?root=ROOT'`. ROOT must already exist and must be an absolute diverted directory. `--eval` remains read-only; `--eval --read-write-mode` opts into registering sources, text, and derivations. Read-only selections retain diverted filesystem reads. It also accepts `--xml --no-location`, `-I`/`--include`, and `--show-trace`/`--no-show-trace` (traces are not yet rendered in CppNix's format), and it does not print the evaluator's own lint warnings. It reads `nix.conf`, the user configuration files and `NIX_CONFIG` as CppNix does, accepts settings flags such as `--option` and `--extra-experimental-features`, and enables the configured experimental features; most other settings are accepted but not yet acted on. This does not realize outputs, create permanent roots, collect derivations recursively, or enable a live-system replacement. See [verification](../../docs/verification.md#rust-core-completion-work).

GPL-3.0-only. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md).

## Unreleased build and store commands

The current source workspace adds `nix-build` and `nix-store`, requiring an explicit diverted store. `nix-build` evaluates and realizes selected derivation outputs and creates optional permanent result roots. `nix-store` implements selected local queries, realization, NAR dumping, verification, optimisation and garbage collection. Recursive selections, modern commands, profiles, complete flag compatibility and live-system replacement remain open. See [distribution](../../docs/distribution.md#local-engine-commands-under-development) for source-build instructions.

## Unreleased modern commands

The source workspace also builds `nix`, with selected expression eval/build, path-info, store, derivation-show, local-log, file-cache-copy, shell and run operations. `nix-shell`, `nix-copy-closure` and `nix develop` report explicit missing-library errors for their remaining environment/transport work. The measured subset and outstanding limits are in the [modern command coverage table](../../docs/compatibility-matrix.md). Use the absolute binary path and a diverted store for development. These commands have not been published or qualified as a system replacement.
