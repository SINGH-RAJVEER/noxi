# Upstream Nix compatibility tests

## Official source checkout

Run `just nix-tests-update` at the repository root to fetch the full official [NixOS/nix](https://github.com/NixOS/nix) source tree and its tests at the newest numbered stable release tag. The command writes the selected tag and exact commit to [`upstream/nix-official.lock`](upstream/nix-official.lock) and places the checkout in `upstream/nix-official/`. That checkout is ignored by the parent repository; a fresh clone can run `just nix-tests-sync` to fetch the recorded commit. Run `bash scripts/update-nix-tests.sh master` to track the newest development commit, or pass a numbered tag such as `2.35.2` to select a specific release. The updater refuses to overwrite local changes inside the official checkout.

The official checkout includes the upstream implementation and its Meson test definitions, so it can be used to run CppNix's own suite as a reference. Those tests are not yet a nix-rs test runner: C++ unit tests build against CppNix, and functional tests require a configured upstream environment and compatible command implementations. The lock file records test provenance, while the checkout itself remains a reproducible local dependency rather than committed duplicate source.

The fixed fixture snapshot below is copied from the 2.35.2 release and pinned in [`upstream/nix-snapshot.lock`](upstream/nix-snapshot.lock), as described under [Snapshot provenance](#snapshot-provenance). Both lock files currently name the same commit. `just nix-tests-update` moves the official checkout to the next tag when one is published, but the snapshot stays on 2.35.2 until it is refreshed deliberately. Check both lock files before comparing results; do not treat differing revisions as the same test oracle or replace one with the other without revising the compatibility matrix and expected results.

## Fixed compatibility snapshot

`upstream/nix/` is a byte-for-byte test snapshot of the CppNix 2.35.2 release. The upstream implementation source is not kept here. Keep the snapshot unchanged when refreshing it; write Rust adaptations and test runners outside `upstream/`.

The snapshot includes:

| Path | Purpose |
|---|---|
| `upstream/nix/tests/functional/` | Shell tests, Nix language fixtures, expected output, and helpers |
| `upstream/nix/tests/nixos/` | NixOS integration and virtual machine test definitions |
| `upstream/nix/tests/installer/`, `filetransfer-retry-backoff/` | Other upstream test groups |
| `upstream/nix/src/lib*-tests/` | C++ unit tests and golden data for the evaluator, fetchers, flakes, store, and utilities |
| `upstream/nix/src/lib*-test-support/` and `upstream/nix/nix-meson-build-support/` | Helpers required by those upstream suites |
| `upstream/nix/COPYING` | Upstream license text for the copied material |

The copied directory shape preserves relative fixture symlinks. One deliberately broken symlink in the language fixtures is part of the test data. The `src/nix-functional-tests` alias also points to the copied functional suite.

## Snapshot provenance

The snapshot was extracted with `git archive` from tag `2.35.2`, commit `2c73b59da29606068c0c98db015dd3a66955525d`, the newest numbered release on 2026-09-28. [`upstream/nix-snapshot.lock`](upstream/nix-snapshot.lock) records that commit and the Git object ID of each of the 17 copied paths: the top-level files and directories, plus each entry under `src/` and `tests/`.

An earlier snapshot, introduced in `046abec1`, came from unreleased `master` commit `8f7ed1c1d` (version 2.36.0). The 2.35.2 tag is on the `2.35-maintenance` branch, which left `master` on 2026-06-22. It lacks several fixture groups that the earlier snapshot had, including the masked derivation fixtures, the valid NAR fixtures, 19 language cases, and `tests/fuzzing-engine/`. [Verification](../docs/verification.md) lists how each missing group affects the compatibility tests.

Run `just nix-snapshot-verify` to check that the working-tree snapshot still has the recorded object IDs and that no unrecorded path was added under the top level, `src/`, or `tests/`. Run `just nix-snapshot-verify upstream` to also fetch the pinned commit's trees without file contents and confirm that it has the same objects. A reference CppNix executable used against these fixtures must be the 2.35.2 release. Binaries from other revisions are different references.

## Using the snapshot

Treat the expected values and serialized files as reference outputs. When a Rust component appears, add focused Rust tests that read fixtures from this snapshot and compare byte output where the format requires it. Start with store paths, NAR, derivations, and protocol data. Run the language fixtures against the Rust evaluator once it has the corresponding operations. Keep the original files intact, including cases that the Rust implementation does not support yet, and report supported, failed, and unrun cases separately.

The shell tests invoke upstream commands such as `nix-instantiate`, `nix-store`, and `nix-build`; Meson normally sets up their environment. They must be pointed at the Rust implementation and its isolated test store before their results count toward replacement compatibility. Running them against installed CppNix only establishes a reference baseline. The copied C++ tests likewise document behavior but do not run as Rust unit tests. NixOS tests need full virtual machine setup.

The snapshot is not a standalone CppNix source build: its original Meson targets depend on upstream implementation code. Run the original suite from a separate CppNix checkout for baseline behavior while Rust-specific runners are developed here.

## Refreshing the snapshot

When refreshing from a separate CppNix checkout, copy the same directories and the `.version` and `COPYING` files again. Preserve symlinks and compare both trees before relying on new expected results. Record the source commit and each path's object ID in `upstream/nix-snapshot.lock`, then run `just nix-snapshot-verify upstream`. Do not edit the vendored fixtures to make a new implementation pass; add a separate compatibility note if an upstream expectation needs investigation.
