# Scoped application builds

## Planned capability

Users must be able to add, update, or remove an application in a selected scope without building or activating a complete NixOS system generation. The planner evaluates an independent application declaration and realizes its package closure and profile. It reuses existing store paths and trusted cache substitutes; local builds cover missing dependencies and the affected profile output. A scoped build can still build dependencies, including shared libraries.

This is a planned native extension after the package-manager MVP. Reference-compatible profiles, builds, shells, and NixOS activation remain in the unchanged compatibility gate. It uses the nix-rs engine and NixBox's shared operations and is not implemented support.

## Scopes

| Selected scope | Build and apply target | Authority and visibility |
|---|---|---|
| User applications | Independent application profile for one user | Owned by that user; exposes tools and supported desktop entries to their sessions |
| Machine applications | Independent application profile shared by users on one machine | Requires administrator authority; exposes tools and supported desktop entries through preconfigured OS integration |
| Project applications | Selected project's tool closure | Owned by the user; visible inside its [native environment](development-environments.md), with that environment's lock and session rules |
| NixOS configuration | `system.build.toplevel` and the requested system activation action | Existing full system-generation workflow for declarations in NixOS modules |

- Default new application requests to the current user's application scope. Show the scope and target before apply; never silently change them.
- Keep application declarations and locked inputs independent of system configuration. Default new native declarations to Lua and also accept Nix. Inputs may use the same pinned revision without evaluating the NixOS module graph.
- Machine scope needs initial OS integration for the stable profile path, executable lookup, and desktop discovery. Enabling that integration can require a system generation; subsequent eligible application changes update only the application profile. Report missing integration instead of silently rebuilding.
- Detect duplicate packages, executable collisions, and precedence across scopes. Removal affects only the owning declaration; report when another scope still supplies the application.

## Eligibility and system changes

An eligible change affects only package selections and supported profile integration. New services, users, groups, privileged wrappers, udev rules, kernel modules, firewall policy, boot files, or arbitrary activation scripts require a separately reviewed system plan. Installing an application's package does not enable its NixOS service or program module.

Packages declared in `environment.systemPackages`, NixOS user package options, or Home Manager keep their existing activation semantics. Building one derivation does not apply those configuration edits. Moving a package to an independent profile is an explicit migration with source and availability checks. NixOS already distinguishes system configuration packages from user-installed packages. [NixOS package management](https://nixos.org/manual/nixos/stable/#sec-package-management)

Ordinary NixOS rebuilds reuse unchanged store paths. This feature avoids constructing and activating a system generation for an eligible application change; it does not claim an ordinary rebuild recompiles every package. Building a system component separately is distinct from activating it. [Building specific parts of NixOS](https://nixos.org/manual/nixos/stable/#sec-building-parts)

## Plan, build, and apply

1. Record scope, owner, source and lock revisions, active profile generation, and package identity. Serialize mutations of that profile and check target authority.
2. Stage and evaluate the candidate application profile. Preview eligibility, file diff, package and dependency changes, reused paths, downloads, builds, collisions, and expected profile generation. Keep unrelated packages pinned unless an input update was requested.
3. Realize only the candidate profile closure through nix-rs. Failed evaluation or builds leave the active profile and committed declaration unchanged. Root the candidate while the operation is pending.
4. Recheck source, lock, and active profile revisions. Journal the owned source replacement, reevaluate the saved declaration, and verify it produces the built profile. Reject stale or mismatched results before switching.
5. Switch the profile generation and refresh supported application discovery. Application-only apply must not invoke `switch-to-configuration`, change the system profile or bootloader, or restart unrelated services.
6. Report saved declaration, built closure, active application generation, and discovery refresh failures separately. Record the unchanged running OS generation and boot default. Recover interrupted operations without erasing external edits.

Provide build-only, save-draft, apply, and rollback operations. Build-only does not save declarations or switch profiles. Rollback selects a retained application generation in the same scope and offers explicit source restoration. OS generations and mutable application data have separate rollback procedures. Retain active and rollback profiles as GC roots until the selected retention policy allows collection. Machine profiles must survive reboot, and ordinary system rebuilds must preserve independent application declarations and generations.

CLI, TUI, and GUI share typed scope, frozen plans, events, and results. Final command spelling, declaration schema, profile paths, precedence, and desktop refresh mechanisms remain implementation decisions. Legacy profile, shell, and rebuild commands retain reference behavior.

## Example and acceptance cases

A user adds `ripgrep` to their user application scope. NixBox resolves it against that scope's locked Nixpkgs, stages its declaration, realizes missing paths, saves and verifies the declaration, and switches the user application profile. The user can run `rg` after apply. The operation creates an application generation; the running OS generation and boot default stay unchanged. Machine scope additionally requires administrator authority and shared profile integration.

Before advertising support, qualify:

- User and machine add, update, remove, build-only, and rollback; project operations follow the existing environment contract.
- Store reuse, cache hits, dependency builds, pinned unrelated packages, collisions, and packages supplied by multiple scopes.
- Trace evidence that eligible operations never target `system.build.toplevel` or invoke system activation, with actual OS and boot generations recorded before and after.
- Denied authority, stale source/lock/profile revisions, failed builds, interrupted writes, failed profile switches, discovery refresh failure, and journal recovery.
- Client parity, session visibility, desktop discovery on supported desktops, reboot persistence, subsequent system rebuilds, and GC retention.
- Required system or Home Manager activation, missing machine integration, and explicit migration from existing declarations. Repeat affected compatibility tests with native declarations present.

Publish the qualified scope/platform matrix and measured evaluation, download, build, and apply times. Comparisons with system-generation operations must use the same locked application and store/cache conditions.
