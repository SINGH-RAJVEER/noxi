# Product and architecture specification

## Product

The first deliverable is nix-rs as an independently installable drop-in replacement for CppNix on the qualified existing-system target. It must preserve the supported reference release's commands, configuration, store and daemon behavior, and public interfaces before it can carry the 100% compatibility claim. A later deliverable is an installable NixOS-derived operating system whose package manager is nix-rs. That system must rebuild and maintain itself with the qualified engine. The distribution retains the Nix language, Nixpkgs package and module definitions, and NixOS system generation model. User configuration on the OS and the native `nixrs` commands defaults to [Lua](lua-configuration.md), which evaluates to the same modules. The initial reference is CppNix 2.35.2, recorded in the [compatibility matrix](compatibility-matrix.md); the exact Nixpkgs revision and platform matrix must be fixed before implementation claims are measured. [NixOS manual](https://nixos.org/manual/nixos/stable/)

The package-manager MVP provides the reference-compatible CLI and daemon before the OS is built. The later OS adds a native TUI and GPUI-based NixBox GUI for system management. Users can keep handwritten Nix configuration or write it in Lua. The interface can manage supported settings, show where effective values came from, preview a build and activation, and select an earlier generation. Additional features must use explicit extension points and preserve compatibility defaults.

After the package-manager MVP, nix-rs adds [native projects and development environments](development-environments.md). Standard flakes retain their complete reference behavior and can supply development shells and native environment outputs. The dedicated way to define tools, shells, tasks, and supervised local servers is a built-in devenv equivalent, usable directly or with an existing flake's inputs. Optional native project manifests provide a Lua-first input/output extension, with Nix support, and are never required for the flake or dedicated environment route. This extension uses the engine directly, can run on a supported existing system, and does not require the later OS or a separate devenv installation. The 100% drop-in replacement gate remains unchanged.

nix-rs OS must support a planned distributed-computing model for large homogeneous and heterogeneous fleets. Shared configuration describes machines, roles, connections, and service topology; exact per-machine generations and safe transition plans remain explicit. Kubernetes through kubeadm or K3s, load balancing, bounded scaling, etcd quorums, and qualified primary/replica services are specified in [distributed computing](distributed-computing.md). This capability follows the single-machine OS foundation and is not present in the current workspace.

## MVP scope and later gates

The scope expansion preserves the package-manager MVP as the complete CppNix replacement gate. [TODO.md](../TODO.md) sections 0 through 8 track that release; the later sections track separately qualified extensions and OS releases.

| Release or capability | Required work |
|---|---|
| Package-manager MVP | Every applicable reference command, setting, evaluator, fetcher, flake, store, cache, build, daemon, and required library interface; existing-store reuse; NixOS and Home Manager replacement; packaging, migration, rollback, and release evidence |
| Reference development workflows within the MVP | Ordinary flakes and `devShells`, `nix develop`, `nix shell`, `nix run`, and legacy `nix-shell`, including reference selection, lock-file effects, hooks, output, and environment behavior |
| Native environments after the MVP | Shared Lua bridge, dedicated entries and locks, flake-exported environments, tasks, built-in unprivileged supervision, PostgreSQL, session recovery, and environment acceptance evidence; this release can run on existing systems |
| Optional native projects after the MVP | Native input/output manifests and `nixrs project` operations with separate qualification; these are not a prerequisite for ordinary flakes or the first native environment release |
| Later OS release | Images, installer, boot, system management, recovery, NixBox CLI/TUI/GUI, and single-machine OS qualification |
| Fleets after the single-machine OS foundation | Authenticated remote execution, fleet plans, topology adapters, bounded capacity management, and measured support limits |

Shared evaluator, input, store, and build behavior required by CppNix remains MVP work even when a native extension will reuse it. Native schemas, the environment supervisor, Lua defaults, improved diagnostic renderers, and graphical interfaces have later implementation gates. Compatibility tests must preserve reference behavior when adjacent native files are present, and extensions must repeat affected tests when they ship.

Record comparable package-manager performance baselines with the MVP release evidence, including the exact packaged artifacts and pinned reference. The [performance architecture](performance.md) inventories optimization hypotheses and their implementation order; the [benchmark specification](performance-benchmarks.md) defines matched work, cold/warm states, correctness checks, resources, and usable-result milestones. Measurement starts during the MVP. Optimization experiments need affected compatibility evidence, and experimental backends and native environments retain separate qualification. The compatibility matrix continues to require every applicable reference behavior.

## Required modules and seams

| Module | Interface obligation | Owned behavior |
|---|---|---|
| Nix language evaluator | Evaluate an expression under defined inputs and settings; return values, derivations, and diagnostics | Parsing, laziness, builtins, string contexts, imports, source positions |
| Lua configuration evaluator | Evaluate a Lua configuration to NixOS or Home Manager modules for native surfaces | Pure Lua runtime, access to Nix values, source positions |
| Input resolver | Resolve Nix paths and flake references under configuration and lock state | Fetching, hashing, filtering, cache policy |
| Project/input module | Resolve an ordinary flake, optional native manifest, or dedicated environment's input authority | Discovery, schema validation, output selection, explicit native lock updates |
| Environment planner | Normalize a dedicated Lua/Nix entry or exported environment and produce its tool closure and runtime graph | Composition, source provenance, task/server dependencies, runtime input requirements |
| Environment supervisor | Run a frozen environment plan under the owning user and report observed state | Readiness, restart, shutdown, process ownership, logs, session recovery |
| Store | Read, register, query, import, export, and collect objects | Store paths, references, metadata, integrity, profiles, roots |
| Build engine | Realize derivations and stream logs and status | Scheduling, sandboxing, substitution, remote builders, cancellation |
| Daemon | Serve required client operations with reference permission semantics | Protocol negotiation, authentication, trusted-user rules, concurrency |
| System manager | Evaluate and realize a NixOS system generation, then request an activation action | Build planning, authorization, generation state, rollback coordination |
| NixBox operation module | Accept typed package and option requests; produce evaluated candidate plans and structured events | Target-aware search, package resolution, module inspection, preview, operation state, recovery |
| Fleet planner | Resolve a fleet revision and typed change into exact targets, closures, and ordered steps | Inventory and placement, dependency graph, capacity checks, safe batches, partial outcomes |
| Topology adapter | Execute authorized service transitions with declared preconditions and verified postconditions | Backend-specific cluster bootstrap, membership, drain, promotion, fencing, upgrade, recovery |
| Machine executor | Apply reviewed steps bound to a machine identity and operation identifier | Local authorization, serialized activation, durable results, reboot verification |
| Provisioning adapter | Resolve bounded capacity changes to admitted machines or explicit capacity failures | Infrastructure-specific creation, enrollment, readiness, and decommissioning |
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

## Fleet operation path

Fleet operations preserve this path for each machine, then add authenticated target identity, a frozen target set, cross-machine dependency ordering, topology-specific safety checks, and durable recovery. Machine groups share policy, not necessarily a closure. Realization is reused only for identical derivations, and each target receives its architecture-correct generation.

Kubernetes owns Pod scheduling and assigned runtime objects. Topology adapters coordinate supported cluster and service transitions; they do not create a universal distributed scheduler or database failover algorithm. NixBox presents one reviewed fleet plan and independent per-machine outcomes. An OS generation rollback cannot reverse mutable datastore state, and no source or activation transaction is atomic across the fleet. [Fleet rollout and recovery](distributed-computing.md#fleet-plan-rollout-and-recovery)

## Repository ownership

`crates/nix-rs/` currently contains selected vendored Rust crates, including evaluation and Nix format code. Their source revision and license are recorded in `crates/nix-rs/PROVENANCE.md`. The extracted `crates/nix-rs-hash/` crate owns Nix hash values and Nix base32 encoding; `crates/nix-rs-flakeref/` owns flake reference parsing and formatting. `nix-compat` reexports their existing module paths. `tests/nix/` contains reference tests and fixtures, and `crates/compat-tests/` has initial derivation classification tests. NixBox currently lives in a separate checkout, so this repo does not yet contain its future engine integration. New project-owned Rust modules and NixOS distribution definitions should live outside the imported trees. The CppNix test snapshot is the 2.35.2 release, pinned in `tests/nix-snapshot.lock`.

No design choice here assumes an alternative store metadata format can replace an existing CppNix store database in place. That requires a separate compatibility result.
