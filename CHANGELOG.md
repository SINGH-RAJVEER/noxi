# Changelog

Changes to each crate in this workspace, grouped by crate and version, newest first. Dates are commit dates. A version marked *unpublished* exists only in this repository. Published versions are permanent on crates.io, so add a new version section before raising a crate's version, and keep the entries for already published versions unchanged.

Changes that have not reached a version yet go under an `Unreleased` heading for that crate.

| Crate | Current version | crates.io | License |
|---|---|---|---|
| [`nix-rs-cli`](#nix-rs-cli) | 0.1.0 | [published](https://crates.io/crates/nix-rs-cli) | Apache-2.0 |
| [`nix-rs-eval`](#nix-rs-eval) | 0.1.0 | [published](https://crates.io/crates/nix-rs-eval) | GPL-3.0-only |
| [`nix-rs-eval-builtin-macros`](#nix-rs-eval-builtin-macros) | 0.0.1 | [published](https://crates.io/crates/nix-rs-eval-builtin-macros) | GPL-3.0-only |
| [`nix-compat`](#nix-compat) | 0.1.0 | unpublished; the name belongs to another owner | GPL-3.0 (Snix) |
| [`nix-compat-derive`](#nix-compat-derive) | 0.1.0 | unpublished; the name belongs to another owner | GPL-3.0 (Snix) |
| [`nix-rs-serde`](#nix-rs-serde) | 0.1.0 | unpublished | GPL-3.0 (Snix) |
| [`nixrs-compat-tests`](#nixrs-compat-tests) | 0.1.0 | internal, `publish = false` | none declared |
| [`nix-compat-derive-tests`](#nix-compat-derive-tests) | 0.1.0 | internal | GPL-3.0 (Snix) |

The imported crates come from [Snix](https://git.snix.dev/snix/snix) revision `22ca63bdb71c221001526304eca934385fb47c26` (2026-09-24), copied on 2026-09-25. Import-time changes common to all of them: formatting with the root `rustfmt.toml` (hard tabs, width 4), tab-indented TOML, removal of Snix's per-crate `default.nix` build files, and renaming of package names, Rust imports, test paths and project-facing diagnostics from their Snix identifiers.

## nix-rs-cli

The `nixrs` preview command. It evaluates pure Nix expressions and does not own a store or daemon.

### 0.1.0 - 2026-09-29

Published to crates.io; `cargo install nix-rs-cli` installs the `nixrs` binary.

#### Added

- `nixrs eval --expr EXPRESSION` and `nixrs eval --file PATH` evaluate a pure expression and print the result, with the file path used in diagnostics. Exit status is 1 for evaluation or read errors and 2 for usage errors (2026-09-26).
- `nixrs --version` prints `nixrs 0.1.0 (preview)` (2026-09-26).
- Flake package `nixrs`, the flake's default package, and an app that runs it (2026-09-26).

#### Changed

- Evaluation deeply forces the result before printing, matching `nix-instantiate --eval --strict` and the differential runner in `nixrs-compat-tests` (2026-09-27).
- Licensed Apache-2.0, with crates.io metadata and a versioned dependency on `nix-rs-eval` (2026-09-29). The binary links the GPL-3.0 evaluator, so distributed binaries fall under GPL-3.0 terms.

## nix-rs-eval

Nix language parser, compiler and bytecode VM with the pure builtins, imported from `snix-eval`.

### 0.1.0 - 2026-09-29

Published to crates.io.

#### Added

- Imported from `snix-eval` at the Snix revision above (2026-09-25). The library is `nix_rs_eval`. The evaluation algorithms are unchanged from Snix; the placeholder `nixVersion` string now carries nix-rs branding.

#### Changed

- crates.io metadata: description, `GPL-3.0-only` license with the GPL text included, repository link, and a versioned dependency on `nix-rs-eval-builtin-macros` (2026-09-29).

#### Known gaps

- Pure evaluation only. Without Snix's `glue` crate there is no `derivation`, `derivationStrict`, fetchers or store-aware builtins, so Nixpkgs cannot be evaluated to `.drv` paths.
- 104 of the 467 CppNix 2.35.2 language fixtures pass. `crates/compat-tests/expectations/upstream-lang.txt` records the outcome of every case.

## nix-rs-eval-builtin-macros

Procedural macros for defining `nix-rs-eval` builtins, imported from `snix-eval-builtin-macros`.

### 0.0.1 - 2026-09-29

Published to crates.io.

#### Added

- Imported from `snix-eval-builtin-macros` at the Snix revision above (2026-09-25), keeping the original author entry.

#### Changed

- crates.io metadata: description, `GPL-3.0-only` license with the GPL text included, and repository link (2026-09-29).

## nix-compat

Nix formats and protocols: store paths, nixbase32, hashes, NAR, ATerm derivations, narinfo, realisations, flake references, `nix.conf` and daemon wire types.

### Unreleased

#### Changed

- Derivation ATerm parsing follows CppNix 2.35.2 instead of the 2.36.0 development rules. The five collections may appear in any order; a repeated output keeps the first entry, a repeated input derivation or environment key keeps the last, and repeated output names and input sources collapse. Input derivations may list no outputs. Every string field decodes escapes as 2.35.2 does, including store paths, output names and the platform, and raw newlines, carriage returns and tabs are accepted. Parsed derivations skip the builder's field validation. The `Unsorted`, `DuplicateMapKey`, `DuplicateInputSource`, `DuplicateInputDerivationOutputName` and `InputDerivationWithoutOutputs` parse errors are removed (2026-09-29).
- Input-addressed hashing lets the later of two input derivations with the same hash replace the earlier one's output names, as 2.35.2 does, instead of merging them (2026-09-29).
- The derivation platform, builder, arguments, environment keys and output names are byte strings. ATerm parsing and writing keep non-UTF-8 bytes and output names that a new derivation could not use, as 2.35.2 does; JSON serialisation returns an error for fields that are not UTF-8 (2026-09-29).
- `hash_derivation_modulo` and `HashDerivationModuloLookup` use `DrvHashModulo`, which keeps a fixed-output derivation's hash under its output name. A requested fixed output is masked as `out`, an empty request contributes nothing, and a missing output is an error, as in 2.35.2 (2026-09-29).

#### Added

- `cppnix_json`, with CppNix 2.35.2's JSON encodings of store paths, SRI hashes, content addresses, output specifications, and deriving paths. Nested deriving paths and `git` content addresses return `Error::Unsupported` with the experimental feature they need (2026-09-30).
- `cppnix_json::{PathInfo, NarInfo, Signature, PathInfoFormat}`: CppNix's `UnkeyedValidPathInfo` and `UnkeyedNarInfo` JSON in format versions 1, 2 and 3, including the version-1 fallbacks for a missing `version` and `storeDir` (2026-09-30).

### 0.1.0 - unpublished

#### Added

- Imported from Snix's `nix-compat` at the revision above (2026-09-25).
- BLAKE3 hashes: `NixHash::Blake3` and `HashAlgo::Blake3`, parsing and printing in the `blake3:` and SRI forms, and digesting. CppNix 2.35.2 gates these behind the `blake3-hashes` experimental feature, which nix-rs does not enforce yet (2026-09-26).
- `ValidPathInfo` can be written to the daemon wire (2026-09-27).
- `BuildResult` keeps built outputs by output name (`BuiltOutputs`, `UnkeyedRealisation`), with the string-map encoding for clients without the `realisation-with-path-not-hash` feature (2026-09-27).

#### Changed

- Derivation ATerm parsing follows CppNix's 2.36.0 development rules: outputs, input derivations, each input derivation's output names, input sources and environment keys must be strictly ascending without duplicates, and an input derivation must name at least one output. Store paths, output names and the platform may not contain escape sequences, and newline, carriage return and tab must be escaped in strings (2026-09-26).
- Derivation outputs can be deferred, and a parsed derivation may have no outputs. Verification rejects zero outputs and accepts deferred ones with a warning. Output environment variables are blanked in the masked form for every output when a derivation is built, and only where present when one is validated. Parsing no longer adds output environment variables (2026-09-26).
- Derivation outputs on the daemon wire are written with their count (2026-09-26).
- Worker protocol: `UnkeyedValidPathInfo` carries `ultimate`, signatures and CA only from protocol 1.16, and writes its NAR hash as Base16. Lengths read off the wire no longer size allocations directly (2026-09-27).
- `NixHash` ordering breaks ties on the algorithm, so equal digests of different algorithms stay distinct in ordered collections (2026-09-26).

#### Fixed

- `StorePath` ordering breaks ties on the name when two paths share a digest, as CppNix compares `<hash>-<name>`. Previously a set of input sources silently dropped one of them (2026-09-26).
- The NAR reader fails a copy whose destination accepts zero bytes instead of looping on it (2026-09-26).
- `Option<StorePath>` reports end of stream instead of an absent path, and `Option<CAHash>` reads CppNix's empty string as absent (2026-09-27).

#### Known divergences from CppNix 2.35.2

- 2.35.2 accepts the five ATerm collections in any order and resolves duplicates per collection, so `nix-compat` rejects its `hello.drv` and `firefox.drv` fixtures. Resolved in Unreleased.
- Input-addressed hashing merges the output names of input derivations whose hashes collide, as imported from Snix and as CppNix fixed in issue #16307 after 2.35.2. 2.35.2 overwrites one of them, so such derivations can get different output paths. Resolved in Unreleased.

## nix-compat-derive

Procedural macros for `nix-compat`'s daemon wire serialisation.

### 0.1.0 - unpublished

#### Added

- Imported from Snix's `nix-compat-derive` at the revision above (2026-09-25).

#### Fixed

- Deserialising an enum that does not cover every protocol version now reports the uncovered version as a compile error before generating the async code that triggered a rustc internal compiler error (2026-09-27).

## nix-rs-serde

Deserialises Nix values into Rust types, imported from `snix-serde`.

### 0.1.0 - unpublished

#### Added

- Imported from `snix-serde` at the revision above (2026-09-25). No behavior changes.

## nixrs-compat-tests

Internal test crate that checks the workspace crates against the CppNix 2.35.2 fixtures in `tests/nix/`. Not published.

### Unreleased

#### Added

- `reference_hash_modulo`, which verifies derivations the 2.35.2 reference wrote for CppNix issue #16307's graph (2026-09-29).
- `derivation_parser_extensions`, which checks that non-UTF-8 fields and unchecked output names round-trip through ATerm (2026-09-29).
- `differential_eval`, which checks the comparison runner against mock executables (2026-09-29).
- `fixture_inventory` and `expectations/fixtures.txt`, which classify every file copied into `tests/nix` and fail on an unclassified fixture, a stale pattern, or a pass claim that the named test does not back (2026-09-30).
- `upstream_store_json`, which checks `nix_compat::cppnix_json` against the store-path, content-address, outputs-spec and derived-path fixtures (2026-09-30).
- `upstream_path_info_json`, which checks path-info and nar-info JSON against the values CppNix's tests construct (2026-09-30).

#### Changed

- `hello.drv` and `firefox.drv` are expected to parse and canonicalise instead of being recorded as divergences (2026-09-29).
- `differential-eval` runs both engines in a cleared environment with temporary empty configuration, a dummy store and a per-command timeout (`NIXRS_DIFF_TIMEOUT_MS`, default 20 seconds), and records timeouts in its report (2026-09-29).

### 0.1.0 - unpublished

#### Added

- ATerm derivation fixture classification against `nix-compat` (2026-09-25).
- Malformed NAR fixtures with their expected error categories, and a stalled-copy check (2026-09-26).
- BLAKE3 digest vectors and hash JSON fixtures (2026-09-26).
- Worker-protocol fixtures decoded at the version each C++ test uses, with byte-identical re-encoding (2026-09-26).
- `differential-eval` binary, which compares `nixrs eval` with a reference `nix-instantiate`; `just eval-compare-reference` runs it against the pinned CppNix 2.35.2 build and matches 10 of 10 cases (2026-09-27, 2026-09-28).
- `upstream_lang`, which runs every copied language fixture against `nix-rs-eval` in its own process and compares each outcome with a reviewed expectation table (2026-09-28).

#### Changed

- Reference moved from an unreleased 2.36.0 development commit to the CppNix 2.35.2 release. The masked-hash, valid NAR round-trip, `bad-ca-fixed-path` and `windows-store-dir` checks were removed because 2.35.2 has no such fixtures, and `hello.drv` and `firefox.drv` are recorded as divergences (2026-09-28).
- Fixtures read from `tests/nix/` instead of `tests/upstream/nix/` (2026-09-29).

## nix-compat-derive-tests

Internal trybuild tests for `nix-compat-derive`.

### 0.1.0 - unpublished

#### Changed

- Compile-error snapshots updated for the renamed crate location and the pinned nightly toolchain's diagnostics (2026-09-25 to 2026-09-27).
