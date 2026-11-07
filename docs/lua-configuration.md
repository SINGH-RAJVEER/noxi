# Lua configuration specification

## Decision

noxi accepts user configuration written in Lua as well as in the Nix language, and its native surfaces default to Lua. A Lua configuration describes the same NixOS or Home Manager modules as a Nix configuration and builds the same system. It is an [extension](compatibility.md#extensions): the compatibility commands keep the reference behavior and never read Lua files, so the drop-in claim is unaffected.

Dedicated environment declarations and optional native project manifests also default to Lua. They normalize to the same input/output and environment definitions as their native Nix equivalents, rather than to a NixOS system module. The dedicated devenv equivalent works directly or with existing flake inputs; standard `flake.nix` remains Nix and is fully supported without conversion. Their contract is in [native projects and development environments](development-environments.md). They reuse the Lua runtime and Nix value bridge specified here, with no change to the 100% drop-in gate.

This is specified, not implemented. The workspace has no Lua runtime, and the open questions at the end must be settled before the first implementation.

## Scope

| Source | Language |
|---|---|
| NixOS system configuration (`configuration.lua`) | Lua by default; Nix still supported |
| Home Manager configuration (`home.lua`) | Lua by default; Nix still supported |
| Dedicated development environments and optional native project inputs/outputs | Lua by default; Nix still supported; standard flakes remain a supported route; planned schema in [development environments](development-environments.md) |
| Native fleet inventory, shared profiles, cluster and service-topology configuration | Lua by default; Nix still supported; planned schema in [distributed computing](distributed-computing.md) |
| Module owned by NixBox (`noxi-managed.lua`) | Same language as the configuration that imports it |
| Nixpkgs, its library, and the NixOS and Home Manager module definitions | Nix, unchanged |
| `flake.lock`, derivations, store paths, and every other reference format | Unchanged; native `project.lock` and standalone `environment.lock` use the reference lock graph encoding |
| `nix.conf` and other engine settings | Unchanged; see the open questions |

Lua is a way to write user configuration, not a second package set. Packages, modules, and the module system itself stay in Nix and are evaluated by the Nix language evaluator.

## Where Lua is the default

| Surface | Behavior |
|---|---|
| Native `noxi` configuration and system commands | Discover, evaluate, and build Lua or Nix configurations; create new ones in Lua |
| Native `noxi project` and `noxi env` commands | Prefer dedicated Lua or Nix environment entries, support existing flakes and optional native projects, and use explicit selection on ambiguity; create new native templates in Lua |
| OS installer | Generates `configuration.lua` and its hardware module in Lua |
| NixBox operations | Write the owned module in Lua when the configuration is Lua |
| Templates and examples shipped with noxi | Lua first, with a Nix equivalent where one is documented |
| Compatibility commands: `nix`, `nix-build`, `nix-instantiate`, `nixos-rebuild`, `home-manager`, and the other reference names | Reference behavior; `.lua` files are ordinary files and are never evaluated |

A native command selects its configuration as follows. An explicit path is used as given. Otherwise the command looks for the Lua and the Nix file in the configuration directory. If only one exists, it is used. If both exist, the command stops and names both files rather than guessing which system to build; the user passes an explicit path or removes one. Existing Nix-only systems therefore keep working with native commands unchanged.

A system configured in Lua is rebuilt with the native system commands. `nixos-rebuild` on such a system behaves as it does under CppNix when `configuration.nix` is missing. Documentation and the OS must say so rather than hide it.

## Semantics

A Lua configuration evaluates to a NixOS or Home Manager module: the same imports, option definitions, and module arguments (`config`, `lib`, `pkgs`, and the others the module system supplies) that a Nix module provides. The Nix module system merges it with every other module. Definition priorities, `mkIf`, `mkMerge`, `mkDefault`, `mkForce`, option types, and assertions behave exactly as they do for Nix modules. The result is the ordinary `system.build.toplevel` closure, so generations, activation, boot entries, and rollback are unchanged.

A Lua configuration may import Nix modules, such as an existing `hardware-configuration.nix`. Whether Nix modules may import Lua modules is an open question.

Evaluation stays pure and reproducible. Lua code cannot read files, the environment, the clock, or the network except through the same explicit, tracked inputs a Nix configuration uses. Lua's `io`, `os`, `require` of arbitrary files, and nondeterministic library functions are unavailable or replaced. Every Lua source a configuration uses is an input of the evaluation, like a Nix file.

For equivalent inputs, a Lua configuration and its Nix translation must produce the same `system.build.toplevel` derivation path. Options that embed the configuration source itself, such as `system.copySystemConfiguration`, are the only expected differences and must be listed in the test results.

For fleet configuration, equivalent Lua and Nix inputs must resolve the same machine identities, relationships, placement and transition policy, and per-machine derivations. Hardware profiles and target-system differences remain explicit. Inventory imported from etcd or a provisioning system must first become a recorded, tracked input; the evaluator cannot discover machines by querying runtime services. [Fleet configuration contract](distributed-computing.md#configuration-contract-and-heterogeneous-machines)

For native projects and environments, equivalent Lua and Nix declarations must resolve the same locked inputs, exported package and tool derivations, and normalized task/server graph. Runtime paths and secret-provider references remain typed declarations until execution; their values cannot be read by Lua evaluation. A development shell or server session does not produce or activate `system.build.toplevel`.

## Diagnostics

Errors in Lua sources use the [structured diagnostics](logging.md) with the `.lua` file, line, and column. A module-system error caused by a Lua definition names that definition's Lua location, and traces that cross Lua and Nix code show both.

## Acceptance cases

- A set of pinned NixOS configurations, from minimal to a desktop and a server with several services, written in both Lua and Nix, produce identical `system.build.toplevel` derivation paths. The same holds for Home Manager activation packages.
- Native discovery is tested with only a Lua file, only a Nix file, both files, and an explicit path.
- Compatibility commands produce identical results with and without `.lua` files present.
- Attempts to reach the filesystem, environment, clock, or network outside tracked inputs fail with a diagnostic.
- Lua syntax errors, runtime errors, and unknown options report Lua source locations.
- The installer's generated Lua configuration evaluates unchanged after the first reboot and builds a second generation.
- NixBox edits to a Lua configuration pass the [configuration workflow](configuration-workflow.md) acceptance cases.
- Equivalent Lua and Nix fleet inputs produce matching inventory, topology, policy, and per-target closures for homogeneous and heterogeneous groups. Live discovery, unknown machine references, and unsupported target systems produce the specified diagnostics rather than changing evaluation implicitly.
- Equivalent Lua and Nix native projects and environments pass the [environment acceptance cases](development-environments.md#acceptance-cases-and-release-evidence), including pure evaluation, identical tool derivations, shared locked inputs, and source-located composition errors.

## Open questions

- Which Lua runtime and version to embed, and how it is pinned.
- How eagerly evaluated Lua reaches lazily evaluated Nix values without forcing `pkgs` or `config`, and how Lua functions are called from Nix.
- How `lib`, `pkgs`, and module arguments appear in Lua, and what the equivalent of `{ config, lib, pkgs, ... }:` looks like.
- How string contexts and store-path dependencies are carried through Lua strings.
- Whether Nix modules may import Lua modules, and whether flakes may declare Lua configurations as outputs.
- Whether engine settings such as `nix.conf` may also be written in Lua.
- Whether noxi provides a Nix-to-Lua converter for existing configurations, and a formatter and editor support for Lua configurations.
