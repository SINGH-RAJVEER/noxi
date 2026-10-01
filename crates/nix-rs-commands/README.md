# nix-rs-commands

Partial compatibility commands for the nix-rs engine. Initial release `0.1.0` is an incomplete preview targeting CppNix 2.35.2. This package currently provides `nix-instantiate --eval` and its Rust command implementation.

## Isolated installation

`cargo install nix-rs-commands` installs a binary named `nix-instantiate`. A default Cargo bin directory early in `PATH` can shadow CppNix. Install into an isolated prefix and invoke the absolute path. Never replace system Nix or put this prefix ahead of it in `PATH`.

```sh
cargo install nix-rs-commands --version 0.1.0 --root "$HOME/.local/nix-rs-commands-preview"
"$HOME/.local/nix-rs-commands-preview/bin/nix-instantiate" --eval --strict --expr '1 + 2'
```

## Scope

Implemented arguments include `--eval`, `--strict`, `--json`, `--expr`/`-E`, `--attr`/`-A`, `--arg`, `--argstr`, files, `<lookup>` paths, and `-` for standard input. This is partial evaluation support; the general CppNix option set, help/version behavior, diagnostics, and full language semantics are incomplete.

Read-only glue computes derivation and imported-path names without writing them to the store. Instantiation without `--eval` is rejected. There are no builds, downloads, substitution, daemon operation, or writable-store maintenance. Partial local Nixpkgs evaluation does not qualify a system rebuild or a drop-in replacement.

GPL-3.0-only. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md).
