# noxi-system provenance

`noxi-system` is project-owned source at https://github.com/SINGH-RAJVEER/nix-rs under `crates/noxi-system/`. It implements the native `noxi flake`, `noxi system` and `noxi snapshot` operations on top of the workspace's flake, store and command crates. It copies no CppNix or Snix source. The NixOS generation semantics it follows (`build`, `test`, `boot`, `switch`, `dry-activate`, the system profile and `switch-to-configuration`) are the documented behavior of NixOS and of CppNix 2.35.2's profile handling, recorded in `docs/system.md`.

[LICENSE](LICENSE) supplies GPLv3 for this crate. The notices of the linked evaluator, glue and command crates remain applicable to distributed binaries.
