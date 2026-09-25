# Verification and release specification

## Reference evidence

The copied CppNix 2.36.0 tests under [tests/upstream/nix/](../tests/upstream/nix/) are the initial test oracle. The snapshot contains language fixtures, shell tests, NixOS integration definitions, C++ unit test sources, and golden data. Its original commit is unknown. Preserve the copied data and record a specific upstream revision on the next refresh. [Test snapshot notes](../tests/README.md)

`crates/compat-tests/` already classifies the copied ATerm derivation fixtures against vendored `nix-compat`. Its expectation table names unsupported forms, including content-addressed and dynamic derivations. These classified gaps are useful progress markers, not passing compatibility cases.

For each compatibility test, record the reference binary and nix-rs binary, platform, input revision, feature settings, store configuration, command or operation, expected result, actual result, and disposition. Tests that cannot yet run against nix-rs remain unrun. Tests that fail for a known reference issue still need an explicit compatibility decision; do not quietly rewrite expected output.

## Differential testing

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

The [configuration workflow](configuration-workflow.md) requires additional cases for external source edits, staged build versus saved-source mismatch, interrupted journal writes, and desired configuration diverging from the running generation after partial activation.

The [NixBox integration](nixbox-integration.md) must be tested at the engine interface: package lookup uses the selected lock revision, a typed operation produces the expected derivation graph and source change, realization uses the single store owner, and CLI/TUI/GUI receive the same events. A test that only inspects generated Nix text does not prove those behaviors.

Qualify the GPUI-based NixBox GUI in a graphical VM: verify package and option inspection, source-edit fallback, preview and confirmation, live progress during a long build, cancellation, disconnect and restart recovery, distinct pending/running/boot states, generation comparison, rollback, keyboard use, and accessible labels. Run the same operations through the CLI and TUI and compare their plans, events, authorization decisions, and resulting generations. Test headless and recovery workflows without a desktop session.

Run tests for the public C and C++ library interfaces or maintain an explicit list of incompatible consumers. A release with a required incompatible interface cannot claim 100% drop-in behavior for its matrix row. Security checks cover multi-user authorization, trusted users, sandbox escape attempts, cache signature rejection, and secret handling.

## Release gate

A release candidate passes only when:

1. The compatibility matrix is complete and every applicable test and interface is passing, including protocol and library interoperability.
2. The installed OS builds and activates its next generation using nix-rs, with no CppNix dependency for normal maintenance.
3. Installer, update, rollback, and recovery workflows pass on every advertised platform.
4. Release artifacts, source revisions, licenses, test reports, signatures, and known limits are published together.

If any gate is open, continue development and report the specific gap. Do not describe the release as 100% compatible or drop-in until all gates close.
