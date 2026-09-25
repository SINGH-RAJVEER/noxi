# Declarative configuration workflow

## Decision

The Nix configuration is the source of truth for persistent system state. The CLI, TUI, and GUI submit typed requests to NixBox. NixBox resolves them through the nix-rs evaluator and store, computes the candidate system generation, and persists the accepted desired state as Nix source before activation. A persistent change is therefore a semantic engine operation with declarative persistence, not a text substitution followed by a shell command. The interface must never make a lasting runtime change first and try to reverse-engineer Nix files afterward.

This matches NixOS's documented model: edit the configuration, run a rebuild, and select the resulting generation. Module definitions can merge, override, or derive values from other definitions, so observing a running system is not enough to reconstruct the expression that produced it. [NixOS configuration and generations](https://nixos.org/manual/nixos/stable/), [NixOS module definitions](https://nixos.org/manual/nixos/stable/)

The approach is hybrid in the interface, not in the source of truth. A form, source editor, CLI command, and package search can all propose the same typed change. NixBox evaluates it against the exact locked inputs, drives store and build work through nix-rs, and records persistent intent in an imported Nix module. The system still follows its committed configuration. [NixBox integration](nixbox-integration.md) defines this lower-level role.

## Ownership and round trips

NixBox owns one explicitly imported module, provisionally `nixrs-managed.nix`. It renders that module deterministically from evaluated package selections and typed option values after constructing a candidate plan. It may also update a lock file in a separate, explicit input-update operation. The generated file records desired state; it does not replace package resolution, module evaluation, store realization, or activation. NixBox must not rewrite an arbitrary user module to make a toggle work.

For every option, the interface shows the effective value, its contributing definitions and source locations when available, and whether the editor owns a writable definition. A value computed by a function, conflicting with another module, or requiring a complex Nix expression can be inspected and edited in source. The UI must not silently insert `mkForce` or claim a change succeeded just because the generated file contains the requested value. Reevaluate and show the effective result.

The interface must tolerate edits made outside it. On every apply, compare the configuration and lock revision with the revision originally read. If either changed, show the new diff and require a fresh preview. It must not overwrite handwritten edits. A user may use any editor or compatible `nixos-rebuild` command; on reopening, the interface reads the current source and current generation rather than trusting its own cached model.

## Save draft and apply

The interface has distinct operations:

- **Save draft** writes a proposed configuration change without building or activating it. It clearly marks the system as still running an older generation.
- **Apply** resolves a typed request with the nix-rs engine, stages its Nix representation, validates and builds the resulting closure, saves the source change, then performs the selected activation action. The final report distinguishes source saved, closure built, running generation, and boot default.
- **Discard draft** removes only application-owned pending edits after checking that no one else changed them.
- **Rollback** selects a prior system generation and, if requested, restores its recorded configuration revision. Rolling back only the running generation must not silently rewrite the source files.

An apply operation follows this sequence:

1. Read and record the source tree, lock file, application-owned module, running generation, and boot default. Acquire an operation lock for the target machine.
2. Resolve the typed request against the locked package universe and evaluated module definitions. Render its declarative representation in an isolated staging area that preserves relative imports and the selected inputs. Present the file diff, effective option changes, resulting derivation graph, and expected store work. Abort on evaluation errors.
3. Realize the candidate system closure through the nix-rs store and build interfaces. Keep the committed source and running system untouched if the build fails. A previewed service effect is an estimate until activation runs.
4. Recheck the base source and lock revision. Save the application-owned file through a journaled replacement and record the prior content. If multiple files change, recover from any interrupted write rather than assuming the filesystem made the whole group atomic.
5. Evaluate the committed files again. Confirm that they produce the closure that was built. If the result differs because staging altered path or import behavior, stop before activation, report the mismatch, and restore the prior application-owned files when no external edit prevents it.
6. Perform the requested activation action. Preserve the prior bootable generation. Report actual running and boot states, including partial activation or failure.

No filesystem transaction can make a source edit, store build, bootloader update, and service restart atomic as one operation. The system therefore keeps a journal and reports partial outcomes honestly. If activation fails after the source change, show that the desired configuration and running system diverge, offer retry and rollback actions, and never erase external edits during recovery.

## Direct runtime actions

Some operations are intentionally temporary: restart a service, stop a failed unit, inspect logs, enter rescue mode, or test a built generation without changing the boot default. These are explicit runtime actions, not configuration edits. The UI labels their effect and warns when a later rebuild may undo it. It never records a runtime action as a persistent option change unless the Nix source is updated and a generation is built.

Compatibility commands may retain their existing imperative behavior, such as operations on user profiles. The OS-native persistent system workflow uses the configuration-first path. The interface must show when packages come from the system configuration, a user profile, or another source, so a package installed through one scope is not falsely presented as part of another.

## Example

A user enables OpenSSH in the GUI. The GUI submits a typed option change to NixBox. NixBox asks the nix-rs evaluator for the effective option and contributing definitions, adds `services.openssh.enable = true;` to a staged application-owned module, and computes and builds the resulting system closure through the engine. After the source revision is rechecked, the manager saves the module and switches to the new generation. The result shows the new running and boot generations and whether `sshd` started. If evaluation reports a conflict or activation fails, the UI names that phase and displays what changed. It never calls `systemctl enable sshd` and later guesses how to encode that state in Nix.

## Acceptance cases

Test a normal package or option edit; an imported conflicting definition; a computed read-only value; an external source edit during preview; a failed evaluation; a failed build; an interrupted multi-file write; staging and final-source evaluation producing different closures; a failed service activation; a temporary runtime restart; and rollback of generation without source rollback. For every case, assert the source revision, built closure, running generation, boot default, and user-facing diagnostic.
