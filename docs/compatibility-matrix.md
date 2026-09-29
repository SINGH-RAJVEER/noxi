# Initial compatibility matrix

This is the first qualification target and its current evidence, not a claim of compatibility. Qualify the existing-system package-manager replacement first; the installed OS and NixBox interface rows are later release gates. Add a row for each additional reference release, architecture, or feature setting. Keep failures and unrun cases visible.

| Field | Initial target |
|---|---|
| Reference package manager | CppNix 2.35.2, tag `2.35.2`, commit `2c73b59da29606068c0c98db015dd3a66955525d`; flake package `nix-reference` at `/nix/store/nskid3yq908g18x6cnvrk2hy96327f3f-nix-2.35.2` |
| Architecture and OS | x86_64-linux, NixOS-derived system |
| Nixpkgs and NixOS module revision | `e94cb152ed51bd6e24eb4a41f1460252beb52cd2` (x86_64-linux); locked in `tests/qualification/flake.lock` as the root's `nixpkgs_2` input |
| Home Manager revision | `7b4c5ec4bedaf1e062bbc1bcaeddbc6bd242aa1b`; its `nixpkgs` follows the qualification flake's pinned Nixpkgs input |
| Reference feature profile | VM module sets `nix-command flakes`; `nix show-config --json` under this profile reports effective `flakes`, `fetch-tree`, and `nix-command`. Other feature profiles remain unqualified. |
| Interface inventory | `tests/reference-inventory/`, generated from the reference build by `just reference-inventory`: 12 executables, 134 settings, 21 experimental features, 96 `nix` commands, 181 legacy command options, 116 builtins, worker protocol 1.39 (minimum 1.18), serve protocol 2.8, and 126 C API functions. The NixOS and Home Manager call sites are in [reference inventory](reference-inventory.md). |
| Store | Existing CppNix store and fresh store, both in isolated environments. Expected states are in [compatibility](compatibility.md#expected-store-states); the reference's fresh-store layout and schema are in `tests/reference-inventory/empty-store.txt`. |
| Client and daemon pairings | Reference/reference, nix-rs/nix-rs, reference/nix-rs, and nix-rs/reference where supported by the reference protocol |
| Boot mode and VM | UEFI QEMU VM from `tests/qualification/flake.nix`, with the pinned CppNix package and Home Manager NixOS module. `nix eval path:./tests/qualification#nixosConfigurations.reference.config.system.build.vm.drvPath` succeeds. The image builds, boots to multi-user mode with the Nix daemon and Home Manager, powers off cleanly, and is kept as a read-only base (`just reference-vm-snapshot`, `just reference-vm-verify`). Rollback evidence is still pending. Physical hardware support must be listed before release. |

| Area | Current status | Evidence needed to pass |
|---|---|---|
| Nix language and evaluator | Copied language fixtures run against the evaluator: 104 of 467 pass; remaining outcomes are tracked in `crates/compat-tests/expectations/upstream-lang.txt` | Every language fixture passing, the `lang.sh` script checks, and Nixpkgs evaluation comparison |
| Hash algorithms | Partial fixture coverage, including BLAKE3 vectors and JSON | All reference formats, feature-gate behavior, hashing commands, and store consumers |
| Derivation and path formats | Partial fixture coverage. ATerm parsing, non-UTF-8 text and output names, colliding hash-modulo inputs, and unusual fixed-output input requests have local tests or reference-generated fixtures | All copied fixtures and byte comparisons; see `crates/compat-tests/` |
| NAR archive format | Malformed-archive fixtures only; 2.35.2 has no valid NAR fixture | Copied valid and malformed NAR fixtures, serialization, import/export, and store exchange |
| Store and garbage collection | Unrun | Existing-store, concurrency, integrity, and return-to-reference tests |
| Build engine and sandbox | Unrun | Local and remote build behavior under the selected system |
| Binary caches and signatures | Unrun | Cache exchange and trust decisions |
| Daemon protocol | Worker-protocol value fixtures round-trip byte for byte, except those needing protocol feature negotiation (`realisation-with-path-not-hash`) and the handshake; daemon pairings unrun | All wire fixtures, handshake and feature negotiation, every operation, and client/daemon pairings; see `crates/compat-tests/tests/upstream_worker_protocol.rs` |
| CLI and configuration | `nixrs` preview pure-expression evaluation tested; replacement command suite unrun | Legacy and new command suites, settings, flakes, profiles |
| C and C++ libraries | Unrun | Consumer inventory and binary interoperability |
| NixOS activation | Unrun | Build, switch, boot, test, dry activation, rollback |
| Installed OS lifecycle | Unrun | Install, self-update, reboot, recovery |
| TUI and GPUI-based NixBox GUI parity | Unrun | Shared operations and events, preview/apply, failures, recovery, accessibility, headless fallback |
| NixBox engine integration | Unrun | Locked-revision resolution, evaluated candidate plans, direct realization, declarative persistence, and recovery |
| OS-native CLI diagnostics | Unrun | Structured rebuild events, concise errors, raw-log retrieval, JSON, TTY, and CI modes |

The ATerm parser accepts entries in any order and resolves duplicates and escapes as 2.35.2 does, and input-addressed paths are checked against a derivation graph the 2.35.2 reference wrote; the remaining differences are listed in [verification](verification.md). Deferred outputs and zero-output derivations parse and round-trip as CppNix accepts them; zero outputs fail verification as CppNix's type check does. Floating content-addressed and impure outputs and `DrvWithVersion` are rejected, which matches CppNix only while `ca-derivations`, `impure-derivations` and `dynamic-derivations` are disabled; configurations enabling them remain gaps. They are listed in `crates/compat-tests/tests/upstream_derivations.rs`. This is not a measured coverage percentage. Qualification requires a pinned Nixpkgs revision, completed interface inventory, test results, and release artifacts as defined in [verification](verification.md).
