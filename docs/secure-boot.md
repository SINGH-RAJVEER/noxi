# OS installer boot and Secure Boot specification

This specification applies to the later noxi OS image and installer. The repository does not yet build that image or implement either boot path. The installer must present two explicit UEFI choices: ordinary systemd-boot, or Secure Boot with Lanzaboote. Selecting systemd-boot alone must never be reported as enabling Secure Boot.

## Verified upstream behavior

NixOS recommends `boot.loader.systemd-boot.enable = true` for UEFI installations. Its EFI module separately controls whether installation may write firmware boot variables through `boot.loader.efi.canTouchEfiVariables`; that option defaults to `false`. The installer must detect the ESP mount point and preserve the choice in the generated configuration. [NixOS manual](https://nixos.org/manual/nixos/stable/), [NixOS EFI options](https://nixos.org/manual/nixos/unstable/options)

Lanzaboote's NixOS module replaces the ordinary systemd-boot module. Its documented configuration imports `lanzaboote.nixosModules.lanzaboote`, forces `boot.loader.systemd-boot.enable = false`, sets `boot.lanzaboote.enable = true`, and points `boot.lanzaboote.pkiBundle` to `/var/lib/sbctl`. Lanzaboote's `lzbt` signs boot artifacts and installs bootable generations on the EFI System Partition. [Lanzaboote preparation guide](https://github.com/nix-community/lanzaboote/blob/master/docs/getting-started/prepare-your-system.md), [Lanzaboote project overview](https://github.com/nix-community/lanzaboote)

The Lanzaboote guide uses `sbctl create-keys` to make local signing keys, then `sbctl verify` to inspect the signed EFI artifacts. Firmware must enter Secure Boot Setup Mode before new keys can be enrolled. The guide enrolls keys with `sbctl enroll-keys --microsoft` to retain compatibility with hardware that needs Microsoft-signed option ROMs. It warns against indiscriminately clearing every Secure Boot database, which can erase the revocation database `dbx`. Firmware menus and enrollment steps vary by device. After enrollment and reboot, `bootctl status` can report whether Secure Boot is enabled in user mode. [Lanzaboote preparation guide](https://github.com/nix-community/lanzaboote/blob/master/docs/getting-started/prepare-your-system.md), [Lanzaboote enrollment guide](https://github.com/nix-community/lanzaboote/blob/master/docs/getting-started/enable-secure-boot.md)

Lanzaboote also discusses measured boot. TPM measurements, disk encryption, and recovery credentials are separate concerns from UEFI signature enforcement. Enabling Secure Boot alone does not enroll a LUKS key in a TPM or establish a complete protection policy for the installed system. [Lanzaboote project overview](https://github.com/nix-community/lanzaboote)

## Installer design requirements

The boot selection screen must state the result of each choice in plain language:

- `systemd-boot`: install the ordinary NixOS UEFI boot manager and generation entries. Leave Secure Boot setup to a separate, later action. Generate a persistent configuration module that enables `boot.loader.systemd-boot.enable` and states an explicit EFI variable policy.
- `Lanzaboote Secure Boot`: install the Lanzaboote module, create or import a machine-owned signing key bundle, sign the selected boot generation, and guide the user through firmware enrollment. Generate a persistent configuration module that disables the ordinary systemd-boot module and enables `boot.lanzaboote` with the selected key location.

Both choices require UEFI boot and a writable EFI System Partition. Before showing the Secure Boot choice as available, the installer must check architecture and firmware support against the pinned Lanzaboote release, whether the ESP can hold at least the required bootable generations, whether signing keys can be stored persistently, and whether a recovery boot path exists. A failed preflight must explain which requirement failed and offer the ordinary systemd-boot path. These are product requirements, not claims that Lanzaboote currently performs every preflight.

The installer records one of these states and shows it at the end of installation:

| State | Meaning | Required next action |
|---|---|---|
| Ordinary boot installed | systemd-boot entries are installed; this choice made no Secure Boot claim | Boot and verify the installed generation |
| Secure Boot pending enrollment | Lanzaboote signed the installed entry, but firmware has not confirmed enforcement | Follow the device-specific enrollment flow, reboot, and verify enforcement |
| Secure Boot verified | The installed signed generation booted with firmware enforcement enabled | Preserve signing keys and a tested recovery route for later updates |
| Boot setup failed | Signing, ESP installation, enrollment, or verification failed | Report the failed step and restore a known bootable entry; never label this state secure |

For the Lanzaboote path, the installer must show each operation before it runs: key creation or import; where private keys will remain; generated configuration; signed artifacts; any firmware variable write; and key enrollment. It must never put private keys in the Nix store, installation logs, or a world-readable generated configuration. It must require an explicit user action before changing firmware trust keys. A user who has Windows, another OS, BitLocker, or vendor firmware updates must see the enrollment choice and its consequences before the installer proceeds. The exact vendor-key policy needs hardware-specific qualification; `--microsoft` is a documented route, not a universal command to run silently.

Enrollment may be completed during installation when the firmware exposes a safe Setup Mode path, the signed installed boot entry has been verified, and recovery media remains available. Otherwise the installer must finish in a clearly labeled pending state and provide a first-boot enrollment flow. It must not claim Secure Boot is active merely because signing succeeded or keys were generated. The final state must be checked after reboot with firmware status, `sbctl` verification, and a boot of the installed generation. The installer must preserve a usable fallback entry until the protected boot path has passed that check.

The system configuration must retain the selected boot mode across rebuilds and upgrades. Each boot-changing update must sign or install the new entry, verify it, keep a prior known-good bootable generation, and report failures without marking the switch complete. Recovery media must be able to restore the boot entry or re-enroll keys without needing a working noxi daemon on the installed system.

Changing from ordinary systemd-boot to Lanzaboote after installation, rotating keys, or returning to the ordinary path must be explicit maintenance operations. Each needs a preview of firmware and ESP changes, a verified new boot entry, a retained recovery path, and a post-reboot check before the old entry or key is removed. The first OS release may leave these transitions manual; its installer documentation must say so if it does.

TPM-bound disk unlocking belongs in a later, separate installer choice. If added, it must display the PCR policy, recovery credential, and effect of firmware, bootloader, or kernel updates before enrollment. Neither boot choice may imply that disk encryption or TPM sealing is enabled.

## Qualification before release

The OS release gate needs isolated UEFI tests for both choices: initial installation, first and second boot, a new signed generation, rollback, a signing failure, an ESP with insufficient space, and recovery from a broken default entry. Secure Boot tests must additionally exercise Setup Mode, key enrollment, enforcement after reboot, rejected unsigned EFI artifacts, and key-loss recovery. Hardware validation must cover at least the documented target devices and distinguish firmware behavior from the generic installer contract. Until those tests and the underlying noxi rebuild/store work pass, this document remains a design target.
