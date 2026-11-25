# System lifecycle specification

## Installation

The distribution must publish an installer image for every supported boot mode and architecture. An installation selects disks, filesystems, accounts, network configuration, bootloader, and a NixOS configuration source. The installer must show the exact storage operations before writing to disk and produce a configuration that the installed noxi engine can evaluate again after reboot.

On supported UEFI machines, the installer offers ordinary systemd-boot and Lanzaboote Secure Boot as distinct choices. It checks firmware, EFI System Partition, signing, and recovery prerequisites before applying the selected boot policy, then records that policy in the generated configuration source. The installer must not claim Secure Boot is active until the installed system boots with enforcement enabled. [Boot and Secure Boot specification](secure-boot.md)

The installed system must include noxi as the default package manager and daemon, its system management tools, a working Nix configuration, and a recovery route. Installation is complete only after the machine boots from the installed system and can build a second system generation with noxi. A successful installer image build is insufficient.

Development may use an isolated reference CppNix instance to bootstrap artifacts and compare outputs. The released installed system must perform normal evaluation, builds, substitutions, and store maintenance through noxi.

## Image construction and first boot

The OS flake must produce installed-system, live-installer, VM, and recovery outputs from pinned Nixpkgs and noxi inputs. Each published image records its source revision, lock revisions, target architecture, supported firmware mode, boot policy, and checksum. Building an image only proves that an artifact exists; boot and self-maintenance need separate tests.

The installer writes a Nix configuration source that identifies the selected disk and boot policy without encoding secrets in the Nix store. After installation, test first boot, daemon startup, network availability where configured, the selected boot entry, and the running generation. Then build and activate a second generation using the installed noxi binaries and daemon. Inspect the resulting closure and operation trace to confirm normal maintenance did not call CppNix. A failed second rebuild must leave the installed generation bootable and report source, store, and boot state separately.

### noxios installer image

The first image is the NixOS minimal installation ISO with noxi in place of CppNix and the noxios name on what people see. [`nix/noxios/default.nix`](../nix/noxios/default.nix) evaluates `installation-cd-minimal.nix` from the qualification row's pinned Nixpkgs (26.11) with these modules:

| Module | Effect |
|---|---|
| [`module.nix`](../nix/noxios/module.nix), also `nixosModules.noxios` | Sets `nix.package` to `packages.x86_64-linux.noxi-replacement` |
| [`branding.nix`](../nix/noxios/branding.nix) | The noxios name, URLs, getty greeting and volume label; drops the NixOS manual |
| [`settings.nix`](../nix/noxios/settings.nix) | Built-in CLI and TUI, plus the graphical Settings application when a desktop session is configured |
| [`installer.nix`](../nix/noxios/installer.nix) | The installer profile, with a `noxi` live user |
| [`manual.nix`](../nix/noxios/manual.nix) | The noxios manual and `noxios-help` |
| [`live.nix`](../nix/noxios/live.nix) | The live system's locked flake in `/etc/nixos` |

The kernel, ZFS support, installer tools and the root `nixos` channel stay as NixOS ships them.

The image is evaluated with `nixos/lib/eval-config.nix`, as Hydra evaluates the official ISO in `nixos/release.nix`, not with `nixpkgs.lib.nixosSystem`. `nixosSystem` sets `nixpkgs.flake.source`, which puts `nixpkgs=flake:nixpkgs` ahead of the root channel in `NIX_PATH`. Resolving that entry needs the `flakes` experimental feature, which the image leaves off as NixOS does. CppNix would then fail `<nixpkgs>` lookups the same way. With `eval-config.nix`, `<nixpkgs>` resolves to the root channel, as on the stock image. A small module sets `system.nixos.versionSuffix` and `system.nixos.revision` from the pinned Nixpkgs input, as the Nixpkgs flake does, so the version label is `26.11.20260925.e94cb15`, as `nixosSystem` gave.

| Output | Contents |
|---|---|
| `nixosConfigurations.noxios-iso` | The image's NixOS configuration |
| `packages.x86_64-linux.noxios-iso` | The ISO, under `iso/` |
| `nixosConfigurations.noxios-iso-graphical` | The image configuration with Xfce, LightDM and the Settings application |
| `packages.x86_64-linux.noxios-iso-graphical` | The graphical ISO, under `iso/` |
| `nixosModules.noxios` | The noxi engine and built-in Settings module for installed systems |
| `checks.x86_64-linux.noxios-iso-closure` | Fails if a CppNix package is in the image's store, or if the engine is not |
| `checks.x86_64-linux.noxios-iso-boot` | Boots the ISO in QEMU and checks that noxi did the work, the branding, the live user, the manual and the flake |

Build the console image with `nix build .#noxios-iso` or the graphical image with `nix build .#noxios-iso-graphical` in the development shell. Both include `noxi settings` and `noxi settings tui`. The graphical image also includes the **noxios Settings** desktop launcher. See [Settings](settings.md) for configuration and commands.

#### What is branded

noxios is a NixOS derivative, named through NixOS's own options for derivative distributions:

- `system.nixos.distroName`, `distroId`, `vendorName` and `vendorId` are `noxios`. `/etc/os-release` reads `ID=noxios`, `NAME=noxios` and `ID_LIKE=nixos`; NixOS writes the last itself for any other `distroId`. The boot menu entries, the stage 1 and stage 2 banners, `nixos-version`'s distribution name, the default host name (`noxios`) and the `CPE_NAME` follow.
- `HOME_URL`, `VENDOR_URL`, `DOCUMENTATION_URL`, `SUPPORT_URL` and `BUG_REPORT_URL` point at the noxi repository.
- The getty greeting reads `<<< Welcome to noxios 26.11… >>>`, and the help line names the `noxi` user, `noxios-help` and the flake.
- The ISO file is `noxios-minimal-26.11.<date>.<rev>-x86_64-linux.iso`, and its volume label is `noxios-minimal-26.11-x86_64`. The initrd mounts the root file system by that label, so every boot checks it.
- The NixOS manual and `nixos-help` are off. With them goes CppNix's last place in the image's build graph: Nixpkgs evaluated the manual's option list with its own build-platform `nix`.
- The noxios manual takes their place. [`manual.nix`](../nix/noxios/manual.nix) converts every `docs/*.md` page with `lowdown` into HTML under `share/doc/noxios` and into a section 7 manual page, `noxios-NAME` (`man noxios-system`), with an index page `noxios(7)`. `noxios-help [PAGE]` opens the HTML index or one page in `$BROWSER`, `xdg-open` or `w3m`, as `nixos-help` does.

#### What stays NixOS-named, and why

Everything other software calls keeps its NixOS name, so NixOS configurations, Home Manager and Nixpkgs scripts run unchanged:

- the `nix*` commands, `/nix/store` (store path hashes and the cache.nixos.org substitutes depend on it), `/nix/var/nix`, `nix.conf` and `NIX_PATH`
- the `nixbld` build users and the `nix-daemon` units
- the `nix.*` and `system.nixos.*` options, and Nixpkgs itself
- the `nixos-*` tools, such as `nixos-rebuild`, `nixos-install` and `nixos-version`
- the root `nixos` channel. NixOS's own ISO test evaluates `nix-env -iA nixos.procps` through it, and `nixos-rebuild --upgrade` updates the channel by that name. Renaming or dropping it would break both, so the image keeps it next to the flake.
- the `system.nixos.variant_id` `installer` and the `LOGO` `nix-snowflake`, which no noxios artwork replaces yet

#### Installer profile

[`installer.nix`](../nix/noxios/installer.nix) replaces Nixpkgs' `profiles/installation-device.nix` and `installer/cd-dvd/installation-cd-base.nix` through `disabledModules`, because both name the live user `nixos`. It is a fork of both at Nixpkgs revision `e94cb152ed51bd6e24eb4a41f1460252beb52cd2`, recorded in its header. The differences:

- The live user is `noxi`. It is in `wheel`, `networkmanager` and `video`, has an empty password, logs in automatically on the consoles, runs `sudo` without a password and is in `nix.settings.trusted-users`. No `nixos` user exists.
- The kernel parameter `live.noxios.passwd=` sets its password. `live.nixos.passwd=` is still accepted.
- The NixOS manual is not forced on.

Everything else is upstream's behaviour: root's empty password, sshd, NetworkManager, polkit, the scanned hardware modules, the root channel, the low-memory settings and the extra store contents.

#### The live system's flake

The root channel stays, and a locked flake of the live system sits next to it. At boot, `/etc/nixos` receives writable copies of `flake.nix`, `flake.lock` and an empty `configuration.nix` unless a `flake.nix` is already there. With `/etc/nixos/flake.nix` present, both `noxi system switch` and `nixos-rebuild switch` build `nixosConfigurations.noxios`, which is named after the default host name. Both enable `nix-command` and `flakes` for their own calls, so the image leaves the experimental features off. Options set in `configuration.nix` are merged into the image's modules. Clone-config's cloned `configuration.nix` is turned off, because it rebuilds a stock NixOS installer without noxi.

The flake has three inputs: `nixpkgs` (`github:NixOS/nixpkgs/nixos-unstable`), and `noxi` (`github:SINGH-RAJVEER/nix-rs`) and `rust-overlay` as plain sources. Its outputs import the noxios modules and the engine package from the `noxi` input. [`live.nix`](../nix/noxios/live.nix) generates the lock from the sources the image itself was built from. Each locked node is a `path` input at the source's store path, with its NAR hash, modification time and revision, and the image's store holds those paths. The flake therefore evaluates offline, and before any update it describes exactly the published image: its toplevel is the image's own. `noxi flake update` (or `noxi system switch --update`) re-locks the inputs from GitHub, which replaces `nixos-rebuild --upgrade`. `nixpkgs` then follows `nixos-unstable` rather than the qualification pin. The image's store gains the Nixpkgs source a second time, besides the channel's copy, and the noxi repository source.

After an update, the image's flake calls `nix/noxios/default.nix` (`nixpkgs`, `rust-overlay`, `source`, `package`) and `nix/package.nix` (`pkgs`, `rust-overlay`, `replacement`) of the newest noxi revision. Those argument sets are an interface of every published image. New arguments need defaults, and existing ones cannot be removed or renamed. An update works only once a noxi revision that accepts them is on the default branch.

`noxi system build --flake /etc/nixos#noxios` has been run on the host against a copy of the generated flake: it realised nothing, linked the image's own toplevel and left the lock unchanged. `noxi flake update` and a `switch` on the live image have not been run, because the boot check has no network.

#### Where noxi runs

Because `nix.package` is replaced, noxi runs every place NixOS calls the package manager:
- the daemon socket and service
- `register-nix-paths` (`nix-store --load-db` and `nix-env --set` of the system profile at boot)
- `nix-channel-init` (`nix-env -i` of the channel before the daemon starts)
- `nixos-install` (`nix-build`/`nix build --store /mnt` and `nix-env --store /mnt --set`)
- `nixos-rebuild`
- `nixos-option`, which in this Nixpkgs is a shell wrapper around `nix-instantiate` rather than a C++ program linked to CppNix

#### Boot check

The boot check adds NixOS's test instrumentation to a copy of the image, as Nixpkgs' own ISO boot test does, and boots it with 3 GiB of memory. It checks:
- both boot units succeeded
- the `nix*` commands in the system path and the running `nix-daemon` resolve into the noxi package
- the `noxi` user reaches the store through the daemon
- `/etc/os-release` (`ID=noxios`, `ID_LIKE=nixos`), the greeting and help line in `/etc/issue`, the host name and the volume label
- the `noxi` user is in `wheel`, has passwordless `sudo`, is a trusted user and is logged in on tty1, and no `nixos` user exists
- `noxios-help` opens the manual and one page, `man noxios-system` resolves, and `nixos-help` and the NixOS manual are absent
- `/etc/nixos` holds writable copies of the flake, and noxi evaluates it offline to the running version label and to the published image's toplevel
- NixOS's own ISO assertions pass: `nix store verify` of the running system, and channel evaluation with `nix-instantiate` and `nix-env --dry-run`

It also checks that `noxi-daemon.service` and `noxi-daemon.socket` are aliases of the nix-daemon units. NixOS ignores the `Alias=` lines of packaged units, so `nixosModules.noxios` sets them through `systemd.services.nix-daemon.aliases` and `systemd.sockets.nix-daemon.aliases`.

Building the image is not a noxi rebuild. The host's package manager builds it. The image is a preview on an unqualified engine.

#### Install test

`checks.x86_64-linux.noxios-install` ([`install-test.nix`](../nix/noxios/install-test.nix), [`install-test.py`](../nix/noxios/install-test.py)) installs the image to an empty disk, boots the installed system, and moves it to a second generation. It follows the offline install of Nixpkgs' `nixos/tests/installer.nix`. It covers BIOS with GRUB only.

In the installer VM, the test:

1. Boots the instrumented ISO with an empty 12 GiB virtio disk.
2. Partitions the disk with `parted` (one MBR partition) and formats it as ext4 with a fixed UUID.
3. Runs `noxi system init --root /mnt`, and `noxi system init --flake` into a separate directory. It checks that the generated flake imports the noxios modules, carries the image's lock, and evaluates offline to a system whose package manager is noxi.
4. Replaces the generated `configuration.nix` with [`install-test/`](../nix/noxios/install-test/)'s flake and configuration, keeping the generated `hardware-configuration.nix`.
5. Copies the image's `/etc/nixos/flake.lock`, so the flake's inputs (Nixpkgs, the noxi source, rust-overlay) resolve offline from the image's store.
6. Checks that noxi evaluates the flake offline to the same toplevel the build host evaluates with [`install-test/hardware-configuration.nix`](../nix/noxios/install-test/hardware-configuration.nix). That file stands in for the generated one. The fixed UUID makes them agree.
7. Runs `noxi install --root /mnt --flake /mnt/etc/nixos#noxios --no-root-passwd` and checks the result:
   - nothing is built
   - `system-1-link` points at the toplevel
   - GRUB's configuration names noxios
   - the installed closure contains no CppNix
8. Runs `noxi install` again. The second run creates no new generation.
9. Runs commands in the installed root with `noxi system enter`.

The test then boots the disk on its own and checks:

1. The booted system is the installed toplevel, and `/etc/os-release` reads `ID=noxios`.
2. The `nix*` commands and the running `nix-daemon` are noxi, `noxi-daemon.socket` is an alias of `nix-daemon.socket`, and the system closure contains no CppNix.
3. `noxi system switch --flake /etc/nixos#noxios-second` builds the second generation's `/etc`, activation script and toplevel inside the installed system and switches to it.
4. After a reboot, the booted system is the second generation.

The installed system uses `nixosModules.noxios` and the noxios branding, which `nix/noxios/default.nix` exports as `modules`, so noxi stays its package manager.

The image holds the installed system's toplevel. The toplevel and the trivial derivations it is assembled from (`system-path`, `system-units`, the `/etc` entries, the unit files) set `allowSubstitutes = false`. Both nixos-install and `noxi install` therefore build them, even though their outputs are in the image. Nixpkgs' installer test puts the build inputs of those derivations in the installer's store. For this configuration, that list did not cover:

- the toplevel's own activation scripts
- the wrapper check, which needs a Go-built verifier
- several environments

So the test passes `--option always-allow-substitutes true` to `noxi install`, and the install copies the system whole from the image's store through `auto?trusted=1`.

The second generation is built for real. The installed system carries two sets of build inputs in `system.extraDependencies`:

- those of the second generation's `/etc` derivation
- those of its own toplevel, which both generations share

noxi then builds exactly `/etc`, `activate` and the toplevel offline. A host-side walk of the derivation graph, which mirrors `queryMissing`, showed that set before the VM ran.

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

[Scoped application profiles](scoped-application-builds.md) have independent declarations and generations. Eligible user or machine application changes build and switch only their profile, leaving the running OS generation and boot default unchanged. Application rollback, system rollback, and application-data recovery remain separate operations. Machine profile visibility requires qualified base OS integration.

## Updates and recovery

Each release must define how it receives Nixpkgs, noxi, kernel, and installer fixes. Pin the inputs used to build a generation. Test upgrades from the preceding supported release and a return to a known-good boot entry. Do not change `system.stateVersion` merely to select newer packages; retain it according to NixOS migration guidance.

Recovery media must be able to inspect and repair an installed system when the current generation cannot boot or the primary daemon cannot start. Recovery must be tested with a broken configuration, failed activation, damaged boot default, and a failed package-manager update. Reverting an OS generation does not undo changes to mutable application data; release notes must name migrations that need a separate backup or reversal.

Fleet updates preserve these generation semantics per machine and add topology-aware batching, canaries, workload drain, quorum and primary availability checks, and post-reboot service verification. A completed activation is not sufficient to declare the cluster healthy. Partial rollouts retain exact per-machine states and a resumable journal; they cannot report a fleet-wide atomic switch. Reject rollback to software incompatible with already migrated persistent data and offer the qualified service recovery path instead. [Fleet rollout and recovery](distributed-computing.md#fleet-plan-rollout-and-recovery)

The initial OS profile must retain NixOS switching support. The NixOS `image-based-appliance` profile disables `switch-to-configuration`; it does not satisfy this product's self-maintaining system requirement without a different, fully specified update model. [NixOS manual](https://nixos.org/manual/nixos/stable/)

## Privilege and trust

The TUI and GUI run without root privileges. A narrowly scoped privileged module authorizes store and system mutations and applies the same trust model used by compatibility commands. Do not accept arbitrary shell commands from an interface client as a substitute for typed operations. Untrusted builders must remain sandboxed; binary-cache signatures and substituter trust decisions must match the reference behavior in compatibility mode.

Only one daemon may own writes to a store at a time. System activation and store migration need exclusive operations where required. Log authorization failures without exposing credentials. Generated Nix store objects are public by default in common deployments, so credentials must stay out of generated configuration and build inputs.

Remote mutations additionally require authenticated target identity, scoped authority, replay protection, durable operation identifiers, and stale-coordinator fencing. Local activation serialization, protected cross-machine topology transitions, and availability reservations across concurrent plans are separate requirements. Routine fleet operations must use typed authorized steps, not unrestricted remote-root shell execution. Machine enrollment, cluster credentials, cache trust, and recovery authority must remain distinct permissions.
