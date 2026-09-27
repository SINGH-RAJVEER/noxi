# Package-manager MVP todo

This checklist tracks the first release: nix-rs replacing the selected CppNix release on an existing x86_64-linux NixOS system. The installed operating system, installer, TUI, GPUI NixBox GUI, and opt-in behavior changes come after this gate. [PLAN.md](PLAN.md) defines the product order; [docs/compatibility.md](docs/compatibility.md), [docs/compatibility-matrix.md](docs/compatibility-matrix.md), and [docs/verification.md](docs/verification.md) define what a drop-in claim requires.

A checked item below means it exists and has focused local evidence in the current working tree. It does not mean the item is committed, published, or qualified for replacement. Keep a failing or unrun test visible until the corresponding behavior passes against a pinned reference.

Each numbered section is a work package. Its checkboxes are the subtasks, and the package is complete only when every applicable checkbox has evidence in the compatibility matrix.

## 0. Make the baseline reproducible

- [x] Pin the CppNix source commit for the copied snapshot. No 2.36.0 release exists; the snapshot matches unreleased `master` commits `8fd18d36f` to `8f7ed1c1d` object for object, and `tests/upstream/nix-snapshot.lock` pins `8f7ed1c1d`. `just nix-snapshot-verify upstream` passes.
- [ ] Decide whether the first reference stays on that development commit or moves to a tagged release, then build the reference executables from the chosen commit.
- [ ] Pin a Nixpkgs and NixOS revision, a Home Manager revision, the reference feature settings, and the x86_64-linux VM image for the first compatibility row.
- [ ] Inventory every executable, `nix.conf` setting, experimental feature, protocol version, and public library interface exposed by that reference build. Record what NixOS and Home Manager actually invoke.
- [ ] Define expected behavior for both an empty store and a copy of an existing CppNix store. Preserve a reference-only VM snapshot for rollback tests.
- [x] Keep the copied fixture tree unchanged and provide a command to fetch a revision-recorded official Nix checkout with `just nix-tests-update` and `just nix-tests-sync`.
- [x] Provide a pinned nightly development shell and a separate preview flake package. The shell and package builds have been exercised locally.
- [x] Resolve the pinned nightly's rustc internal compiler error in `nix-compat-derive-tests` by diagnosing uncovered enum versions before generating the async match. The UI suite and full `just ci` pass without accepting an ICE as expected output.
- [x] Resolve the long-running `fixtures_match_expectations` worker-protocol test by reading the fixture's stated value count. The full worker-protocol test now completes in normal Cargo test runs.
- [x] Commit the intended working-tree changes in reviewable Jujutsu changes and verify a clean-clone flake build of the `nixrs` preview package.

## 1. Build a differential compatibility runner

- [x] Add an initial ten-case pure-evaluation comparison that records both executable versions, exit codes, stdout, stderr, expressions, and mismatches as JSON. A host Nix 2.34.8 check passes 10/10; the pinned 2.36.0 reference and full language suite remain unrun.
- [ ] Run reference CppNix and nix-rs in isolated test stores with identical environment, configuration, feature flags, inputs, and timeouts.
- [ ] Capture exit code, stdout, stderr, structured output, produced paths, derivation bytes, NAR bytes, store metadata, and daemon exchanges where applicable.
- [ ] Make every copied fixture discoverable and classified as passing, known gap, reference anomaly, or unrun. Fail when an unclassified fixture appears.
- [ ] Adapt the upstream language tests to run against the Rust evaluator; retain source locations and expected failures.
- [ ] Adapt upstream functional shell tests to call nix-rs binaries. Do not count tests that still call installed CppNix as nix-rs passes.
- [ ] Add wire-level tests for all supported daemon protocol versions and feature negotiations, using the copied worker fixtures as an initial corpus.
- [ ] Store machine-readable results with reference revision, platform, feature settings, and command so each row in the compatibility matrix has inspectable evidence.
- [ ] Run the same harness in CI and in disposable NixOS VMs; separate fast format checks from store, daemon, and system tests.

## 2. Finish formats and cryptographic primitives

- [x] Exercise selected hash, derivation, NAR, and masked-output-path fixtures against the imported compatibility crates.
- [ ] Cover every reference hash encoding and algorithm, including invalid input behavior and experimental feature gates.
- [ ] Complete store-path, content-address, derivation, output-specification, and realisation parsing and serialization for the enabled reference features.
- [ ] Implement NAR streaming, import/export, compression, and malformed-input behavior beyond the current small fixture set.
- [ ] Implement narinfo parsing and serialization, signatures, trusted-key checks, and cache metadata validation.
- [ ] Verify exact bytes where the reference format requires exactness; record intentional nondeterminism separately.

## 3. Complete Nix evaluation and flakes

- [x] Ship a non-conflicting `nixrs` preview binary that evaluates pure `--expr` and `--file` inputs, with focused CLI tests.
- [ ] Run the copied Nix language suite against `nix-rs-eval`; close gaps in laziness, coercion, equality, error spans, traces, string contexts, paths, and imports.
- [ ] Implement the required builtins and impure evaluation boundary, including filesystem and environment access, fetchers, and import-from-derivation.
- [ ] Match source filtering and path-copy behavior because these change derivation inputs and store paths.
- [ ] Parse, resolve, update, and write flake references and `flake.lock` with registry and follows semantics matching the reference.
- [ ] Evaluate flake outputs and selected Nixpkgs packages with the same values and derivation graphs as CppNix.
- [ ] Evaluate a pinned NixOS configuration and both standalone and module-based Home Manager configurations before claiming they are supported.
- [ ] Keep improved evaluator diagnostics behind an opt-in interface when their output would change legacy command behavior.

## 4. Implement the store, caches, and build engine

- [ ] Define a store interface that supports local and daemon-backed operations without bypassing the single store owner.
- [ ] Read and validate existing store metadata, references, validity records, roots, and profiles without modifying a user's live store during development.
- [ ] Implement atomic path registration, NAR import/export, integrity verification, locking, and crash recovery; test concurrent clients and interrupted writes.
- [ ] Implement binary-cache lookup, download, substitution, narinfo and signature verification, upload, and offline behavior.
- [ ] Implement derivation realization, dependency scheduling, fixed-output checks, logs, cancellation, and failure propagation.
- [ ] Implement sandboxed local builds with the reference environment and isolation rules; then add supported remote-builder behavior.
- [ ] Implement profile generations, roots, garbage collection, and store optimization with existing-store compatibility tests.
- [ ] Test a copied CppNix store in both directions: use it with nix-rs, then return it to CppNix without data loss or invalid metadata.

## 5. Implement the daemon and authorization boundary

- [ ] Complete worker-protocol framing, handshakes, version negotiation, operations, responses, errors, and feature negotiation for the pinned reference.
- [ ] Implement multi-user authorization, trusted users, allowed store operations, daemon configuration, sockets, and service lifecycle.
- [ ] Test reference client with reference daemon, nix-rs client with nix-rs daemon, and both cross-engine pairings where the protocol permits them.
- [ ] Verify simultaneous clients, disconnects, cancellations, daemon restart, malformed messages, and privilege boundaries.
- [ ] Package the daemon and service units expected by the selected NixOS module without altering a live host during qualification.

## 6. Complete the command and configuration contract

- [ ] Provide the `nix` executable and required legacy commands, including `nix-build`, `nix-instantiate`, `nix-store`, `nix-env`, and other names found in the reference inventory.
- [ ] Implement command arguments, option aliases, environment variables, `nix.conf`, experimental feature gates, registries, and profiles with matching precedence.
- [ ] Match exit codes, stdout, stderr, JSON fields, log modes, and command side effects; use differential tests for scripts that parse output.
- [ ] Cover core workflows: evaluate, build, develop, shell, run, flake, profile, store, copy, path-info, log, search, and garbage collection.
- [ ] Inventory public C and C++ library consumers on the supported target and either provide compatible interfaces or keep the drop-in gate open.
- [ ] Keep new diagnostics, TUI, and GUI entry points opt-in so compatibility commands retain their reference behavior.

## 7. Prove NixOS and Home Manager replacement

- [ ] Build a replacement package with the complete executable set and wire it to the selected Nixpkgs `nix.package` and daemon service options in a disposable VM.
- [ ] Compare `nixos-rebuild build`, `test`, `boot`, `switch`, and `dry-activate` results, activation effects, boot entries, generations, and failure recovery.
- [ ] Rebuild an existing CppNix-managed NixOS VM with nix-rs, reboot into the resulting generation, make a second change, and roll back.
- [ ] Test standalone Home Manager build and switch, flake mode, and Home Manager as a NixOS module using the same engine.
- [ ] Compare user profiles, activation effects, generations, and rollback for the pinned Home Manager revision.
- [ ] Demonstrate a documented return to CppNix after a failed or successful nix-rs switch, including existing-store reuse.

## 8. Package and qualify the MVP release

- [ ] Make the flake build reproducibly from a clean clone and provide the qualified replacement package, app, checks, daemon, and NixOS integration module.
- [ ] Decide which Rust crates have stable public APIs; make their dependencies independently resolvable, run `cargo package`, and publish only after license and provenance review.
- [ ] Audit imported code and copied test licenses; include attribution and source revisions in release artifacts.
- [ ] Test installation, upgrade, failed upgrade, rollback, and removal in disposable VMs before documenting host installation.
- [ ] Complete the x86_64-linux compatibility matrix with zero applicable gaps or unrun behaviors for the exact declared reference and feature set.
- [ ] Run security checks for daemon authorization, sandbox escape attempts, untrusted caches, signatures, malformed inputs, and secret handling.
- [ ] Publish binaries, source, signatures, test reports, known limits, and a safe migration and rollback procedure. Call the release a 100% drop-in replacement only after every applicable gate passes.

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
- [ ] Generate Nix source and a lock file that the installed engine can evaluate unchanged. Keep installer-only state out of the permanent configuration unless it is needed for future rebuilds.
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
- [ ] Implement safe structured edits to an application-owned Nix module, with source editing for expressions the interface cannot represent and explicit Save draft, Apply, and Rollback actions.
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
- [ ] Maintain compatibility matrices for later CppNix releases and Nixpkgs/Home Manager revisions. Separate additive nix-rs features from reference behavior with versioned opt-in settings.
- [ ] Support explicit migration between ordinary systemd-boot and Lanzaboote Secure Boot, plus signing-key rotation, only after their preview, recovery, and post-reboot verification paths are tested.
- [ ] Treat TPM-bound disk unlocking as a separate installer feature with a stated PCR policy, recovery credential, and tested response to firmware, bootloader, and kernel updates.
- [ ] Define a public extension API for NixBox operations and diagnostics without granting third-party interfaces unrestricted store or root access.
- [ ] Add remote-machine management only after target identity, authorization, replay protection, and disconnected-operation recovery are specified and tested.
- [ ] Establish performance baselines for evaluation, cache substitution, local and remote builds, daemon startup, memory use, and large rebuild diagnostics; compare with the pinned reference before claiming improvements.
- [ ] Maintain release signing, vulnerability response, provenance records for imported crates, compatibility reports, migration notes, and supported upgrade paths across OS releases.
