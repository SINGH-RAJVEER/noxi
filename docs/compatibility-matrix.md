# Initial compatibility matrix

This is the first qualification target and its current evidence, not a claim of compatibility. Qualify the existing-system package-manager replacement first; the installed OS and NixBox interface rows are later release gates. Add a row for each additional reference release, architecture, or feature setting. Keep failures and unrun cases visible.

| Field | Initial target |
|---|---|
| Reference package manager | CppNix 2.36.0 pre-release, NixOS/nix `master` commit `8f7ed1c1d1246bfa98cefd152c87d1ea26a3d463`; no 2.36.0 release tag existed on 2026-09-27 |
| Architecture and OS | x86_64-linux, NixOS-derived system |
| Nixpkgs revision | To pin before system qualification |
| Experimental features | Inventory all features present in the reference; record enabled and disabled configurations |
| Store | Existing CppNix store and fresh store, both in isolated environments |
| Client and daemon pairings | Reference/reference, nix-rs/nix-rs, reference/nix-rs, and nix-rs/reference where supported by the reference protocol |
| Boot mode | UEFI virtual machine first; physical hardware support to be listed before release |

| Area | Current status | Evidence needed to pass |
|---|---|---|
| Nix language and evaluator | Copied language fixtures run against the evaluator: 105 of 486 pass; remaining outcomes are tracked in `crates/compat-tests/expectations/upstream-lang.txt` | Every language fixture passing, the `lang.sh` script checks, and Nixpkgs evaluation comparison |
| Hash algorithms | Partial fixture coverage, including BLAKE3 vectors and JSON | All reference formats, feature-gate behavior, hashing commands, and store consumers |
| Derivation and path formats | Partial fixture coverage; remaining gaps in `nix-compat` | All copied fixtures and byte comparisons; see `crates/compat-tests/` |
| NAR archive format | Partial fixture coverage | Copied valid and malformed NAR fixtures, serialization, import/export, and store exchange |
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

The ATerm parser follows CppNix's ordering, duplicate and escape rules, and the masked-hash fixtures check input-addressed path computation. Deferred outputs and zero-output derivations parse and round-trip as CppNix accepts them; zero outputs fail verification as CppNix's type check does. Floating content-addressed and impure outputs and `DrvWithVersion` are rejected, which matches CppNix only while `ca-derivations`, `impure-derivations` and `dynamic-derivations` are disabled; configurations enabling them remain gaps. They are listed in `crates/compat-tests/tests/upstream_derivations.rs`. This is not a measured coverage percentage. Qualification requires a pinned Nixpkgs revision, completed interface inventory, test results, and release artifacts as defined in [verification](verification.md).
