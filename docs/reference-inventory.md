# Reference inventory

This page lists what CppNix 2.35.2 exposes and what the pinned NixOS and Home Manager revisions call. The drop-in gate in the [compatibility matrix](compatibility-matrix.md) covers everything listed here. An entry is not supported until the matrix records evidence for it.

## Generated data

`just reference-inventory` builds the reference package and runs `scripts/reference-inventory.sh`, which writes `tests/reference-inventory/` from the reference build at `/nix/store/nskid3yq908g18x6cnvrk2hy96327f3f-nix-2.35.2` and the official checkout at `2c73b59` (`just nix-tests-sync`). The script clears the environment and points Nix at an empty configuration directory, so the files record built-in defaults, not this machine's `nix.conf`. Running it twice gives identical files; review any diff after a reference change.

| File | Contents | 2.35.2 count |
|---|---|---|
| `files.txt` | Executables, libexec helpers, systemd units, tmpfiles and profile scripts, and shell completions in the reference package | 12 executables, 2 libexec helpers, 3 service files, 4 profile scripts |
| `settings.json` | Every `nix.conf` setting with its aliases, gating experimental feature, and documented default. Undocumented defaults depend on the build machine and are recorded as `null`. `scripts/settings-table.py` turns this file and `experimental-features.json` into `nix_compat::nixcpp`'s settings table; rerun it when they change. | 134 settings, 7 gated |
| `experimental-features.json` | Experimental feature names | 21 |
| `cli.json` | The `nix` command tree with each command's flags (short name, arity, gating feature), positional arguments (label, arity, optional) and gating feature, the global flags, the store types, and the fetcher types. An arity of `null` means any number of values, as CppNix's `ArityAny`; flag aliases and the `-j` short name of `--max-jobs` are absent from the dump, and `scripts/nix-command-table.py` adds them back when it generates noxi-commands' command table | 96 commands and subcommands, 218 global flags, 10 store types, 9 fetchers |
| `builtins.json` | Every builtin and constant with its argument names and gating feature | 116, 6 gated |
| `protocols.txt` | Worker and serve protocol magic numbers, versions, feature strings, and operation numbers, read from the pinned source | Worker protocol 1.39 (minimum 1.18), 37 operations, 3 features; serve protocol 2.8, 8 commands |
| `legacy-options.txt` | Options named in each legacy command page's synopsis and option list in the reference manual, and the options common to all commands (`opt-common`) | 181 command options, 24 common options |
| `empty-store.txt` | The directories, files, schema version, recorded migrations, journal mode, and SQL schema of a fresh local store as 2.35.2 creates it. Needs `sqlite3`. | Schema 10, 1 migration, 4 tables |
| `c-api.txt` | Functions declared by the public C API headers, per library | 126 functions in `libutil-c`, `libstore-c`, `libexpr-c`, `libfetchers-c`, `libflake-c`, `libmain-c` |

`cli.json` does not cover the legacy commands (`nix-build`, `nix-channel`, `nix-collect-garbage`, `nix-copy-closure`, `nix-daemon`, `nix-env`, `nix-hash`, `nix-instantiate`, `nix-prefetch-url`, `nix-shell`, `nix-store`). They parse their own arguments, and CppNix has no machine-readable dump of them, so `legacy-options.txt` reads the reference manual's command pages instead. An option the parser accepts but the manual omits is missing from that file; the differential tests are the check for those. Every legacy name is a symlink to `bin/nix`, which dispatches on its executable name, so a replacement must dispatch the same way or install separate executables at the same paths.

The reference also installs C++ headers and shared libraries (`nix-util`, `nix-store`, `nix-fetchers`, `nix-expr`, `nix-flake`, `nix-main`, `nix-cmd`, and their `-c` variants). The C++ interfaces have no stability promise upstream. Which consumers of either interface need support on the target system is still open. Discover consumers during §0 of the [TODO](../TODO.md), then implement and qualify their interfaces in §6. Record source-build and unchanged-binary interoperability separately; matching exported function names alone does not prove ABI compatibility.

## What NixOS calls

This list comes from reading the pinned Nixpkgs `e94cb152` source. It shows what the system depends on at build, activation, and service time, and ranks the command set for the MVP. The rows marked in [traced calls](#traced-calls) were confirmed on a running system; the rest are read from source only.

| Caller | Invocation |
|---|---|
| `nix.conf` generation (`pkgs.formats.nixConf`, `nix.checkConfig`) | `nix config show` under a build-time `NIX_CONF_DIR`, run with `nix.package` to reject unknown settings while building the system |
| `nix-daemon` service and socket (`nix-daemon.nix`) | `systemd.packages` installs the package's `nix-daemon.service` and `.socket`; the unit runs `nix-daemon --daemon`. The unprivileged-daemon mode wraps it in `libexec/nix-nswrapper` and adds `nix store roots-daemon` |
| Garbage collection timer (`nix-gc.nix`) | `nix-collect-garbage` with `nix.gc.options` |
| Store optimisation timer (`nix-optimise.nix`) | `nix-store --optimise` |
| `nixos-rebuild` (`nixos-rebuild-ng`, classic mode) | `nix-build <path> --attr <attr>`, `nix-instantiate <path> --attr <attr> --add-root`, `nix-store --realise <drv> --add-root`, `nix-copy-closure --to/--from`, `nix-instantiate --find-file`, `nix-instantiate --eval --strict --json --expr` |
| `nixos-rebuild` (flake mode) | `nix --extra-experimental-features 'nix-command flakes'` with `build --print-out-paths`, `eval --raw`, `eval --json [--apply]`, `copy`, `flake metadata --json`, `edit`, `repl`, and `store diff-closures` |
| `nixos-rebuild` profile handling | `nix-env -p <profile> --set <path>`, `nix-env -p <profile> --list-generations`, `nix-env --rollback -p <profile>`, `nix-channel --update` |
| `nixos-install` | `nix-build --out-link`, `nix-env --store <root> -p <profile> --set` |

The boot-loader installers read profile generation links directly and do not call Nix.

## What Home Manager calls

From the pinned Home Manager `7b4c5ec4` source:

| Caller | Invocation |
|---|---|
| `home-manager` command | `nix-build` and `nix build` for the activation package, `nix-instantiate --eval --json` for news and options, `nix eval --expr builtins.getFlake` to detect flake support, `nix eval --impure --raw --expr builtins.currentSystem`, `nix-store -q --references` |
| Activation (`lib-bash/activation-init.sh`) | `nix-build --quiet --expr '{}' --no-out-link` and `nix-env -q` to initialise per-user state, `nix-env --version`, `nix-store --realise <path> --add-root` to migrate old profiles, `nix profile list [--json]` and `nix profile remove`, `nix-env -e` |
| Profile update (`home-environment.nix`) | `nix-env --profile <profile> --set <generation>`, or `nix profile` equivalents when the user's profile is a `nix profile` manifest |

As a NixOS module, Home Manager activates each user through a systemd unit that runs the same activation script, so the activation-time calls above apply to both modes.

## Traced calls

`just reference-trace` runs the `trace` check of `tests/qualification/flake.nix`. It boots a NixOS test machine built from the pinned Nixpkgs, Home Manager and reference CppNix, with Home Manager for one user, the garbage-collection and optimisation timers, and kernel auditing of every `execve`. The audit sees each command whichever package it comes from, so callers that bypass `nix.package` would also show up. The machine then runs, in order: `nixos-rebuild switch --flake` to a second generation, a Home Manager service restart, the `nix-gc` and `nix-optimise` services, `nixos-rebuild list-generations`, `nixos-rebuild switch --rollback --flake`, and `nix-collect-garbage -d`. The rebuild target is prebuilt and every locked flake input is in the guest's store, so the offline guest evaluates it to the same derivations and builds nothing. The check writes the raw audit log, `calls.json` (every Nix call with its phase, arguments, user and parent commands, plus each scenario's output) and `calls.txt` (one line per call) to its output; `just reference-trace` links it at `target/qualification/trace`.

The run on 2026-10-04 recorded these calls. All of them ran the reference package's `bin/nix` under the legacy name shown; nothing ran another Nix.

| Phase | Caller | Invocation |
|---|---|---|
| Boot | `nix-daemon.service` | `nix-daemon --daemon` |
| Boot | The test machine's store registration | `nix-store --load-db`. This comes from the test framework's `regInfo`, not from NixOS on a real machine |
| Boot and service restart | Home Manager activation, as the user | `nix-build --quiet --expr {} --no-out-link`, `nix-env -q`, `nix-store --realise <generation> --add-root <state>/gcroots/new-home`, `nix-env -i <home-manager-path>`, `nix-store --realise <generation> --add-root <state>/gcroots/current-home` |
| `nixos-rebuild switch --flake` | `nixos-rebuild`, then the target's `nixos-rebuild` it re-executes | `nix --extra-experimental-features 'nix-command flakes' build --print-out-paths <flake>#nixosConfigurations."<name>".config.system.build.nixos-rebuild --no-link`, then `nix-instantiate --find-file nixos-system`, the same `nix build` for `config.system.build.toplevel`, and `nix-env -p /nix/var/nix/profiles/system --set <toplevel>` |
| `nixos-rebuild list-generations` | `nixos-rebuild` | `nix-instantiate --find-file nixos-system`; generations are read from the profile links |
| `nixos-rebuild switch --rollback --flake` | `nixos-rebuild` | The re-execution build above, `nix-instantiate --find-file nixos-system`, and `nix-env --rollback -p /nix/var/nix/profiles/system` |
| `nix-gc.service` | systemd | `nix-collect-garbage` with `nix.gc.options` (none here) |
| `nix-optimise.service` | systemd | `nix-store --optimise` |
| By hand | root | `nix-collect-garbage -d` |

Compared with the source reading above:

- **Confirmed:** `nix-daemon --daemon`, the timers' commands, flake-mode `nix build --print-out-paths`, `nix-env -p <profile> --set` and `--rollback`, and Home Manager's `nix-build --expr {}`, `nix-env -q` and `nix-store --realise --add-root`.
- **Newly found:** Home Manager as a NixOS module installs its generation with `nix-env -i <path>`, not `nix-env --set`. `nixos-rebuild` calls `nix-instantiate --find-file nixos-system` even in flake mode and for `list-generations`. With the default re-execution, every `nixos-rebuild` action first builds the target's own `nixos-rebuild`.
- **Not traced:** classic-mode `nixos-rebuild` (`nix-build`, `nix-instantiate --add-root`, `nix-channel --update`), remote targets (`nix-copy-closure`, `nix copy`), `nixos-install`, the standalone `home-manager` command, `nix profile` profiles, and `nix config show`, which runs only while the system is built. These rows remain read from source.

The trace records commands, not daemon traffic. Home Manager runs as an unprivileged user, so its calls reach the store through the daemon socket, while `nixos-rebuild` runs as root and opens the store directly.

## Command implementation update, 2026-11-09

The command crate now provides actual `nix-prefetch-url` and `nix-channel` Cargo executables. The former has fixed-output download comparisons; the latter currently implements subscription-file add/list/remove only. Channel update/rollback/generation commands still fail explicitly, so the classic NixOS channel-update caller remains unsupported. Packaging must replace the two existing placeholders and rerun its layout/VM gates. Modern flake dispatch and installable integration have bounded differential evidence recorded in [verification](verification.md#modern-flake-command-integration-2026-11-09) and [flakes](flakes.md#modern-command-integration-2026-11-09). Historical inventory captures and VM results above are unchanged.

## Profile and nix-env implementation inventory, 2026-11-09

This is the owned command inventory derived from `tests/reference-inventory/cli.json`, `legacy-options.txt`, and the pinned 2.35.2 `src/nix/profile.cc` and `src/nix/nix-env/nix-env.cc`. It does not replace the full reference contract or close the package-manager MVP. Earlier dated results above remain historical evidence.

| Operation | Implemented behavior and differential evidence | Remaining qualification |
|---|---|---|
| `profile add` | Store paths, expression/file installables, flake source fields, explicit output selection, content-addressed manifest and generation; `reference_profile` compares source tracking and changed flake realization | v1/v2/v3 and legacy migration compare exactly; default-output metadata, all shared source flags and source kinds need expanded comparisons |
| `profile list` | JSON and text including original/locked source and output selection; exact stdout in `reference_profile` | Pretty/no-pretty flags and legacy/v1/v2/v3 migrations compare exactly; malformed manifests need expanded comparisons |
| `profile remove` | Name, path, case-insensitive regex and all selection; shared generation transaction | Selector warnings/errors and regex are compared; inactive elements need expanded comparisons |
| `profile upgrade` | Re-resolves recorded mutable source, records locked URL and explicit outputs, realizes changed output, preserves priority and active state; exact output/status/manifests/generation comparisons | All locked source kinds, dry-run realization boundaries and shared lock flags need expanded comparisons |
| `profile history`, `diff-closures` | Generation history and closure version/size differences, compared in `reference_profile` | Size thresholds, output suffixes, empty versions and clock boundaries need expanded comparisons |
| `profile rollback`, `wipe-history` | Shared legacy generation operations; rollback accepts `--to`; prior generation tests retained | Modern `--to`, dry-run and wipe operations compare exactly; age boundaries need expanded comparisons |
| `nix-env --install`, `--set` | Store paths, expression/file/default-source discovery, attribute/function/profile selection, auto arguments, newest version/system/priority selection, force-name, priority, preserve/remove-all and prebuilt filtering | Source-mode precedence is compared; default directory discovery, multi-output selection, malformed derivations, selector suggestions and all flag combinations need expanded comparisons |
| `nix-env --upgrade` | `--lt`, `--leq`, `--eq`, `--always`, source/attribute selection, keep flags and priority/version choice; fixture requires an actual `expression-1` to `expression-2` upgrade | Substituter-backed prebuilt selection, equal-version rebuilds and priority ties need expanded comparisons |
| `nix-env --query` | Installed/available sources, attribute prefix, name/system/paths/description/status/version columns, JSON/XML and metadata; exact local/daemon transcript comparisons | Lazy malformed derivations, invalid metadata and full prebuilt/substituter matrix remain open |
| `nix-env --uninstall`, `--set-flag` | Selector operations and metadata transactions; uninstall follows the reference's bidirectional partition order so manifests/generation hashes agree | Full regular-expression dialect and malformed selectors remain open |
| Legacy generations and profile switching | Existing `reference_nix_env` exact filesystem/output/status comparison retained | No new system/VM qualification |

The only shared dispatcher change is enabling these profile flags in `crates/noxi-commands/src/nix/mod.rs`. The flake adapter still rejects `--commit-lock-file` and `--inputs-from`; those require integration with the flake owner. Shared `--eval-store`, debugger, repair and argument-from-file/stdin behavior also remains outside the evidence above. Production operations are Rust and Nix expressions evaluated by noxi, never calls to a reference executable.

### Exact owned flag inventory

These are command-local flags from the pinned inventory, including shared flags inherited by each command. They remain required even where the implementation or qualification above is incomplete. Global flags remain in `tests/reference-inventory/cli.json` and are not redefined here.

| Modern command | Flags from `cli.json` |
|---|---|
| `nix profile add` | `--arg`, `--arg-from-file`, `--arg-from-stdin`, `--argstr`, `--commit-lock-file`, `--debugger`, `--eval-store`, `--expr`, `--file`, `--impure`, `--include`, `--inputs-from`, `--no-registries`, `--no-update-lock-file`, `--no-write-lock-file`, `--output-lock-file`, `--override-flake`, `--override-input`, `--priority`, `--profile`, `--recreate-lock-file`, `--reference-lock-file`, `--repair`, `--stdin`, `--update-input` |
| `nix profile diff-closures` | `--profile` |
| `nix profile history` | `--arg`, `--arg-from-file`, `--arg-from-stdin`, `--argstr`, `--debugger`, `--eval-store`, `--impure`, `--include`, `--override-flake`, `--profile`, `--repair` |
| `nix profile list` | `--arg`, `--arg-from-file`, `--arg-from-stdin`, `--argstr`, `--debugger`, `--eval-store`, `--impure`, `--include`, `--json`, `--no-pretty`, `--override-flake`, `--pretty`, `--profile`, `--repair` |
| `nix profile remove` | `--all`, `--arg`, `--arg-from-file`, `--arg-from-stdin`, `--argstr`, `--debugger`, `--eval-store`, `--impure`, `--include`, `--override-flake`, `--profile`, `--regex`, `--repair` |
| `nix profile rollback` | `--dry-run`, `--profile`, `--to` |
| `nix profile upgrade` | `--all`, `--arg`, `--arg-from-file`, `--arg-from-stdin`, `--argstr`, `--commit-lock-file`, `--debugger`, `--dry-run`, `--eval-store`, `--expr`, `--file`, `--impure`, `--include`, `--inputs-from`, `--no-registries`, `--no-update-lock-file`, `--no-write-lock-file`, `--output-lock-file`, `--override-flake`, `--override-input`, `--profile`, `--recreate-lock-file`, `--reference-lock-file`, `--regex`, `--repair`, `--update-input` |
| `nix profile wipe-history` | `--dry-run`, `--older-than`, `--profile` |

| Legacy operation | Flags from `legacy-options.txt` |
|---|---|
| `--delete-generations` | `--delete-generations` |
| `--install` | `--attr`, `--from-expression`, `--from-profile`, `--install`, `--prebuilt-only`, `--preserve-installed`, `--priority`, `--remove-all`, `-A`, `-E`, `-P`, `-b`, `-i`, `-r` |
| `--list-generations` | `--list-generations` |
| `--query` | `--attr`, `--attr-path`, `--available`, `--compare-versions`, `--description`, `--drv-path`, `--installed`, `--json`, `--meta`, `--no-name`, `--out-path`, `--prebuilt-only`, `--query`, `--status`, `--system`, `--xml`, `-A`, `-P`, `-a`, `-b`, `-c`, `-q`, `-s` |
| `--rollback` | `--rollback` |
| `--set-flag` | `--set-flag` |
| `--switch-generation` | `--switch-generation`, `-G` |
| `--switch-profile` | `--switch-profile`, `-S` |
| `--uninstall` | `--uninstall`, `-e` |
| `--upgrade` | `--always`, `--attr`, `--eq`, `--from-expression`, `--from-profile`, `--leq`, `--lt`, `--prebuilt-only`, `--preserve-installed`, `--upgrade`, `-A`, `-E`, `-P`, `-b`, `-u` |
| `common` | `--arg`, `--argstr`, `--dry-run`, `--file`, `--option`, `--profile`, `--system-filter`, `-f`, `-p` |

Source inspection additionally includes `--set`, `--force-name`, `--version`, common evaluator argument/include flags and source-profile selection. The manual-derived upgrade inventory lists `--preserve-installed`/`-P`, but 2.35.2's upgrade parser rejects them; the differential scenario retains that rejection. The inventory is not narrowed to accepted or already-tested flags.

## Additional modern command implementations, 2026-11-12

The dispatcher now connects the native Rust implementations below. This inventory describes implemented operations, not full compatibility with every command flag or store backend. Production commands do not delegate their work to the reference Nix executable.

| Commands | Implemented behavior | Evidence and boundaries |
|---|---|---|
| `nar pack`, `nar dump-path`, `nar ls`, `nar cat` | Native NAR serialization, indexed traversal, JSON/text listings, executable bits, symlinks, byte-range extraction | `reference_modern_extra` compares packed bytes, recursive and long listings, JSON and extracted contents. NAR input is a seekable file; malformed archive and complete diagnostic coverage remain open. |
| `key generate-secret`, `key convert-secret-to-public` | Native Ed25519 secret generation and public-key conversion using the reference key format | Public-key conversion is compared using a freshly generated reference secret. Random generated bytes cannot be compared exactly. |
| `store prefetch-file` | Native HTTP/file downloads, flat or executable/unpacked NAR hashing, expected hash validation, selected digest, custom names and JSON output | File URL downloads compare SHA-256/SHA-512, executable contents and renamed outputs. HTTP retry/proxy behavior, unpacked modern-command comparisons and mismatch diagnostics need broader qualification. |
| `store sign`, `store copy-sigs`, `store path-from-hash-part` | Local-store signing and signature persistence, signature import from local stores or binary caches, database lookup by hash part | Signing, displayed signatures and hash lookup compare exactly in private stores. Daemon-backed signing and the full substituter matrix remain unavailable. |
| `store diff-closures`, `store make-content-addressed` | Closure version/size comparison and recursive NAR reference/self-reference rewriting into a destination local store | Empty closure differences and flat-to-NAR content-address conversion compare exactly. Complex cyclic and self-referential closures require more differential evidence. |
| `store copy-log`, `store repair`, `store roots-daemon`, `store build-trace info` | Local build-log transfer, cache-backed repairing import, temporary-root polling, opaque-path trace output | These are bounded implementations. Repair restores contents from configured binary caches after archive/trust validation; rebuilding a deriver remains unsupported. Trace queries reject derivation-output tracing; roots polling and log transfer need lifecycle and daemon qualification. |
| `search` | Evaluator-backed package traversal, name/version/description matching, exclusions, JSON/text results | Expression search compares names, descriptions and exclusions. Search uses Rust regex syntax; unsupported dialect differences and lazy evaluation diagnostics remain open. |
| `edit` | Installable evaluation, `meta.position` resolution and editor invocation | Source position and editor-specific argument handling need expanded differential tests. |
| `fmt`, `formatter run`, `formatter build` | Flake formatter derivation resolution, realization, program execution or output links | Expression/file formatter sources fail explicitly. Real formatter execution and remote-store behavior remain unqualified. |
| `flake init`, `flake new`, `flake clone` | Template evaluation/copy with conflict detection, local Git intent-to-add, fetcher-backed cloning | Template Markdown rendering supports a documented subset and fails on unsupported constructs. Full template/Git/fetcher behavior needs expanded differential evidence. |
| `derivation add` | Version-4 JSON import, input-addressed/fixed/deferred outputs, recursive input hash verification, dry-run path calculation and native store insertion | Import and dry-run compare against `derivation show` output. Floating/impure outputs, dynamic input trees and non-flat/non-NAR fixed methods fail explicitly. |
| `upgrade-nix` | Native fallback URL download and pure evaluation, dry-run version reporting, target binary verification, native legacy profile installation | File-based fallback dry-run compares exactly. Actual upgrade, profile discovery and downloaded package execution remain unqualified. Modern `nix profile` manifests are rejected during discovery. |

`reference_modern_extra` contains six focused differential scenarios and an explicit help-rendering rejection test covering the comparisons listed above. Unsupported shared evaluator/store flags still fail explicitly at dispatch. Many store operations accept only explicit store paths and local stores; `--all`, derivation selection, remote filesystem access and full expression/flake installable support are not completed by this update. `help` validates command-page lookup, but existing pages and `help-stores` fail explicitly because the embedded manual is unavailable. These handlers are not completed help implementations. Complete CppNix manual/pager parity and global `--help` output remain unsupported. Historical command counts elsewhere in the documentation refer to their dated snapshots.

The command store URL audit added the pinned reference spelling `local://?root=ROOT`, present in `tests/nix/src/libstore-tests/data/store-reference/local_1.txt`. `reference_operations::local_url_initializes_a_missing_diverted_root` compares successful `store ping` and database creation for a missing isolated root. The comparison passed in the coordinated command test slot, alongside both local spellings and explicit JSON formatting modes. This bounded evidence is not a compatibility qualification.

The current generated command table contains 84 leaf commands. Source dispatch inspection connects all 84 to handlers, including the existing REPL, daemon, bundler and upgrade handlers. Two help handlers still reject existing manual pages explicitly, so the count is dispatch coverage and must not be reported as 84 implemented commands or evidence that all flags, diagnostics or backends are compatible. The REPL remains line-oriented without terminal completion; daemon remote forwarding, full help/manual parity and arbitrary store-query installables remain gaps. Legacy `nix-store --dump` now reaches the existing filesystem NAR serializer rather than the store-path maintenance handler; modified `--verify-path` diagnostics use the reference plain error-level log. The corrections pass the pinned-reference legacy store suite, including existing-directory restore diagnostics and blank lines between printed environment exports.

Fresh `build --json` remains explicitly unsupported when the build API cannot return BuildResult metadata; inventing timing/resource fields would misrepresent the operation. File-cache command uploads require an explicit supported codec: `none`, `gzip`, `bzip2` or `zstd`, and a signing key. The command integration test uploads each codec, imports with pinned CppNix and verifies the restored contents. Other codecs and the reference default compression remain unqualified. Content-addressed functional tests additionally reach an evaluator `derivationStrict` limitation for `__contentAddressed`. These remain qualification blockers rather than successful command coverage.

Coordinated validation on 2026-11-13 ran six command suites with two build jobs and serial test execution. `reference_flake_commands` passed 14, `reference_modern_extra` 8, `reference_operations` 8, `reference_store_archive` 8 and `reference_store_dir` 1. The initial `reference_nix_store_ops` run passed 2 and exposed one remaining environment-export whitespace mismatch; after correction its focused rerun passed all 3. These 42 passing tests combine that final focused rerun with the unchanged passing suites, rather than claiming a second complete all-green invocation. Logs are `/tmp/noxi-command-final-20261113.log` and `/tmp/noxi-command-store-final-20261113.log`. The earlier engine functional report belongs to its pre-LZMA candidate and was not rerun by this command batch.
