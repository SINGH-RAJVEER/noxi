# noxios Settings

Settings is the system application for package management and declarative NixOS options. The built-in interfaces share package search, flake browsing, managed modules, configuration review, activation, and recovery state. The CLI runs in scripts and ordinary consoles. The TUI needs a terminal and works over SSH. The GPUI desktop application opens in a graphical session.

| Where | Entry point |
|---|---|
| Console or script | `noxi settings --help` |
| Interactive terminal | `noxi settings tui` |
| Graphical desktop | **noxios Settings** in the application menu, or `noxi settings gui` |

Build the console image with `nix build .#noxios-iso`. Build the graphical image with `nix build .#noxios-iso-graphical`. The graphical image starts Xfce and includes the same terminal tools. Systems installed from a generated noxios flake import the Settings module. It brings in the CLI and TUI by default and installs the desktop app when X11 is enabled or a desktop session is registered. Set `noxios.settings.graphical = true;` for a compositor started independently of those options. See [system lifecycle](system.md#noxios-installer-image).

## Configuration and privileges

Settings defaults to `/etc/nixos` and selects the running system's configured hostname. The live administrator owns the live image's ephemeral configuration directory. For an installed system, use an administrator-writable checkout through **Configuration directory** in Settings or the `NOXIOS_SETTINGS_CONFIG_DIR` environment variable. With a root-owned `/etc/nixos`, run configuration mutations from an elevated terminal. The desktop app runs as your user. It requests sudo when switching a system configuration or changing a system profile.

Set the target before managing packages. System is the default. Home Manager is available for systems with a Home Manager configuration. Preferences and queued work are stored per user under `$XDG_CONFIG_HOME/noxios-settings`, which defaults to `~/.config/noxios-settings`. Managed packages and options are declared in `noxios-settings-*.nix` files imported into the selected configuration.

```console
noxi settings search ripgrep
noxi settings install ripgrep --dry-run
noxi settings list
```

Package search uses the selected configuration's locked nixpkgs when its catalog can be loaded. The live images pin a store path so that catalog lookup can use the bundled source. A configured channel is available as a fallback. The packaged commands point evaluation, search, flake locking, and profile operations at noxi's Nix-compatible binaries. System activation calls `noxi system switch`. The build view streams plain diagnostics. Usage reporting has no configured destination.

Apply currently saves the managed declarations before requesting a build. A failed or cancelled build can leave saved changes pending. Review those files or retry Apply when the build issue has been corrected. The candidate-build workflow below remains planned.

## Planned semantic package operations

For `Install { package = "ripgrep", scope = NixOS }`, Settings must:

1. Read the target configuration and immutable `flake.lock` revision. Search and resolve `ripgrep` in that exact package universe for the target system. A moving channel label is not enough to identify the candidate.
2. Construct a typed desired-state change. Determine whether the package is already provided by an application-owned definition, another module, a user profile, or a flake output. Report those sources instead of guessing from a package name.
3. Ask the noxi evaluator to compute the candidate NixOS module result and `system.build.toplevel`. Report the exact resulting derivation, affected closure, expected download/build work, and any option conflict.
4. Request needed store paths or builds through the noxi engine and stream structured events. At this stage a cache or store may change, but the active OS generation does not.
5. Persist the successful desired-state change as configuration source, re-evaluate the saved source, and verify it produces the built closure before activation.
6. Invoke the selected generation action through the system manager and report source, build, running generation, and boot default separately.

Removing a package is the same kind of semantic operation. It may remove an application-owned declaration, but it must not erase a package that still comes from another module. Garbage collection is a later explicit store operation, not an automatic side effect of removing a declaration.

The planned [scoped application operations](scoped-application-builds.md) add independent user and machine application profiles; project selections reuse the native environment contract. Settings resolves their declarations against the selected scope's locked inputs and builds only its candidate profile closure. It persists and verifies the owned declaration before switching that profile. The NixOS-scoped request above still requires the system-generation workflow. Application-only requests must not silently become NixOS edits or invoke system activation.

## Declarative persistence

For independent application profiles, Settings persists the owned application declaration in the selected scope rather than importing its package selection into the NixOS module graph. That declaration is authoritative for the profile; system configuration remains authoritative for system generations.

The durable desired state is expressed in configuration source written in Nix. Settings may generate a small imported Nix module for declarations it owns, and it may update a lock in an explicit input-update operation. Rendering that module is a persistence step for a typed, evaluator-checked plan, not the full implementation of package management. UI settings or an internal operation journal are not competing sources of OS truth.

For handwritten or computed definitions in either language, Settings can inspect effective options and source locations through the evaluator. It may offer a source edit when it cannot make a safe structured edit. It must not silently use `mkForce`, rewrite arbitrary expressions, or infer persistent intent from a `systemctl` change. The [configuration workflow](configuration-workflow.md) specifies staging, source revision checks, saved-source verification, and partial failure handling.

The current write-before-rebuild behavior must be replaced or explicitly mapped to the separate **Save draft** operation. **Apply** stages and builds first so a failed evaluation or build leaves the committed configuration unchanged. If activation fails after persistence, keep the new desired state visible as pending and report the actual running state.

## Fleet operations

The later [fleet planner and adapters](distributed-computing.md#module-interfaces-and-responsibility) extend Settings's typed target scope to exact machine sets, independently identified clusters, and service topologies. Plans record the fleet revision, resolved identities, per-target closures, authority, dependency graph, availability gates, and resumable step identifiers. Selectors resolve before approval; the executor must not add newly discovered hosts to an approved plan.

Settings must expose connected-machine relationships and their effects, including etcd voters/learners, database primary/replicas, Kubernetes control-plane/worker roles, traffic endpoints, and capacity ownership. It delegates service transitions to qualified topology adapters and infrastructure admission to provisioning adapters. Kubernetes remains responsible for workload scheduling. Remote Nix builders and Kubernetes workers are distinct roles.

The shared interface must expose inventory and topology inspection, drift, bounded rollout, bootstrap/join, drain/removal, capacity changes, supported recovery, and resume. Results identify saved desired state and actual per-machine generations and service membership. Durable journals, identity checks, protected topology transitions, shared availability reservations, and stale-coordinator fencing must prevent an interrupted or competing operation from repeating unsafe work or jointly exceeding disruption limits. Generic shell hooks or regexes over command output cannot replace these contracts.

## Integration and qualification

The console and desktop suites exercise the shared operation engine and its frontends. The console ISO boot check runs `noxi settings` as the live user in QEMU. It tests locked offline search, dry-run, package install and removal in a temporary configuration, and an independent system profile build and rollback. It also checks CLI and TUI discovery without a display manager. Run it with `nix build .#checks.x86_64-linux.noxios-iso-boot`. Desktop startup and system generation switching have separate verification requirements.

Acceptance for staged operations includes locked-revision search, package resolution, flake output selection, duplicate packages from another module, typed option conflicts, cached and local builds, failed builds with unchanged configuration, source changes during preview, saved-source mismatch, partial activation, and recovery of interrupted operations. See the [compatibility matrix](compatibility-matrix.md) and [logging contract](logging.md).

Fleet qualification follows the [fleet acceptance cases](distributed-computing.md#acceptance-cases-and-release-evidence).
