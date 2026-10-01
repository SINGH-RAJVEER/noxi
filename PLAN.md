# nix-rs plan

This is the only product plan for this repository. First deliver nix-rs as a 100% drop-in replacement for the supported CppNix release. Then build a NixOS-derived operating system whose default package manager is that qualified Rust implementation. The OS includes a CLI, TUI, and GPUI-based NixBox GUI backed by the same operations. New behavior is additive and cannot change legacy results unless a user explicitly selects it. The native surfaces, not the compatibility commands, may choose their own defaults: they read and write user configuration in Lua by default and still accept Nix.

The first deliverable is an independently installable package manager that can replace CppNix on a supported existing system and operate its store through the declared compatibility matrix. The later OS deliverable is an installed system that can evaluate its own configuration, build its next generation, activate it, reboot into it, roll back, and maintain its store using nix-rs. An image that merely boots with nix-rs installed does not meet this goal. The product and acceptance criteria are specified in [docs/](docs/), indexed by the [README](README.md).

## Compatibility promise

The first reference is CppNix 2.35.2, the newest numbered release when it was selected, with x86_64-linux as the initial qualification target. Before any release is called a 100% drop-in replacement, every applicable observable behavior of that reference must meet the [compatibility contract](docs/compatibility.md), including legacy and new commands, Nix language evaluation, fetchers, flakes, store formats and paths, daemon protocols, profiles, garbage collection, cache exchange, build behavior, configuration settings, and public library interfaces used by supported consumers. The [release matrix](docs/compatibility-matrix.md) must name the exact operating systems, architectures, Nixpkgs revisions, and feature flags tested. Later reference releases or additional architectures require a new matrix and qualification run.

The phrase "100% compatible" is a release gate, not a claim about the current code. A missing interface or untested behavior cannot be silently omitted from the gate. If a required interface cannot be reproduced, the release cannot use the drop-in claim. New tweaks use new commands, settings, or negotiated protocol extensions so existing scripts and artifacts retain their meaning.

## Product architecture

- A Nix-compatible Rust engine evaluates the Nix language, resolves inputs, builds derivations, owns the local store, substitutes from caches, and speaks required protocols.
- A NixOS-derived system definition uses Nixpkgs modules and activation machinery to build, switch, boot, test, and roll back generations. The system's default engine is nix-rs.
- The OS-native CLI, TUI, and GPUI-based NixBox GUI share NixBox's structured operations and events. Legacy compatibility commands call the nix-rs engine with their reference semantics. Privileged operations pass through a constrained authorization interface.
- The [NixBox crates](docs/nixbox-integration.md) evolve into the typed operation and planning layer. They resolve packages and options against the locked inputs through the nix-rs engine, compare the resulting build graph, and coordinate realization and generation changes. They do more than edit Nix text or shell out to rebuild commands.
- Persistent changes use the [declarative configuration workflow](docs/configuration-workflow.md): NixBox plans and builds the semantic change, persists the desired state in configuration source, verifies it, and activates that generation. Direct runtime actions are marked temporary.
- [Lua configuration](docs/lua-configuration.md) lets users write NixOS and Home Manager configurations in Lua. Native surfaces default to it and still accept Nix; the configurations evaluate to the same modules and build the same system. Nixpkgs and the module definitions stay in Nix.
- The OS-native CLI uses [structured rebuild diagnostics](docs/logging.md): precise errors, meaningful progress, full raw logs on demand, and truthful final system state. Compatibility commands retain reference output unless users opt into the new renderer.
- The distribution supplies an installer image, signed releases, updates, a recovery path, and documented support for its target hardware.

The current Cargo workspace contains selected imported Rust crates under `crates/nix-rs/`, with their upstream revision recorded in `crates/nix-rs/PROVENANCE.md`. These are implementation candidates, not evidence of complete compatibility. The Rust engine may adapt them behind its own interfaces after differential tests show that each adopted behavior matches the reference.

## Work sequence

1. Establish the source provenance, compatibility inventory, test environments, and x86_64-linux baseline. Keep the copied upstream tests in `tests/nix/` unchanged.
2. Implement and qualify Nix formats and primitives: paths, hashes, NAR, derivations, narinfo, signatures, and protocol encodings.
3. Connect a minimal compatibility command, evaluator, and local store to create and realize a small derivation with matching bytes, paths, output, and metadata. Extend it through the daemon, substitution, and sandboxed builds; test cross-engine pairings early. Pure evaluator fixes can proceed in parallel, while store-backed builtins and import-from-derivation depend on store and build support.
4. Complete the evaluator, builtins, fetchers, flakes, commands, store operations, and required public library interfaces. Compare outputs, errors, and build graphs with the pinned reference; test real Nixpkgs configurations. Test existing-store interoperation and interrupted operations before recommending replacement of a user's system daemon.
5. Complete the CppNix 2.35.2 compatibility matrix on the supported existing-system target, including commands, daemon and library interfaces, existing-store interoperation, and NixOS rebuilds driven by nix-rs. Publish the drop-in package manager only when every applicable compatibility gate passes.
6. Build the OS definition, installer, system management operations, CLI, TUI, and GPUI-based NixBox GUI on that qualified engine. Qualify installation, system switches, boot, upgrades, recovery, and parity among interfaces before publishing the OS.

Reference CppNix may be used in isolated development and test environments to produce expected results. Public OS releases under this plan use nix-rs for normal evaluation, building, and store maintenance. There is no separate product track based on an upstream Nix engine.

## Current state

The repository has a Cargo workspace with selected imported crates under the nix-rs name, a copied CppNix test snapshot, and initial derivation fixture classification tests in `crates/compat-tests/`. Those tests record current format gaps; they do not establish engine parity. NixBox is currently in a separate checkout and still writes managed files and invokes rebuild subprocesses; its lower-level engine integration is specified but not implemented here. The repository does not yet contain a complete nix-rs engine, OS image, or installer. The copied C++ and shell tests need Rust runners or an isolated compatibility environment before they can qualify a release. The test snapshot's origin and limits are recorded in [verification](docs/verification.md#test-snapshot).

## Decisions required before the package-manager release candidate

- Public project name and distribution branding.
- Licensing and redistribution treatment of imported code and upstream test material.
- Required public C and C++ library consumers and their compatibility procedure. Discover these during baseline work, not after the command set is complete.
- Release signing keys, package-manager update channel, and release cadence.

The initial Nixpkgs/NixOS and Home Manager revisions are already pinned in the [compatibility matrix](docs/compatibility-matrix.md). Changing them requires a new qualification run, not another baseline decision.

## Decisions for the later OS release

- Boot modes and additional CPU architectures after the initial x86_64-linux target.
- The first set of opt-in tweaks beyond reference Nix behavior. Lua configuration is already decided; its runtime and Nix interoperation remain open in [Lua configuration](docs/lua-configuration.md#open-questions).
- Installer storage and encryption support, signing keys, update channel, and release cadence.

These decisions select concrete support targets within this plan.
