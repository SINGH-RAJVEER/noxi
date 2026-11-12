# System snapshot journal

A snapshot is the durable record that ties a NixOS system generation to the configuration source and flake lock it was built from, together with the requested action, the actor, and the observed outcome. It is the audit record that [system generations](system.md#system-generations) ask for, and it is what lets [rollback](configuration-workflow.md#save-draft-and-apply) restore a recorded configuration revision as well as select a prior generation.

The journal is implemented in `crates/noxi-system/src/snapshot.rs` and exposed as `noxi snapshot`.

## Why generations alone are insufficient

A profile generation is a symlink `system-N-link` to a store path, plus the link's modification time. It does not say which source tree, which revision of that tree, or which flake lock produced the closure, who asked for it, which action was requested, or whether activation succeeded. A failed build or activation leaves no generation at all, or leaves one that looks identical to a good one. The snapshot journal keeps those facts beside the generation number so that a rollback can be checked against them and the lock can be put back.

## What a snapshot records

Each record is a JSON object with `schema_version: 1` and these fields:

| Field | Meaning |
|---|---|
| `id` | Zero-padded, monotonically increasing integer such as `0001`. |
| `created_at`, `finished_at` | RFC 3339 UTC timestamps with second precision. `finished_at` is null until the operation finishes. |
| `action` | One of `build`, `test`, `boot`, `switch`, `dry-activate`, `rollback`. |
| `actor` | `user` (from the password database, else `$USER`), `uid`, and `hostname`. |
| `source` | `kind` (`flake` or `file`), `path` (local path or flake reference), `attribute` (flake fragment or `--attr`), `revision`, and `dirty`. |
| `lock` | `{ path, sha256 }` of the flake lock, or null when there is none. |
| `closure` | The system toplevel store path, or null if evaluation or build failed. |
| `profile` | The profile the generation belongs to, normally `/nix/var/nix/profiles/system`. |
| `generation` | `{ before, after }`: the profile's current generation number before and after, or null. |
| `running_before`, `running_after` | The target of `/run/current-system`, or null if absent. |
| `boot_before`, `boot_after` | The target of the profile link. |
| `outcome` | `status` (`success`, `failed`, `not-applied`), `phase` (`resolve`, `update`, `evaluate`, `build`, `prepare-activation`, `activate`, `verify`), and `message`. |

The source revision is taken from the checkout that holds a local source path. When a `.git` entry is found, the revision is `git rev-parse HEAD` and `dirty` reflects `git --no-optional-locks status --porcelain`. When only a `.jj` directory is found, the revision is the change id of `@` read with `jj --ignore-working-copy`, and `dirty` is null because jj cannot tell without snapshotting the working copy, which would modify the repository. Remote flake references, missing tools, and failing commands record null.

Running and boot states, and the generation numbers, are read from the filesystem when the record is written. They describe the observed state, never the intended one.

Records are read without rejecting unknown fields, so a record written by a newer version with extra fields still loads. A record whose enumerated values (action, status, phase) are unknown to this version does not load.

## Storage layout and atomicity

The journal lives in `<nix state dir>/noxi/snapshots`, which is `/nix/var/nix/noxi/snapshots` by default. The state directory is resolved as the store resolves it, honoring `NIX_STATE_DIR`. Setting `NOXI_SNAPSHOT_DIR` to a non-empty path overrides the whole location, which lets tests and unprivileged users divert it.

The directory holds:

- `<id>.json`: one record per snapshot.
- `<id>.flake.lock`: a verbatim copy of the lock file, present when the record has a lock.
- `journal.lock`: the lock file that serializes id allocation.

Every file is written atomically: the bytes go to a temporary file in the same directory, the file is synced, it is renamed over the target, and the directory is synced. A lock copy is written before the record that refers to it. Ids are allocated while holding an exclusive `flock` on `journal.lock`; the next id is one more than the highest id among existing records and lock copies, and the initial record is written before the lock is released, so concurrent writers cannot receive the same id. The directory is created by the first write, not by reading.

Nothing prunes old snapshots yet.

## Commands

All commands read the journal at the location above. They exit 0 on success, 1 on an error, and 2 on a usage error, which also prints the usage text. Errors are printed with the `error: ` prefix the other noxi commands use, name the phase that stopped, and give a next step when one can be justified.

### `noxi snapshot list [--json]`

Prints a table with the id, creation time, action, generation after, outcome status, short source revision (with `-dirty` when the checkout had uncommitted changes), and closure basename, oldest first. Missing values are shown as `-`. With an empty journal it prints `no snapshots recorded`. `--json` prints the records as a JSON array.

### `noxi snapshot show ID [--json]`

Prints every field of one record. `ID` may be written with or without leading zeros. In text mode, when both this snapshot and the one immediately before it have stored locks, it also prints the lock changes between them. `--json` prints the record as JSON, without the lock comparison; fields this version does not know are not included.

### `noxi snapshot diff A B [--json]`

Compares the stored lock copies of snapshots `A` and `B` input by input. Nodes are matched by name, excluding the root node. An input is reported as added, removed, or changed; a change lists which of `rev`, `narHash`, and `lastModified` differ, with the before and after values. It fails if either snapshot has no stored lock.

### `noxi snapshot rollback ID [--no-activate] [--restore-lock] [--force] [--dry-run] [--profile PATH]`

Selects the generation snapshot `ID` recorded and, by default, activates it.

- `--no-activate` switches the profile without running `switch-to-configuration`, so the running system and boot loader entries are unchanged.
- `--restore-lock` writes the snapshot's stored lock copy back to the recorded lock path.
- `--force` overwrites a lock that was edited since the most recent snapshot, and allows a target whose outcome was not `success`.
- `--dry-run` performs every check and prints what would happen (the generation switch, whether `switch-to-configuration switch` would run, and which lock would be restored), without switching, activating, writing the lock, or recording a snapshot.
- `--profile PATH` names the profile; the default is `/nix/var/nix/profiles/system`.

On success it prints the selected generation, the restored lock if any, the observed generation, running and boot targets, and the id of the snapshot that records the rollback. If a step fails after the profile was touched, it prints the failing phase, the observed state, and the recording snapshot id, and exits 1.

## Rollback semantics and refusals

Rollback first checks, without changing anything, and refuses with an error when:

- the requested profile differs from the profile the snapshot recorded;
- the snapshot recorded no generation or no closure, such as a failed build;
- the snapshot's outcome was not `success`, unless `--force` is given;
- the recorded generation no longer exists in the profile;
- the generation link now points at a different store path than the recorded closure, meaning the number was reused;
- the generation's closure no longer exists;
- `--restore-lock` is given but the snapshot has no stored lock;
- `--restore-lock` is given, `--force` is not, and the current lock file differs from the lock recorded by the most recent snapshot that recorded a lock at the same path. A missing lock file counts as different. The error names `--force`.

It then records a new snapshot with action `rollback`, copying the target's source record, and in order: switches the profile to the generation with `noxi_store::profiles::switch_generation`, writes the lock back atomically when `--restore-lock` is given, runs `<generation link>/bin/switch-to-configuration switch` unless activation is disabled, and verifies that the profile points at the selected generation. The rollback record's lock is the lock on disk when it finishes, which is the restored copy or the untouched current file, so the next edit check compares against it. A failure in any of these steps is recorded as a `failed` outcome with phase `prepare-activation`, `activate`, or `verify`, and the steps after it are not run; the profile switch is not undone.

Rollback does not touch the source files other than the lock, does not undo changes to mutable application data, and does not check out the recorded source revision.

## Library API

The rebuilder records its operations through `noxi_system::snapshot`:

- `SnapshotStore::open(dir: PathBuf) -> Result<SnapshotStore>` and `SnapshotStore::default_location() -> Result<SnapshotStore>` open the journal.
- `SnapshotStore::begin(&self, request: Request) -> Result<Pending>` allocates an id and writes an initial record with outcome `not-applied`, the actor, the source revision, the lock copy, and the before states. `Request::new(action, kind, path)` fills the system profile and `/run/current-system`; `attribute`, `lock`, `profile`, and `current_system` are public fields.
- `Pending::finish(self, observed: Observed, outcome: Outcome) -> Result<Snapshot>` records the closure from `Observed`, reads the profile's generation and the running and boot links, stores the outcome, and rewrites the record. `Outcome::success`, `Outcome::failed`, and `Outcome::not_applied` build outcomes.
- `SnapshotStore::list`, `get`, and `latest` read records; `list` is oldest first.
- `SnapshotStore::diff_locks(&self, a, b) -> Result<LockDiff>` and `LockDiff::between(before, after)` compare locks.
- `SnapshotStore::rollback(&self, id, options: RollbackOptions) -> Result<RollbackReport>` performs a rollback. Refusals are returned as errors before anything changes. Once the profile has been touched, failures are reported through the recorded outcome; `RollbackReport::succeeded` says whether it completed.

Errors are the typed `snapshot::Error`; `Error::phase` and `Error::hint` give the stopping phase and a next step for rendering. Rendering stays in the command layer.

Because `begin` writes a record before any work starts, a rebuild that stops part way leaves a record whose outcome is `not-applied` with the message `operation started; no outcome was recorded`.

## Verification

The integration tests in `crates/noxi-system/tests/snapshot.rs` and `crates/noxi-system/tests/snapshot_cli.rs` run without root against temporary directories. They cover the begin and finish round trip and the lock copy, id order and `latest`, distinct ids for concurrent writers, loading a record with a newer schema version and extra fields, the lock diff on version 7 lock files, and rollback against a fake profile whose generations point at temporary closures with a logging `switch-to-configuration` script. The rollback tests check that the profile moves, that the script is run with `switch`, that `--no-activate` and `--dry-run` do not run it, that `--restore-lock` writes the lock back and refuses an edited lock without `--force`, that a missing generation is refused, that a failing activation is recorded as failed, and that the rollback itself is recorded. The command tests exercise `list`, `show`, `diff`, a missing id, and usage errors through `snapshot::run`.

## Not yet covered

- No automatic pruning or retention policy for snapshots.
- This change does not wire `noxi system` to the journal; here records are written by rollback and by library callers.
- No VM qualification; rollback has not been run against a real NixOS system profile, boot loader, or `switch-to-configuration`.
- Rollback does not check out the recorded source revision, and it does not check that the target is compatible with migrated persistent data.
- No structured event stream or JSON output for `rollback`.
