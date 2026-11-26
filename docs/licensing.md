# Licensing and third-party code

This is the license audit for the MVP release item in [TODO section 8](../TODO.md#8-package-and-qualify-the-mvp-release). It inventories the imported and copied code, the copied tests, and the dependency licenses of the `noxi` preview binary, and it records the conflicts that need a decision. The release notice is [THIRD_PARTY.md](../THIRD_PARTY.md). The audit covers the state of the repository on 2026-10-23 and was updated on 2026-11-20 for the crates and embedded files added since; the license facts come from the manifests, the per-crate `LICENSE` and `COPYING` files, and the `PROVENANCE.md` files, not from a legal review.

## Project license

The repository has no root `LICENSE` file. Each crate carries its own license field and its own `LICENSE` or `COPYING` copy:

- Every library and tool crate declares `GPL-3.0-only`, and its `LICENSE` reproduces the GPLv3 text.
- `crates/noxi-cli` (the `noxi` binary) declares `Apache-2.0`, with the Apache text in its `LICENSE`.

The effective license of the `noxi` binary is GPL-3.0-only, because it links GPL-3.0-only crates. See [Conflicts](#conflicts).

## Imported code

Each row lists the upstream project, the revision that was imported, the license that applies, and the files. The revision and license for each crate are also recorded in its `PROVENANCE.md`.

| Component | Local location | Upstream and revision | License that applies | Notes |
|---|---|---|---|---|
| Snix evaluator, builtins, value types | `crates/noxi/eval` (`noxi-eval`) | Snix `snix/eval`, `22ca63bdb71c221001526304eca934385fb47c26` (2026-09-24), copied 2026-09-25 | GPL-3.0-only | Compiled into `noxi`. |
| Snix builtin macros | `crates/noxi/eval/builtin-macros` | Snix `snix/eval/builtin-macros`, same revision | GPL-3.0-only | Proc macro run at build time by `noxi-eval`; the code it generates is compiled in. |
| Snix hash and flake reference | `crates/noxi-hash`, `crates/noxi-flakeref` | Snix `snix/nix-compat/src`, same revision, extracted 2026-09-30 | GPL-3.0-only | `noxi` uses `noxi-hash`. |
| Snix serde | `crates/noxi/serde` (`noxi-serde`) | Snix `snix/serde`, same revision | GPL-3.0-only | |
| Snix compat library and derive macros | `crates/noxi/nix-compat`, `crates/noxi/nix-compat-derive` | Snix `snix/nix-compat`, `snix/nix-compat-derive`, same revision | GPL-3.0-only | |
| Snix glue adaptations | `crates/noxi-glue` | Snix `snix/glue/src/builtins/derivation.rs`, `snix/build-glue/src/known_paths.rs`, and the Nix-derived wrapper `snix/glue/src/derivation.nix`, same revision | GPL-3.0-only for Rust; see the `derivation.nix` note under [Open items](#open-items) | Derivation and context handling, known-path tracking. |
| CppNix derivation helper | `crates/noxi/eval/src/compiler/imported-drv-to-derivation.nix` | CppNix 2.35.2, `src/libexpr/imported-drv-to-derivation.nix`, `2c73b59da29606068c0c98db015dd3a66955525d` | LGPL-2.1-or-later | Adapted: layout and notice changed. Embedded in `noxi` through `include_str!`. |
| CppNix `fetchurl.nix` | `crates/noxi-glue/src/corepkgs/fetchurl.nix` | CppNix 2.35.2, `src/libexpr/fetchurl.nix`, same revision | LGPL-2.1-or-later | Verbatim copy, embedded by `noxi-glue`. |
| CppNix `call-flake.nix` | `crates/noxi-glue/src/corepkgs/call-flake.nix` | CppNix 2.35.2, `src/libflake/call-flake.nix`, same revision | LGPL-2.1-or-later | Indentation changed to tabs, SPDX and source header added. Embedded by `noxi-glue`. |
| CppNix store schemas | `crates/noxi-store/src/schema.sql`, `crates/noxi-store/src/ca-specific-schema.sql` | CppNix 2.35.2, `src/libstore/schema.sql` and `ca-specific-schema.sql`, same revision | LGPL-2.1-or-later | `schema.sql` is verbatim and keeps its notice; the other has tabs for indentation. Embedded by `noxi-store`. |
| CppNix `get-env.sh` | `crates/noxi-commands/src/nix/get-env.sh` | CppNix 2.35.2, `src/nix/get-env.sh`, same revision | LGPL-2.1-or-later | Byte-for-byte copy with no added notice, because `nix develop` adds its exact text to the store. Embedded by `noxi-commands`. |
| CppNix setting metadata | `crates/noxi-build/src/config-settings.json` | The 2.35.2 reference's `GlobalConfig::toJSON` output | LGPL-2.1-or-later for the description text | Embedded by `noxi-build` for `nix config show` and the settings a build hook is sent. Moved from `noxi-commands` on 2026-11-26. |
| CppNix behavior, no copied source | `crates/noxi-store`, `crates/noxi-commands`, `crates/noxi-flake`, `crates/noxi/nix-compat` | CppNix 2.35.2, same revision | Project code, GPL-3.0-only | Per their `PROVENANCE.md`, these follow CppNix behavior and copy no source apart from the files listed above. |
| CppNix behavior, close to the C++ | `crates/noxi-build`, `crates/noxi-daemon` | CppNix 2.35.2, same revision | Project code, GPL-3.0-only | `noxi-build`'s `PROVENANCE.md` says it "adapts routines"; `noxi-daemon` follows `daemon.cc`'s `performOp` and `TunnelLogger`. Whether any file is a translation is open. See [Open items](#open-items). |
| Project-owned crates | `crates/noxi-system`, `crates/noxi-events`, `crates/noxi-cli`, `crates/noxi-test-support` | None | GPL-3.0-only; `noxi-cli` declares Apache-2.0 | Each `PROVENANCE.md` states that no source is copied. |

Every file in this table that is embedded with `include_str!` is compiled into the packaged executables, because `noxi-cli` depends on `noxi-system`, which links `noxi-commands`, `noxi-glue` and `noxi-store`.

The Snix revision is the one recorded in [crates/noxi/PROVENANCE.md](../crates/noxi/PROVENANCE.md), which also lists the Snix components that were not copied. CppNix behavior is taken from the pinned reference in [tests/nix-snapshot.lock](../tests/nix-snapshot.lock).

## Copied tests

| Copied set | Local location | Upstream and revision | License that applies | Notes |
|---|---|---|---|---|
| CppNix test snapshot | `tests/nix/` (2,098 files) | CppNix tag `2.35.2`, commit `2c73b59da29606068c0c98db015dd3a66955525d`, recorded in [tests/nix-snapshot.lock](../tests/nix-snapshot.lock) | LGPL-2.1 ([tests/nix/COPYING](../tests/nix/COPYING)) | Each copied path's Git object ID is recorded in the lock file. One file carries another license: see below. |
| BSD-2 test helper | `tests/nix/src/libutil-test-support/include/nix/util/tests/gtest-with-params.hh` | Emil Eriksson, 2014, as marked in the file header | BSD-2-Clause | Its SPDX header is kept. It is not covered by the LGPL `COPYING`. |
| Nix language cases | `crates/noxi/eval/src/tests/nix_tests` (227 files) | Nix `tests/lang` at tag `2.3.16`, commit `8c5b39875edb3b88f5825cc76d1cae91b3afd130`, via Snix, plus cases from later Nix releases | LGPL-2.1-or-later ([crates/noxi/eval/COPYING](../crates/noxi/eval/COPYING)) | Compared on 2026-11-20 with that commit: 179 files are identical and 18 differ. Of the 30 files absent from it, 28 have the same name in CppNix 2.35.2's `tests/functional/lang` (15 identical, 13 different); `README.md` is Snix's import note, and the origin of `eval-fail-tail-call-1.nix` is unverified. |
| Evaluator and serde test cases | `crates/noxi/eval/src/tests/noxi_tests`, `crates/noxi/nix-compat` tests, `crates/noxi/serde` tests | Snix, same revision, plus local additions | GPL-3.0-only | Local additions are project-owned. The imported cases keep their upstream authors. |

Snapshot integrity is checked by `just nix-snapshot-verify`, which compares the copied tree with the object IDs in the lock file. The grep for license terms across `tests/nix/` found the LGPL text, the BSD-2 header, and copyright lines in comments and headers. It found no other license text.

Project-generated fixtures are not third-party copies, and each one names the reference it was checked against:

- `crates/compat-tests/fixtures/nar-reference` is generated from the CppNix 2.35.2 `nix-store --dump` output by `generate.py`, and its `provenance.json` records the reference executable and revision.
- `crates/compat-tests/fixtures/hash-modulo-collision` is project-authored derivation fixtures, added in commit `29ea399`.
- `tests/reference-inventory`, `tests/qualification`, and `tests/vm` are project-authored specifications and captured reference output. They copy no CppNix source.

## Dependency licenses

### Shipped dependency graph

The flake packages executables from `crates/noxi-cli`, `crates/noxi-commands` and `crates/noxi-daemon` (see `nix/package.nix`). Since `noxi-cli` gained its `noxi-system` dependency on 2026-11-12, its normal and build dependency closure on `x86_64-unknown-linux-gnu` contains `noxi-commands`, `noxi-daemon`, `noxi-store`, `noxi-build`, `noxi-flake` and their closures. `cargo tree` on 2026-11-20 found no crate in the `noxi-commands` or `noxi-daemon` closure that is missing from `noxi-cli`'s, so one notice generated from `noxi-cli` covers all three.

[THIRD_PARTY.md](../THIRD_PARTY.md), regenerated on 2026-11-20, lists 206 crates including `noxi-cli`. cargo-about assigns each crate one license from its expression: Apache-2.0 132, MIT 31, Unicode-3.0 19, GPL-3.0-only 13 (the workspace crates), BSD-3-Clause 6, ISC 3, 0BSD 1, CDLA-Permissive-2.0 1, Zlib 1, and bzip2-1.0.6 1.

Every term in that list is compatible with distributing a GPL-3.0-only binary. None of these crates is GPL-2.0-only, LGPL, AGPL, SSPL, BUSL, non-commercial, or unlicensed. Dual-licensed entries are satisfied by their MIT or Apache option. `CDLA-Permissive-2.0` is `webpki-roots`, a bundled root-certificate data crate, and `ISC` covers `ring`, `rustls-webpki` and `untrusted`; they enter through `ureq` in `noxi-store` and now ship in every packaged executable. `webpki-roots` is accepted as permissive, but a reviewer should confirm its terms before a qualified release. The embedded CppNix files are not Cargo dependencies, so they are absent from that listing and are recorded in [THIRD_PARTY.md](../THIRD_PARTY.md#bundled-upstream-sources) and [NOTICE](../NOTICE) instead.

### Whole-workspace policy check

`deny.toml` runs `cargo deny check licenses` over the whole non-dev graph on `x86_64-unknown-linux-gnu`. It passes with the accepted set below. That graph includes `noxi-store`, `noxi-commands`, and `noxi-daemon`, so it covers the dependency closures of the other binaries too, not only `noxi`:

- License terms counted across the workspace on 2026-10-23: `Apache-2.0` 151, `MIT` 176, `ISC` 4, `BSD-3-Clause` 8, `BSD-2-Clause` 2, `Unicode-3.0` 19, `Unlicense` 4, `Zlib` 3, `Apache-2.0 WITH LLVM-exception` 3, `CC0-1.0` 2, `0BSD` 2, `bzip2-1.0.6` 1, `CDLA-Permissive-2.0` 1, `MIT-0` 1, and `GPL-3.0-only` 12 for the workspace crates. A crate with an OR expression is counted under each of its terms. The check still passes on 2026-11-20; the counts were not recomputed.
- The two `publish = false` test crates, `nix-compat-derive-tests` and `noxi-compat-tests`, declare `GPL-3.0-only` since 2026-11-20. `deny.toml` still ignores private crates, and neither is distributed.

## Conflicts

### noxi-cli declares Apache-2.0 but links GPL-3.0-only code

`crates/noxi-cli` declares `Apache-2.0`, but it depends on `noxi-eval`, `noxi-eval-builtin-macros`, `noxi-hash` and, through `noxi-system`, the rest of the workspace, which are `GPL-3.0-only`. The executables the flake builds are therefore a combined work, and their distributed terms are GPL-3.0-only. Apache-2.0 is generally treated as compatible with GPLv3, so the combination itself is not the problem. The declared license is. It understates the terms that apply when the binary is distributed, and the published `noxi-cli` metadata says Apache-2.0. Whether this is acceptable is a licensing judgment for the maintainer, not something this audit settles.

This needs a decision, and it was not changed in this audit:

1. Relicense `noxi-cli` to `GPL-3.0-only` to match the binary. This changes the published crate metadata and needs a patch bump under AGENTS.md.
2. Keep `Apache-2.0` for the package's own source and state in the release notice that the binary is GPL-3.0-only. [THIRD_PARTY.md](../THIRD_PARTY.md) and [NOTICE](../NOTICE) already do this.

### No other conflicts found

- The LGPL-2.1-or-later CppNix files embedded in `noxi-eval`, `noxi-glue`, `noxi-store` and `noxi-commands` are compatible with GPL-3.0-only when combined. Their notices are kept, except in `get-env.sh`, whose text must stay byte-identical; its `PROVENANCE.md` entry and the release notices record it instead.
- The BSD-2-Clause header in `tests/nix` is compatible and is kept in place.
- No dependency in the shipped closure is GPL-2.0-only or otherwise incompatible.

## Release notices

[THIRD_PARTY.md](../THIRD_PARTY.md) is generated by `just third-party`. It runs `cargo about generate --locked --offline --fail` on `crates/noxi-cli`, whose closure contains every packaged executable's crate, using [about.toml](../about.toml) for the accepted set and [third-party.hbs](../third-party.hbs) for the template. It contains the bundled upstream table with revisions, a summary by license, and the full text of each license with the crates that use it. The `third-party` recipe then appends the LGPL-2.1 text for the bundled CppNix files, because they are not Cargo dependencies and cargo-about does not see them. Generation fails if a license outside the accepted set appears. Two consecutive runs on 2026-11-20 produced byte-identical output.

[NOTICE](../NOTICE) at the repository root is the short attribution: the project license, and each upstream project with its revision, license and what is compiled in. It is written by hand; update it with the bundled-sources table in [third-party.hbs](../third-party.hbs).

Current gaps for release artifacts:

- The flake installs `THIRD_PARTY.md`, `NOTICE`, and every workspace `LICENSE`, `COPYING` and `PROVENANCE.md` under `share/doc/noxi`, and the replacement package's check tests that `NOTICE`, `THIRD_PARTY.md` and the new `noxi-commands` `COPYING` and `noxi-flake` `LICENSE` are present. A root `LICENSE` is still absent. No qualified binary release exists yet (see the "Publish binaries" item in TODO section 8).

## Open items

1. Decide the `noxi-cli` license (see [Conflicts](#conflicts)).
2. Add a root `LICENSE` file that states the project license. This depends on item 1 and is the maintainer's decision.
3. Confirm whether any file in `crates/noxi-build` or `crates/noxi-daemon` translates CppNix C++ code. If one does, it needs a file-level LGPL-2.1-or-later notice with the CppNix revision, as the copied files already have. The `PROVENANCE.md` wording does not settle it.
4. `crates/noxi-glue/src/derivation.nix` keeps the LGPL header of the Nix file it wraps, while Snix's copy is GPL-3.0-only. The GPL notice does not relicense the Nix expression. Keep both notices; the file's `PROVENANCE.md` already says so. Its content equals CppNix 2.35.2's `src/libexpr/primops/derivation.nix` with tabs and a leading newline.
5. Find the origin of `crates/noxi/eval/src/tests/nix_tests/eval-fail-tail-call-1.nix`, which matches no file in Nix 2.3.16 or CppNix 2.35.2.
6. Confirm the terms of `webpki-roots` (`CDLA-Permissive-2.0`) before a qualified release.

Resolved on 2026-11-20: notices for every packaged executable (one `noxi-cli` notice covers them), the root `NOTICE`, the Nix 2.3.16 commit, `PROVENANCE.md` for `noxi-eval-builtin-macros`, `noxi-daemon` and `noxi-flake`, a `LICENSE` for `noxi-flake`, a `COPYING` for `noxi-commands`, and `license` fields for the two test crates.

## Regenerating

```sh
just licenses        # cargo deny check licenses, against deny.toml
just third-party     # rewrites THIRD_PARTY.md from the locked graph
```

Both run inside `devenv shell`, which provides `cargo-deny` and `cargo-about`.

## Shipped dependency listing

[THIRD_PARTY.md](../THIRD_PARTY.md) lists every crate in the packaged closure with its version under the license text that applies to it. This page no longer repeats that list, which went stale when the closure grew.

## Packaged notices

The flake package installs [THIRD_PARTY.md](../THIRD_PARTY.md), [NOTICE](../NOTICE), and every workspace `LICENSE`, `COPYING`, `NOTICE` and `PROVENANCE.md` under `share/doc/noxi`, keeping the source directory structure. It also installs a root `LICENSE` once one exists. `THIRD_PARTY.md` and `NOTICE` cover the `noxi`, `nix`, legacy `nix-*` and `nix-daemon` executables, including the imported Snix code and the embedded CppNix files.
