# Flake operation library

`noxi-flake` provides operations for the `nix` command owner. It adds no executable or command-line parser. Its evaluator operations run inside an existing evaluator generator and take that generator's `GenCo`; the caller owns the evaluator, feature settings, fetch configuration and store. Use `noxi_glue::configure` with `flakes` enabled before evaluating. The `operations.rs` integration tests show a builtin callback that invokes each operation.

| Entry point | Result and effects |
|---|---|
| `lock(store, co, reference, options)` | Returns `LockedFlake` without writing the checkout's lock file. Reuses unchanged entries. |
| `update(store, co, reference, options)` | Updates `options.update_inputs`; when that set is empty, recreates all input locks. |
| `LockedFlake::write_lock_file(path)` | Atomically writes version-7 JSON with the reference's two-space layout and final newline; preserves an existing file's permissions and skips identical contents. |
| `metadata(store, locked)` | Returns original/resolved/locked references, description, source metadata, source path, lock graph and the supported metadata fingerprint as JSON. It does not evaluate outputs. |
| `show(store, co, locked, options)` | Returns `ShowResult { json, warnings }` for conventional output namespaces. Options select the local system, all systems and legacy packages. Unknown outputs are reported without forcing their values. |
| `check(store, co, locked, options)` | Validates conventional outputs and returns derivation paths, local check build targets, omitted systems and warnings. With `build = true`, realizes local-system checks through the selected store owner. With `build = false`, only evaluates. |
| `archive(store, locked)` | Returns the recursive source-path/input JSON. Copies fetched sources when the selected evaluator store is writable. A read-only evaluator computes source paths without registering them. Follows edges are omitted and relative inputs have no separate source path. |
| `prefetch(store, reference)` | Requires a writable store, fetches and registers the source, and returns `storePath`, `hash`, `original` and `locked` JSON. It does not require `flake.nix`. |
| `LockedFlake::evaluate(store, co)` | Calls the pinned `call-flake.nix` helper and returns the lazy flake result, including `outputs`, `inputs`, `sourceInfo` and self attributes. |

`LockOptions::update_inputs` contains input paths as vectors of names, such as `["tool", "nixpkgs"]`. `override_inputs` maps those paths to replacement reference strings. Replacements resolve against the fetch configuration's working directory, can override registry resolution, and keep the declared original reference in the resulting lock. Overrides remain sticky when the lock is written. `recreate` discards old lock choices. `allow_unlocked` controls whether new unlocked input entries can be computed; when absent, it follows the evaluator's pure-mode setting. `builtins.getFlake` also rejects an unlocked root in pure mode.

Configure fetching with `FetchConfig` before starting evaluation. It accepts an explicit cache directory, `offline`, registry overrides, fetch settings and `Rc<dyn http::Client>`. HTTP clients receive method, URL, headers and body and return status, headers, effective URL and body. An injected client controls forge APIs, archives and LFS requests. Git transport uses Git's transport implementation and persistent `gitv3` cache. All default tests use temporary local data or injected responses.

For writable operations, use `EvaluatorStore::with_store` with the caller's local build backend or daemon owner and its diverted store directory. `EvaluatorStore::writable` registers paths but has no build engine. It can prefetch and archive; fresh check builds need an owner implementing `Store::build_paths`. The tests use `LocalBackend` in isolated temporary stores and verify the check output bytes. No operation selects or writes the host's system store on the caller's behalf.

The library returns `ErrorKind`; the command owner renders it and chooses exit status and trace policy. Show/check warnings are returned as strings. Fetcher warnings continue through the existing fetcher warning handler. The command integration below supplies a bounded subset. Command text output, progress activities, destination-store archive copying, repair/keep-going handling, evaluation-cache behavior and the remaining command flags still require integration. IFD policy must be configured on the evaluator store by the caller; the complete per-output CppNix policy has not been qualified here.

The current differential tests cover path and pinned Git metadata/prefetch JSON, repeated registry metadata, initial/selective/full locks, archive JSON with relative/follows/non-flake inputs, show JSON with system omission and conventional output types, a realized check derivation, and selected schema error messages. Path and clean revision fingerprints match these fixtures. Dirty Git fingerprints, every legacy/Hydra/overlay edge case, contextual-string rejection, source positions, malformed JSON types and the full upstream flake suite remain open. See [verification](verification.md) and the [compatibility matrix](compatibility-matrix.md); this API does not close the overall flake compatibility gate.

## Modern command integration, 2026-11-09

`noxi-commands` now calls this library for `nix flake lock`, `update`, JSON `metadata`, JSON `archive`, JSON `prefetch`, JSON `show`, and `check`. `update` accepts input names and `--flake`; selected overrides and updates use the same graph resolver. Local path and `git+file` roots have atomic lock-file writes, `--no-write-lock-file` suppresses them, and `--no-update-lock-file` refuses required changes. An input-free flake does not acquire an empty lock file. The CLI explicitly permits unlocked root resolution, while the default pure `builtins.getFlake` policy remains unchanged. Dirty fingerprints, complete lock-change diagnostics, remote-root lock effects, alternate lock-file paths, registry rewrites from CLI flags, clone/init/new/prefetch-inputs and text-format metadata/archive/show remain incomplete.

Modern eval/build/shell/run/develop/print-dev-env installables select conventional packages, legacyPackages, apps and devShells for the configured system, then explicit root attributes. Pure command source access uses the fetched-tree set, including sources registered in writable stores; it does not enable general host filesystem reads. This bounded pure-source support still rejects pure `builtins.path`, `filterSource` and storePath, and does not qualify complete IFD/source-path policy. Deprecated default output aliases, mixed store/flake installables, output specifications such as `^dev`, complete missing-attribute diagnostics and the full upstream command suite remain open.

Development commands can retain their environment output with `--profile`. With flakes enabled, `develop` attempts `nixpkgs#bashInteractive` through the same resolver and falls back to `bash` if lookup fails. Fallback diagnostics are not yet reference-identical. `--redirect`, full interactive shell/signal behavior and complete profile lifecycle qualification remain open. The existing `nix profile` command family is separate and remains unsupported.

`reference_flake_cli` compares metadata/archive/show JSON values, inputful lock bytes, forbidden updates, no-write overrides, input-free update/check behavior and pure explicit/default output selection with CppNix 2.35.2. `reference_develop` compares environment-profile links and environment JSON, and compares status/stdout for flake package builds, shell/run, print-dev-env and develop with an offline shell fallback. A separate local-registry fixture verifies that the interactive shell is built in the selected diverted store. Execution comparisons accept the matching namespace refusal when the host cannot execute inside a diverted store; those outcomes are not successful command execution evidence. Full command qualification and system/VM qualification remain open.

## Command inventory follow-up, 2026-11-09

The pinned inventory is `tests/reference-inventory/cli.json`, generated from CppNix 2.35.2 at `2c73b59`. The corresponding implementation oracle is `src/nix/flake.cc`, `src/nix/flake-prefetch-inputs.cc`, `src/libcmd/installables.cc`, `src/libflake/flake.cc`, `src/nix/nix-channel/nix-channel.cc`, and `src/nix/prefetch.cc` in the pinned official source checkout. The following records implementation and measured cases separately. It does not close the MVP gate.

| Command | Implemented and compared in this follow-up | Remaining work |
|---|---|---|
| `flake show` | Text tree with exact ANSI bytes, JSON, evaluation activity order, omission warnings, `--legacy`, `--all-systems`, configured system selection, lazy unknown output values and package descriptions in text mode | Partial text emitted before evaluation errors, IFD omission rendering, all legacy/Hydra recursion and schema diagnostics |
| `flake metadata`, alias `info` | Existing JSON/text and deprecated `info` alias plus input timestamp rendering; command-local `--override-flake` and `--inputs-from`, including retained final input attributes | Full dirty fingerprints, every metadata tree variant, inherited evaluation flags and source-position errors |
| `flake lock`, `update` | Compare semantic old/new lock graphs rather than checkout file existence, exact creation/no-write/refusal diagnostics, lock bytes, pinned source-cache behavior after writes, input path validation, duplicate/missing input warnings, update TTL zero, successful selective Git-input updates, Git intent-to-add and `--commit-lock-file` with matching commit hash/message | Full unlocked/dirty-lock policy, flake `nixConfig` acceptance, all graph and error cases |
| `flake archive` | Existing source graph JSON/text; native `--to` local destination import with validated NAR bytes; `--dry-run` destination remains absent; `--no-check-sigs` reaches the copy operation | Full daemon/cache/remote destination matrix and dry-run source side effects |
| `flake prefetch` | Existing JSON/text, explicit `--out-link` permanent root | All transports, link failure output ordering and non-local-store errors |
| `flake prefetch-inputs` | Native locked-input fetches and progress; successful offline local-input comparison | Parallel failure ordering, relative nodes and complete multi-input failure diagnostics |
| `flake clone`, `init`, `new` | Implemented on 2026-11-12; see [scaffolding and lookup paths](#flake-scaffolding-installables-and-lookup-paths-2026-11-12) | Terminal Markdown rendering and remote clone transports |
| `nix-channel` | Add/list/remove, archive updates, generation changes and rollback; comments and URL-only subscription lines, stable/unstable default names, identifier validation and usage diagnostics; loopback redirects, successful selective update, TTL cache reuse and refresh | Packaged guest qualification, conditional revalidation/error fallback matrix, transfer setting parity and full download diagnostics |
| `nix-prefetch-url` | Fixed-output paths/hashes, unpack/executable modes, expected-hash reuse, loopback redirects, exact HTTP 404 body diagnostic | `--attr`/`-A` expression sources and mirror URLs; complete transfer setting/retry/TLS/proxy/error matrix and legacy help/flag diagnostics |

Common command flags now admitted include `--reference-lock-file`, `--output-lock-file`, `--no-write-lock-file`, `--no-update-lock-file`, `--override-input`, `--update-input`, `--recreate-lock-file`, `--override-flake`, `--inputs-from`, `--commit-lock-file`, deprecated `--no-registries`, `--json`, `--pretty`, and `--no-pretty`, where the reference command declares them. `--refresh` sets fetch TTL to zero. Deprecated flag warnings are emitted when parsed. `--override-input` implies no-write for metadata, show, archive and output selection; lock/update retain their explicit write policy. The parser rejects flags absent from the pinned command declaration. Inherited argument/include/eval-store/debugger/repair handling, and check's `--out-link`/`--print-out-paths` remain explicit failures rather than ignored flags. Pretty/no-pretty output has an exact show comparison; several other admitted flag combinations still need dedicated differential qualification.

The local backend now implements the existing `Store::add_multiple_to_store` interface by spooling each NAR to a temporary file and calling `LocalStore::add_to_store` with the requested repair/signature policy. This was necessary for archive destination copies. It uses the existing hash, size and content-address validation; no CppNix execution path was added.

The loopback selective channel test requires actual success. It copies only the oracle executable closure into a private diverted store, mounts that store in a private user/mount namespace, downloads two channels, verifies no requests during a fresh cache hit, changes the archive, expires the TTL and updates one channel. Both engines must produce `demo/default.nix = 43`, `other/default.nix = 42`, identical profile output paths and identical profile NAR bytes. A namespace refusal fails the test. The fixture disables supplementary-group dropping only in its isolated reference configuration because the user namespace does not permit `setgroups`.

For Opus's packaged guest qualification, reproduce this same two-channel scenario using the installed candidate executables and a separate reference home/state/cache/store. Run as a normal guest user, retain loopback request logs, compare the two installed channel contents and profile NARs, then exercise rollback and removal. Also run archive to a second private local store and verify the copied source bytes, prefetch with an explicit output link and verify its permanent root, then check a flake with a native `builtin:unpack-channel` derivation and verify the realized output. These are required successful package execution scenarios; the host comparisons do not establish their guest results. Flake `nixConfig` trust/acceptance and transfer settings must use the settings/security owner's policy before that integration is qualified.

## Native noxi flake update

`noxi flake update` and `noxi flake lock` are the native counterparts of `nix flake update` and `nix flake lock`. They live in `crates/noxi-system/src/flake.rs`. They do not reimplement locking: each call runs the same in-process `nix flake update|lock` implementation in `crates/noxi-commands/src/nix/flakes.rs`, so there is one lock algorithm, one atomic lock-file write and one `--commit-lock-file` path. The native command reads the lock file before and after the call and reports the difference by input.

```text
noxi flake update [INPUT...] [--flake PATH] [--commit-lock-file] [--override-input NAME REF]... [--json]
noxi flake lock [--flake PATH] [--override-input NAME REF]... [--json]
```

`update` re-resolves the named inputs, or every input when none are named. Input names are input paths such as `nixpkgs` or `home-manager/nixpkgs`. `lock` creates or completes the lock without changing existing entries and takes no input names. `PATH` defaults to the current directory. It must be a local directory that contains `flake.nix`, and it is canonicalized before use. A directory inside a Git work tree resolves as a `git+file` flake, as it does for `nix`. `--commit-lock-file` (update only) and `--override-input` are passed through to the shared implementation unchanged. The command enables the `nix-command` and `flakes` experimental features itself, so `--extra-experimental-features` is not needed. It enables them ahead of `NIX_CONFIG`, so flake settings given there, such as `flake-registry`, take effect.

### Report

When the lock file bytes are unchanged, the command prints `no changes`, and the shared implementation does not rewrite the file. Otherwise it prints `created LOCKPATH` or `updated LOCKPATH`, then one line per input that was added, removed or changed, then one line listing the unchanged inputs:

```text
updated /home/me/config/flake.lock
changed  dep  rev 1a2b3c4 -> 5d6e7f8, last modified 2001-01-01 -> 2002-02-02
changed  a    narHash sha256-old... -> sha256-new..., last modified 2026-11-12
added    b    narHash sha256-..., last modified 2026-11-12
unchanged: 1 input (c)
```

An entry is identified by its short revision (7 characters), or by its NAR hash when it has no revision. `lastModified` is shown as a UTC date. A change to an input's `original` reference is shown as `source OLD -> NEW`.

Inputs are compared by their path from the root node, not by node name, because node names such as `nixpkgs_2` can be renumbered between locks. The graph is walked breadth-first from `root`, and `follows` edges are resolved to their target node. A node reachable by several paths is reported once, under its first path in that order. An entry counts as changed when its `locked` or `original` attributes, or its `flake` flag, differ. Added and changed inputs are listed in the new lock's order, followed by removed inputs in the old lock's order.

With `--json`, stdout carries one JSON object instead:

```json
{
	"flake": "/home/me/config",
	"lock": "/home/me/config/flake.lock",
	"changes": [
		{
			"input": "dep",
			"kind": "changed",
			"old": { "url": "git+file:///src/dep?rev=...", "original": "git+file:///src/dep", "rev": "...", "narHash": "sha256-...", "lastModified": 978307200 },
			"new": { "url": "...", "original": "...", "rev": "...", "narHash": "sha256-...", "lastModified": 1012608000 }
		}
	],
	"unchanged": 1
}
```

`kind` is `added`, `removed` or `changed`. `old` is `null` for an added input, and `new` is `null` for a removed input. `rev`, `narHash` and `lastModified` appear only when the lock entry has them.

### Exit status and errors

Success exits 0. A failure exits 1 with one line `error: PHASE: CAUSE` on stderr. The phases are:

- `resolve`: the flake directory is missing, is not a directory, has no `flake.nix`, the existing lock file cannot be read or parsed, or settings cannot be loaded.
- `lock`: the shared implementation failed. This covers fetching inputs, writing the lock file and committing it, because all of these happen inside that call.
- `write`: the lock file written by the lock phase cannot be read back or parsed.

A usage error, including an unknown flag, a missing flag value, an unknown subcommand, or input names given to `lock`, exits 2 with `error: MESSAGE` followed by the usage text.

### Library entry points

`noxi_system::flake::update(UpdateOptions) -> Result<UpdateReport, UpdateError>` runs the same operation for other callers, such as a future `noxi system switch --update`. `UpdateOptions` can carry preloaded `Settings`. `UpdateReport` holds the canonical flake directory, the lock path, whether the lock existed before and was rewritten, and the `LockDiff`. `diff_locks(old, new) -> LockDiff` compares any two version-7 lock documents and has no side effects. `run(args, out, err)` is the command with injectable output streams, and `main(args)` wraps it with stdout and stderr. Calls into the shared implementation are serialized within one process.

### Tests and open work

`crates/noxi-system/tests/flake.rs` runs offline in temporary directories. The store, configuration, fetcher cache and Git configuration are diverted to a private directory under the Cargo target directory. The tests cover:

- an initial lock with two `path:` inputs
- a selective update that changes one `path:` input and leaves the other unchanged
- a full update of both inputs, then a repeat that prints `no changes` and leaves the lock bytes and mtime untouched
- a local Git input moving between two commits, with short revisions and dates
- the JSON shape
- `resolve`, `lock` and usage failures
- `follows` and renamed-node handling in `diff_locks`

These limits remain:

- The shared implementation still writes its own diagnostics directly to the process's stderr, so they are not captured into the native error line. These include its `warning: updating lock file` change list and, on failure, its `error:` line. A `lock` failure therefore says that the cause is reported above.
- A write or commit failure inside the shared call is reported as `lock`, not `write`.
- Only local flake directories are accepted. Remote flake references, `--inputs-from`, `--recreate-lock-file`, `--no-write-lock-file`, `--output-lock-file`, `--reference-lock-file` and a `--store` flag are not offered.
- `--commit-lock-file` and `--override-input` are passed through but are not exercised by the native tests.
- Remote inputs (GitHub, tarball and others) are only as qualified as the shared implementation, and the native tests do not cover them.
- Flake settings in configuration files that do not themselves enable `flakes` are still ignored, as for `nix`.
- The report is plain text. It does not yet use the structured event stream described in [logging](logging.md).

## Flake scaffolding, installables and lookup paths, 2026-11-12

`nix flake init` copies a template into the current directory and `nix flake new DIR` into `DIR`. `-t REF#NAME` selects `templates.NAME`, then `NAME`; without a fragment the candidates are `templates.default` and `defaultTemplate`, and `#.PATH` selects exactly `PATH`. The default template reference is the registry name `templates`. The template's lock file is never written. Regular files are written with mode 0666 less the umask, so executable bits are not kept; symlinks are recreated. An identical existing file is skipped, a different one is a conflict that is reported, left alone and counted in the final error. New paths are added to an existing Git checkout with `git add --intent-to-add --force`. A `welcomeText` is printed after copying, rendered as CppNix's lowdown renderer does off a terminal for headings, filled paragraphs, flat lists, fenced code, quotes and inline markup; other Markdown, and rendering on a terminal, fail explicitly. `_NIX_TEST_RAW_MARKDOWN=1` prints it unchanged.

`nix flake clone REF --dest DIR` resolves `REF` through the registries. Git inputs are cloned with `git clone`, with `--branch` for a `ref`; a `rev` is refused as in CppNix. GitHub, GitLab and SourceHut inputs clone their HTTPS Git repository. Other inputs are fetched and copied into `DIR`, which must not exist, keeping only executable bits as a NAR restore does.

Flake installables of `eval`, `build`, `shell`, `run`, `develop`, `print-dev-env`, `edit`, `search`, `profile add`, `fmt` and `formatter` use the reference's candidate attribute paths. Without a fragment they are `packages.SYSTEM.default` and `defaultPackage.SYSTEM`, preceded by `apps.SYSTEM.default` and `defaultApp.SYSTEM` for `run`, and by `devShells.SYSTEM.default` and `devShell.SYSTEM` for the development commands. A fragment is tried under the prefixes `packages.SYSTEM.` and `legacyPackages.SYSTEM.` (and `apps.SYSTEM.` or `devShells.SYSTEM.`), then as given; `#.PATH` is exact. `search` uses `packages.SYSTEM` and `legacyPackages.SYSTEM`, the formatter commands only `formatter.SYSTEM`. When no candidate exists, the error is `flake 'REF' does not provide attribute 'A', 'B' or 'C'`.

A lookup-path entry `NAME=flake:REF`, as flake-built NixOS sets `nixpkgs=flake:nixpkgs` in `NIX_PATH`, is resolved through the flag, user, system and global registries, fetched into the selected store and used as the entry's directory. This applies to evaluation (`-I`, `NIX_PATH`, `nix-path`) and to `nix-instantiate --find-file`. An entry that cannot be resolved, or a `flake:` entry without the `flakes` experimental feature, is an error, as in CppNix. The `… while calling the 'findFile' builtin` frame CppNix prints above it is still missing, as for every failed lookup. `reference_flake_commands` compares these cases with CppNix 2.35.2.

The installable comparisons cover `eval`, `build --dry-run` and `edit`: the `defaultPackage` fallback, the `packages` and `legacyPackages` prefixes, `#.PATH` and missing attributes. The `run` and development-command candidate lists follow the same code but are not compared separately, because those comparisons build derivations and depend on the build engine.
