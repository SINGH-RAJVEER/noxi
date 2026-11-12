# Native system commands

`noxi system` builds a NixOS system toplevel and applies it with the [NixOS action meanings](system.md#system-generations): `build`, `test`, `boot`, `switch`, and `dry-activate`. `noxi system rollback` returns the system profile to an earlier generation. Both commands evaluate and build through the in-process compatibility implementation. They do not start the `nix`, `nix-build`, or `nix-env` executables. Each run is recorded in the [snapshot journal](snapshots.md).

The implementation is `crates/noxi-system/src/rebuild.rs`, and the `noxi` binary dispatches to it.

```
noxi system build|test|boot|switch|dry-activate [--flake PATH[#HOST] | --file PATH [--attr ATTR]] [--update [INPUT...]] [--profile-name NAME | --profile PATH] [--install-bootloader] [--no-snapshot] [--json] [--out-link PATH]
noxi system rollback [--to GENERATION] [--profile-name NAME | --profile PATH] [--no-snapshot] [--json]
```

## Mapping to nixos-rebuild

The behavior follows `nixos-rebuild-ng` 26.11. Its `services.py`, `nix.py` and `models.py` were the reference.

| Step | nixos-rebuild | noxi system |
|---|---|---|
| Default source | `--flake /etc/nixos` when `/etc/nixos/flake.nix` exists (its symbolic link resolved), else the classic path | The same. The classic fallback is always `<nixpkgs/nixos>`. |
| Flake attribute | `PATH#nixosConfigurations."HOST".config.system.build.toplevel`, where HOST defaults to the host name, then `default` | The same |
| File attribute | `nix-build FILE --attr ATTR.config.system.build.toplevel`, or `config.system.build.toplevel` without `--attr`. A directory means its `system.nix`, else its `default.nix` | The same, evaluated and built in-process |
| Build | `nix build --print-out-paths` or `nix-build`, with a `./result` link only for `build` | The result is read from a link. For `build` the link is `./result` or `--out-link`. Otherwise the link is a temporary one, removed after the run. |
| Profile | `--profile-name NAME` selects `/nix/var/nix/profiles/system-profiles/NAME` | The same. `--profile PATH` and `NOXI_SYSTEM_PROFILE` also select an explicit path. |
| `switch`, `boot` | `nix-env -p PROFILE --set TOPLEVEL`, then `TOPLEVEL/bin/switch-to-configuration ACTION` | The same steps, in-process, under the profile lock |
| `test`, `dry-activate` | `switch-to-configuration ACTION` only | The same |
| Bootloader | `NIXOS_INSTALL_BOOTLOADER` is `1` with `--install-bootloader`, else `0` | The same |
| Activation output | Sent to standard error | The same |
| Rollback | `nixos-rebuild switch --rollback` runs `nix-env --rollback`, then `switch-to-configuration switch` | `noxi system rollback`, plus `--to N` |

These are the exact differences:

- noxi does not wrap activation in `systemd-run`. nixos-rebuild does this when `/run/systemd/system` exists, so that activation survives a lost terminal. noxi runs `switch-to-configuration` as a direct child.
- noxi does not run nixos-rebuild's check that `TOPLEVEL/nixos-version` exists before `--set`. Instead, `noxi system` requires the activation program to exist before it changes the profile.
- noxi does not elevate privileges. An unprivileged `test`, `boot`, `switch`, or `rollback` is refused before it reads the profile (see below). nixos-rebuild offers `--sudo` for this.
- noxi does not use the classic fallbacks `system.nix` in the current directory, `<nixos-system>`, or `/etc/nixos/system.nix`. Without `--flake` or `--file`, the only fallback is `<nixpkgs/nixos>`. `--attr` requires `--file`.
- `rollback` is a separate subcommand, and it always activates with `switch`. nixos-rebuild also has `test --rollback` and `boot --rollback`.
- Unsupported nixos-rebuild options: `--upgrade`, `--upgrade-all`, `--target-host`, `--build-host`, `--specialisation`, `--store-path`, `build-vm`, `build-image`, `repl`, `edit`, `list-generations`, and pass-through Nix flags.
- A switch whose closure is already the newest generation reuses that generation instead of creating a new one, as `nix-env --set` does.

## Flags

| Flag | Meaning |
|---|---|
| `--flake PATH[#HOST]` | Build `nixosConfigurations."HOST"` of the flake. A local directory, `path:` or `git+file://` reference has its `flake.lock` reported and recorded. Any other reference is passed through unchanged. |
| `--file PATH` | Build from a Nix file, a directory, or `<lookup-path>`. |
| `--attr ATTR` | Build `ATTR.config.system.build.toplevel` of `--file`. |
| `--update [INPUT...]` | Before evaluation, re-lock the named inputs of a local flake, or all inputs when none are named. This uses [`noxi flake update`](flakes.md#native-noxi-flake-update). A failed update stops the run before any build. |
| `--profile-name NAME` | Use `/nix/var/nix/profiles/system-profiles/NAME`. `system` names the default profile. |
| `--profile PATH` | Use an explicit profile path. |
| `--install-bootloader` | Run the activation program with `NIXOS_INSTALL_BOOTLOADER=1`. |
| `--no-snapshot` | Write no journal record, and say so. |
| `--json` | Print one JSON object on standard output instead of the text report. |
| `--out-link PATH` | `build` only. The result link; `./result` by default. |
| `--to GENERATION` | `rollback` only. The generation to select; by default, the newest one older than the current. |

## Phases and the final report

Phase lines go to standard error, one line per phase, in the order given in [docs/logging.md](logging.md#rebuild-phases-and-visible-progress):

| Phase | Line |
|---|---|
| `resolve` | The action, the source kind, the reference and attribute, the lock path with its sha256 (or "not present yet"), and the source revision |
| `update` | Only with `--update`. The lock path and the change counts, followed by the change report indented by two spaces. |
| `evaluate` | The attribute and source being evaluated |
| `build` | The realized toplevel path |
| `prepare activation` | The profile, its current generation, the proposed generation (for `switch` and `boot`) and the running system. For `build`, this says the phase is skipped. |
| `activate` | The activation program and action that are running. For `build`, "no activation occurred". |
| `verify` | Whether the running system and the boot default changed |

The compatibility implementation's own output, such as "this derivation will be built", and the activation program's output appear between these lines. There are no percentages and no spinners. Notes such as `note: --no-snapshot ...` and warnings such as `warning: cannot write the snapshot journal ...` are separate lines.

The final report goes to standard output. It lists:

- the action and the outcome (with the failed phase)
- the built closure and, for `build`, the out link
- the profile
- `running system:` and `boot default:`, each marked `changed` or `unchanged`, with the before and after paths and their generation numbers when a generation of the profile points at them
- the current and previous generation
- the snapshot id
- a `rollback:` command

The rollback command is `noxi system rollback --to N`, followed by `noxi snapshot rollback ID` when a successful snapshot recorded generation N. After `test`, which changes only the running system, the report names `PROFILE/bin/switch-to-configuration switch` or a reboot instead. All of these values are read from the filesystem after the run. They are not inferred from the action. A successful closure alone is never reported as a successful `switch`.

The action-specific notes are:

- `build` stops after realization, and no activation occurred.
- `boot` leaves the running system unchanged until the next boot.
- `test` leaves the boot default unchanged.
- For `dry-activate`, the intended changes are listed by the activation program, and none were applied.

The boot default is the target of the profile link, that is, the profile's current generation. `switch-to-configuration boot` makes that generation the bootloader default. noxi does not read the bootloader configuration itself.

### Verification checks

After activation, the run fails in the `verify` phase if one of these checks does not hold:

- After `switch` and `boot`, the profile points at the new closure.
- After `switch` and `test`, the running-system link points at the new closure.
- After `rollback`, the profile is at the selected generation and the running-system link points at its closure.

## Failures

An error prints one block on standard error:

```
error: PHASE: what failed; the profile ... (boot default changed|unchanged); the running system link is changed|unchanged
hint: one next step
```

The state sentence is written after the profile and running-system links are read again. If `switch-to-configuration` exits non-zero after `switch` or `boot` moved the profile, the error says that the profile now points at the new generation, and whether the running-system link changed. Activation may have applied some of its changes, and the error says so. The hint names `noxi system rollback`.

| Phase | Typical cause |
|---|---|
| `resolve` | A missing flake directory or `flake.nix`, a missing file, `--update` with a non-local flake, or for rollback a missing or already current generation or a profile with only one generation |
| `update` | The lock operation failed |
| `evaluate` | The `--file` expression failed to evaluate, or did not produce exactly one derivation |
| `build` | Realization failed. In flake mode, evaluation and build run as one `nix build` call, so an evaluation error there is also reported as `build`. |
| `prepare activation` | The root check refused the run, the snapshot journal is unwritable (for `test`, `boot`, `switch`, `rollback`), the activation program is missing, or the profile could not be switched |
| `activate` | The activation program exited non-zero or could not be started |
| `verify` | A verification check failed, or a mutating run's snapshot record could not be finished |

## JSON output

`--json` prints one object:

```json
{
	"action": "switch",
	"source": { "kind": "flake", "path": "/etc/nixos", "attribute": "nixosConfigurations.\"host\".config.system.build.toplevel", "revision": "...", "dirty": false },
	"lock": { "path": "/etc/nixos/flake.lock", "exists": true, "sha256": "..." },
	"update": null,
	"closure": "/nix/store/...-nixos-system-host",
	"out_link": null,
	"profile": "/nix/var/nix/profiles/system",
	"current_system": "/run/current-system",
	"generation": { "before": 41, "after": 42 },
	"running": { "before": "/nix/store/...", "after": "/nix/store/..." },
	"boot": { "before": "/nix/store/...", "after": "/nix/store/..." },
	"snapshot": "0007",
	"outcome": { "status": "success", "phase": "verify", "message": null },
	"rollback": "noxi system rollback --to 41"
}
```

Field notes:

- `lock` is null for `--file` and for non-local flake references.
- `update` is the [`noxi flake update --json` report](flakes.md#report), or null.
- `status` is `success`, `failed`, or `not-applied`. As in the snapshot journal, a success records `verify` as its phase.
- For a refused or unresolved run, the state pairs are null.
- For `rollback`, `source` is the source recorded by the snapshot that created the selected generation, or null when no snapshot did.

## Exit status

The exit status is:

- `0` when every phase succeeded
- `1` when any phase failed, including a refusal
- `2` for a usage error

## Privilege and diversion

`test`, `boot`, `switch`, and `rollback` change the running system or its profile. When the effective uid is not 0, they are refused before anything else happens. In particular, the refusal comes before the profile is read and before the journal is opened, unless the run is diverted:

- For `test`, the activation program must be overridden.
- For `boot`, `switch`, and `rollback`, both the activation program and the profile must be overridden.

`build` and `dry-activate` are never refused. noxi does not run sudo itself.

The environment overrides are:

| Variable | Overrides |
|---|---|
| `NOXI_SYSTEM_PROFILE` | The profile path, unless `--profile` or `--profile-name` is given |
| `NOXI_CURRENT_SYSTEM` | The running-system link, `/run/current-system` by default |
| `NOXI_ACTIVATION_PROGRAM` | The program run instead of `TOPLEVEL/bin/switch-to-configuration` (for rollback, instead of `GENERATION/bin/switch-to-configuration`) |
| `NOXI_SNAPSHOT_DIR` | The snapshot journal directory |

The store, substituters, and other Nix settings come from the usual configuration and `NIX_CONFIG`, as for the compatibility commands. Flake builds also enable `nix-command flakes`.

## Snapshots

After resolve, a run begins a journal record with:

- the action
- the source kind, path, and attribute
- the lock path
- the profile
- the running-system link

Every later exit path finishes the record with the closure (null when the build failed) and the outcome: success, a failure with its phase, or not-applied for a refusal after the record began.

For flakes, the lock is read again when the record is finished. The record therefore keeps the lock the closure was built from, including one written by `--update` or by the build itself.

A journal that cannot be opened or written is handled by action:

- For `build` and `dry-activate`, it is a warning, and the run continues without a record. An unprivileged user without `NOXI_SNAPSHOT_DIR` is the usual case.
- For `test`, `boot`, `switch`, and `rollback`, it is a failure in the prepare-activation phase, reported after the build and before anything is changed.

`--no-snapshot` skips the journal and says so.

`noxi system rollback` uses the profile generations directly, so it works for generations that no snapshot recorded. It prints the id of the snapshot that recorded the selected generation, when there is one, and writes a new `rollback` record.

`noxi snapshot rollback ID` remains the way to roll back to a recorded snapshot and optionally restore its lock.

## Library API

`noxi_system::rebuild` exposes the following:

| Item | Purpose |
|---|---|
| `parse(args, env)` | Builds an `Invocation` from the arguments, reading the variables above through the `env` function. |
| `execute`, `run`, `main` | Run an invocation. `run` and `main` read the process environment. |
| `rebuild(RebuildOptions, out, err)` | Runs one rebuild. |
| `rollback(RollbackRequest, out, err)` | Runs one rollback. |
| `RebuildOptions` | Carries the injectable pieces: the `builder`, `activation_program`, `profile`, `current_system`, `journal`, `euid`, and `hostname`. |
| `Builder` | The build trait. `build(&BuildRequest)` returns the toplevel path or a `BuildError` naming the evaluate or build phase. `register_generation` creates the profile generation. |
| `CompatBuilder` | The default `Builder`. It runs the compatibility implementation on a 64 MiB stack thread, one build at a time per process. Its `register_generation` uses `noxi_store::profiles::create_generation`, which also makes the generation a garbage-collector root, when the closure is a store path. |
| `link_generation` | Creates a plain generation link, numbered the same way, for any other path. It is also the default `register_generation`. |

Any `Fn(&BuildRequest) -> Result<PathBuf, BuildError>` is a `Builder`.

## Verification

`crates/noxi-system/tests/rebuild.rs` runs the commands in-process with an injected builder. Its fixture closures have a `switch-to-configuration` shell script that logs its action and `NIXOS_INSTALL_BOOTLOADER`. For `switch` and `test`, the script moves the running-system link as the real program does. The profile, link, and journal are in a temporary directory.

The tests cover:

- `build` with its out-link and no activation
- `switch`, `boot`, `test`, and `dry-activate`, with their generation, profile, and running-system effects and records
- `--install-bootloader` and `--no-snapshot`
- an activation failure and a build failure
- a closure without an activation program
- the non-root refusal without diversion, and acceptance with full diversion
- rollback to the previous generation and with `--to`, including a missing or already current target and a profile with one generation
- the JSON shape
- flake host attribute selection
- usage errors

`crates/noxi-system/tests/rebuild_e2e.rs` builds for real through `CompatBuilder`, in a diverted `local?root=` store with no substituters. The toplevel is a `builtin:buildenv` derivation linking a directory that contains `bin/switch-to-configuration`. One test uses `--file` with `--attr`. The other uses `--flake PATH#host` with `--update` of a `path:` input, and checks that the snapshot kept the lock written by the update. The `builtin:buildenv` builder is used because a `/bin/sh` builder cannot run in a diverted store without the shell's closure in that store.

## Not yet qualified

The following are not verified, or not implemented:

- No VM qualification. No test has activated a real NixOS toplevel, switched a real system profile, or run a real `switch-to-configuration`. Every activation in the tests is a fixture script.
- `CompatBuilder::register_generation` through a real store (`create_generation` with a garbage-collector root) is not exercised by tests. The tests use the plain-link path.
- `test`, `boot`, `switch`, and `rollback` were never run against the real `/nix/var/nix/profiles/system` or `/run/current-system`.
- With a diverted `local?root=` store, the closure path names the logical `/nix/store` location. Activation of such a closure therefore needs `NOXI_ACTIVATION_PROGRAM`.
- There are no remote targets, build hosts, or `--sudo`.
- There is no `--upgrade` or channel update path.
- There is no Home Manager integration, and no specialisations.
- Activation does not run under `systemd-run`.
- The `nixos-version` sanity check is not performed.
- In flake mode, an evaluation failure is reported as phase `build`.
- The output is not the structured event stream of [docs/logging.md](logging.md#structured-event-contract). Phase lines are plain text, and there is no in-place terminal status area.
- Stale-preview rejection and serialization against competing activations, beyond the profile lock held while the generation is created, are not implemented.
