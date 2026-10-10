# CLI, TUI, and GUI specification

The graphical client is the GPUI-based NixBox GUI. GPUI is the Rust UI framework used by Zed; its upstream API is still evolving, so the GUI must pin and qualify the revision it builds against. [GPUI upstream](https://github.com/zed-industries/zed/blob/main/crates/gpui/README.md)

## Shared operation interface

The OS-native CLI, TUI, and GUI must call the NixBox operation module for package and system management. Its interface includes the typed request, target scope, selected configuration revision, evaluated candidate plan, required authority, progress events, cancellation, result, and structured errors. Each client must present the same operation plan and apply the same validation. Frontends may differ in presentation; they cannot silently add different semantics for the same action. Legacy compatibility commands call the nix-rs engine under their reference command semantics. [NixBox integration](nixbox-integration.md) defines the module's engine interaction.

The module must support at least: inspect current configuration and generations; search and select packages; inspect option metadata and effective values; propose a configuration change; evaluate and preview; build; activate with the selected NixOS action; inspect logs; roll back; and manage supported store operations. An operation must report whether it changed configuration, store state, the running system, or the boot default.

## Compatibility commands

Existing Nix and NixOS command names, options, exit codes, JSON fields, stdout, stderr, and environment effects belong to the [compatibility contract](compatibility.md). A friendly interactive interface does not alter those commands. New interface commands must be distinguishable and opt-in. Automated callers must be able to request stable machine-readable results without parsing TUI or GUI text.

## Configuration editing

NixOS options are defined and merged by modules; effective values can come from imports, functions, conditions, and override priorities. A GUI cannot assume that every effective value corresponds to one editable literal. The NixOS manual describes module definitions and merges. [NixOS modules](https://nixos.org/manual/nixos/stable/)

The initial structured editor owns a clearly identified imported module, written in Lua when the configuration is Lua and in Nix otherwise. It writes only that module, keeps unrelated handwritten files unchanged, and reevaluates the complete configuration before presenting a result. Where an option derives from user code or conflicts with another definition, show the source location and offer a source edit path rather than silently inserting a stronger override. Preserve comments and formatting in files the application does not own. The source and rebuild transaction is defined in [the declarative configuration workflow](configuration-workflow.md).

The UI must expose all operations offered by its shared operation module and all option metadata available from the selected configuration. It may render an option as a form control only when it can represent the value and write it back without changing unrelated semantics. Complex expressions remain accessible through source editing. A visible "unsupported structured edit" result is preferable to a misleading toggle.

## Preview and apply

Before a privileged change, display the exact file diff, selected inputs, build target, expected generation, and requested activation mode. Stage and build the candidate, then recheck the source revision before saving owned files and activating. Reevaluate the saved source and reject activation if it differs from the built candidate. Show evaluation failures, build failures, source conflicts, and activation failures as distinct outcomes. Retain a way to inspect the full Nix diagnostic and build log.

The TUI and GUI should provide generation comparison and rollback controls, but neither may imply that rolling back the system also restores mutable user data. Accessibility, keyboard navigation, and text alternatives are required interface acceptance criteria before a graphical release.

## Project and environment operations

The planned [native project and environment interface](development-environments.md#native-commands-and-operation-interface) adds a dedicated devenv equivalent for environment planning and preparation, shell entry, tasks, and local server lifecycle. It works directly or with existing flake inputs, and flakes may export the same environment definitions. Optional native project operations add input/output inspection and explicit lock updates. These extension operations retain the selected source and lock revision in their results. Their planner uses the engine directly; their built-in supervisor runs under the owning user. No native manifest conversion is required, and compatibility flake and shell commands retain their full reference behavior.

The initial release may provide the headless CLI before graphical environment controls. Every shipped client uses the same typed operations and reports the selected environment, frozen plan, actual process readiness, state paths, endpoints, failed dependencies, and stale session revision. An environment definition, realized tool closure, and running session are separate facts. Starting a development server is a runtime action and does not enable a NixOS service, write a configuration change, or create a system generation.

## Fleet and topology operations

When the later [distributed-computing capability](distributed-computing.md) is offered, all three clients must support fleet inventory and relationship inspection, per-machine generation and drift views, fleet plan preview, safe apply, cancellation, resume, and qualified cluster/capacity/recovery actions. The CLI must support the full headless workflow with versioned machine-readable results. A GUI session cannot be required for fleet coordination or recovery.

Preview shows the fleet revision, resolved target count and identities, architecture-specific closures, membership and connection changes, load-balancing and capacity policy, disruption limits, dependency order, and requested authority. Large target sets require searchable or paginated detail, not a hidden wildcard behind an Apply button. The interface must distinguish a workload replica change from a worker-capacity change or an etcd membership change.

Execution shows completed, failed, blocked, unreachable, and not-started targets with service-specific health and correlated logs. Saved desired state, actual generations, boot defaults, runtime membership, and replication lag remain separate facts. Unknown remote state must not look healthy. Recovery must explain its service-specific data and availability effects; no control may imply an atomic fleet rollback or automatic repair of split brain.

## GPUI-based NixBox GUI

The desktop application belongs in a planned `nixbox-gui` crate. It consumes the same versioned NixBox operation interface as the CLI and TUI; GPUI owns rendering and interaction, not evaluation, store mutation, authorization, or activation. Keep the GUI process unprivileged. A narrowly scoped system service authorizes and executes privileged operations after the user has reviewed the evaluated plan. The GUI must show the target host and scope before applying a change.

Provide package search tied to the selected lock revision, option inspection with provenance, safe structured edits, a source editor path for expressions that cannot be represented as controls, plan and file-diff preview, build and activation progress, concise diagnostics with full logs on demand, generation comparison, and rollback. Long builds must leave the window responsive and support cancellation where the engine can cancel safely. After reconnecting or restarting, the GUI must recover operation state rather than imply that a still-running build failed or stopped.

The GUI must display configuration revision, built generation, running generation, and boot default as separate facts. It must label a saved but unactivated change as pending. A desktop session is not required to rebuild or recover the OS: the CLI and TUI expose the same operations on headless systems and recovery media.

All three clients use the event and diagnostic objects specified in [rebuild diagnostics and CLI logging](logging.md). The OS-native CLI is the reference renderer for concise errors; the compatibility commands keep their declared output behavior.
