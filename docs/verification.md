# Verification and release specification

## Reference evidence

The copied CppNix 2.36.0 tests under [tests/upstream/nix/](../tests/upstream/nix/) are the initial test oracle. The snapshot contains language fixtures, shell tests, NixOS integration definitions, C++ unit test sources, and golden data. It matches unreleased `master` commit `8f7ed1c1d` object for object, and `just nix-snapshot-verify` checks that it stays unchanged. Preserve the copied data and record the upstream revision on each refresh. [Test snapshot notes](../tests/README.md)

The snapshot's `.version` says `2.36.0`, but the official Nix repository had no `2.36*` release tag or maintenance branch when checked with `git ls-remote` on 2026-09-27. Matching Git object IDs against `master` history places the snapshot between first-parent commits `8fd18d36f` and `8f7ed1c1d`, which differ only in a C API source file outside the snapshot. [`tests/upstream/nix-snapshot.lock`](../tests/upstream/nix-snapshot.lock) pins `8f7ed1c1d1246bfa98cefd152c87d1ea26a3d463` as the 2.36.0 pre-release reference. Reference executables for fixture comparisons must be built from that commit. The pinned official checkout is a separate 2.35.2 reference, not provenance for the copied snapshot.

The full official Nix repository can be fetched into the ignored `tests/upstream/nix-official/` checkout with `just nix-tests-update`; `tests/upstream/nix-official.lock` records its exact revision. It is a separate, current upstream reference for running CppNix's own build and test suite. Its results do not substitute for running the same cases against nix-rs, and its release tag must not be mixed with the fixed 2.36.0 fixture expectations. [Upstream test workflow](../tests/README.md)

`crates/compat-tests/` already classifies the copied ATerm derivation fixtures against vendored `nix-compat`. Its expectation table names unsupported forms, including content-addressed and dynamic derivations. These classified gaps are useful progress markers, not passing compatibility cases.

The ATerm parser follows CppNix 2.36's parsing rules for ordering, duplicates and escapes. The anonymised `hello.drv` and `firefox.drv` fixtures contain distinct paths that share a digest; they now round-trip byte for byte. `crates/compat-tests/tests/upstream_masked_hash.rs` checks the hashes that decide input-addressed output paths against CppNix's masked-hash fixtures: fixed-output hashing, hash derivation modulo, and output paths computed from the fully masked form. The `intermediate-*` fixtures state output paths computed before their output environment variables existed. They disagree with CppNix's own fully masked form, and verification rejects them; the test records this upstream fixture quirk explicitly. These checks cover narrow format behavior, not complete derivation or store compatibility.

`crates/compat-tests/tests/upstream_worker_protocol.rs` decodes the number of values stated by each JSON fixture at the version CppNix's test uses, and requires byte-identical re-encoding (decoding only, for the lossy reads CppNix also only reads). A count-bound read is necessary because an optional value at EOF may be interpreted as absent rather than as stream termination. It also checks decoded values against those the C++ test constructs. Three fixtures in the snapshot are not referenced by its C++ tests and are marked stale. Fixtures that need protocol feature negotiation, and the handshake, remain gaps.

The pinned nightly previously crashed while compiling the derive UI fixture for an enum whose version ranges leave gaps. `nix-compat-derive` now reports the first uncovered minor version before emitting the async match, preserving a compile-fail regression without treating the compiler crash as expected output. The full `just ci` command passes on the pinned development toolchain.

`crates/compat-tests/tests/upstream_nars.rs` runs the copied CppNix NAR fixtures through the Rust archive reader and writer. It checks the valid one-byte archive byte for byte, leaves trailing data unread, and rejects the copied malformed archives with their expected error categories. This covers archive parsing only; store import/export and cache exchange remain unrun.

`crates/compat-tests/tests/upstream_hashes.rs` checks BLAKE3 digests against three copied CppNix vectors and checks JSON encoding against its copied fixture. The library accepts BLAKE3 hashes, but the reference exposes them behind the `blake3-hashes` experimental feature. No nix-rs command or configuration layer enforces that feature gate yet, so BLAKE3 command compatibility remains unrun.

For each compatibility test, record the reference binary and nix-rs binary, platform, input revision, feature settings, store configuration, command or operation, expected result, actual result, and disposition. Tests that cannot yet run against nix-rs remain unrun. Tests that fail for a known reference issue still need an explicit compatibility decision; do not quietly rewrite expected output.

## Differential testing

The initial `differential-eval` runner compares ten pure expressions using `nix-instantiate --eval --strict --expr` and `nixrs eval --expr`, with an isolated temporary home and identical empty `NIX_PATH` and `NIX_CONFIG`. Run `just eval-compare /path/to/nix-instantiate /path/to/report.json` in the development shell. The JSON report contains the executable versions, arguments, expressions, exit codes, stdout, stderr, and mismatch disposition. The local Nix 2.34.8 run matched 10/10 after the preview CLI was made strict; it is a development check, not evidence for the pinned CppNix 2.36.0 development commit. The flake preview package also built from a clean local Git clone, and its installed binary evaluated a nested value. This runner covers no store operations and has no per-case timeout yet.

Compare both engines with identical inputs and isolated stores. The comparison must cover:

- Parse and evaluation results, strictness, warnings, errors, traces, source positions, string contexts, and import-from-derivation.
- Serialized derivations, input graphs, store paths, NAR contents, narinfo, cache signatures, and profile links.
- CLI flags, exit status, stdout, stderr, JSON, environment changes, and daemon protocol responses.
- Local and remote builds, sandbox restrictions, substitutions, cancellation, concurrent operations, corruption handling, and garbage collection.
- NixOS module evaluation, `system.build.toplevel`, activation actions, boot entries, generations, and rollback.
- Rebuild event semantics, rendered diagnostics, raw-log access, redaction, and machine-readable output as specified in [logging](logging.md).

Compare bytes where formats or command output require exactness. For build outputs whose content is intrinsically nondeterministic under the reference, compare behavior and invariants without incorrectly treating a changing output hash as proof of an engine defect. Keep these cases identified in the matrix.

The copied shell suite needs an isolated test store and compatible executables before it can validate nix-rs. The copied C++ tests are specifications for Rust ports and format fixtures; they are not Rust test results. NixOS tests require VM execution. [Nix test snapshot notes](../tests/README.md)

## System tests

For each release matrix row, run installation and upgrade tests in virtual machines, then qualify claimed hardware targets. Verify first boot, login, networking, a system change through each client, rebuild after reboot, failed evaluation, failed build, stale preview rejection, interrupted switch, rollback, and recovery media. Include a migration test from a reference CppNix installation if existing-system drop-in support is claimed for that row.

For the later OS image, qualify ordinary systemd-boot and Lanzaboote Secure Boot as separate installer paths. Test key enrollment and enforcement on the Secure Boot path, signed generation updates, refusal to boot an unsigned artifact, key loss, failed signing, and recovery. The image must rebuild itself through nix-rs after installation. [Boot security](secure-boot.md), [system lifecycle](system.md)

The [configuration workflow](configuration-workflow.md) requires additional cases for external source edits, staged build versus saved-source mismatch, interrupted journal writes, and desired configuration diverging from the running generation after partial activation.

The [NixBox integration](nixbox-integration.md) must be tested at the engine interface: package lookup uses the selected lock revision, a typed operation produces the expected derivation graph and source change, realization uses the single store owner, and CLI/TUI/GUI receive the same events. A test that only inspects generated Nix text does not prove those behaviors.

Qualify the GPUI-based NixBox GUI in a graphical VM: verify package and option inspection, source-edit fallback, preview and confirmation, live progress during a long build, cancellation, disconnect and restart recovery, distinct pending/running/boot states, generation comparison, rollback, keyboard use, and accessible labels. Run the same operations through the CLI and TUI and compare their plans, events, authorization decisions, and resulting generations. Test headless and recovery workflows without a desktop session.

Run tests for the public C and C++ library interfaces or maintain an explicit list of incompatible consumers. A release with a required incompatible interface cannot claim 100% drop-in behavior for its matrix row. Security checks cover multi-user authorization, trusted users, sandbox escape attempts, cache signature rejection, and secret handling.

## Release gates

The package-manager MVP comes first. It can be described as a 100% drop-in replacement only when the CppNix 2.36.0 compatibility matrix is complete for its declared existing-system target and every applicable command, setting, format, build, store, daemon, cache, and public library interface passes. This includes rebuilding a supported existing NixOS installation with nix-rs, existing-store interoperation, return to the reference implementation, and a safe installation and rollback procedure for the package manager. Publish the tested binaries, source revisions, licenses, reports, signatures, and known limits with the release. OS installation, recovery-media, TUI, and GUI rows remain separate later gates and do not weaken this first compatibility claim.

The later OS release additionally requires:

1. The installed OS builds and activates its next generation using nix-rs, with no CppNix dependency for normal maintenance.
2. Installer, update, rollback, and recovery workflows pass on every advertised platform.
3. CLI, TUI, and GPUI-based NixBox GUI operations pass their shared behavior and accessibility tests.

If a gate is open, continue development and report the specific gap. Neither release can claim behavior that its matrix has not qualified.
