# Package-manager MVP todo

This checklist tracks the first release: nix-rs replacing the selected CppNix release on an existing x86_64-linux NixOS system. The installed operating system, installer, TUI, GPUI NixBox GUI, Lua configuration, and opt-in behavior changes come after this gate. [PLAN.md](PLAN.md) defines the product order; [docs/compatibility.md](docs/compatibility.md), [docs/compatibility-matrix.md](docs/compatibility-matrix.md), and [docs/verification.md](docs/verification.md) define what a drop-in claim requires.

A checked item below means it exists and has focused local evidence in the current working tree. It does not mean the item is committed, published, or qualified for replacement. Keep a failing or unrun test visible until the corresponding behavior passes against a pinned reference.

## 0. Make the baseline reproducible

- [x] Pin the CppNix source commit for the copied snapshot. The first snapshot matched unreleased 2.36.0 `master` commit `8f7ed1c1d`.
- [x] Choose the reference: the newest numbered release, CppNix 2.35.2 (`2c73b59`). The snapshot was refreshed from that tag, and `just nix-snapshot-verify upstream` passes.
- [x] Build the 2.35.2 reference executables and record their store path for differential runs. The `nix-reference` flake package is upstream's build at `/nix/store/nskid3yq908g18x6cnvrk2hy96327f3f-nix-2.35.2`; `just reference` builds it.
- [x] Resolve the derivation divergences from 2.35.2: ATerm ordering, duplicates, escapes and parse-time validation follow `parseDerivation`, and colliding hash-modulo inputs follow `insert_or_assign`, checked against derivations the 2.35.2 reference wrote.
- [x] Match 2.35.2's fixed-output input handling in `nix-compat` ([verification](docs/verification.md)). The lookup distinguishes regular and per-output fixed hashes; empty and missing requested outputs have focused tests, and reference-generated derivations check ordinary output paths. ATerm retains non-UTF-8 platform, builder, argument, environment-key, and output-name bytes, including invalid fixed-output names; JSON remains UTF-8-only for names.
- [x] Pin a Nixpkgs and NixOS revision, a Home Manager revision, the reference feature settings, and the x86_64-linux VM image for the first compatibility row. `tests/qualification/flake.lock` pins the inputs; its UEFI CppNix reference VM built, reached multi-user mode, started Nix and Home Manager, and shut down cleanly. The matrix records the exact revisions and scope.
- [x] Inventory every executable, `nix.conf` setting, experimental feature, protocol version, and public library interface exposed by that reference build. Record what NixOS and Home Manager actually invoke. `just reference-inventory` writes `tests/reference-inventory/` from the reference build and source, and [reference inventory](docs/reference-inventory.md) lists the pinned NixOS and Home Manager call sites, read from their source.
- [ ] Identify public C and C++ library consumers on the pinned target and record their headers, shared libraries, ABI, and packaging requirements before committing to the replacement architecture. Implement and qualify their interfaces in §6; an unsupported required consumer keeps the drop-in gate open.
- [ ] Confirm the NixOS and Home Manager call sites by tracing the executables they run in the qualification VM during rebuild, switch, garbage collection, and Home Manager activation.
- [x] Define expected behavior for both an empty store and a copy of an existing CppNix store. Preserve a reference-only VM snapshot for rollback tests. [Expected store states](docs/compatibility.md#expected-store-states) specifies both, with the reference's fresh-store layout recorded in `tests/reference-inventory/empty-store.txt`. `just reference-vm-snapshot` keeps a read-only CppNix-only VM base, and `just reference-vm-verify` boots an overlay of it without changing the base.
- [x] Keep the copied fixture tree unchanged and provide a command to fetch a revision-recorded official Nix checkout with `just nix-tests-update` and `just nix-tests-sync`.
- [x] Provide a pinned nightly development shell and a separate preview flake package. The shell and package builds have been exercised locally.
- [x] Resolve the pinned nightly's rustc internal compiler error in `nix-compat-derive-tests` by diagnosing uncovered enum versions before generating the async match. The UI suite and full `just ci` pass without accepting an ICE as expected output.
- [x] Resolve the long-running `fixtures_match_expectations` worker-protocol test by reading the fixture's stated value count. The full worker-protocol test now completes in normal Cargo test runs.
- [x] Commit the intended working-tree changes in reviewable Jujutsu changes and verify a clean-clone flake build of the `nixrs` preview package.

## 1. Build a differential compatibility runner

- [x] Add an initial ten-case pure-evaluation comparison that records both executable versions, exit codes, stdout, stderr, expressions, timeouts, and mismatches as JSON. The 2.35.2 reference run (`just eval-compare-reference`) passes 10/10 with controlled environment and per-command timeouts; the full language suite is not yet compared against the reference binary.
- [ ] Run reference CppNix and nix-rs in isolated test stores with identical environment, configuration, feature flags, inputs, and timeouts.
- [ ] Capture exit code, stdout, stderr, structured output, produced paths, derivation bytes, NAR bytes, store metadata, and daemon exchanges where applicable.
- [x] Make every copied fixture discoverable and classified as passing, known gap, reference anomaly, or unrun. Fail when an unclassified fixture appears. `crates/compat-tests/expectations/fixtures.txt` classifies all 1,916 copied files, and `fixture_inventory` fails on an unclassified file, a stale pattern, or an unbacked pass claim.
- [ ] Classify the remaining `unrun` fixtures with observed results, one fixture group at a time. The current count is in [verification](docs/verification.md#reference-evidence). Start with the remaining common and serve protocol cases; keep gaps open until the behavior passes.
	- [x] `store-path`, `content-address`, `outputs-spec`, and `derived-path` JSON against `nix-compat`. `nix_compat::cppnix_json` implements 2.35.2's encodings; `upstream_store_json` round-trips 14 of the 17 fixtures byte for byte, and the 3 nested deriving paths need `dynamic-derivations`, which both engines reject by default.
	- [x] `nar-info` and `path-info` JSON against `nix-compat`. `cppnix_json::{PathInfo, NarInfo}` read and write format versions 1 to 3, with and without impure fields; `upstream_path_info_json` decodes all 20 fixtures to the values CppNix's tests build and re-encodes the 18 written ones byte for byte.
	- [x] `realisation` and `build-result` JSON against `nix-compat`. `cppnix_json` adds `DrvOutput`, `Realisation`, `UnkeyedRealisation` with fingerprints, signing and verification, and `BuildResult`. `upstream_realisation_json` matches all 12 fixtures, including the test key's ed25519 signatures byte for byte.
	- [x] Finish `common-protocol` fixture dispositions. Seven shared primitive cases pass byte-for-byte round trips in `upstream_shared_protocols`; six old hash-based drv-output and realisation files have no corresponding 2.35.2 common codec or test and are recorded as anomalies, not passes. The test guards all 20 files and their reference dispositions.
	- [x] Cover the active `serve-protocol` value and handshake fixtures. `cppnix_serve` matches 25 additional copied files across versions 2.1 to 2.8; `upstream_serve_protocol` checks semantics, exact writes, version gates, malformed inputs, and the intentionally read-only 2.6 build result. Six stale hash-based files absent from the pinned C++ tests are recorded as anomalies, not passes. Serve commands and real client/server exchange remain separate gates.
	- [x] `derivation` JSON and derivation output JSON fixtures. `cppnix_json::derivation` reads and writes all 18 copied version-4 derivation and output JSON fixtures byte for byte, including feature-gated output kinds, dynamic inputs, and structured attributes; `upstream_derivation_json` checks their decoded fields and default feature gates.
	- [ ] `derivation-options` fixtures (advanced attributes).
	- [ ] `derivation/invariants` fixtures (filling in deferred output paths).
	- [ ] `derivation/try-resolve` fixtures (resolving content-addressed inputs).
	- [ ] `store-reference`, `machines`, `dummy-store`, and `worker-substitution` fixtures.
	- [ ] libutil `memory-source-accessor` and `nar-listing` fixtures.
	- [x] libutil `git` object fixtures. `nix_compat::git` parses and writes the copied blob and SHA-1/SHA-256 trees byte for byte, checks each listed entry, and rejects malformed headers and entries; source-accessor integration and `git-hashing` feature enforcement remain separate gates.
	- [x] Remaining libutil `hash` fixtures. `sha256.json` matches the reference value; `sha512.json` is a reference anomaly because CppNix 2.35.2's `sha512` test constructs SHA-256. Custom tests preserve both outcomes.
	- [x] libfetchers `public-key` JSON. `nix_compat::fetchers::PublicKey` matches both round-trip fixtures and the read-only missing-type fixture, which defaults to `ssh-ed25519`.
- [x] Run every copied language fixture against the Rust evaluator in isolated child processes with timeouts, fail on unclassified fixture files, and record each case's observed outcome so any change fails the test. 104 of 467 cases pass; see `crates/compat-tests/expectations/upstream-lang.txt`.
- [ ] Extend the language runner to compare every case and the checks embedded in `lang.sh` against the pinned reference binary, including `--parse` and `.flags` options as their compatibility commands become available. Semantic and diagnostic fixes belong to §3.
- [ ] Adapt upstream functional shell tests to call nix-rs binaries. Do not count tests that still call installed CppNix as nix-rs passes.
- [ ] Add wire-level tests for all supported daemon protocol versions and feature negotiations, using the copied worker fixtures as an initial corpus.
- [ ] Store machine-readable results with reference revision, platform, feature settings, and command so each row in the compatibility matrix has inspectable evidence.
- [ ] Map every applicable inventory entry, including commands, options, settings, builtins, fetchers, store types, protocol operations, and library interfaces, to test cases and results. Add reference-generated cases where copied fixtures do not cover behavior. Record out-of-scope, reference-disabled, and anomaly decisions explicitly; none is a nix-rs pass by itself.
- [ ] Run the same harness in CI and in disposable NixOS VMs; separate fast format checks from store, daemon, and system tests.

## 2. Finish formats and cryptographic primitives

- [x] Exercise selected hash, derivation, and NAR fixtures against the imported compatibility crates. The masked-output-path fixtures went with the move to 2.35.2.
- [ ] Cover every reference hash encoding and algorithm, including invalid input behavior and experimental feature gates. A custom 2.35.2 `nix hash convert` matrix covers MD5, SHA-1, SHA-256, SHA-512, and BLAKE3 in hex, nix32, base64, and SRI, including malformed inputs. The command-level BLAKE3 feature gate and remaining CLI behavior are open.
- [ ] Complete store-path, content-address, derivation, output-specification, and realisation parsing and serialization for the enabled reference features.
- [x] Generate valid NAR byte oracles with the pinned reference for regular files, executable files, directories, and symlinks. `reference_nars` checks both streaming writers and readers in 16 tests against the four 2.35.2 dumps in `fixtures/nar-reference/`, including binary payloads, empty nodes, nested entries, and a non-UTF-8 filename; the existing malformed-archive tests also pass.
- [ ] Complete NAR boundary and resource-limit behavior beyond the selected valid and malformed cases; qualify store import/export and cache compression in §4.
- [ ] Implement narinfo parsing and serialization, signatures, trusted-key checks, and cache metadata validation.
- [ ] Verify exact bytes where the reference format requires exactness; record intentional nondeterminism separately.

## 3. Complete Nix evaluation and flakes

- [x] Ship a non-conflicting `nixrs` preview binary that evaluates pure `--expr` and `--file` inputs, with focused CLI tests.
- [ ] Close the language gaps recorded by §1: laziness, coercion, equality, missing builtins, paths, imports, string contexts, CppNix-format diagnostics with source locations and traces, and invalid inputs accepted by nix-rs. Match integer-overflow errors and deep-value handling in both debug and release builds rather than accepting panics or silent wrapping.
	- [x] Replace integer arithmetic panics and wrapping with evaluation errors for addition, subtraction, multiplication, division, and negation, including builtin/operator parity and division by integer or float zero. All 43 focused arithmetic cases pass in debug and release. The four copied overflow cases now record `error-message E043`, not `pass`; full reference diagnostic rendering remains open.
- [ ] Integrate derivation creation with the store: preserve string-context dependencies and compare `derivationStrict` results, `.drv` bytes, and computed output paths against the reference before attempting full Nixpkgs evaluation. **[locked: claude-glue]**
- [ ] Implement the required builtins and impure evaluation boundary, including filesystem and environment access, fetchers, and import-from-derivation. **[locked: claude-glue]**
- [ ] Match source filtering and path-copy behavior because these change derivation inputs and store paths. **[locked: claude-glue]**
- [ ] Parse, resolve, update, and write flake references and `flake.lock` with registry and follows semantics matching the reference.
- [ ] Evaluate flake outputs and selected Nixpkgs packages with the same values and derivation graphs as CppNix.
- [ ] Evaluate a pinned NixOS configuration and both standalone and module-based Home Manager configurations before claiming they are supported.
- [ ] Keep improved evaluator diagnostics behind an opt-in interface when their output would change legacy command behavior.

## 4. Implement the store, caches, and build engine

- [ ] Define a store interface that supports local and daemon-backed operations without bypassing the single store owner.
- [ ] Read and validate existing store metadata, references, validity records, roots, and profiles without modifying a user's live store during development.
- [ ] Match fresh-store initialization, supported schema upgrades, named migrations, read-only opens, and unsupported-schema rejection from [expected store states](docs/compatibility.md#expected-store-states). After each supported write or upgrade, require CppNix to reopen the store without repair or nix-rs-only migrations.
- [ ] Implement atomic path registration, NAR import/export, integrity verification, locking, and crash recovery; test concurrent clients and interrupted writes.
- [ ] Implement binary-cache lookup, download, substitution, narinfo and signature verification, upload, and offline behavior.
- [ ] Implement derivation realization, dependency scheduling, fixed-output checks, logs, cancellation, and failure propagation. `nix-rs-build`'s `LocalBuilder` realises input-addressed and fixed-output derivations depth first through the library API; `reference_build` compares nine cases with 2.35.2, including multiple outputs, input derivations, fixed-output hashes and mismatches, and builder, missing-output, cycle and reference-check failures. Build logs, structured attributes, floating content-addressed outputs, substitution, parallel scheduling, cancellation and a command entry point remain open.
- [ ] Implement sandboxed local builds with the reference environment and isolation rules; then add supported remote-builder behavior. The Linux sandbox runs unprivileged builds in a diverted `--store ROOT`, and a probe of its environment, identity, generated `/etc` files, uid and gid maps and directory layout matches the reference byte for byte. Builds without a sandbox, build users, `auto-allocate-uids`, seccomp and network-isolation probes, and remote builders remain open.
- [ ] Implement profile generations, roots, garbage collection, and store optimization with existing-store compatibility tests.
- [ ] Test a copied CppNix store in both directions: use it with nix-rs, then return it to CppNix without data loss or invalid metadata.

## 5. Implement the daemon and authorization boundary

- [ ] Complete worker-protocol framing, handshakes, version negotiation, operations, responses, errors, and feature negotiation for the pinned reference.
- [ ] Implement multi-user authorization, trusted users, allowed store operations, daemon configuration, sockets, and service lifecycle.
- [ ] Test reference client with reference daemon, nix-rs client with nix-rs daemon, and both cross-engine pairings where the protocol permits them.
- [ ] Verify simultaneous clients, disconnects, cancellations, daemon restart, malformed messages, and privilege boundaries.
- [ ] Package the daemon and service units expected by the selected NixOS module without altering a live host during qualification.

## 6. Complete the command and configuration contract

- [ ] Provide the `nix` executable and required legacy commands, including `nix-build`, `nix-instantiate`, `nix-store`, `nix-env`, and other names found in the reference inventory. **[locked: claude-glue]**
- [ ] Implement command arguments, option aliases, environment variables, `nix.conf`, experimental feature gates, registries, and profiles with matching precedence.
- [ ] Match exit codes, stdout, stderr, JSON fields, log modes, and command side effects; use differential tests for scripts that parse output.
- [ ] Cover core workflows: evaluate, build, develop, shell, run, flake, profile, store, copy, path-info, log, search, and garbage collection.
- [ ] Provide the C and C++ interfaces required by the §0 consumer inventory, including headers, library names, SONAMEs, symbols, link metadata, and installation paths. Rebuild source consumers and run unchanged reference-linked binaries against the replacement libraries; keep the drop-in gate open for any required incompatible consumer.
- [ ] Keep new diagnostics, TUI, and GUI entry points opt-in so compatibility commands retain their reference behavior. Compatibility commands never evaluate `.lua` files, even though native surfaces default to [Lua configuration](docs/lua-configuration.md).

## 7. Prove NixOS and Home Manager replacement

- [ ] Build a provisional replacement package with the complete executable set and wire it to the selected Nixpkgs `nix.package` and daemon service options only in disposable qualification VM overlays. Verify that migration selects exactly one store-writing daemon; host replacement instructions remain gated on successful qualification.
- [ ] Compare `nixos-rebuild build`, `test`, `boot`, `switch`, and `dry-activate` results, activation effects, boot entries, generations, and failure recovery.
- [ ] Rebuild an existing CppNix-managed NixOS VM with nix-rs, reboot into the resulting generation, make a second change, and roll back.
- [ ] Test standalone Home Manager build and switch, flake mode, and Home Manager as a NixOS module using the same engine.
- [ ] Compare user profiles, activation effects, generations, and rollback for the pinned Home Manager revision.
- [ ] Demonstrate a documented return to CppNix after a failed or successful nix-rs switch, including existing-store reuse.

## 8. Package and qualify the MVP release

- [ ] Make the flake build reproducibly from a clean clone and provide the qualified replacement package, app, checks, daemon, and NixOS integration module.
- [x] Publish the preview command set to crates.io: `nix-rs-eval-builtin-macros` 0.0.1, `nix-rs-eval` 0.2.0 and `nix-rs-cli` 0.1.1 ([distribution](docs/distribution.md#published-crates)).
- [x] Publish the selected additional preview crates with versioned registry dependencies and package-local license/provenance notices. `nix-rs-hash`, `nix-rs-flakeref`, `nix-rs-compat`, `nix-rs-compat-derive`, `nix-rs-serde`, `nix-rs-glue`, and `nix-rs-commands` 0.1.0 passed package/build checks and were confirmed on crates.io on 2026-10-01. Their APIs are unstable; internal harnesses and the in-progress store stay private. Stable Rust APIs and additional crate publication are not prerequisites for the drop-in binary release.
- [ ] Audit imported code and copied test licenses; include attribution and source revisions in release artifacts.
- [ ] Test installation, upgrade, failed upgrade, rollback, and removal in disposable VMs before documenting host installation.
- [ ] Run the applicable compatibility suite against the exact packaged release-profile executables and libraries, not only debug builds or workspace tests. Record artifact hashes and build inputs in the reports, and rerun affected qualification when an artifact changes.
- [ ] Complete the x86_64-linux compatibility matrix with zero applicable gaps or unrun behaviors for the exact declared reference and feature set.
- [ ] Run security checks for daemon authorization, sandbox escape attempts, untrusted caches, signatures, malformed inputs, and secret handling.
- [ ] Publish binaries, source, signatures, test reports, known limits, and a safe migration and rollback procedure. Call the release a 100% drop-in replacement only after every applicable gate passes.

## After the package-manager MVP: Lua configuration

Native surfaces default to Lua for user configuration and still accept Nix, as specified in [Lua configuration](docs/lua-configuration.md). This work is not a drop-in gate, and it must be usable before the installer generates Lua configurations.

- [ ] Choose, pin, and embed a Lua runtime; record its version and license.
- [ ] Define how a Lua configuration maps to a NixOS or Home Manager module: imports, option definitions, module arguments, `lib` helpers such as `mkIf` and `mkForce`, and access to lazy Nix values without forcing `pkgs` or `config`.
- [ ] Carry string contexts and store-path dependencies through Lua values so derivation inputs match the Nix equivalent.
- [ ] Sandbox the runtime: no `io`, `os`, arbitrary `require`, clock, randomness, or network outside tracked inputs; test that each attempt fails with a diagnostic.
- [ ] Implement native configuration discovery: explicit path, Lua only, Nix only, and a stop naming both files when both exist.
- [ ] Report Lua syntax, runtime, and module-system errors with `.lua` locations in the structured diagnostics, including traces that cross Lua and Nix code.
- [ ] Write pinned NixOS configurations in both Lua and Nix and require identical `system.build.toplevel` derivation paths; list any differences from options that embed the configuration source.
- [ ] Do the same for standalone and module-based Home Manager configurations.
- [ ] Test that compatibility commands, including `nixos-rebuild` and `home-manager`, give identical results with and without `.lua` files present, and document how a Lua-configured system is rebuilt.
- [ ] Settle the open questions: Nix modules importing Lua, flake outputs for Lua configurations, Lua engine settings, and a Nix-to-Lua converter with formatter and editor support.

## After the package-manager MVP: OS image and boot

- [ ] Define the first OS release matrix: architecture, UEFI firmware, installation media, supported filesystems, storage layouts, graphics and headless profiles, and the Nixpkgs baseline. Record unsupported hardware rather than implying universal support.
- [ ] Create a NixOS flake that builds the installed system closure, live installer image, VM image, and recovery image from pinned inputs.
- [ ] Make the installed image use nix-rs for evaluation, realization, substitution, daemon service, profile management, and future rebuilds. Check the runtime closure for unintended CppNix dependencies.
- [ ] Preserve NixOS module semantics, generation activation, boot entries, and rollback. Keep `system.build.toplevel` and the generated activation program as qualified integration points.
- [ ] Define a small default system profile with working networking, account creation, logs, time synchronization, and a documented initial package set; support a headless installation path.
- [ ] Offer a conventional systemd-boot installation and a Lanzaboote Secure Boot installation on supported UEFI systems, with explicit prerequisites and distinct resulting configurations. Follow [the boot security spec](docs/secure-boot.md).
- [ ] Build image artifacts reproducibly in CI, record input revisions, and publish checksums, signatures, and a manifest connecting each image to its source and test results.
- [ ] Boot every advertised image in a VM, then use the installed nix-rs engine to build and activate a second generation after reboot.

## After the package-manager MVP: installer and first boot

- [ ] Design an installer plan that shows the selected disks, partitions, filesystems, encryption, boot path, users, network, and configuration source before any destructive write.
- [ ] Implement guided and manual storage paths with explicit support boundaries. Test preservation of untouched disks, cancellation before writes, interrupted partitioning, and install retry.
- [ ] Generate a Lua configuration, including the hardware module, and a lock file that the installed engine can evaluate unchanged. Keep installer-only state out of the permanent configuration unless it is needed for future rebuilds.
- [ ] Implement boot choice validation: firmware mode, EFI system partition, key state, signing capability, and recovery route. Do not silently downgrade a requested Secure Boot installation.
- [ ] Configure initial users and authentication, secret handling, networking, locale, and hardware-derived settings without placing credentials in the Nix store.
- [ ] Install and verify the bootloader, boot the installed system, confirm the active generation and daemon, and rebuild once from the installed configuration.
- [ ] Provide a text installer or equivalent headless path and recovery media able to mount the system, inspect generations, repair boot state, and restore a known-good configuration.

## After the package-manager MVP: self-maintenance and recovery

- [ ] Implement OS-native `build`, `test`, `boot`, `switch`, and `dry-activate` operations with the distinct NixOS effects specified in [system lifecycle](docs/system.md).
- [ ] Make configuration and lock revisions authoritative for persistent changes. Stage, evaluate, build, save, verify, and activate each change as specified in [the configuration workflow](docs/configuration-workflow.md).
- [ ] Journal source edits and activation state so interrupted operations can report and recover partial outcomes without erasing unrelated edits.
- [ ] Track built, running, and boot-default generations separately. Keep a known-good boot entry until a user-directed retention policy allows collection.
- [ ] Define channels for nix-rs, Nixpkgs, kernel, and installer updates; pin the inputs used for every generation and test upgrade from the preceding supported OS release.
- [ ] Test recovery after a bad configuration, failed build, daemon failure, partial activation, bootloader failure, bad update, and a generation that cannot boot.
- [ ] Document which rollback actions affect system generations and which mutable application data needs a separate backup or migration reversal.

## After the package-manager MVP: NixBox and user interfaces

- [ ] Decide how to version the separate NixBox crates with this repository, record their exact revision and license, and migrate existing CLI/TUI settings and queued operations.
- [ ] Implement typed package, option, flake, system, and store operations backed by nix-rs evaluation and realization, with target-lock-revision resolution and source provenance.
- [ ] Give the OS-native CLI, TUI, and GPUI GUI one versioned operation and event interface. No client should parse human-readable build output to infer engine state.
- [ ] Implement the structured diagnostics, progress, full-log retrieval, JSON mode, redaction, and actual final-state reporting specified in [logging](docs/logging.md).
- [ ] Implement safe structured edits to an application-owned module in the configuration's language, Lua or Nix, with source editing for expressions the interface cannot represent and explicit Save draft, Apply, and Rollback actions.
- [ ] Keep desktop clients unprivileged; authorize privileged actions through a narrow service that checks the reviewed plan and target.
- [ ] Test equivalent operations through CLI, TUI, and GUI, including headless recovery, keyboard use, accessibility labels, long builds, cancellation, reconnect, and restart recovery.

## After the package-manager MVP: OS qualification and release

- [ ] Maintain a hardware and VM test matrix for each advertised architecture, firmware mode, boot option, storage layout, and update path.
- [ ] Test installer image creation, installation, first boot, self-rebuild, reboot into a new generation, rollback, recovery media, and reinstallation without data loss where preservation is claimed.
- [ ] Test the Secure Boot installation and later signed-generation updates separately from conventional systemd-boot. Include key-loss and firmware-reset recovery cases.
- [ ] Run installer and system security reviews for disk selection, key custody, privilege boundaries, untrusted caches, sandboxing, secret handling, and image-signing infrastructure.
- [ ] Publish installation and recovery instructions, supported hardware, known limitations, signed artifacts, source revisions, and test evidence. Do not call an image self-maintaining until it rebuilds and switches using nix-rs after installation.

## Longer-term work after the first OS release

- [ ] Add architectures, firmware modes, filesystems, encrypted-storage layouts, and hardware targets only with explicit build, boot, rollback, and recovery qualification for each new matrix row.
- [ ] Maintain compatibility matrices for later CppNix releases and Nixpkgs/Home Manager revisions. Separate additive nix-rs features from reference behavior with versioned opt-in settings; native-surface defaults such as Lua configuration stay out of compatibility commands.
- [ ] Support explicit migration between ordinary systemd-boot and Lanzaboote Secure Boot, plus signing-key rotation, only after their preview, recovery, and post-reboot verification paths are tested.
- [ ] Treat TPM-bound disk unlocking as a separate installer feature with a stated PCR policy, recovery credential, and tested response to firmware, bootloader, and kernel updates.
- [ ] Define a public extension API for NixBox operations and diagnostics without granting third-party interfaces unrestricted store or root access.
- [ ] Add remote-machine management only after target identity, authorization, replay protection, and disconnected-operation recovery are specified and tested.
- [ ] Establish performance baselines for evaluation, cache substitution, local and remote builds, daemon startup, memory use, and large rebuild diagnostics; compare with the pinned reference before claiming improvements.
- [ ] Maintain release signing, vulnerability response, provenance records for imported crates, compatibility reports, migration notes, and supported upgrade paths across OS releases.
