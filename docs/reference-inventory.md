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
