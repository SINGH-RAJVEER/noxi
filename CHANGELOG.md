# Changelog

Changes to each crate in this workspace, grouped by crate and version, newest first. New release entries use publication dates; older entries retain their recorded commit dates. A version marked *unpublished* or *pending publication* exists only in this repository. Published versions are permanent on crates.io, so add a new version section before raising a crate's version, and keep the entries for already published versions unchanged.

Changes that have not reached a version yet go under an `Unreleased` heading for that crate.

## Repository - Unreleased

- The reference inventory records the C and C++ library consumers on the pinned Nixpkgs: which packages link the 2.35.2 libraries, which link their own older Nix, which load into the `nix` process, and which only run the commands, with the libraries' pkg-config names, SONAMEs, headers and exported symbols. `scripts/library-consumers.nix` reproduces the input listing from the qualification lock.
- The license audit covers the files embedded since 2026-10-23 (`call-flake.nix`, `ca-specific-schema.sql`, `get-env.sh`, `config-settings.json`) and the Nix 2.3.16 test commit. A root `NOTICE` records each upstream project's revision and license, and `THIRD_PARTY.md` is regenerated from `noxi-cli`'s current closure, which contains every packaged executable. The flake package installs `NOTICE`, and its check tests for it.
- `noxi-flake` gains a `LICENSE` and a `PROVENANCE.md`; `noxi-daemon` and `noxi-eval-builtin-macros` gain a `PROVENANCE.md`; `noxi-commands` gains the LGPL `COPYING` for its copied CppNix files, and its `config.rs` no longer calls the CppNix setting metadata GPL-3.0-only. The two private test crates declare `GPL-3.0-only`.

## noxi 0.0.6 - 2026-11-19, pending publication

Publishes the evaluator performance and pure flake evaluation work. `noxi system build` now evaluates a NixOS flake in pure mode, using the evaluator store's allow list as CppNix's `allowPath` does and canonicalizing string paths before access. The evaluator enters bytecode frames directly, prints `genericClosure` trace text only when an error needs it, layers small `//` updates over large sets, skips builtin error contexts for evaluated arguments, reclaims uninterned strings by default (`NOXI_RECLAIM_STRINGS=0` restores the old policy), and stores thunks in a 40-byte cell. Locked flake inputs whose store path is already valid are no longer rehashed, derivations already in the store are not written again, and mimalloc no longer requests transparent huge pages. Evaluating a real NixOS configuration fell from 48.2 s and 5.21 GiB to 25.7 s and 3.67 GiB; CppNix 2.35.6 takes 10.1 s and 1.66 GiB.

It publishes `noxi-compat`, `noxi-store`, `noxi-eval`, `noxi-commands`, `noxi-daemon` and `noxi-cli` at 0.0.5, and `noxi-eval-builtin-macros`, `noxi-build`, `noxi-glue`, `noxi-flake`, `noxi-serde` and `noxi-system` at 0.0.4. `noxi-daemon`, `noxi-flake`, `noxi-serde`, `noxi-system` and `noxi-cli` have no source changes of their own and move because their exact requirements change.

## noxi 0.0.5 - 2026-11-17, pending publication

Publishes `noxi-daemon` 0.0.4 with `NOXI_DAEMON_IDLE_TIMEOUT`, which lets a socket-activated daemon exit when idle. It also publishes `noxi-commands` 0.0.4 with two changes: the `auto` store falls back to a per-user chroot store, and the store execution helper runs programs in a chroot when the store directory is missing. Together they let a user without root use noxi on a host without `/nix`, with no daemon. `noxi-system` 0.0.3 and `noxi-cli` 0.0.4 follow because their exact requirements on these crates change.

## noxi 0.0.4 - 2026-11-14, pending publication

Publishes the crates whose 0.0.3 publication failed (`noxi-compat` and `noxi-eval` and `noxi-store` at 0.0.4; `noxi-build`, `noxi-commands`, `noxi-daemon` and `noxi-glue` at 0.0.3) together with `noxi-flake` 0.0.3, `noxi-serde` 0.0.3, `noxi-system` 0.0.2, `noxi-cli` 0.0.3 and the new `noxi-events` 0.0.1. The 0.0.3 publication failed because the published `noxi-flake` 0.0.2 still required `noxi-glue` 0.0.2 and `noxi-store` 0.0.3, so verifying `noxi-commands` resolved two copies of the evaluator. `noxi-flake` now has its own version instead of the workspace version, and `noxi-serde` moves to 0.0.3 because its published 0.0.2 requires `noxi-eval` 0.0.3. The release carries the language compatibility work that matches all 466 active copied language fixtures, the rustc-style `noxi system` output, and `noxi explain`.

## noxi 0.0.3 - 2026-11-12

Release of the whole workspace. The seven previously published crates move to 0.0.3; `noxi-serde`, `noxi-glue`, `noxi-build`, `noxi-flake`, `noxi-daemon`, `noxi-commands` and `noxi-cli` are published for the first time under the `noxi-` names at 0.0.2; the new `noxi-system` crate is published at 0.0.1. The release carries the native `noxi flake update`, `noxi system` and `noxi snapshot` commands, the profile, `nix-env`, flake and channel compatibility work since 0.0.2, and the dependency bumps that tie each crate to the versions released together. Per-crate entries remain under `Unreleased` until they are split by version.

## noxi 0.0.2 - 2026-11-10

Patch release of the seven published crates: `noxi-hash`, `noxi-eval-builtin-macros`, `noxi-compat-derive`, `noxi-flakeref`, `noxi-compat`, `noxi-eval`, and `noxi-store`. It carries the evaluator diagnostics and arithmetic fixes, the store, build and profile work, flake reference and lock handling, and the pure-evaluation path guard that refuses symlink and `..` escapes from fetched sources. The per-crate entries below list each change under `Unreleased` until they are split by version.

## noxi 0.0.1 - 2026-10-31, publication in progress

The workspace is being published under the `noxi-` names. `noxi-hash`, `noxi-eval-builtin-macros`, `noxi-compat-derive`, `noxi-flakeref`, `noxi-compat`, `noxi-eval`, and `noxi-store` are published. crates.io rate-limited the remaining uploads. Historical `nix-rs-` entries below describe the previous package names.

| Crate | Current version | crates.io | License |
|---|---|---|---|
| [`nix-rs-cli`](#nix-rs-cli) | 0.1.1 | [published](https://crates.io/crates/nix-rs-cli/0.1.1) | Apache-2.0 |
| [`nix-rs-eval`](#nix-rs-eval) | 0.2.2 | [published](https://crates.io/crates/nix-rs-eval/0.2.2) | GPL-3.0-only |
| [`nix-rs-eval-builtin-macros`](#nix-rs-eval-builtin-macros) | 0.0.1 | [published](https://crates.io/crates/nix-rs-eval-builtin-macros) | GPL-3.0-only |
| [`nix-rs-compat`](#nix-rs-compat) | 0.1.2 | [published](https://crates.io/crates/nix-rs-compat/0.1.2) | GPL-3.0-only |
| [`nix-rs-hash`](#nix-rs-hash) | 0.1.0 | [published](https://crates.io/crates/nix-rs-hash/0.1.0) | GPL-3.0-only |
| [`nix-rs-flakeref`](#nix-rs-flakeref) | 0.1.0 | [published](https://crates.io/crates/nix-rs-flakeref/0.1.0) | GPL-3.0-only |
| [`nix-rs-compat-derive`](#nix-rs-compat-derive) | 0.1.0 | [published](https://crates.io/crates/nix-rs-compat-derive/0.1.0) | GPL-3.0-only |
| [`nix-rs-serde`](#nix-rs-serde) | 0.1.0 | [published](https://crates.io/crates/nix-rs-serde/0.1.0) | GPL-3.0-only |
| [`nix-rs-glue`](#nix-rs-glue) | 0.1.2 | [published](https://crates.io/crates/nix-rs-glue/0.1.2) | GPL-3.0-only |
| [`nix-rs-commands`](#nix-rs-commands) | 0.1.2 | [published](https://crates.io/crates/nix-rs-commands/0.1.2) | GPL-3.0-only |
| [`nix-rs-store`](#nix-rs-store) | 0.1.0 | [published](https://crates.io/crates/nix-rs-store/0.1.0) | GPL-3.0-only |
| [`nix-rs-build`](#nix-rs-build) | 0.1.0 | [published](https://crates.io/crates/nix-rs-build/0.1.0) | GPL-3.0-only |
| [`nixrs-compat-tests`](#nixrs-compat-tests) | 0.1.0 | internal, `publish = false` | none declared |
| [`nix-compat-derive-tests`](#nix-compat-derive-tests) | 0.1.0 | internal, `publish = false` | GPL-3.0 |

## nix-rs-hash

### Unreleased

- `CAHash::from_nix_hex_str` accepts base-16 and base-64 digests as well as nix32, chosen by length as CppNix's `ContentAddress::parse` does (2026-11-20).

### 0.1.0 - 2026-10-01

- Extracted Nix hash values, digest helpers, Nix base32 encoding, and their tests from `nix-compat`. `nix-compat` reexports the previous paths.

## nix-rs-flakeref

### Unreleased

- Expose fetcher-attribute parsing and formatting for evaluator builtins, with reference checks for common inputs and repeated query parameters. (2026-10-03).

#### Changed

- **Breaking:** `FlakeRef::Mercurial` has a required `url` field holding the transport URL without the `hg+` prefix or its `ref` and `rev` attributes (2026-10-03).

#### Fixed

- Parsing and formatting round trip for forge references with refs, revisions and hosts, transport query parameters (which stay out of the fetcher attributes and survive formatting), Mercurial and plain Git locations, explicit `file` and `tarball` types, and relative paths. A repeated query parameter keeps its first value, as CppNix 2.35.2's `decodeQuery` does, and boolean attributes accept only `1` (2026-10-03).

### 0.1.0 - 2026-10-01

- Extracted flake reference parsing, formatting, and their tests from `nix-compat`. The `flakeref` feature still exposes `nix_compat::flakeref`.

## noxi-system

### Unreleased

- New crate behind the native `noxi flake`, `noxi system` and `noxi snapshot` commands. `noxi flake update|lock` runs the shared lock algorithm in-process and reports changed, added and removed inputs; `noxi system build|test|boot|switch|dry-activate|rollback` evaluates and builds a NixOS toplevel through the in-process compatibility implementation, registers the system profile generation as `nix-env --set` does, runs `switch-to-configuration`, and reports built closure, running generation and boot default separately; `noxi snapshot list|show|diff|rollback` keeps a per-rebuild journal of source revision, lock copy, closure, generations and outcome under the state directory, and rolls back to a recorded generation with optional lock restoration. Offline integration tests use diverted profiles, activation programs and journal directories; no VM qualification yet (2026-11-12).
- `noxi system` prints cargo-style status verbs for each phase and a closing `Finished`, `Failed` or `Stopped` line with the elapsed time, and reports a failure as one rustc-style diagnostic with a stable code, notes for the phase and the resulting system state, and help. Color follows the terminal, `NO_COLOR`, `CLICOLOR_FORCE` and `TERM`. The `--json` output and the stdout report are unchanged (2026-11-14).

## noxi-events

### Unreleased

- New crate for structured operation events and their terminal rendering: a versioned event type with an emitter, null, recording, JSON and fan-out sinks and secret redaction; whole-line splitting of each activity's output; cargo-style status lines with the rustc palette; diagnostics rendered through `annotate-snippets`; and a table of stable diagnostic codes (2026-11-14).

## nix-rs-cli

The `nixrs` preview command. It evaluates pure Nix expressions and does not own a store or daemon.

### Unreleased

- The `noxi` binary dispatches `flake`, `system` and `snapshot` to `noxi-system` and drops its own mimalloc declaration, which `noxi-commands` now supplies through that dependency (2026-11-12).
- `noxi explain CODE` prints the summary and explanation of a diagnostic code (2026-11-14).

### 0.1.1 - 2026-10-01

- Depends on `nix-rs-eval` 0.2.0 so the preview reports integer-overflow and division-by-zero errors consistently in debug and release builds.
- Includes package-local README, license, and provenance notices. The command remains a pure-evaluation preview, not a CppNix replacement.

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

### Unreleased

- `builtins.appendContext` prints a context key that is not a store path as CppNix prints the symbol, quoted unless it is a plain identifier (`context key '"/bad"' is not a store path`) (2026-11-21).
- Avoid recursive JSON conversion, serialization and release, and release long attribute-set chains iteratively. The two recorded deep-value crashes now return controlled errors (2026-10-03).

### 0.2.2 - 2026-10-03

#### Added

- `builtins.break`, which returns its argument, as CppNix does without a debugger (2026-10-03).
- `builtins.convertHash`, after CppNix's `prim_convertHash`: SRI, `algo:digest` and bare digests in base16, nix32 or base64, printed as `base16`, `nix32`, `base32` (with CppNix's deprecation warning), `base64` or `sri`. BLAKE3 is refused as when the `blake3-hashes` experimental feature is disabled. The evaluator now depends on `nix-rs-hash` 0.1.0 (2026-10-03).
- `ErrorKind::EvalError`, for errors CppNix reports as a plain `EvalError` with a message (2026-10-03).
- `EvaluationBuilder::experimental_features` and `experimental_feature_enabled`, the CppNix experimental features builtins consult. `builtins.convertHash` accepts BLAKE3 when `blake3-hashes` is enabled (2026-10-03).

#### Fixed

- `builtins.fromJSON` and `builtins.fromTOML` reject NUL bytes and match CppNix's integer range, timestamp feature, and errors; `builtins.sort` uses stable peeksort and propagates catchable comparator errors; `builtins.genericClosure` orders keys using Nix comparison.
- The compiler rejects duplicate static attribute bindings and embedded NUL bytes in string literals, and derivation construction rejects invalid or empty output sets.
- `EvaluationBuilder::max_call_depth` applies the configured recursion limit to `builtins.toJSON`.
- `builtins.replaceStrings` forces a replacement only when it is first used, matches an empty pattern after the last character, and rejects `from` and `to` lists of different lengths, after CppNix's `prim_replaceStrings` (2026-10-03).
- `builtins.floor` and `builtins.ceil` return an integer argument unchanged and reject a rounded float outside the integer range (2026-10-03).
- `builtins.baseNameOf` removes one trailing slash before taking the last component, so `baseNameOf "a/"` is `"a"` (2026-10-03).
- `builtins.functionArgs` gives each attribute the position of its formal argument, for `builtins.unsafeGetAttrPos` (2026-10-03).
- A lone carriage return ends a `#` comment and counts as a line break, and a carriage return, alone or before a line feed, reads as a line feed in a `"` string, as in CppNix's lexer. Indented strings and escaped carriage returns are unchanged (2026-10-03).
- `builtins.toXML` and XML output write `null` as `<null />`, as CppNix does, instead of an open and a close tag (2026-10-03).
- `builtins.appendContext` adds a context to a string that had none, and keeps an existing context (2026-10-03).
- `abort` and `throw` coerce their message weakly, as CppNix does, and propagate a `throw` raised while coercing it (2026-10-03).
- `builtins.split` advances one byte after a zero-width match, including inside multibyte input, instead of one character (2026-10-03).
- `builtins.readDir` returns entries whose names are not valid UTF-8 (2026-10-03).

### 0.2.1 - 2026-10-02

#### Added

- `Evaluation::evaluate_in_dir` evaluates code that has no file, such as an `--expr` argument, with relative paths resolved against a given directory instead of the process's current directory. Diagnostics and `__curPos` still treat the code as having no file (2026-10-01).
- `EvaluationBuilder::get_nix_path` returns the search path set so far (2026-10-01).

`nix-rs-glue` and `nix-rs-commands` require this evaluator version for these methods.

### 0.2.0 - 2026-10-01

#### Changed

- Adds public `ErrorKind::IntegerOverflow` and diagnostic code `E043`. This changes an exhaustively matchable enum, so the evaluator uses a new minor version rather than a patch release.
- Adds `Lambda::formals_ellipsis()` for callers implementing Nix function-argument behavior.
- Includes standalone provenance and the license text for bundled LGPL language fixtures.

#### Fixed

- Integer operators and arithmetic builtins return evaluation errors for overflow in debug and release builds instead of panicking or wrapping. Negating the minimum integer follows checked `0 - value`, and builtin division now rejects integer and float zero with the reference's divisor-first behavior. Full reference diagnostic rendering remains open.

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

## nix-rs-compat

Nix formats and protocols: store paths, nixbase32, hashes, NAR, ATerm derivations, narinfo, realisations, flake references, `nix.conf` and daemon wire types.

### Unreleased

- `cppnix_json::derivation::Derivation::from_aterm` ports CppNix 2.35.2's `parseDerivation` to the full derivation model: floating, impure and text-hashed outputs and the `DrvWithVersion("xp-dyn-drv", ...)` form with nested dynamic inputs parse behind the experimental features the reference requires, with its error messages, its optional list separators, its first-output-wins and last-entry-wins duplicate rules, lexical store path canonicalisation and fixed-output hashes in any encoding. `cppnix_json::derivation_aterm::parse_store_path` is `parseStorePath` with the reference's messages (2026-11-20).
- `cppnix_json::dump` reproduces nlohmann's `json::dump()`, and `Derivation::unparse` writes structured attributes with it, so `__json` floats (`1e+100`, `1.5e-07`) and the derivation paths that hash them match the reference (2026-11-20).
- Store path names, output names and output specifications reject a first dash-separated component of `.` or `..`, as CppNix's `checkName` does (2026-11-20).
- `cppnix_json::DrvOutput::parse` and `render` read and write the 2.35.2 `<derivation path>^<output>` form with the reference's messages; the older `realisation::DrvOutput` keeps the pre-2.35 `<hash>!<output>` form (2026-11-20).
- `log::logger` implements CppNix's simple, `internal-json` and progress-bar loggers behind one activity API (`Activity`, `BuildLog`, `log`, `warn`, `log_error`) and the worker's progress counters (`WorkerGuard`, `worker_update`). `filter_ansi_escapes_width` truncates to a terminal width, and `prefix_build_logs` now follows the selected format (2026-11-21).
- `Settings::forwarded` follows CppNix for settings that need an experimental feature: one given while its feature is disabled is ignored but still forwarded with its default once a later file, `NIX_CONFIG` or flag enables the feature, and none is forwarded while its feature is disabled (2026-11-09).
- `nixcpp::settings` parses integer settings with CppNix's `K`/`M`/`G`/`T` suffixes in each setting's own C++ type and signedness, reports invalid `max-jobs` and `sandbox` values with the reference's messages, and tracks CppNix's overridden flag: settings from the system `nix.conf` are no longer forwarded to a daemon (`Settings::forwarded`) (2026-11-09).

### 0.1.2 - 2026-10-03

#### Added

- `nixcpp::settings` reads configuration as CppNix 2.35.2 does: `$NIX_CONF_DIR/nix.conf`, the user configuration files from `NIX_USER_CONF_FILES` or the XDG directories in reverse order, then `NIX_CONFIG`; `include` and `!include` relative to the including file; `experimental-features` lines before the others; `extra-NAME` appending to list settings and their defaults; aliases; settings flags (`--option`, `--NAME`, `--no-NAME`, `--extra-NAME`); ignored settings of disabled experimental features; and the reference's warnings and error messages. The table of the 134 known settings and 21 experimental features is generated from the reference inventory by `scripts/settings-table.py` (2026-10-03).

#### Fixed

- The async NAR reader rejects reuse of a node after an abandoned child or an exhausted directory, and poisons its reader tree after an error or a cancelled, partially polled directory read. The three poisoning tests that were ignored now run (2026-10-03).

### 0.2.1 - 2026-10-02

#### Added

- `store_path::downstream_placeholder`, CppNix's `DownstreamPlaceholder::unknownCaOutput`: the placeholder string for an output of a content-addressed derivation that has not been built (2026-10-02).

#### Fixed

- `cppnix_serve` is compiled only with the `daemon` feature, whose wire implementations it needs. Enabling `serde` and `wire` without `daemon` failed to compile (2026-10-02).

### 0.1.0 - 2026-10-01

#### Changed

- Renamed the package from the occupied `nix-compat` name to `nix-rs-compat`; the library remains `nix_compat`. Registry dependencies on the extracted crates and derive macro are versioned. This is the first public release of this project's implementation, not an update to upstream's crate.
- Moved Nix hash and Nix base32 implementation into `nix-rs-hash`, retaining `nix_compat::nixhash`, `nix_compat::nixbase32`, and `format_sha256!` as reexports (2026-09-30).
- Moved flake reference parsing and formatting into `nix-rs-flakeref`, retaining `nix_compat::flakeref` behind the existing feature (2026-09-30).
- Derivation ATerm parsing follows CppNix 2.35.2 instead of the 2.36.0 development rules. The five collections may appear in any order; a repeated output keeps the first entry, a repeated input derivation or environment key keeps the last, and repeated output names and input sources collapse. Input derivations may list no outputs. Every string field decodes escapes as 2.35.2 does, including store paths, output names and the platform, and raw newlines, carriage returns and tabs are accepted. Parsed derivations skip the builder's field validation. The `Unsorted`, `DuplicateMapKey`, `DuplicateInputSource`, `DuplicateInputDerivationOutputName` and `InputDerivationWithoutOutputs` parse errors are removed (2026-09-29).
- Input-addressed hashing lets the later of two input derivations with the same hash replace the earlier one's output names, as 2.35.2 does, instead of merging them (2026-09-29).
- The derivation platform, builder, arguments, environment keys and output names are byte strings. ATerm parsing and writing keep non-UTF-8 bytes and output names that a new derivation could not use, as 2.35.2 does; JSON serialisation returns an error for fields that are not UTF-8 (2026-09-29).
- `hash_derivation_modulo` and `HashDerivationModuloLookup` use `DrvHashModulo`, which keeps a fixed-output derivation's hash under its output name. A requested fixed output is masked as `out`, an empty request contributes nothing, and a missing output is an error, as in 2.35.2 (2026-09-29).

#### Added

- `cppnix_serve`, with serve 2.x value codecs, versioned build options, and client/server handshakes. Copied fixtures check exact bytes and decoded values, including the deliberately lossy 2.6 build result; real serve commands remain unimplemented.
- `cppnix_json`, with CppNix 2.35.2's JSON encodings of store paths, SRI hashes, content addresses, output specifications, and deriving paths. Nested deriving paths and `git` content addresses return `Error::Unsupported` with the experimental feature they need (2026-09-30).
- `cppnix_json::{PathInfo, NarInfo, Signature, PathInfoFormat}`: CppNix's `UnkeyedValidPathInfo` and `UnkeyedNarInfo` JSON in format versions 1, 2 and 3, including the version-1 fallbacks for a missing `version` and `storeDir` (2026-09-30).
- `cppnix_json::{DrvOutput, Realisation, UnkeyedRealisation, BuildResult}`: CppNix's realisation and build-result JSON, realisation fingerprints, and signing and verification with Nix signing keys (2026-09-30).

### Pre-publication history

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

- 2.35.2 accepts the five ATerm collections in any order and resolves duplicates per collection, so the original import rejected its `hello.drv` and `firefox.drv` fixtures. Resolved in this project's first public release.
- Input-addressed hashing merged the output names of input derivations whose hashes collide, as imported from Snix and as CppNix fixed in issue #16307 after 2.35.2. 2.35.2 overwrites one of them, so such derivations could get different output paths. Resolved in this project's first public release.

## nix-rs-compat-derive

Procedural macros for `nix-compat`'s daemon wire serialisation.

### 0.1.0 - 2026-10-01

#### Added

- Imported from Snix's `nix-compat-derive` at the revision above (2026-09-25).
- Published package name is `nix-rs-compat-derive`; the library remains `nix_compat_derive`, preserving existing generated-code paths and dependency aliases.

#### Fixed

- Deserialising an enum that does not cover every protocol version now reports the uncovered version as a compile error before generating the async code that triggered a rustc internal compiler error (2026-09-27).

## nix-rs-serde

Serializes Rust values into Nix and deserializes Nix values into Rust types, imported from `snix-serde`.

### 0.1.0 - 2026-10-01

#### Added

- Imported from `snix-serde` at the revision above (2026-09-25), with local `from_value` and Rust-to-Nix serialization APIs recorded in the package provenance.
- Exposes `from_str`, `from_str_with_config`, `from_value`, `to_value`, and `to_string`; depends on `nix-rs-eval` 0.2.0 with standalone GPL/provenance notices.

## nix-rs-glue

### Unreleased

- `derivationStrict` writes structured attributes (`__json`) with `cppnix_json::dump` instead of serde_json, so floats such as `1.5e-7` are written `1.5e-07` as by the reference and the derivation gets the reference's store path. The `noxi-compat` dependency now enables its `serde` feature (2026-11-21).
- `builtins.getFlake`, `builtins.fetchurl` and `builtins.fetchTarball` force their argument before adding their "while evaluating" context, as CppNix does, so an error thrown by the argument no longer shows that frame; `generated_fetcher_validation` matches the reference again (2026-11-21).
- Add `addDrvOutputDependencies`, `parseFlakeRef` and `flakeRefToString` store-aware builtins, and configure writable evaluator stores through shared settings. (2026-10-03).

### 0.1.2 - 2026-10-03

- Requires `nix-rs-eval` 0.2.2 for the CppNix-compatible evaluator behavior.

#### Fixed

- Derivation construction rejects an output named `drvPath` and empty output sets, and reports duplicate output names using CppNix's error text.

#### Added

- `EvaluatorStore::writable(root)` evaluates against a diverted local store under an existing absolute `root` and registers text files, copied and filtered sources, and derivation ATerm bytes with their references. Expected source hashes are checked against the captured bytes before those bytes are registered, and source filters see logical `/nix/store` paths. `EvaluatorStore::read_only(root)` reads through the same diversion without opening the database for writing. System roots and symlinked store or state directories are rejected. Outputs are not built, and import-from-derivation is not supported (2026-10-03).
- `ReadOnlyStore` is now an alias of `EvaluatorStore` (2026-10-03).

#### Changed

- `configure` adds `builtins.fetchTree` only when the builder enables the `fetch-tree` experimental feature, which `flakes` implies, as in CppNix (2026-10-03).

### 0.1.1 - 2026-10-02

- Requires `nix-rs-eval` 0.2.1 for search-path inspection and explicit-directory evaluation.

#### Fixed

- `derivationStrict` returns a `throw` from `__ignoreNulls`, `__structuredAttrs`, `args`, `outputs`, an output name, or a value nested in a structured attribute as a catchable error, so `builtins.tryEval` catches it as CppNix 2.35.2 does. It previously reported a type error and the command exited 1 (2026-10-01).
- `configure` appends the built-in `<nix/...>` files to a search path already set on the builder instead of replacing it with `NIX_PATH` (2026-10-01).

### 0.1.0 - 2026-10-01

- Adapted store-aware builtins and derivation handling from the recorded Snix glue crates. Read-only evaluation computes derivation, file, and filtered-source store paths while retaining dependencies in memory; it does not write to the store or build derivations.
- Includes `derivation`, `derivationStrict`, path and text-file handling, and embedded Nix support expressions. Network fetchers and flake evaluation remain explicitly unsupported.
- Unix-only preview with unstable APIs, versioned registry dependencies, and bundled GPL/LGPL provenance notices.

## nix-rs-commands

### Unreleased

- `nix copy --to file://` accepts the default compression and unsigned caches, reports invalid `compression` values as a usage error, reads `secret-key` files before copying, and reports and uploads each missing path once instead of re-exporting its closure (2026-11-21).
- `nix derivation show` prints with `cppnix_json::dump`, so structured-attribute floats appear as the reference prints them (`1.5e-07`, `1e+100`). `language_boundaries` checks the `drvPath` of structured-attribute derivations with floats against the reference and against recorded paths, and `reference_nix` compares their `derivation show` output byte for byte (2026-11-21).
- `--log-format internal-json` is implemented: builds, cached outputs, failures, dependency failures, `file://` substitutions and `nix store prefetch-file` downloads print CppNix 2.35.2's `@nix` start, stop, result and msg lines, with builder `setPhase` lines as phase results and errors with `raw_msg`. `bar` and `bar-with-logs` use a port of CppNix's progress bar: on a terminal it draws the build and copy counters and the current activity, and redirected it prints like the raw logger with `NAME> ` log lines. `-Q` selects the raw logger, `nix` starts with the bar and stops it before handing the terminal to another program or printing results, and `multiline` formats are rejected as 2.35.2 rejects them. `reference_log_format` compares each case with the reference (2026-11-21).

- The store execution helper behind `nix shell`, `nix run`, `nix develop` and `nix fmt` runs programs in a temporary root of bind mounts when the logical store directory does not exist, as CppNix's `chrootHelper` does. A per-user store therefore works on a host without `/nix`. Sandboxed builds into the per-user chroot store are compared with the reference (2026-11-17).
- The `auto` store falls back to a per-user chroot store in `DATA_DIR/root` on Linux when `/nix/var/nix` and the daemon socket are missing and the user is not root, as CppNix's `resolveStoreConfig` does, with the same notice (2026-11-17).

- Pure flake evaluation reads stored `path:` inputs by name, resolves the built-in `<nix/...>` files, answers `pathExists` with false for paths it may not access, and allows `builtins.path` and `builtins.filterSource` for paths inside fetched sources, refusing others with CppNix's message. Every pure read and import is checked on its resolved path, so a symlink or `..` can no longer reach host files (2026-11-09).
- Daemon clients send only overrides through `SetOptions`, so an untrusted client on NixOS no longer prints a restricted-setting warning for every system `nix.conf` entry, and a missing `local?root=` store root is created as CppNix does (2026-11-09).

- Add diverted-store `nix-build` and selected `nix-store` operations. Build evaluation retains temporary roots, honors selected output names and creates optional permanent result roots; reference comparisons cover both output selections and six metadata/closure queries. (2026-10-03).

### 0.1.2 - 2026-10-03

- Requires `nix-rs-eval` 0.2.2 and `nix-rs-glue` 0.1.2 for the updated evaluator and derivation behavior.

#### Fixed

- `--max-call-depth` applies to recursive JSON conversion. The executable evaluates on a 64 MiB thread stack, matching CppNix's stack limit.

#### Fixed

- `nix-instantiate --eval --json` printed the JSON text as a quoted Nix string (2026-10-03).
- `nix-instantiate` no longer prints the evaluator's own lint warnings, such as unused bindings and legacy `let`, which CppNix does not have (2026-10-03).

#### Added

- `nix-instantiate` reads `nix.conf`, the user configuration files and `NIX_CONFIG` through `nix_compat::nixcpp::settings`, accepts settings flags such as `--option`, `--extra-experimental-features` and `--max-call-depth`, prints CppNix's configuration warnings, and enables the configured experimental features in the evaluator. `parse_args` takes the loaded settings. Most settings are accepted without changing behaviour yet (2026-10-03).
- `nix-instantiate` accepts `--xml` together with `--no-location` (the last of `--json` and `--xml` wins, as in CppNix), `-I` and `--include`, whose entries are searched before `NIX_PATH`, and `--show-trace` and `--no-show-trace`, which do not change the output yet (2026-10-03).
- `nix-instantiate` without `--eval` instantiates a selection that is a single derivation into an explicit diverted store given as `--store local?root=ROOT`, and prints its `.drv` path. `--eval --read-write-mode` evaluates with that store writable. Other `--store` values are rejected, `--json` requires `--eval`, and no builds or permanent roots are made (2026-10-03).

### 0.1.1 - 2026-10-02

- Requires `nix-rs-eval` 0.2.1 and `nix-rs-glue` 0.1.1 so registry builds include the corrected evaluation and builtin behavior.

#### Fixed

- `instantiate::run` resolves relative paths in `--expr` against the directory it is given rather than the process's current directory. The `nix-instantiate` binary passes its current directory, so its behavior is unchanged (2026-10-01).

### 0.1.0 - 2026-10-01

- Adds the partial `nix-instantiate --eval` command with strict/JSON output, expressions, files/stdin, attribute selection, and `--arg`/`--argstr`, backed by the read-only glue.
- Instantiation without `--eval` requires a writable store and remains unsupported. Full command/output/configuration compatibility is not qualified.
- Installing this crate provides the name `nix-instantiate`, which can shadow CppNix. Use an isolated installation prefix and an explicit executable path during development.

## nixrs-compat-tests

Internal test crate that checks the workspace crates against the CppNix 2.35.2 fixtures in `tests/nix/`. Not published.

### Unreleased

#### Added

- `reference_store`, with isolated fresh-store layout, path/NAR import and metadata, and bidirectional CppNix store checks. Five cases pass against the pinned 2.35.2 executables; broader store qualification remains open.
- `reference_hash_modulo`, which verifies derivations the 2.35.2 reference wrote for CppNix issue #16307's graph (2026-09-29).
- `derivation_parser_extensions`, which checks that non-UTF-8 fields and unchecked output names round-trip through ATerm (2026-09-29).
- `differential_eval`, which checks the comparison runner against mock executables (2026-09-29).
- `fixture_inventory` and `expectations/fixtures.txt`, which classify every file copied into `tests/nix` and fail on an unclassified fixture, a stale pattern, or a pass claim that the named test does not back (2026-09-30).
- `upstream_store_json`, which checks `nix_compat::cppnix_json` against the store-path, content-address, outputs-spec and derived-path fixtures (2026-09-30).
- `upstream_path_info_json`, which checks path-info and nar-info JSON against the values CppNix's tests construct (2026-09-30).
- `upstream_realisation_json`, which checks realisation and build-result JSON, realisation fingerprints, and signatures made with CppNix's test key (2026-09-30).
- `reference_daemon`, which runs unchanged reference clients against the reference daemon and the nix-rs daemon serving copies of one store and compares exit status, standard output and standard error (2026-10-02).
- `reference_gc`, which compares printed roots, live and dead paths, collection results and remaining paths, and specific deletions with the reference on copies of one store, and checks that a temporary root sent to the nix-rs GC socket during a collection keeps its path (2026-10-02).
- `reference_substitute`, which has the reference sign and copy a closure into `file://` caches with six compression methods and checks narinfo round trips, substitution metadata including signatures and derivers, unsigned and trusted caches, and corrupt NARs against the reference (2026-10-02).
- `upstream_lang` gives each case the configuration `common/init.sh` writes for functional tests through `NIX_CONF_DIR` (2026-10-03).
- `upstream_lang` runs `eval-*` cases through nix-rs's `nix-instantiate` with the arguments and `.flags` `lang.sh` uses, instead of the bare evaluator, and is no longer listed as an ignored test, which made `cargo nextest run` skip it (2026-10-03).
- `reference_substitute` also checks `BinaryCache::upload_closure`: a signed nix-rs upload has the same narinfo text and NAR bytes as the reference's uncompressed upload and round-trips, signatures survive key rotation, corrupt sources and failed publications advertise nothing, and the source keeps temporary roots until its handle is dropped. It checks that the same unsafe NAR URLs fail with both engines (2026-10-03).
- `reference_build`, which builds derivations the 2.35.2 reference wrote with both the reference and `nix-rs-build` in separate temporary stores and compares the outputs, `nix path-info --json` metadata, the reference's verification of the nix-rs store, a probe of the sandbox environment, and failure messages and statuses (2026-10-02).
- `upstream_derivation_options`, which reads and writes the six `derivation-options` JSON fixtures byte for byte and checks that `nix-rs-build` derives the same options, required system features, substitution and `uid-range` decisions from the eight advanced-attributes derivations as CppNix's `derivation-advanced-attrs.cc` (2026-10-02).

#### Changed

- `hello.drv` and `firefox.drv` are expected to parse and canonicalise instead of being recorded as divergences (2026-09-29).
- `differential-eval` runs both engines in a cleared environment with temporary empty configuration, a dummy store and a per-command timeout (`NIXRS_DIFF_TIMEOUT_MS`, default 20 seconds), and records timeouts in its report (2026-09-29).
- `differential-eval` records the signal that ended each run and never counts a run ended by a signal as a match, since a signal leaves no exit code and two different crashes would otherwise compare equal. Output that is not valid UTF-8 is also recorded as hex, and the report gives each executable's resolved path and SHA-256 (2026-10-01).
- `upstream_lang` records the signal that ended a `crash` case in its JSON report (2026-10-01).

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

## nix-rs-store

### Unreleased

- A binary cache's priority defaults to 0, as `StoreConfig::priority` does in 2.35.2, rather than 50, so a cache whose `nix-cache-info` sets `Priority` is tried after caches that set none (2026-11-21).
- HTTP cache requests follow `FileTransferSettings`: transient failures (connection errors, HTTP 408, 429 and most 5xx) are retried `filetransfer-retry-attempts` times with the reference's exponential backoff, `Retry-After` floor and optional jitter, each retry warned as `...; retrying in N ms (attempt k/n)`. Errors use curl's wording (`Could not connect to server (7) Failed to connect to HOST port PORT after N ms: ...`) and include a short response body. `connect-timeout` bounds connecting, the `retry-*` cache URL parameters override the global settings, 410 reads as a missing file, and an HTTP cache without `nix-cache-info` is refused as `does not appear to be a binary cache`. `BinaryCache::open_with` takes these settings (2026-11-21).
- Binary cache uploads follow `BinaryCacheStoreConfig`: `xz` is the default compression, and `lzma`, `compression-level`, `parallel-compression`, `write-nar-listing`, `index-debug-info`, `secret-key`, `secret-keys`, `priority` and `want-mass-query` URL parameters are read, with the reference's `unknown setting` warning for others. xz and lzma go through the system liblzma, bzip2 uses level 9 and zstd writes 16 MiB frames with up to four workers, so xz, lzma, bzip2, zstd and uncompressed uploads are byte-identical to CppNix's. Unsigned uploads are allowed, the cache's `nar`, `build-trace-v2` and `log` directories are created as on open, an existing NAR is kept, and no lock file is left beside the narinfo. `BinaryCache::upload` publishes one path whose references are already cached (2026-11-21).
- Substitution from a binary cache reports CppNix's `substitute` and `copy-path` activities and the worker's substitution counts, and a cache's first narinfo query for a path reports `query-path-info` (2026-11-21).

- `unix_socket::connect` and `bind` handle paths longer than `sun_path` as CppNix's `bindConnectProcHelper` does, from a child in the socket's directory; the garbage collector's roots socket and its clients use them (2026-11-09).
- `add_perm_root` creates the directories up to a permanent root (`gc::make_root_symlink`), as Home Manager's first activation requires, and `LocalStoreConfig::diverted` creates a missing absolute root outside `/nix` (2026-11-09).

- Add NAR-hash inode deduplication and streamed compressed build-log imports, plus diverted-root validation and shared local-store settings. (2026-10-03).

- `BinaryCache::upload_closure` exports a path and its references to a `file://` cache as uncompressed NARs with signed narinfo files, publishing references first. NAR bytes are checked against the source's metadata before signing, existing signatures are kept when keys rotate, temporary files are not left behind, the root is advertised only after its closure, and the source holds temporary roots for the uploaded paths. HTTP destinations and read-only sources are rejected (2026-10-03).
- `BinaryCache::nar` refuses a narinfo `URL` in a `file://` cache that is absolute or has `.` or `..` segments, as CppNix does (2026-10-03).
- `LocalStore::verify_store` takes a log callback and reports CppNix's progress and error messages at their verbosity levels, checks the hashes of `.links` entries with `check_contents`, and reports filled-in NAR sizes (2026-10-02).
- `LocalStore::add_indirect_root` and `add_perm_root` create `gcroots/auto` links and permanent roots as CppNix does, and `CachedPath` carries the narinfo `FileSize` (2026-10-02).
- `gc` implements garbage collection after CppNix 2.35.2's `gc.cc`: roots under `gcroots` and `profiles` with indirect and stale `auto` links, live processes' temporary roots (removing stale files), runtime roots from `/proc`, the `keep-outputs` and `keep-derivations` closure rules, deletion of referrers before references under the exclusive `gc.lock`, `max-freed`, `.links` cleanup, and a `gc-socket/socket` server so clients of either implementation can add temporary roots during a collection. `collect_garbage` returns live or dead paths, deletes dead or specific paths, and reports freed bytes as CppNix estimates them (2026-10-02).

### 0.1.0 - 2026-10-02

- `binary_cache::BinaryCache` reads `file://`, `http://` and `https://` binary caches (HTTP through `ureq` with rustls; 404 and 403 mean a missing file, as in CppNix): `nix-cache-info` (rejecting a cache for another store directory), narinfo files, and NARs compressed with none, xz, bzip2, zstd, gzip or brotli. `LocalStore::substitute` substitutes a path and its references in cache priority order after CppNix's `PathSubstitutionGoal`, skipping copies not signed by a trusted key unless the cache has `trusted=1`, and imports each NAR with its hash and size checked (2026-10-02).
- `LocalStore::create_temp_dir_in_store` and `ca_hash_of` are public, for the build engine (2026-10-02).
- First registry release of the local-store preview with the schema-10 SQLite layout, validity/reference metadata, path registration, NAR import/export, canonicalization, and store locks.
- Two local unit tests and six isolated local-store comparisons pass, along with selected cache-substitution comparisons. Garbage collection, full cache behavior, daemon ownership, concurrency/crash recovery, and full existing-store downgrade qualification remain open.
- Every `LocalStore` handle to one store in a process appends to the same `temproots/<pid>` file. A second handle used to delete and recreate it, which hid the first handle's temporary roots from the garbage collector. Dropping the last handle now also removes the file, as CppNix's `~LocalStore` does (2026-10-01).

## nix-rs-daemon

### Unreleased

- `NOXI_DAEMON_IDLE_TIMEOUT` lets a socket-activated daemon exit with status 0 after the given number of seconds with no connection and no live worker. The service manager reactivates it for the next client. A self-bound daemon ignores the variable, and an invalid value is an error (2026-11-17).

- The daemon socket and its clients work at paths longer than `sun_path`. A connection refused during the handshake is reported as CppNix reports it, and the store is named `daemon` only for the default socket (2026-11-09).
- The daemon client creates the directories up to a permanent root before registering it, matching `IndirectRootStore::makeSymlink` (2026-11-09).

- Add a standalone diverted-store daemon, per-connection client settings, worker version 1.39, substitutable-path query handlers, optimisation and trusted framed build-log uploads. Reference comparisons cover client build cores, optimisation and log uploads; dedicated substitution-query wire comparisons and three further operations remain open (2026-10-03).

- Garbage collection, NAR imports, signatures and verification over the daemon: `CollectGarbage` with and without the `delete-dead-specific-referrers` feature, sending the collector's progress lines to the client as they happen; `AddToStoreNar` and `AddMultipleToStore`, which check signatures unless a trusted client asks otherwise and clear `ultimate` as CppNix does; `AddSignatures`; and `VerifyStore`, which refuses repair for untrusted clients and does not repair yet. `DaemonSettings` gains `gc_keep_outputs` and `gc_keep_derivations` (2026-10-02).
- Builds and store writes over the daemon: `AddToStore` (text, flat and recursive content addresses from a framed source), `AddTextToStore`, `AddIndirectRoot`, `AddPermRoot` for trusted clients, and `BuildPaths` and `BuildPathsWithResults`, which realise through `nix-rs-build` and report each build as an `actBuild` activity with one `resBuildLogLine` result per output line. Failed builds end their error with the log tail, as a CppNix daemon worker's do (2026-10-02).
- New crate: a Nix daemon library that speaks CppNix 2.35.2's worker protocol over a Unix socket. It performs the 1.38 handshake with feature negotiation, reports trust from peer credentials with `trusted-users`, `allowed-users` and `build-users-group`, forks a process per connection, sends log lines and structured errors on the stderr channel, applies `SetOptions` with CppNix's restricted-setting warnings for untrusted clients, and implements the path, referrer, deriver, derivation-output, validity, hash-part, temporary-root, root-listing, NAR and missing-path queries. Builds, imports, GC and permanent roots are not implemented yet, and there is no `nix-daemon` binary (2026-10-02).

## nix-rs-build

### Unreleased

- A substituter that cannot be opened is reported as CppNix's `getDefaultSubstituters` reports it, as a plain warning of the error with continuation lines indented, and HTTP substituters use the configured transfer and retry settings (2026-11-21).
- `LocalBackend::build_paths` runs as CppNix's worker for the loggers: `realise`, `builds` and `copy-paths` activities with the progress the goals report, one `build` activity per derivation whose output arrives as log lines, builder `@nix` messages handled as `handleJSONLogMessage` does, and intermediate failures logged through the logger. A carriage return now truncates the line it rewinds, as `BuildLog::flushLine` does (2026-11-21).

- Under a root daemon, sandboxed builders run as a member of `build-users-group` (`nixbld` by default) locked under `STATE_DIR/userpool`, as CppNix's `SimpleUserLock` does: the sandbox user maps to it, it owns the build directory, builder files and terminal, the chroot store is owned by root and the build group, its processes are killed before and after the build, and outputs it does not own are rejected. Previously such builders ran as host root and owned their bind-mounted inputs (2026-11-09).
- Apply shared build settings, honor substitution and fallback decisions, and reject local builds when max-jobs is zero. Execution remains sequential (2026-10-03).

- `LocalBuilder::realise_with_events` reports each build's start, output and end as `BuildEvent`s (2026-10-02).
- `missing::query_missing` follows CppNix's `queryMissing`: which paths realising some derived paths would build, substitute or not find, with download and NAR sizes (2026-10-02).
- `LocalBuilder` realises only the outputs a derivation uses from each input derivation, and `realise_outputs` realises the named outputs of a derivation (`drv^out,dev`). Previously every output of every input had to be valid, so a garbage-collected `dev` or `man` output caused the whole input to be rebuilt (2026-10-02).

### 0.1.0 - 2026-10-02

- First registry release of the experimental local builder, with versioned dependencies on `nix-rs-store` 0.1.0 and `nix-rs-compat` 0.1.1. The tested development target is x86_64-linux; this is not a complete build-engine or sandbox-security qualification.
- `LocalBuilder::with_substituters` tries configured binary caches for missing outputs and inputs before building. Complete substitution/fallback and build-engine behavior remains unqualified.

- Builds write their output to `LOGDIR/drvs/XX/REST.bz2`, or uncompressed with `compress-build-log = false`, unless `keep-build-log` is off, so `nix-store -l` reads the logs nix-rs writes. A failed build lists its outputs sorted by path and, with `show_log_tail`, ends with the last `log-lines` lines of output as CppNix's `nix build` does, leaving out `@nix` messages and text overwritten by a carriage return (2026-10-02).
- `LocalBuilder` builds derivations with structured attributes. `structured` writes `.attrs.json` with nlohmann's compact formatting, sorted keys and float notation, and `.attrs.sh` with CppNix's rules, including its conversion of numbers through `float` and 32-bit `int`. `exportReferencesGraph` entries become path-info JSON with closure sizes, and `outputs` maps each output to its path (2026-10-02).
- `local::LocalBuilder` realises a derivation in a local store, building its input derivations first, following CppNix 2.35.2's `DerivationBuildingGoal`, `DerivationBuilderImpl` and `ChrootLinuxDerivationBuilder`. It computes the input closure, locks and temporarily roots the outputs, removes invalid leftovers, builds the reference environment (including `passAsFile`, `exportReferencesGraph` and output placeholders), runs the builder in the sandbox with only the input closure visible, and then canonicalises, scans, orders and registers the outputs with the derivation as deriver. Fixed outputs are checked against their content address, and a mismatched output is kept under its actual path as CppNix does. `checkOutputs` size and reference rules apply. Builds without a sandbox, `builtin:` builders and floating content-addressed outputs are rejected (2026-10-02).
- The crate became a workspace member with a library entry point before `LocalBuilder` connected the sandbox to the local build path. Scheduling across concurrent or remote builders remains unimplemented (2026-10-02).
- `options::DerivationOptions` follows CppNix 2.35.2's `DerivationOptions<Inputs>`: it is generic over `StorePath` and `DerivedInput` inputs, reads `__sandboxProfile` and `__darwinAllowLocalNetworking`, maps content-addressed placeholders to the input outputs they stand for when `ca-derivations` is enabled, names a derivation's own outputs as `DrvRef::Output`, returns the warnings CppNix prints for top-level attributes that structured attributes ignore, and reads and writes CppNix's JSON form. `try_resolve`, `get_required_system_features`, `substitutes_allowed` and `use_uid_range` match their CppNix counterparts (2026-10-02).
- Saves draft build-option parsing, reference scanning, settings, seccomp, and Linux sandbox helpers. Until 2026-10-02 the crate had no entry point and was not in the Cargo workspace.
- Initial draft fixes were checked in a separate scratch crate before real sandbox builds were connected. The builder closes its inherited copy of the PID pipe's write end, which kept the parent's read to EOF waiting while the builder waited for the parent. A seccomp rule whose architecture or syscall number does not match skips to the next rule instead of returning `EPERM`, which denied every syscall; a test runs the filter program without installing it. The builder is killed and reaped on every error return after it starts. Reference scanning finds a hash split across more than two short writes (2026-10-01).
