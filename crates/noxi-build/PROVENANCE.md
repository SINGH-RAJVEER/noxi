# noxi-build provenance

## Implementation and reference

The Rust implementation is maintained for noxi at https://github.com/SINGH-RAJVEER/nix-rs under `crates/noxi-build/`. Its behavior follows and adapts routines from [CppNix 2.35.2](https://github.com/NixOS/nix/tree/2c73b59da29606068c0c98db015dd3a66955525d), revision `2c73b59da29606068c0c98db015dd3a66955525d`, under LGPL-2.1-or-later.

Reference scopes include `src/libstore/build` and `src/libstore/unix/build`, especially derivation building goals, builder setup, derivation options, Linux sandboxing, syscall filters, structured attributes, output checks, reference scanning, settings, logs, and failure messages. Adapted reference material retains the Nix authors' and contributors' authorship and license notices. [COPYING](COPYING) supplies the reference LGPL 2.1 text; [LICENSE](LICENSE) supplies GPLv3 for this package.

The package depends on the separate `noxi-store` and Snix-derived `noxi-compat` packages. Their own source scopes and license notices remain applicable; this archive does not include an imported Snix source tree or the repository-root copied CppNix fixture suite.

## Local changes and limits

Release `0.1.0` connects the local store to sandbox execution, cache substitution, output registration, structured-attribute generation, and persistent build logs. It includes fixes for PID-pipe lifetime, child cleanup, seccomp rule fallthrough, and reference scanning across short writes. Its public dependencies require store `0.1.0` and compatibility `0.1.1`.

Selected local builds and format cases are compared with the pinned reference in repository-owned tests. The current development target is x86_64-linux; other architectures, remote scheduling, full substitution/fallback behavior, complete build/store recovery, and sandbox security are not qualified. Publication does not establish drop-in package-manager or OS compatibility.
