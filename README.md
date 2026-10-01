# nix-rs specifications

The [root plan](PLAN.md) states the sole product direction: qualify the drop-in package manager first, then build the OS on it. The [MVP todo](TODO.md) tracks the work and release evidence. The documents in [docs/](docs/) define what the replacement engine and later OS must do and how their claims are verified. They are design specifications, not statements that the current workspace already implements the behavior.

## Try the preview

The `nixrs` preview evaluates pure expressions and does not touch the store or replace `nix`. [Distribution](docs/distribution.md) lists what it can and cannot do.

```sh
cargo install nix-rs-cli
nixrs eval --expr '1 + 2'
```

From a checkout, `nix run . -- eval --expr '1 + 2'` builds and runs the same command through the flake, `just` lists the development recipes, `just ci` runs the full local check, and `just eval-compare-reference` compares the preview with the pinned CppNix 2.35.2 reference. [Development](docs/development.md) describes each one.

## Specifications

| Document | Subject |
|---|---|
| [Product and architecture](docs/product.md) | Package-manager MVP, later OS behavior, engine modules, and shared interface |
| [Compatibility contract](docs/compatibility.md) | Exact meaning of a drop-in replacement and its release matrix |
| [Compatibility matrix](docs/compatibility-matrix.md) | Initial reference target and visible qualification status |
| [Reference inventory](docs/reference-inventory.md) | Everything CppNix 2.35.2 exposes, and what the pinned NixOS and Home Manager call |
| [System lifecycle](docs/system.md) | Installation, activation, upgrades, recovery, and authority |
| [Distributed computing and fleets](docs/distributed-computing.md) | Homogeneous and heterogeneous fleets, kubeadm/K3s, service topologies, load balancing, scaling, and coordinated recovery |
| [Boot and Secure Boot](docs/secure-boot.md) | Installer choice between systemd-boot and Lanzaboote, signing, enrollment, and recovery |
| [Interface behavior](docs/interfaces.md) | CLI, TUI, GUI, configuration edits, and parity |
| [Declarative configuration workflow](docs/configuration-workflow.md) | Source of truth, staged edits, rebuild, runtime actions, and rollback |
| [Lua configuration](docs/lua-configuration.md) | Lua as the default language for user configuration on native surfaces, alongside Nix |
| [NixBox integration](docs/nixbox-integration.md) | Typed package operations, direct engine use, and declarative persistence |
| [Rebuild diagnostics and logging](docs/logging.md) | Rebuild phases, event schema, error rendering, output modes, and log tests |
| [Verification and release](docs/verification.md) | Test oracles, differential tests, and release gates |
| [Development environment](docs/development.md) | Pinned Rust shell and local check commands |
| [Flake and crate distribution](docs/distribution.md) | Installable preview, replacement milestones, and NixOS and Home Manager gates |

Use [CONTEXT.md](CONTEXT.md) for project vocabulary. The copied CppNix tests remain in [tests/nix/](tests/nix/) with provenance in [verification](docs/verification.md#test-snapshot). The imported Rust crates live in `crates/nix-rs/`, and their extracted hash and flake reference code lives in `crates/nix-rs-hash/` and `crates/nix-rs-flakeref/`, with the upstream revision in `crates/nix-rs/PROVENANCE.md`; their current presence is not a compatibility result. `crates/compat-tests/` contains initial fixture checks and records known gaps. [CHANGELOG.md](CHANGELOG.md) records each crate's changes by version.
