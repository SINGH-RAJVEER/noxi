# Development environment

Run `devenv shell` at the repository root to enter the pinned Rust 1.95 development shell. It supplies Cargo, rustc, rustfmt, Clippy, rust-analyzer, Git, and Just. The Rust version matches the imported crates' minimum supported version and the compiler used for their UI diagnostic snapshots. [`devenv.nix`](../devenv.nix) defines the tools, `devenv.yaml` declares the Rust overlay input, and `devenv.lock` pins the resolved inputs so all developers use the same package revisions.

Run `just` to list recipes. `just fmt-check` checks formatting, `just check` type-checks the workspace, `just test-fixtures` runs the copied derivation fixture checks, and `just test` runs the full workspace suite. `just ci` runs formatting, checking, and the full suite in sequence.

The host's Rust 1.100.0 nightly currently fails the imported `nix-compat-derive-tests` trybuild UI snapshots: several compiler diagnostics differ and one test triggers an internal compiler error. `just test-core` runs the remaining workspace tests when using an unpinned host toolchain; it does not replace `just test` as the complete check. The copied CppNix shell and NixOS VM tests still need dedicated runners, as described in [verification](verification.md).

The four UI diagnostic snapshots affected by the crate move and compiler wording have been refreshed for the pinned Rust 1.95.0 shell. Run `devenv shell -- just ci` to verify the complete Rust workspace with those snapshots.
