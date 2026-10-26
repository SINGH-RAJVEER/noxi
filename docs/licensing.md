# Licensing and third-party code

This is the license audit for the MVP release item in [TODO section 8](../TODO.md#8-package-and-qualify-the-mvp-release). It inventories the imported and copied code, the copied tests, and the dependency licenses of the `nixrs` preview binary, and it records the conflicts that need a decision. The release notice is [THIRD_PARTY.md](../THIRD_PARTY.md). The audit covers the state of the repository on 2026-10-23; the license facts come from the manifests, the per-crate `LICENSE` and `COPYING` files, and the `PROVENANCE.md` files, not from a legal review.

## Project license

The repository has no root `LICENSE` file. Each crate carries its own license field and its own `LICENSE` or `COPYING` copy:

- Every library and tool crate declares `GPL-3.0-only`, and its `LICENSE` reproduces the GPLv3 text.
- `crates/nix-rs-cli` (the `nixrs` binary) declares `Apache-2.0`, with the Apache text in its `LICENSE`.

The effective license of the `nixrs` binary is GPL-3.0-only, because it links GPL-3.0-only crates. See [Conflicts](#conflicts).

## Imported code

Each row lists the upstream project, the revision that was imported, the license that applies, and the files. The revision and license for each crate are also recorded in its `PROVENANCE.md`.

| Component | Local location | Upstream and revision | License that applies | Notes |
|---|---|---|---|---|
| Snix evaluator, builtins, value types | `crates/nix-rs/eval` (`nix-rs-eval`) | Snix `snix/eval`, `22ca63bdb71c221001526304eca934385fb47c26` (2026-09-24), copied 2026-09-25 | GPL-3.0-only | Compiled into `nixrs`. |
| Snix builtin macros | `crates/nix-rs/eval/builtin-macros` | Snix `snix/eval/builtin-macros`, same revision | GPL-3.0-only | Proc macro run at build time by `nix-rs-eval`. Has `LICENSE` but no `PROVENANCE.md`. |
| Snix hash and flake reference | `crates/nix-rs-hash`, `crates/nix-rs-flakeref` | Snix `snix/nix-compat/src`, same revision, extracted 2026-09-30 | GPL-3.0-only | `nixrs` uses `nix-rs-hash`. |
| Snix serde | `crates/nix-rs/serde` (`nix-rs-serde`) | Snix `snix/serde`, same revision | GPL-3.0-only | |
| Snix compat library and derive macros | `crates/nix-rs/nix-compat`, `crates/nix-rs/nix-compat-derive` | Snix `snix/nix-compat`, `snix/nix-compat-derive`, same revision | GPL-3.0-only | |
| Snix glue adaptations | `crates/nix-rs-glue` | Snix `snix/glue/src/builtins/derivation.rs`, `snix/build-glue/src/known_paths.rs`, and the Nix-derived wrapper `snix/glue/src/derivation.nix`, same revision | GPL-3.0-only for Rust; see the `derivation.nix` note under [Open items](#open-items) | Derivation and context handling, known-path tracking. |
| CppNix derivation helper | `crates/nix-rs/eval/src/compiler/imported-drv-to-derivation.nix` | CppNix 2.35.2, `src/libexpr/imported-drv-to-derivation.nix`, `2c73b59da29606068c0c98db015dd3a66955525d` | LGPL-2.1-or-later | Adapted: layout and notice changed. Embedded in `nixrs` through `include_str!`. |
| CppNix `fetchurl.nix` | `crates/nix-rs-glue/src/corepkgs/fetchurl.nix` | CppNix 2.35.2, `src/libexpr/fetchurl.nix`, same revision | LGPL-2.1-or-later | Verbatim copy. Not in the `nixrs` binary. |
| CppNix store schema | `crates/nix-rs-store/src/schema.sql` | CppNix 2.35.2, `src/libstore/schema.sql`, same revision | LGPL-2.1-or-later | Verbatim copy, keeps its notice. Not in the `nixrs` binary. |
| CppNix behavior, no copied source | `crates/nix-rs-store`, `crates/nix-rs-commands`, `crates/nix-rs/nix-compat` | CppNix 2.35.2, same revision | Project code, GPL-3.0-only | Per their `PROVENANCE.md`, these follow CppNix behavior and do not copy its source. |
| CppNix behavior, adapted routines | `crates/nix-rs-build` | CppNix 2.35.2, same revision | Project code, GPL-3.0-only | `PROVENANCE.md` says it "adapts routines". Whether any file is a translation is open. See [Open items](#open-items). |

The Snix revision is the one recorded in [crates/nix-rs/PROVENANCE.md](../crates/nix-rs/PROVENANCE.md), which also lists the Snix components that were not copied. CppNix behavior is taken from the pinned reference in [tests/nix-snapshot.lock](../tests/nix-snapshot.lock).

## Copied tests

| Copied set | Local location | Upstream and revision | License that applies | Notes |
|---|---|---|---|---|
| CppNix test snapshot | `tests/nix/` (2,098 files) | CppNix tag `2.35.2`, commit `2c73b59da29606068c0c98db015dd3a66955525d`, recorded in [tests/nix-snapshot.lock](../tests/nix-snapshot.lock) | LGPL-2.1 ([tests/nix/COPYING](../tests/nix/COPYING)) | Each copied path's Git object ID is recorded in the lock file. One file carries another license: see below. |
| BSD-2 test helper | `tests/nix/src/libutil-test-support/include/nix/util/tests/gtest-with-params.hh` | Emil Eriksson, 2014, as marked in the file header | BSD-2-Clause | Its SPDX header is kept. It is not covered by the LGPL `COPYING`. |
| Nix 2.3.16 language cases | `crates/nix-rs/eval/src/tests/nix_tests` | Nix `tests/lang` at tag `2.3.16`, via Snix | LGPL-2.1-or-later ([crates/nix-rs/eval/COPYING](../crates/nix-rs/eval/COPYING)) | The import records the tag, not the exact commit. See [Open items](#open-items). |
| Evaluator and serde test cases | `crates/nix-rs/eval/src/tests/nix_rs_tests`, `crates/nix-rs/nix-compat` tests, `crates/nix-rs/serde` tests | Snix, same revision, plus local additions | GPL-3.0-only | Local additions are project-owned. The imported cases keep their upstream authors. |

Snapshot integrity is checked by `just nix-snapshot-verify`, which compares the copied tree with the object IDs in the lock file. The grep for license terms across `tests/nix/` found the LGPL text, the BSD-2 header, and copyright lines in comments and headers. It found no other license text.

Project-generated fixtures are not third-party copies, and each one names the reference it was checked against:

- `crates/compat-tests/fixtures/nar-reference` is generated from the CppNix 2.35.2 `nix-store --dump` output by `generate.py`, and its `provenance.json` records the reference executable and revision.
- `crates/compat-tests/fixtures/hash-modulo-collision` is project-authored derivation fixtures, added in commit `29ea399`.
- `tests/reference-inventory`, `tests/qualification`, and `tests/vm` are project-authored specifications and captured reference output. They copy no CppNix source.

## Dependency licenses

### Shipped dependency graph

The `nixrs` preview package is built from `crates/nix-rs-cli` only (see `flake.nix`). Its normal, non-dev dependency closure on `x86_64-unknown-linux-gnu` has 76 entries besides the package itself:

- 3 workspace crates, all `GPL-3.0-only`: `nix-rs-eval`, `nix-rs-eval-builtin-macros`, `nix-rs-hash`.
- 73 crates.io crates. Their license expressions are 44 `MIT OR Apache-2.0`, 9 `MIT/Apache-2.0`, 7 `MIT`, 3 `Apache-2.0 OR MIT`, 3 `Unlicense OR MIT`, and single entries for `Unlicense/MIT`, `Apache-2.0/MIT`, `Zlib`, `0BSD`, `CC0-1.0 OR MIT-0 OR Apache-2.0`, `CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception`, and `(MIT OR Apache-2.0) AND Unicode-3.0`.

Every term in that list is compatible with distributing a GPL-3.0-only binary. None of these crates is GPL-2.0-only, LGPL, AGPL, SSPL, BUSL, non-commercial, or unlicensed. Dual-licensed entries are satisfied by their MIT or Apache option. The embedded CppNix helper is LGPL-2.1-or-later and is not a Cargo dependency, so it is absent from this listing and is recorded in [THIRD_PARTY.md](../THIRD_PARTY.md) instead.

The full listing is at the end of this file.

### Whole-workspace policy check

`deny.toml` runs `cargo deny check licenses` over the whole non-dev graph on `x86_64-unknown-linux-gnu`. It passes with the accepted set below. That graph includes `nix-rs-store`, `nix-rs-commands`, and `nix-rs-daemon`, so it covers the dependency closures of the other binaries too, not only `nixrs`:

- License terms counted across the workspace: `Apache-2.0` 151, `MIT` 176, `ISC` 4, `BSD-3-Clause` 8, `BSD-2-Clause` 2, `Unicode-3.0` 19, `Unlicense` 4, `Zlib` 3, `Apache-2.0 WITH LLVM-exception` 3, `CC0-1.0` 2, `0BSD` 2, `bzip2-1.0.6` 1, `CDLA-Permissive-2.0` 1, `MIT-0` 1, and `GPL-3.0-only` 12 for the workspace crates. A crate with an OR expression is counted under each of its terms.
- `CDLA-Permissive-2.0` is `webpki-roots`, a bundled root-certificate data crate. It enters through `ureq`, which `nix-rs-store` uses, so it is in the closure of `nix-rs-store` and of every crate that depends on it, including `nix-rs-commands` and `nix-rs-daemon`. It is not in the `nixrs` closure. It is accepted as permissive, but a reviewer should confirm its terms before any of those binaries ships.
- `ISC` covers `ring`, `rustls`, `rustls-webpki`, and `untrusted`. They enter through the same `ureq` path, so they are in the same store-dependent closures and not in `nixrs`.
- Two workspace crates have no license field: `nix-compat-derive-tests` and `nixrs-compat-tests`. Both are `publish = false`, and `deny.toml` ignores them. Neither is distributed.

## Conflicts

### nix-rs-cli declares Apache-2.0 but links GPL-3.0-only code

`crates/nix-rs-cli` declares `Apache-2.0`, but it depends on `nix-rs-eval`, `nix-rs-eval-builtin-macros`, and `nix-rs-hash`, which are `GPL-3.0-only` and derive from Snix. The `nixrs` binary the flake builds is therefore a combined work, and its distributed terms are GPL-3.0-only. Apache-2.0 is generally treated as compatible with GPLv3, so the combination itself is not the problem. The declared license is. It understates the terms that apply when the binary is distributed, and the published `nix-rs-cli` 0.1.1 metadata says Apache-2.0. Whether this is acceptable is a licensing judgment for the maintainer, not something this audit settles.

This needs a decision, and it was not changed in this audit:

1. Relicense `nix-rs-cli` to `GPL-3.0-only` to match the binary. This changes the published crate metadata and needs a patch bump under AGENTS.md.
2. Keep `Apache-2.0` for the package's own source and state in the release notice that the binary is GPL-3.0-only. [THIRD_PARTY.md](../THIRD_PARTY.md) already does this.

### No other conflicts found

- The LGPL-2.1-or-later CppNix helper in `nix-rs-eval` is compatible with GPL-3.0-only when combined, and its notice is kept.
- The BSD-2-Clause header in `tests/nix` is compatible and is kept in place.
- No dependency in the shipped closure is GPL-2.0-only or otherwise incompatible.

## Release notices

[THIRD_PARTY.md](../THIRD_PARTY.md) is generated by `just third-party`. It runs `cargo about generate --locked --offline --fail` on `crates/nix-rs-cli`, using [about.toml](../about.toml) for the accepted set and [third-party.hbs](../third-party.hbs) for the template. It contains the bundled upstream table with revisions, a summary by license, and the full text of each license with the crates that use it. The `third-party` recipe then appends the LGPL-2.1 text for the bundled CppNix helper, because that helper is not a Cargo dependency and cargo-about does not see it. Generation fails if a license outside the accepted set appears. Two consecutive runs produce byte-identical output.

Current gaps for release artifacts:

- The flake's `nixrs-preview` package does not install `THIRD_PARTY.md`, `LICENSE`, or source revisions. Nothing installs them into the artifact yet, and no binary release exists yet (see the "Publish binaries" item in TODO section 8).
- `nix-rs-commands` and `nix-rs-daemon` are built by `just` but not packaged. Their closures are larger, and they need their own generated notice, for example `cargo about generate -m crates/nix-rs-commands/Cargo.toml`, before they ship.

## Open items

1. Decide the `nix-rs-cli` license (see [Conflicts](#conflicts)).
2. Add a root `LICENSE` file that states the project license.
3. Install `THIRD_PARTY.md` and the root `LICENSE` in the packaged artifact, and generate a notice for each packaged binary.
4. Confirm whether any file in `crates/nix-rs-build` translates CppNix C++ code. If one does, it needs a file-level LGPL-2.1-or-later notice with the CppNix revision, as the copied files already have. The `PROVENANCE.md` wording "adapts routines" does not settle it.
5. The Nix 2.3.16 test import records a tag, not an exact commit. Record the commit, or state that the exact source is unknown.
6. `crates/nix-rs-glue/src/derivation.nix` keeps the LGPL header of the Nix file it wraps, while Snix's copy is GPL-3.0-only. The GPL notice does not relicense the Nix expression. Keep both notices and say so in the file's `PROVENANCE.md`, which already does.
7. Add `PROVENANCE.md` to `crates/nix-rs/eval/builtin-macros` and `crates/nix-rs-daemon`, which have a `LICENSE` but no provenance record.
8. Add a `license` field to the two `publish = false` test crates, for consistency with the rest of the workspace.

## Regenerating

```sh
just licenses        # cargo deny check licenses, against deny.toml
just third-party     # rewrites THIRD_PARTY.md from the locked graph
```

Both run inside `devenv shell`, which provides `cargo-deny` and `cargo-about`.

## Shipped dependency listing

Crates in the `nixrs` closure, excluding `nix-rs-cli` itself. The license is each crate's own `license` field.

| Crate | Version | License | Repository |
|---|---|---|---|
| aho-corasick | 1.1.5 | Unlicense OR MIT | https://github.com/BurntSushi/aho-corasick |
| allocator-api2 | 0.2.21 | MIT OR Apache-2.0 | https://github.com/zakarumych/allocator-api2 |
| arrayvec | 0.7.8 | MIT OR Apache-2.0 | https://github.com/bluss/arrayvec |
| blake3 | 1.8.7 | CC0-1.0 OR Apache-2.0 OR Apache-2.0 WITH LLVM-exception | https://github.com/BLAKE3-team/BLAKE3 |
| block-buffer | 0.10.4 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| bstr | 1.13.1 | MIT OR Apache-2.0 | https://github.com/BurntSushi/bstr |
| bytes | 1.12.1 | MIT | https://github.com/tokio-rs/bytes |
| cfg-if | 1.0.4 | MIT OR Apache-2.0 | https://github.com/rust-lang/cfg-if |
| codemap | 0.1.3 | MIT/Apache-2.0 | https://github.com/kevinmehall/codemap |
| codemap-diagnostic | 0.1.2 | MIT/Apache-2.0 | https://github.com/kevinmehall/codemap-diagnostic |
| constant_time_eq | 0.4.2 | CC0-1.0 OR MIT-0 OR Apache-2.0 | https://github.com/cesarb/constant_time_eq |
| countme | 3.0.1 | MIT OR Apache-2.0 | https://github.com/matklad/countme |
| cpufeatures | 0.2.17 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| cpufeatures | 0.3.1 | MIT OR Apache-2.0 | https://github.com/RustCrypto/utils |
| crypto-common | 0.1.7 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| data-encoding | 2.11.1 | MIT | https://github.com/ia0/data-encoding |
| digest | 0.10.7 | MIT OR Apache-2.0 | https://github.com/RustCrypto/traits |
| either | 1.18.0 | MIT OR Apache-2.0 | https://github.com/rayon-rs/either |
| equivalent | 1.0.2 | Apache-2.0 OR MIT | https://github.com/indexmap-rs/equivalent |
| foldhash | 0.2.0 | Zlib | https://github.com/orlp/foldhash |
| genawaiter | 0.99.1 | MIT | https://github.com/whatisaphone/genawaiter |
| genawaiter-macro | 0.99.1 | MIT/Apache-2.0 | https://github.com/whatisaphone/genawaiter |
| generic-array | 0.14.7 | MIT | https://github.com/fizyk20/generic-array.git |
| hashbrown | 0.12.3 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| hashbrown | 0.14.5 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| hashbrown | 0.17.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/hashbrown |
| indexmap | 1.9.3 | Apache-2.0 OR MIT | https://github.com/bluss/indexmap |
| itertools | 0.12.1 | MIT OR Apache-2.0 | https://github.com/rust-itertools/itertools |
| itoa | 1.0.18 | MIT OR Apache-2.0 | https://github.com/dtolnay/itoa |
| lexical-core | 0.8.5 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-parse-float | 0.8.5 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-parse-integer | 0.8.6 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-util | 0.8.5 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-write-float | 0.8.5 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| lexical-write-integer | 0.8.5 | MIT/Apache-2.0 | https://github.com/Alexhuszagh/rust-lexical |
| md-5 | 0.10.6 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| memchr | 2.8.3 | Unlicense OR MIT | https://github.com/BurntSushi/memchr |
| nix-rs-eval | 0.2.2 | GPL-3.0-only | https://github.com/SINGH-RAJVEER/nix-rs |
| nix-rs-eval-builtin-macros | 0.0.1 | GPL-3.0-only | https://github.com/SINGH-RAJVEER/nix-rs |
| nix-rs-hash | 0.1.0 | GPL-3.0-only | https://github.com/SINGH-RAJVEER/nix-rs |
| nom8 | 0.2.0 | MIT | https://github.com/epage/nom-experimental |
| os_str_bytes | 6.6.1 | MIT OR Apache-2.0 | https://github.com/dylni/os_str_bytes |
| path-clean | 0.1.0 | MIT OR Apache-2.0 | https://github.com/danreeves/path-clean |
| proc-macro2 | 1.0.107 | MIT OR Apache-2.0 | https://github.com/dtolnay/proc-macro2 |
| quote | 1.0.47 | MIT OR Apache-2.0 | https://github.com/dtolnay/quote |
| regex | 1.13.1 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| regex-automata | 0.4.18 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| regex-syntax | 0.8.11 | MIT OR Apache-2.0 | https://github.com/rust-lang/regex |
| rnix | 0.14.0 | MIT | https://github.com/nix-community/rnix-parser |
| rowan | 0.16.1 | MIT OR Apache-2.0 | https://github.com/rust-analyzer/rowan |
| rustc-hash | 1.1.0 | Apache-2.0/MIT | https://github.com/rust-lang-nursery/rustc-hash |
| rustc-hash | 2.1.3 | Apache-2.0 OR MIT | https://github.com/rust-lang/rustc-hash |
| serde | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| serde_core | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| serde_derive | 1.0.229 | MIT OR Apache-2.0 | https://github.com/serde-rs/serde |
| serde_json | 1.0.151 | MIT OR Apache-2.0 | https://github.com/serde-rs/json |
| serde_spanned | 0.6.9 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| sha1 | 0.10.7 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| sha2 | 0.10.9 | MIT OR Apache-2.0 | https://github.com/RustCrypto/hashes |
| smol_str | 0.2.2 | MIT OR Apache-2.0 | https://github.com/rust-analyzer/smol_str |
| static_assertions | 1.1.0 | MIT OR Apache-2.0 | https://github.com/nvzqz/static-assertions-rs |
| syn | 1.0.109 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn |
| syn | 3.0.4 | MIT OR Apache-2.0 | https://github.com/dtolnay/syn |
| tabwriter | 1.4.1 | Unlicense/MIT | https://github.com/BurntSushi/tabwriter |
| termcolor | 1.4.1 | Unlicense OR MIT | https://github.com/BurntSushi/termcolor |
| text-size | 1.1.1 | MIT OR Apache-2.0 | https://github.com/rust-analyzer/text-size |
| thiserror | 2.0.20 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| thiserror-impl | 2.0.20 | MIT OR Apache-2.0 | https://github.com/dtolnay/thiserror |
| toml | 0.6.0 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| toml_datetime | 0.5.1 | MIT OR Apache-2.0 | https://github.com/toml-rs/toml |
| toml_edit | 0.18.1 | MIT OR Apache-2.0 | https://github.com/ordian/toml_edit |
| typenum | 1.20.1 | MIT OR Apache-2.0 | https://github.com/paholg/typenum |
| unicode-ident | 1.0.24 | (MIT OR Apache-2.0) AND Unicode-3.0 | https://github.com/dtolnay/unicode-ident |
| unicode-width | 0.2.2 | MIT OR Apache-2.0 | https://github.com/unicode-rs/unicode-width |
| vu128 | 1.1.0 | 0BSD | https://github.com/jmillikin/rust-vu128 |
| zmij | 1.0.23 | MIT | https://github.com/dtolnay/zmij |

## Packaged notices

The flake package installs [THIRD_PARTY.md](../THIRD_PARTY.md) and every workspace `LICENSE`, `COPYING`, `NOTICE` and `PROVENANCE.md` under `share/doc/nix-rs`, keeping the source directory structure. It also installs a root `LICENSE` or `NOTICE` if one exists; there is currently no root `LICENSE`. `THIRD_PARTY.md` covers the `nixrs` preview, including imported Snix code and the embedded CppNix derivation helper. It is not a complete dependency notice for the commands and daemon executables, so installing the available notices does not close the release license audit.
