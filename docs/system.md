# System lifecycle specification

## Installation

The distribution must publish an installer image for every supported boot mode and architecture. An installation selects disks, filesystems, accounts, network configuration, bootloader, and a NixOS configuration source. The installer must show the exact storage operations before writing to disk and produce a configuration that the installed nix-rs engine can evaluate again after reboot.

On supported UEFI machines, the installer offers ordinary systemd-boot and Lanzaboote Secure Boot as distinct choices. It checks firmware, EFI System Partition, signing, and recovery prerequisites before applying the selected boot policy, then records that policy in the generated configuration source. The installer must not claim Secure Boot is active until the installed system boots with enforcement enabled. [Boot and Secure Boot specification](secure-boot.md)

The installed system must include nix-rs as the default package manager and daemon, its system management tools, a working [Lua configuration](lua-configuration.md) by default, and a recovery route. Installation is complete only after the machine boots from the installed system and can build a second system generation with nix-rs. A successful installer image build is insufficient.

Development may use an isolated reference CppNix instance to bootstrap artifacts and compare outputs. The released installed system must perform normal evaluation, builds, substitutions, and store maintenance through nix-rs.

## Image construction and first boot

The OS flake must produce installed-system, live-installer, VM, and recovery outputs from pinned Nixpkgs and nix-rs inputs. Each published image records its source revision, lock revisions, target architecture, supported firmware mode, boot policy, and checksum. Building an image only proves that an artifact exists; boot and self-maintenance need separate tests.

The installer writes a Lua configuration source that identifies the selected disk and boot policy without encoding secrets in the Nix store. After installation, test first boot, daemon startup, network availability where configured, the selected boot entry, and the running generation. Then build and activate a second generation using the installed nix-rs binaries and daemon. Inspect the resulting closure and operation trace to confirm normal maintenance did not call CppNix. A failed second rebuild must leave the installed generation bootable and report source, store, and boot state separately.

## Fleet enrollment and role lifecycle

The later [distributed-computing capability](distributed-computing.md) must support headless enrollment into an authorized fleet, including provisioning images and existing installed machines. Enrollment binds a stable inventory identity to authenticated runtime credentials and records the target system and hardware profile. A hostname, address, or copied image alone cannot establish that identity. A generic image must not embed shared long-lived machine credentials.

Cluster bootstrap and role assignment follow a reviewed fleet plan. An enrolled machine is not yet a ready Kubernetes worker, etcd voter, or database replica. The selected topology adapter verifies its service-specific readiness before adding it to active traffic or scheduling. Configuration must distinguish shared policy from architecture-specific generations, runtime membership, and persistent service data.

Retirement requires draining assigned workloads, completing supported membership removal, applying data-retention policy, revoking fleet credentials, and establishing backend-specific denial of further service access before releasing infrastructure. Deleting a Kubernetes Node or bootstrap token does not revoke an already-issued client certificate. A failed drain, ineffective access denial, or unknown remote state blocks destructive cleanup or requires explicit quarantine. Replacement is an explicit identity and membership operation, not reuse of an unreachable machine's IP address.

## System generations

The manager must preserve the NixOS distinction among `build`, `test`, `boot`, `switch`, and `dry-activate`. `build` realizes a system without activation. `test` activates it for the running system without changing the boot default. `boot` sets the next boot target without changing the running system. `switch` changes both. `dry-activate` reports intended changes without performing them. NixOS documents these behaviors and the generated `switch-to-configuration` program. [NixOS configuration changes](https://nixos.org/manual/nixos/stable/), [system switch](https://nixos.org/manual/nixos/stable/)

The manager must distinguish the running generation, boot default, and last successfully built generation in its results. Failed builds do not change either the running generation or boot default. Activation failures require an explicit status and a recovery action; a successful build alone does not mark a switch complete. Prior bootable generations remain reachable until a user-directed retention policy allows collection.

An activation request must identify the exact configuration revision and built closure it applies. If the source changes after preview or a competing activation completes first, reject the stale request and ask for a new preview. Serialize operations that mutate the system profile or boot target. Keep an audit record of actor, requested action, source revision, closure, outcome, and time.

Persistent changes follow the [declarative configuration workflow](configuration-workflow.md): stage source edits, evaluate and build the candidate, save the owned files, verify the saved source produces the built closure, then activate. Direct runtime actions remain explicitly temporary.

Every rebuild phase, including partial activation, must emit the [structured events and final state report](logging.md). A failed switch cannot be reported as a generic build failure or as success merely because the new closure exists.

## Updates and recovery

Each release must define how it receives Nixpkgs, nix-rs, kernel, and installer fixes. Pin the inputs used to build a generation. Test upgrades from the preceding supported release and a return to a known-good boot entry. Do not change `system.stateVersion` merely to select newer packages; retain it according to NixOS migration guidance.

Recovery media must be able to inspect and repair an installed system when the current generation cannot boot or the primary daemon cannot start. Recovery must be tested with a broken configuration, failed activation, damaged boot default, and a failed package-manager update. Reverting an OS generation does not undo changes to mutable application data; release notes must name migrations that need a separate backup or reversal.

Fleet updates preserve these generation semantics per machine and add topology-aware batching, canaries, workload drain, quorum and primary availability checks, and post-reboot service verification. A completed activation is not sufficient to declare the cluster healthy. Partial rollouts retain exact per-machine states and a resumable journal; they cannot report a fleet-wide atomic switch. Reject rollback to software incompatible with already migrated persistent data and offer the qualified service recovery path instead. [Fleet rollout and recovery](distributed-computing.md#fleet-plan-rollout-and-recovery)

The initial OS profile must retain NixOS switching support. The NixOS `image-based-appliance` profile disables `switch-to-configuration`; it does not satisfy this product's self-maintaining system requirement without a different, fully specified update model. [NixOS manual](https://nixos.org/manual/nixos/stable/)

## Privilege and trust

The TUI and GUI run without root privileges. A narrowly scoped privileged module authorizes store and system mutations and applies the same trust model used by compatibility commands. Do not accept arbitrary shell commands from an interface client as a substitute for typed operations. Untrusted builders must remain sandboxed; binary-cache signatures and substituter trust decisions must match the reference behavior in compatibility mode.

Only one daemon may own writes to a store at a time. System activation and store migration need exclusive operations where required. Log authorization failures without exposing credentials. Generated Nix store objects are public by default in common deployments, so credentials must stay out of generated configuration and build inputs.

Remote mutations additionally require authenticated target identity, scoped authority, replay protection, durable operation identifiers, and stale-coordinator fencing. Local activation serialization, protected cross-machine topology transitions, and availability reservations across concurrent plans are separate requirements. Routine fleet operations must use typed authorized steps, not unrestricted remote-root shell execution. Machine enrollment, cluster credentials, cache trust, and recovery authority must remain distinct permissions.
