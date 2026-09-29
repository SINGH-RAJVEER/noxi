# System lifecycle specification

## Installation

The distribution must publish an installer image for every supported boot mode and architecture. An installation selects disks, filesystems, accounts, network configuration, bootloader, and a NixOS configuration source. The installer must show the exact storage operations before writing to disk and produce a configuration that the installed nix-rs engine can evaluate again after reboot.

On supported UEFI machines, the installer offers ordinary systemd-boot and Lanzaboote Secure Boot as distinct choices. It checks firmware, EFI System Partition, signing, and recovery prerequisites before applying the selected boot policy, then records that policy in the generated configuration source. The installer must not claim Secure Boot is active until the installed system boots with enforcement enabled. [Boot and Secure Boot specification](secure-boot.md)

The installed system must include nix-rs as the default package manager and daemon, its system management tools, a working [Lua configuration](lua-configuration.md) by default, and a recovery route. Installation is complete only after the machine boots from the installed system and can build a second system generation with nix-rs. A successful installer image build is insufficient.

Development may use an isolated reference CppNix instance to bootstrap artifacts and compare outputs. The released installed system must perform normal evaluation, builds, substitutions, and store maintenance through nix-rs.

## Image construction and first boot

The OS flake must produce installed-system, live-installer, VM, and recovery outputs from pinned Nixpkgs and nix-rs inputs. Each published image records its source revision, lock revisions, target architecture, supported firmware mode, boot policy, and checksum. Building an image only proves that an artifact exists; boot and self-maintenance need separate tests.

The installer writes a Lua configuration source that identifies the selected disk and boot policy without encoding secrets in the Nix store. After installation, test first boot, daemon startup, network availability where configured, the selected boot entry, and the running generation. Then build and activate a second generation using the installed nix-rs binaries and daemon. Inspect the resulting closure and operation trace to confirm normal maintenance did not call CppNix. A failed second rebuild must leave the installed generation bootable and report source, store, and boot state separately.

## System generations

The manager must preserve the NixOS distinction among `build`, `test`, `boot`, `switch`, and `dry-activate`. `build` realizes a system without activation. `test` activates it for the running system without changing the boot default. `boot` sets the next boot target without changing the running system. `switch` changes both. `dry-activate` reports intended changes without performing them. NixOS documents these behaviors and the generated `switch-to-configuration` program. [NixOS configuration changes](https://nixos.org/manual/nixos/stable/), [system switch](https://nixos.org/manual/nixos/stable/)

The manager must distinguish the running generation, boot default, and last successfully built generation in its results. Failed builds do not change either the running generation or boot default. Activation failures require an explicit status and a recovery action; a successful build alone does not mark a switch complete. Prior bootable generations remain reachable until a user-directed retention policy allows collection.

An activation request must identify the exact configuration revision and built closure it applies. If the source changes after preview or a competing activation completes first, reject the stale request and ask for a new preview. Serialize operations that mutate the system profile or boot target. Keep an audit record of actor, requested action, source revision, closure, outcome, and time.

Persistent changes follow the [declarative configuration workflow](configuration-workflow.md): stage source edits, evaluate and build the candidate, save the owned files, verify the saved source produces the built closure, then activate. Direct runtime actions remain explicitly temporary.

Every rebuild phase, including partial activation, must emit the [structured events and final state report](logging.md). A failed switch cannot be reported as a generic build failure or as success merely because the new closure exists.

## Updates and recovery

Each release must define how it receives Nixpkgs, nix-rs, kernel, and installer fixes. Pin the inputs used to build a generation. Test upgrades from the preceding supported release and a return to a known-good boot entry. Do not change `system.stateVersion` merely to select newer packages; retain it according to NixOS migration guidance.

Recovery media must be able to inspect and repair an installed system when the current generation cannot boot or the primary daemon cannot start. Recovery must be tested with a broken configuration, failed activation, damaged boot default, and a failed package-manager update. Reverting an OS generation does not undo changes to mutable application data; release notes must name migrations that need a separate backup or reversal.

The initial OS profile must retain NixOS switching support. The NixOS `image-based-appliance` profile disables `switch-to-configuration`; it does not satisfy this product's self-maintaining system requirement without a different, fully specified update model. [NixOS manual](https://nixos.org/manual/nixos/stable/)

## Privilege and trust

The TUI and GUI run without root privileges. A narrowly scoped privileged module authorizes store and system mutations and applies the same trust model used by compatibility commands. Do not accept arbitrary shell commands from an interface client as a substitute for typed operations. Untrusted builders must remain sandboxed; binary-cache signatures and substituter trust decisions must match the reference behavior in compatibility mode.

Only one daemon may own writes to a store at a time. System activation and store migration need exclusive operations where required. Log authorization failures without exposing credentials. Generated Nix store objects are public by default in common deployments, so credentials must stay out of generated configuration and build inputs.
