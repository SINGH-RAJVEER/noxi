# Packaged notices

`THIRD_PARTY.md` is copied from the `mvp/license-audit` branch. It covers the `nixrs` preview, including imported Snix code and the embedded CppNix derivation helper. It is not a complete dependency notice for the commands and daemon executables. The audit branch's `docs/licensing.md` remains the source of the broader audit and its unresolved decisions.

The flake package installs that notice and every existing workspace `LICENSE`, `COPYING`, `NOTICE` and `PROVENANCE.md` under `share/doc/nix-rs`, keeping the source directory structure. It also installs a root `LICENSE` or `NOTICE` if one exists. There is currently no root `LICENSE`.

The audit records a conflict between the preview crate's Apache-2.0 declaration and the GPL-3.0-only libraries it links. It also leaves the commands/daemon dependency notices and some imported-source provenance unresolved. Installing the available notices does not close the release license audit. This branch changes packaging only and does not change any crate's license declaration.
