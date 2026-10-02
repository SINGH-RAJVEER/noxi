# Flake and crate distribution path

## Current package boundary

The first installable artifact is the `nixrs` preview command. The repository flake exposes `packages.x86_64-linux.nixrs`, `packages.x86_64-linux.default`, and an app that runs that command. `cargo install --path crates/nix-rs-cli --locked` installs the same binary from a checkout. It currently evaluates a pure expression with `nixrs eval --expr EXPRESSION` or `nixrs eval --file PATH`. The CLI deeply forces the result before printing it, matching the strict evaluator mode used by the initial differential runner. File evaluation reports the source path in diagnostics, but the pure evaluator cannot perform store-backed or impure operations. Its name deliberately differs from `nix`, and it does not own the store or daemon. It cannot replace `nix.package` or satisfy a NixOS or Home Manager rebuild. The preview is also on crates.io, so `cargo install nix-rs-cli` installs the same `nixrs` binary without a checkout.

A NixOS user may add the preview flake package to `environment.systemPackages`, or a Home Manager user may add it to `home.packages`. Both methods install `nixrs` alongside CppNix. Neither changes the daemon or the `nix` command. Do not expose a replacement NixOS module until the replacement gates below pass.

## Published crates

The crates below are published on crates.io under the SINGH-RAJVEER account. The names `nixrs` and `nix-rs` belong to unrelated crates; upstream owns `nix-compat` and `nix-compat-derive`. This project's format crates are published as `nix-rs-compat` and `nix-rs-compat-derive`, retaining Rust library names `nix_compat` and `nix_compat_derive`.

| Crate | Version | License | Contents |
|---|---|---|---|
| [`nix-rs-eval-builtin-macros`](https://crates.io/crates/nix-rs-eval-builtin-macros) | 0.0.1 | GPL-3.0-only | Procedural macros for defining builtins, derived from `snix-eval-builtin-macros` |
| [`nix-rs-eval`](https://crates.io/crates/nix-rs-eval) | 0.2.0 | GPL-3.0-only | Nix language evaluator, derived from `snix-eval` |
| [`nix-rs-cli`](https://crates.io/crates/nix-rs-cli) | 0.1.1 | Apache-2.0 | The `nixrs` preview command |
| [`nix-rs-hash`](https://crates.io/crates/nix-rs-hash) | 0.1.0 | GPL-3.0-only | Hash and Nix base32 formats |
| [`nix-rs-flakeref`](https://crates.io/crates/nix-rs-flakeref) | 0.1.0 | GPL-3.0-only | Flake reference parser and formatter |
| [`nix-rs-compat`](https://crates.io/crates/nix-rs-compat) | 0.1.0 | GPL-3.0-only | Nix formats and protocol values |
| [`nix-rs-compat-derive`](https://crates.io/crates/nix-rs-compat-derive) | 0.1.0 | GPL-3.0-only | Wire derive macros |
| [`nix-rs-serde`](https://crates.io/crates/nix-rs-serde) | 0.1.0 | GPL-3.0-only | Bidirectional serde bridge |
| [`nix-rs-glue`](https://crates.io/crates/nix-rs-glue) | 0.1.0 | GPL-3.0-only | Read-only store-aware evaluation on Unix |
| [`nix-rs-commands`](https://crates.io/crates/nix-rs-commands) | 0.1.0 | GPL-3.0-only | Partial read-only `nix-instantiate --eval` command |

Imported code retains Snix's GPL-3.0 license, with package-local text and provenance; bundled LGPL Nix material has its own notices and license text. The CLI's own source is Apache-2.0, but the `nixrs` binary links `nix-rs-eval`, so a distributed binary must meet GPL-3.0 terms. Published versions are permanent: a version can be yanked but its source stays public. Record each release in [CHANGELOG.md](../CHANGELOG.md), bump changed crates and versioned path dependencies, package and verify the archives, then publish the selected packages in dependency order. Reuse an unchanged dependency's existing registry release rather than attempting to upload the same version again. These are preview crates: their APIs are not stable, and publication does not close the drop-in release gate.

The `nix-rs-hash` crate contains hash and Nix base32 code extracted from the compatibility library and retains GPL-3.0-only. The library reexports its former paths, so the split does not require downstream source changes.

The `nix-rs-flakeref` crate contains extracted flake reference parsing and formatting under the same license. The compatibility library's `flakeref` feature still exposes `nix_compat::flakeref`.

### Release scope and verification

Nine new versions were published on 2026-10-01 after all-feature package verification, archive/license/provenance inspection, and the workspace test suite. The registry API confirmed all ten versions from that release existed, were not yanked, and belonged to SINGH-RAJVEER. The unchanged builtin-macro crate remains at its existing 0.0.1 release. Internal compatibility and derive test crates remain private. Store and build publication was excluded from that release, not permanently prohibited.

The 2026-10-02 release set is being prepared: evaluator 0.2.1, compatibility 0.1.1, glue 0.1.1, commands 0.1.1, and first releases of store and build at 0.1.0. Registry upload and checksum confirmation remain pending. Unchanged CLI, hash, flake-reference, Serde, and macro crates reuse their published releases. Glue and commands require the new evaluator methods, and build requires compatibility 0.1.1's downstream placeholder and corrected feature gate.

The new store and build crates are experimental library previews for isolated temporary stores on the tested x86_64-linux target. Store includes local metadata/NAR handling and selected local/HTTP(S) cache substitution. Build includes the local sandbox path, output checks, structured attributes, logs, and configured substitution. Neither supplies a complete command/daemon replacement or qualifies live-store maintenance, sandbox security, NixOS rebuilds, remote scheduling, or other architectures.

The occupied compatibility package names are replaced without changing internal import paths. Consumers can use `nix-compat = { package = "nix-rs-compat", version = "0.1.0" }` and the corresponding derive-macro alias. Package archives carry their license text and source provenance. All APIs are previews; registry publication does not close the drop-in release gate.

`cargo install nix-rs-commands` installs `nix-instantiate`, unlike the non-conflicting `nixrs` preview. Use `cargo install nix-rs-commands --root /path/to/isolated-prefix` and call that executable by absolute path; do not replace or shadow the system's CppNix during development.

## Replacement milestones

| Milestone | Work required | Evidence before advancing |
|---|---|---|
| Installable preview | Reproducible flake build, local Cargo installation, versioned binary, non-conflicting command name | Build the flake package and run the installed binary; test its actual supported command and failure status |
| Nix command compatibility | Implement the `nix` command family and legacy executables, flags, output formats, environment behavior, `nix.conf`, feature gates, profiles, registries, and exit codes | Differential command inventory against the pinned CppNix release, including scripts invoked by Nixpkgs and Home Manager |
| Evaluation and flakes | Complete lazy Nix evaluation, builtins, string contexts, imports, fetchers, source filtering, flake references, `flake.lock`, registry resolution, output selection, and import-from-derivation | Compare fixed Nixpkgs and Home Manager revisions, resulting values, derivation graphs, errors, and flake lock files with the reference |
| Store and builds | Implement existing-store metadata, NAR and narinfo exchange, signatures, substitution, sandboxed local and remote builds, roots, garbage collection, profiles, and crash recovery | Exercise copied stores, concurrent clients, interrupted writes, cache trust, and return to CppNix on isolated test systems |
| Daemon and system integration | Implement worker protocol negotiation and operations, multi-user authorization, `nix-daemon`, service/socket behavior, and every executable NixOS expects from `nix.package` | Test reference and nix-rs clients against each supported daemon pairing; rebuild and roll back an existing NixOS VM using nix-rs as `nix.package` |
| Home Manager | Support standalone `home-manager build` and `switch`, its flake mode, and Home Manager as a NixOS module through the same engine | Compare built user profiles, activation, generations, and failure recovery on a pinned Home Manager revision |
| Distribution and optional crate release | Publish the complete flake package; separately publish additional versioned Rust crates only if selected, with resolvable dependencies, source licenses, and documented API stability | Build from a clean clone, `cargo package` each selected crate, verify clean installation and upgrade, and run the exact packaged release-profile artifacts through the compatibility matrix before publication |

These milestones are qualification areas, not a strict implementation order. Follow the [plan's work sequence](../PLAN.md#work-sequence): connect minimal command, evaluation, and store operations first, then extend them through builds and the daemon before completing all workflows. Discover public C and C++ library consumers during baseline work so ABI and packaging requirements are known before the replacement architecture is fixed. Additional Rust crate publication is optional for the binary release; it does not require a stable Rust API promise.

The matrix in [compatibility](compatibility-matrix.md) remains the release gate. A command shim that delegates to CppNix, or a binary placed earlier on `PATH`, does not establish engine compatibility. Provisional replacement packages may be wired to `nix.package` in disposable qualification VM overlays to obtain rebuild and rollback evidence, with one store-writing daemon at a time. Before publishing a replacement module or recommending host deployment, the package must provide the expected executable set and daemon, preserve the existing store, and pass a tested rollback to CppNix.

## Installation path

For the preview package, a NixOS flake can include the nix-rs input and add `inputs.nix-rs.packages.${pkgs.system}.nixrs` to `environment.systemPackages`. A Home Manager flake can add the same package to `home.packages`. Run `nixrs --version` and `nixrs eval --expr '1 + 2'` to inspect the preview. The system's existing `nix` and `nix-daemon` stay in place.

The replacement stage will package nix-rs for the NixOS `nix.package` option and verify the `nix-daemon` service wiring in the selected Nixpkgs revision. Home Manager may run standalone or inside NixOS, so both paths need independent qualification. The target CppNix release, Nixpkgs revision, Home Manager revision, platform, and feature settings must be pinned together before any drop-in claim. The current copied fixture tree is the CppNix 2.35.2 release. [Test snapshot](verification.md#test-snapshot)

The NixOS package option and daemon wiring are defined in [Nixpkgs' Nix module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/config/nix.nix) and [daemon module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/services/system/nix-daemon.nix). Home Manager documents its [standalone and NixOS-module installation paths](https://nix-community.github.io/home-manager/installation.html) and [flake update and switch commands](https://nix-community.github.io/home-manager/usage/updating.html). These upstream interfaces are the integration targets, not evidence that nix-rs supports them today.
