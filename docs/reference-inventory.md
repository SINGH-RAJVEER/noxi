# Reference inventory

This page lists what CppNix 2.35.2 exposes and what the pinned NixOS and Home Manager revisions call. The drop-in gate in the [compatibility matrix](compatibility-matrix.md) covers everything listed here. An entry is not supported until the matrix records evidence for it.

## Generated data

`just reference-inventory` builds the reference package and runs `scripts/reference-inventory.sh`, which writes `tests/reference-inventory/` from the reference build at `/nix/store/nskid3yq908g18x6cnvrk2hy96327f3f-nix-2.35.2` and the official checkout at `2c73b59` (`just nix-tests-sync`). The script clears the environment and points Nix at an empty configuration directory, so the files record built-in defaults, not this machine's `nix.conf`. Running it twice gives identical files; review any diff after a reference change.

| File | Contents | 2.35.2 count |
|---|---|---|
| `files.txt` | Executables, libexec helpers, systemd units, tmpfiles and profile scripts, and shell completions in the reference package | 12 executables, 2 libexec helpers, 3 service files, 4 profile scripts |
| `settings.json` | Every `nix.conf` setting with its aliases, gating experimental feature, and documented default. Undocumented defaults depend on the build machine and are recorded as `null`. `scripts/settings-table.py` turns this file and `experimental-features.json` into `nix_compat::nixcpp`'s settings table; rerun it when they change. | 134 settings, 7 gated |
| `experimental-features.json` | Experimental feature names | 21 |
| `cli.json` | The `nix` command tree with each command's flags and gating feature, the global flags, the store types, and the fetcher types | 96 commands and subcommands, 218 global flags, 10 store types, 9 fetchers |
| `builtins.json` | Every builtin and constant with its argument names and gating feature | 116, 6 gated |
| `protocols.txt` | Worker and serve protocol magic numbers, versions, feature strings, and operation numbers, read from the pinned source | Worker protocol 1.39 (minimum 1.18), 37 operations, 3 features; serve protocol 2.8, 8 commands |
| `legacy-options.txt` | Options named in each legacy command page's synopsis and option list in the reference manual, and the options common to all commands (`opt-common`) | 181 command options, 24 common options |
| `empty-store.txt` | The directories, files, schema version, recorded migrations, journal mode, and SQL schema of a fresh local store as 2.35.2 creates it. Needs `sqlite3`. | Schema 10, 1 migration, 4 tables |
| `c-api.txt` | Functions declared by the public C API headers, per library | 126 functions in `libutil-c`, `libstore-c`, `libexpr-c`, `libfetchers-c`, `libflake-c`, `libmain-c` |

`cli.json` does not cover the legacy commands (`nix-build`, `nix-channel`, `nix-collect-garbage`, `nix-copy-closure`, `nix-daemon`, `nix-env`, `nix-hash`, `nix-instantiate`, `nix-prefetch-url`, `nix-shell`, `nix-store`). They parse their own arguments, and CppNix has no machine-readable dump of them, so `legacy-options.txt` reads the reference manual's command pages instead. An option the parser accepts but the manual omits is missing from that file; the differential tests are the check for those. Every legacy name is a symlink to `bin/nix`, which dispatches on its executable name, so a replacement must dispatch the same way or install separate executables at the same paths.

The reference also installs C++ headers and shared libraries (`nix-util`, `nix-store`, `nix-fetchers`, `nix-expr`, `nix-flake`, `nix-main`, `nix-cmd`, and their `-c` variants). The C++ interfaces have no stability promise upstream. Which consumers of either interface need support on the target system is still open. Discover consumers during §0 of the [TODO](../TODO.md), then implement and qualify their interfaces in §6. Record source-build and unchanged-binary interoperability separately; matching exported function names alone does not prove ABI compatibility.

## What NixOS calls

This list comes from reading the pinned Nixpkgs `e94cb152` source. It shows what the system depends on at build, activation, and service time, and ranks the command set for the MVP. It is not a trace of a running system; the qualification VM still has to confirm it.

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
