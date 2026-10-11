# NixBox integration specification

## Role in nix-rs OS

NixBox becomes the operation and planning layer for package and option management in the OS. The OS-native CLI, TUI, and GPUI-based NixBox GUI submit typed requests to it. NixBox resolves those requests against the target's locked inputs, asks the nix-rs engine to evaluate and realize the resulting build graph, and coordinates the system generation workflow. It must do substantially more than insert text into a `.nix` file or spawn `nixos-rebuild`. Legacy compatibility commands retain their direct engine semantics.

The nix-rs engine owns Nix semantics, the store, builds, substitution, daemon protocols, and compatibility. NixBox owns user intent, target selection, candidate plans, configuration provenance, preview, operation state, and presentation. NixOS's generated activation program remains responsible for applying a built system generation. There is one writer per store and one active generation-mutation owner per machine.

## Current code and intended change

The separate NixBox checkout currently has `nixbox-core`, `nixbox-config`, `nixbox-nix`, `nixbox-cmd`, `nixbox-cli`, and `nixbox-tui` crates. The planned `nixbox-gui` crate does not exist yet. `nixbox-core::Op` already represents typed install, uninstall, flake, and migration requests. `nixbox-nix::PackageCatalog` keys search to the nixpkgs revision in the target `flake.lock`. Those are useful starting points.

Today, `nixbox-nix::manifest` writes managed Nix files, `nixbox-nix::build` launches `nixos-rebuild` or Home Manager as subprocesses, and build events carry output lines. The NixBox README says it writes configuration before rebuilding and leaves that file in place on build failure. That is the current implementation, not the target integration described here. This spec does not claim the crates have already been changed.

| Existing crate | Intended responsibility in the OS |
|---|---|
| `nixbox-core` | Typed package and system-option requests; target scope; candidate plan and operation state; idempotency and recovery identifiers |
| `nixbox-config` | Locate the selected configuration and lock; track source revisions and application-owned declarations; keep UI preferences separate from OS configuration |
| `nixbox-nix` | Query the exact locked package universe; resolve a selected attribute or flake output with the nix-rs evaluator; inspect effective module values and definitions; compare derivation graphs; request realization through the engine interface |
| `nixbox-cmd`, `nixbox-cli`, `nixbox-tui`, and planned `nixbox-gui` | Render the same plans, structured progress, diagnostics, and final state; do not duplicate store or rebuild logic. `nixbox-gui` uses GPUI for its desktop interface. |

Do not turn NixBox into a second Nix implementation or a second root daemon. The shared engine interface must return typed evaluation results, derivation identities, dependency and closure changes, build events, and structured failures. NixBox must not infer these from regexes over CLI output. A temporary subprocess adapter may help migration during development, but it is not the final OS architecture or evidence of full compatibility.

## Semantic package operation

For `Install { package = "ripgrep", scope = NixOS }`, NixBox must:

1. Read the target configuration and immutable `flake.lock` revision. Search and resolve `ripgrep` in that exact package universe for the target system. A moving channel label is not enough to identify the candidate.
2. Construct a typed desired-state change. Determine whether the package is already provided by an application-owned definition, another module, a user profile, or a flake output. Report those sources instead of guessing from a package name.
3. Ask the nix-rs evaluator to compute the candidate NixOS module result and `system.build.toplevel`. Report the exact resulting derivation, affected closure, expected download/build work, and any option conflict.
4. Request needed store paths or builds through the nix-rs engine and stream structured events. At this stage a cache or store may change, but the active OS generation does not.
5. Persist the successful desired-state change as configuration source, re-evaluate the saved source, and verify it produces the built closure before activation.
6. Invoke the selected generation action through the system manager and report source, build, running generation, and boot default separately.

Removing a package is the same kind of semantic operation. It may remove an application-owned declaration, but it must not erase a package that still comes from another module. Garbage collection is a later explicit store operation, not an automatic side effect of removing a declaration.

The planned [scoped application operations](scoped-application-builds.md) add independent user and machine application profiles; project selections reuse the native environment contract. NixBox resolves their declarations against the selected scope's locked inputs and builds only its candidate profile closure. It persists and verifies the owned declaration before switching that profile. The NixOS-scoped request above still requires the system-generation workflow. Application-only requests must not silently become NixOS edits or invoke system activation.

## Declarative persistence

For independent application profiles, NixBox persists the owned application declaration in the selected scope rather than importing its package selection into the NixOS module graph. That declaration is authoritative for the profile; system configuration remains authoritative for system generations.

The durable desired state is expressed in configuration source: Lua by default, or Nix when the configuration is written in Nix, as specified in [Lua configuration](lua-configuration.md). NixBox may generate a small imported module in that language for declarations it owns, and it may update a lock in an explicit input-update operation. Rendering that module is a persistence step for a typed, evaluator-checked plan, not the full implementation of package management. UI settings or an internal operation journal are not competing sources of OS truth.

For handwritten or computed definitions in either language, NixBox can inspect effective options and source locations through the evaluator. It may offer a source edit when it cannot make a safe structured edit. It must not silently use `mkForce`, rewrite arbitrary expressions, or infer persistent intent from a `systemctl` change. The [configuration workflow](configuration-workflow.md) specifies staging, source revision checks, saved-source verification, and partial failure handling.

NixBox's current write-before-rebuild behavior must be replaced or explicitly mapped to the separate **Save draft** operation. **Apply** stages and builds first so a failed evaluation or build leaves the committed configuration unchanged. If activation fails after persistence, keep the new desired state visible as pending and report the actual running state.

## Fleet operations

The later [fleet planner and adapters](distributed-computing.md#module-interfaces-and-responsibility) extend NixBox's typed target scope to exact machine sets, independently identified clusters, and service topologies. Plans record the fleet revision, resolved identities, per-target closures, authority, dependency graph, availability gates, and resumable step identifiers. Selectors resolve before approval; the executor must not add newly discovered hosts to an approved plan.

NixBox must expose connected-machine relationships and their effects, including etcd voters/learners, database primary/replicas, Kubernetes control-plane/worker roles, traffic endpoints, and capacity ownership. It delegates service transitions to qualified topology adapters and infrastructure admission to provisioning adapters. Kubernetes remains responsible for workload scheduling. Remote Nix builders and Kubernetes workers are distinct roles.

The shared interface must expose inventory and topology inspection, drift, bounded rollout, bootstrap/join, drain/removal, capacity changes, supported recovery, and resume. Results identify saved desired state and actual per-machine generations and service membership. Durable journals, identity checks, protected topology transitions, shared availability reservations, and stale-coordinator fencing must prevent an interrupted or competing operation from repeating unsafe work or jointly exceeding disruption limits. Generic shell hooks or regexes over command output cannot replace these contracts.

## Integration and qualification

The separate NixBox checkout is not currently part of this Cargo workspace. Decide whether to version it as local crates, a pinned repository dependency, or a maintained copy before wiring interfaces. Record the exact revision and license. Existing NixBox CLI/TUI behavior must be migrated through a versioned interface so users do not lose queued operations or stored settings.

Acceptance cases include a locked-revision search, package resolution, flake output selection, duplicate package from another module, typed option conflict, cache hit, local build, failed build with unchanged active generation, source change during preview, saved-source mismatch, partial activation, and recovery of an interrupted queued operation. Compare evaluator and store results to the [compatibility matrix](compatibility-matrix.md); use the [logging contract](logging.md) for progress and errors.

Fleet acceptance additionally verifies frozen target sets, equivalent heterogeneous target evaluation, topology-specific transition ordering, scoped remote authority, lost acknowledgements, coordinator replacement, unreachable hosts, and partial convergence through the same CLI/TUI/GUI operation interface. Qualification is backend-specific and follows the [fleet acceptance cases](distributed-computing.md#acceptance-cases-and-release-evidence); the current NixBox checkout does not implement this support.
