# Upstream Nix compatibility tests

`upstream/nix/` is a byte-for-byte test snapshot copied from a CppNix 2.36.0 reference tree. Its source tree had no Git commit metadata in this checkout, so the version alone does not identify a unique upstream commit. The upstream implementation source is not kept here. Keep the snapshot unchanged when refreshing it; write Rust adaptations and test runners outside `upstream/`.

The snapshot includes:

| Path | Purpose |
|---|---|
| `upstream/nix/tests/functional/` | Shell tests, Nix language fixtures, expected output, and helpers |
| `upstream/nix/tests/nixos/` | NixOS integration and virtual machine test definitions |
| `upstream/nix/tests/installer/`, `fuzzing-engine/`, `filetransfer-retry-backoff/` | Other upstream test groups |
| `upstream/nix/src/lib*-tests/` | C++ unit tests and golden data for the evaluator, fetchers, flakes, store, and utilities |
| `upstream/nix/src/lib*-test-support/` and `upstream/nix/nix-meson-build-support/` | Helpers required by those upstream suites |
| `upstream/nix/COPYING` | Upstream license text for the copied material |

The copied directory shape preserves relative fixture symlinks. One deliberately broken symlink in the language fixtures is part of the test data. The `src/nix-functional-tests` alias also points to the copied functional suite.

## Using the snapshot

Treat the expected values and serialized files as reference outputs. When a Rust component appears, add focused Rust tests that read fixtures from this snapshot and compare byte output where the format requires it. Start with store paths, NAR, derivations, and protocol data. Run the language fixtures against the Rust evaluator once it has the corresponding operations. Keep the original files intact, including cases that the Rust implementation does not support yet, and report supported, failed, and unrun cases separately.

The shell tests invoke upstream commands such as `nix-instantiate`, `nix-store`, and `nix-build`; Meson normally sets up their environment. They must be pointed at the Rust implementation and its isolated test store before their results count toward replacement compatibility. Running them against installed CppNix only establishes a reference baseline. The copied C++ tests likewise document behavior but do not run as Rust unit tests. NixOS tests need full virtual machine setup.

The snapshot is not a standalone CppNix source build: its original Meson targets depend on upstream implementation code. Run the original suite from a separate CppNix checkout for baseline behavior while Rust-specific runners are developed here.

## Refreshing the snapshot

When refreshing from a separate CppNix checkout, copy the same directories and the `.version` and `COPYING` files again. Preserve symlinks and compare both trees before relying on new expected results. Record the source revision when one is available. Do not edit the vendored fixtures to make a new implementation pass; add a separate compatibility note if an upstream expectation needs investigation.
