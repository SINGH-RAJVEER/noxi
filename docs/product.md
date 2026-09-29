# Product and architecture specification

## Product

The first deliverable is nix-rs as an independently installable drop-in replacement for CppNix on the qualified existing-system target. It must preserve the supported reference release's commands, configuration, store and daemon behavior, and public interfaces before it can carry the 100% compatibility claim. A later deliverable is an installable NixOS-derived operating system whose package manager is nix-rs. That system must rebuild and maintain itself with the qualified engine. The distribution retains the Nix language, Nixpkgs package and module definitions, and NixOS system generation model. User configuration on the OS and the native `nixrs` commands defaults to [Lua](lua-configuration.md), which evaluates to the same modules. The initial reference is CppNix 2.35.2, recorded in the [compatibility matrix](compatibility-matrix.md); the exact Nixpkgs revision and platform matrix must be fixed before implementation claims are measured. [NixOS manual](https://nixos.org/manual/nixos/stable/)

The package-manager MVP provides the reference-compatible CLI and daemon before the OS is built. The later OS adds a native TUI and GPUI-based NixBox GUI for system management. Users can keep handwritten Nix configuration or write it in Lua. The interface can manage supported settings, show where effective values came from, preview a build and activation, and select an earlier generation. Additional features must use explicit extension points and preserve compatibility defaults.

## Required modules and seams

| Module | Interface obligation | Owned behavior |
|---|---|---|
| Nix language evaluator | Evaluate an expression under defined inputs and settings; return values, derivations, and diagnostics | Parsing, laziness, builtins, string contexts, imports, source positions |
| Lua configuration evaluator | Evaluate a Lua configuration to NixOS or Home Manager modules for native surfaces | Pure Lua runtime, access to Nix values, source positions |
| Input resolver | Resolve Nix paths and flake references under configuration and lock state | Fetching, hashing, filtering, cache policy |
| Store | Read, register, query, import, export, and collect objects | Store paths, references, metadata, integrity, profiles, roots |
| Build engine | Realize derivations and stream logs and status | Scheduling, sandboxing, substitution, remote builders, cancellation |
| Daemon | Serve required client operations with reference permission semantics | Protocol negotiation, authentication, trusted-user rules, concurrency |
| System manager | Evaluate and realize a NixOS system generation, then request an activation action | Build planning, authorization, generation state, rollback coordination |
| NixBox operation module | Accept typed package and option requests; produce evaluated candidate plans and structured events | Target-aware search, package resolution, module inspection, preview, operation state, recovery |
| Distribution | Produce and update the installed OS | Installer image, defaults, boot, release artifacts, recovery |

The seams between these modules must keep compatibility decisions in the engine, not scatter them across three clients. NixBox calls the engine for evaluation, store realization, and build events; it does not parse command output as its source of truth. Each module's interface must specify accepted inputs, side effects, failure modes, and what may be retried. The imported Rust code can be adapted internally where qualified, but its current APIs and behavior do not establish drop-in compatibility. The planned NixBox changes are specified in [NixBox integration](nixbox-integration.md).

## System operation path

1. Read the selected NixOS configuration, its imported modules, lock state, and current generation.
2. Submit a typed operation to NixBox, resolve it against the locked inputs, evaluate the exact candidate, and present the file diff, derivation graph, store work, and potential activation effects.
3. Build the resulting `system.build.toplevel` closure through the nix-rs engine, with an isolated and authorized store operation.
4. Recheck the source revision, save the owned edit, and verify the saved source evaluates to the built closure. Stop if it does not.
5. Perform the requested NixOS activation action. Keep the previous bootable generation and report the new running and boot default states separately.

The NixOS manual identifies `system.build.toplevel` as the complete system closure and documents `switch-to-configuration` as the activation entry point. The distribution must preserve those semantics unless a change is an explicit extension. [NixOS system switch](https://nixos.org/manual/nixos/stable/)

The [configuration workflow](configuration-workflow.md) defines source ownership, staging, external edits, and partial failures. Configuration is authoritative for persistent system state; direct runtime actions are explicit and temporary.

## Repository ownership

`crates/nix-rs/` currently contains selected vendored Rust crates, including evaluation and Nix format code. Their source revision and license are recorded in `crates/nix-rs/PROVENANCE.md`. `tests/nix/` contains reference tests and fixtures, and `crates/compat-tests/` has initial derivation classification tests. NixBox currently lives in a separate checkout, so this repo does not yet contain its future engine integration. New project-owned Rust modules and NixOS distribution definitions should live outside the imported trees. The CppNix test snapshot is the 2.35.2 release, pinned in `tests/nix-snapshot.lock`.

No design choice here assumes an alternative store metadata format can replace an existing CppNix store database in place. That requires a separate compatibility result.
