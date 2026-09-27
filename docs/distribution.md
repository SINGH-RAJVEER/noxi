# Flake and crate distribution path

## Current package boundary

The first installable artifact is the `nixrs` preview command. The repository flake exposes `packages.x86_64-linux.nixrs`, `packages.x86_64-linux.default`, and an app that runs that command. `cargo install --path crates/nix-rs-cli --locked` installs the same binary from a checkout. It currently evaluates a pure expression with `nixrs eval --expr EXPRESSION` or `nixrs eval --file PATH`. The CLI deeply forces the result before printing it, matching the strict evaluator mode used by the initial differential runner. File evaluation reports the source path in diagnostics, but the pure evaluator cannot perform store-backed or impure operations. Its name deliberately differs from `nix`, and it does not own the store or daemon. It cannot replace `nix.package` or satisfy a NixOS or Home Manager rebuild. Publishing a crate on crates.io is a later step; a working local Cargo installation is not a published, independently resolvable crate.

A NixOS user may add the preview flake package to `environment.systemPackages`, or a Home Manager user may add it to `home.packages`. Both methods install `nixrs` alongside CppNix. Neither changes the daemon or the `nix` command. Do not expose a replacement NixOS module until the replacement gates below pass.

## Replacement milestones

| Milestone | Work required | Evidence before advancing |
|---|---|---|
| Installable preview | Reproducible flake build, local Cargo installation, versioned binary, non-conflicting command name | Build the flake package and run the installed binary; test its actual supported command and failure status |
| Nix command compatibility | Implement the `nix` command family and legacy executables, flags, output formats, environment behavior, `nix.conf`, feature gates, profiles, registries, and exit codes | Differential command inventory against the pinned CppNix release, including scripts invoked by Nixpkgs and Home Manager |
| Evaluation and flakes | Complete lazy Nix evaluation, builtins, string contexts, imports, fetchers, source filtering, flake references, `flake.lock`, registry resolution, output selection, and import-from-derivation | Compare fixed Nixpkgs and Home Manager revisions, resulting values, derivation graphs, errors, and flake lock files with the reference |
| Store and builds | Implement existing-store metadata, NAR and narinfo exchange, signatures, substitution, sandboxed local and remote builds, roots, garbage collection, profiles, and crash recovery | Exercise copied stores, concurrent clients, interrupted writes, cache trust, and return to CppNix on isolated test systems |
| Daemon and system integration | Implement worker protocol negotiation and operations, multi-user authorization, `nix-daemon`, service/socket behavior, and every executable NixOS expects from `nix.package` | Test reference and nix-rs clients against each supported daemon pairing; rebuild and roll back an existing NixOS VM using nix-rs as `nix.package` |
| Home Manager | Support standalone `home-manager build` and `switch`, its flake mode, and Home Manager as a NixOS module through the same engine | Compare built user profiles, activation, generations, and failure recovery on a pinned Home Manager revision |
| Distribution and crate release | Publish the complete flake package and separately publish a coherent set of versioned Rust crates with resolvable dependencies, source licenses, and documented APIs | Build from a clean clone, `cargo package` every public crate, verify clean installation and upgrade, and run the published artifacts through the compatibility matrix |

The matrix in [compatibility](compatibility-matrix.md) remains the release gate. A command shim that delegates to CppNix, or a binary placed earlier on `PATH`, does not establish engine compatibility. Before setting `nix.package` to nix-rs, the package must provide the expected executable set and daemon, preserve the existing store, and pass a tested rollback to CppNix. Use isolated VMs and store copies for these gates.

## Installation path

For the preview package, a NixOS flake can include the nix-rs input and add `inputs.nix-rs.packages.${pkgs.system}.nixrs` to `environment.systemPackages`. A Home Manager flake can add the same package to `home.packages`. Run `nixrs --version` and `nixrs eval --expr '1 + 2'` to inspect the preview. The system's existing `nix` and `nix-daemon` stay in place.

The replacement stage will package nix-rs for the NixOS `nix.package` option and verify the `nix-daemon` service wiring in the selected Nixpkgs revision. Home Manager may run standalone or inside NixOS, so both paths need independent qualification. The target CppNix release, Nixpkgs revision, Home Manager revision, platform, and feature settings must be pinned together before any drop-in claim. The current copied fixture tree identifies itself as CppNix 2.36.0 and matches unreleased `master` commit `8f7ed1c1d`; no 2.36.0 release exists yet. [Snapshot provenance](../tests/README.md#snapshot-provenance)

The NixOS package option and daemon wiring are defined in [Nixpkgs' Nix module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/config/nix.nix) and [daemon module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/services/system/nix-daemon.nix). Home Manager documents its [standalone and NixOS-module installation paths](https://nix-community.github.io/home-manager/installation.html) and [flake update and switch commands](https://nix-community.github.io/home-manager/usage/updating.html). These upstream interfaces are the integration targets, not evidence that nix-rs supports them today.
