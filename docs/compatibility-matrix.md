# Initial compatibility matrix

This is the first qualification target and its current evidence, not a claim of compatibility. Add a row for each additional reference release, architecture, or feature setting. Keep failures and unrun cases visible.

| Field | Initial target |
|---|---|
| Reference package manager | CppNix 2.36.0 |
| Architecture and OS | x86_64-linux, NixOS-derived system |
| Nixpkgs revision | To pin before system qualification |
| Experimental features | Inventory all features present in the reference; record enabled and disabled configurations |
| Store | Existing CppNix store and fresh store, both in isolated environments |
| Client and daemon pairings | Reference/reference, nix-rs/nix-rs, reference/nix-rs, and nix-rs/reference where supported by the reference protocol |
| Boot mode | UEFI virtual machine first; physical hardware support to be listed before release |

| Area | Current status | Evidence needed to pass |
|---|---|---|
| Nix language and evaluator | Unrun | Copied language suite and Nixpkgs evaluation comparison |
| Derivation and path formats | Partial fixture coverage; remaining gaps in `nix-compat` | All copied fixtures and byte comparisons; see `crates/compat-tests/` |
| Store and garbage collection | Unrun | Existing-store, concurrency, integrity, and return-to-reference tests |
| Build engine and sandbox | Unrun | Local and remote build behavior under the selected system |
| Binary caches and signatures | Unrun | Cache exchange and trust decisions |
| Daemon protocol | Unrun | Wire fixtures and client/daemon pairings |
| CLI and configuration | Unrun | Legacy and new command suites, settings, flakes, profiles |
| C and C++ libraries | Unrun | Consumer inventory and binary interoperability |
| NixOS activation | Unrun | Build, switch, boot, test, dry activation, rollback |
| Installed OS lifecycle | Unrun | Install, self-update, reboot, recovery |
| TUI and GPUI-based NixBox GUI parity | Unrun | Shared operations and events, preview/apply, failures, recovery, accessibility, headless fallback |
| NixBox engine integration | Unrun | Locked-revision resolution, evaluated candidate plans, direct realization, declarative persistence, and recovery |
| OS-native CLI diagnostics | Unrun | Structured rebuild events, concise errors, raw-log retrieval, JSON, TTY, and CI modes |

The ATerm parser now accepts repeated input-source paths and canonicalizes them to a set. Two copied derivation fixtures exercise that behavior. Other fixture gaps remain, and this is not a measured coverage percentage. Qualification requires a pinned Nixpkgs revision, completed interface inventory, test results, and release artifacts as defined in [verification](verification.md).
