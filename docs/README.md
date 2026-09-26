# nix-rs specifications

The [root plan](../PLAN.md) states the sole product direction: qualify the drop-in package manager first, then build the OS on it. The [MVP todo](../TODO.md) tracks the work and release evidence. These documents define what the replacement engine and later OS must do and how their claims are verified. They are design specifications, not statements that the current workspace already implements the behavior.

| Document | Subject |
|---|---|
| [Product and architecture](product.md) | Package-manager MVP, later OS behavior, engine modules, and shared interface |
| [Compatibility contract](compatibility.md) | Exact meaning of a drop-in replacement and its release matrix |
| [Compatibility matrix](compatibility-matrix.md) | Initial reference target and visible qualification status |
| [System lifecycle](system.md) | Installation, activation, upgrades, recovery, and authority |
| [Boot and Secure Boot](secure-boot.md) | Installer choice between systemd-boot and Lanzaboote, signing, enrollment, and recovery |
| [Interface behavior](interfaces.md) | CLI, TUI, GUI, configuration edits, and parity |
| [Declarative configuration workflow](configuration-workflow.md) | Source of truth, staged edits, rebuild, runtime actions, and rollback |
| [NixBox integration](nixbox-integration.md) | Typed package operations, direct engine use, and declarative persistence |
| [Rebuild diagnostics and logging](logging.md) | Rebuild phases, event schema, error rendering, output modes, and log tests |
| [Verification and release](verification.md) | Test oracles, differential tests, and release gates |
| [Development environment](development.md) | Pinned Rust shell and local check commands |
| [Flake and crate distribution](distribution.md) | Installable preview, replacement milestones, and NixOS and Home Manager gates |

Use [CONTEXT.md](../CONTEXT.md) for project vocabulary. The copied CppNix tests remain in [tests/upstream/nix/](../tests/upstream/nix/) with provenance in [tests/README.md](../tests/README.md). The imported Rust crates live in `crates/nix-rs/`, with their upstream revision in `crates/nix-rs/PROVENANCE.md`; their current presence is not a compatibility result. `crates/compat-tests/` contains initial fixture checks and records known gaps.
