# nix-rs-commands provenance

The command driver is project-owned source at https://github.com/SINGH-RAJVEER/nix-rs under `crates/nix-rs-commands/`. Initial release `0.1.0` contains the `nix-instantiate` binary and `instantiate` library module. Project ownership does not imply authorship of the linked evaluator and glue, whose original contributors and source scopes are recorded in their packaged provenance files.

The behavior reference is [CppNix 2.35.2](https://github.com/NixOS/nix/tree/2c73b59da29606068c0c98db015dd3a66955525d), revision `2c73b59da29606068c0c98db015dd3a66955525d`. Argument processing, attribute selection, automatic function calls, output, and read-only evaluation follow `src/nix-instantiate/nix-instantiate.cc` and `src/libexpr/attr-path.cc` as implemented so far. That reference source is LGPL-2.1-or-later, with its license at https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/COPYING.

This package bundles no imported Snix module tree or copied Nix fixture suite. It depends on the GPL-3.0-only Snix-derived evaluator and adapted glue, rooted in Snix revision `22ca63bdb71c221001526304eca934385fb47c26` at https://git.snix.dev/snix/snix. Glue separately bundles LGPL Nix expressions. [LICENSE](LICENSE) supplies GPLv3 for this command package; dependent source notices remain applicable to distributed binaries.

This preview only implements part of `nix-instantiate --eval`. It does not instantiate into a writable store, build, fetch, substitute, or provide full CppNix command behavior. Its binary name can shadow CppNix; the README documents isolated installation. No complete reference-release or system-rebuild qualification is claimed.

Release `0.1.1` corrects relative-path handling for callers that supply an evaluation directory and requires evaluator `0.2.1` and glue `0.1.1`. It adds no copied source or new license scope.
