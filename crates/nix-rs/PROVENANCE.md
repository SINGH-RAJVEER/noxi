# Imported Rust crate provenance

The crates in this directory are copied from Snix, a Rust reimplementation of Nix components.

| Field | Value |
|---|---|
| Upstream | https://git.snix.dev/snix/snix |
| Revision | `22ca63bdb71c221001526304eca934385fb47c26` (2026-09-24) |
| Copied on | 2026-09-25 |
| Upstream licence | GPL-3.0, text in [LICENSE](LICENSE). Snix's protocol buffer definitions are MIT, but none are vendored here. |

## What was copied and why

| Directory | Crate | Reason |
|---|---|---|
| `nix-compat/` | `nix-compat` | Nix formats and protocols: store paths, NAR, ATerm derivations, narinfo, realisations, `nix.conf`, daemon wire types. Its former nixbase32, hash, and flake reference modules are extracted to `crates/nix-rs-hash/` and `crates/nix-rs-flakeref/` and reexported. |
| `nix-compat-derive/` | `nix-compat-derive` | Proc macros that `nix-compat` uses for daemon wire serialisation |
| `nix-compat-derive-tests/` | `nix-compat-derive-tests` | Tests for those macros |
| `eval/` | `nix-rs-eval` (upstream `snix-eval`) | Nix language parser, compiler and bytecode VM, with the pure builtins |
| `eval/builtin-macros/` | `nix-rs-eval-builtin-macros` (upstream `snix-eval-builtin-macros`) | Proc macros for defining builtins |
| `serde/` | `nix-rs-serde` (upstream `snix-serde`) | Deserialises Nix values into Rust types, for reading configuration |

## What was not copied and why

| Snix component | Reason |
|---|---|
| `castore`, `store`, `nar-bridge`, `castore-http` | Built on Snix's own chunked content-addressed storage and gRPC services. nix-rs has to own the CppNix `/nix/store` layout and SQLite database, which this design does not provide. |
| `build`, `build-glue` | Sandboxes through OCI and bubblewrap behind a gRPC build protocol, not the CppNix build-user and namespace model |
| `glue` | Supplies `derivation`, `derivationStrict`, the fetchers, `import` and the store-aware builtins. It depends on `castore`, `store` and `build`. |
| `nix-daemon` | A thin adapter onto `castore`/`store`. The protocol types it relies on are already in `nix-compat::nix_daemon`. |
| `cli/*`, `tracing` | Snix-specific binaries and telemetry setup |

Without `glue`, `nix-rs-eval` evaluates only the pure language. It cannot instantiate derivations or fetch sources, so nothing in this workspace can evaluate Nixpkgs to `.drv` paths yet.

## Local modifications

- 2026-09-30: Evaluator integer operators and arithmetic builtins use checked addition, subtraction, multiplication, and division. Negation uses checked subtraction from zero, matching CppNix 2.35.2. Overflow yields `IntegerOverflow` rather than a debug panic or release wrapping, and builtin division now rejects integer and float zero before inspecting the numerator. Focused tests cover both profiles; reference diagnostic rendering remains incomplete.
- 2026-09-30: Added `cppnix_serve` value codecs and handshake functions following CppNix 2.35.2 commit `2c73b59da29606068c0c98db015dd3a66955525d`, especially `src/libstore/serve-protocol.cc` and its characterization tests. The reference source's license is recorded in `tests/nix/COPYING`. These 2.x codecs are separate from the imported worker-protocol types and preserve the reference's version-dependent, lossy fields; fixture coverage does not qualify serve commands or daemon pairings.
- 2026-09-30: Moved `nixbase32` and `nixhash` into the project-owned `nix-rs-hash` crate. `nix-compat` depends on it and reexports the existing module paths and `format_sha256!` macro. The extracted source remains derived from the recorded Snix revision and retains its GPL-3.0-only license.
- 2026-09-30: Moved `flakeref` into the project-owned `nix-rs-flakeref` crate. `nix-compat` reexports the existing module path through its `flakeref` feature. The extracted source remains derived from the recorded Snix revision and retains its GPL-3.0-only license.
- Reformatted with the root `rustfmt.toml` (hard tabs, width 4), and changed TOML indentation to tabs.
- Removed Snix's depot build files (each crate's `default.nix`). They depend on Snix's monorepo tooling.
- The workspace root `Cargo.toml` copies Snix's `[workspace.dependencies]` and `[workspace.package]`, and leaves out its `[patch.crates-io]` overrides of hyper and tonic, which none of these crates use.
- The root `Cargo.lock` began as Snix's lock file at the revision above.
- The imported directory was renamed to `crates/nix-rs/`; package names, Rust imports, evaluator test paths, and project-facing diagnostics were renamed from their upstream identifiers. The evaluator's placeholder `nixVersion` string changed from its upstream branding to nix-rs branding. These edits are not compatibility fixes.
- 2026-09-26: `StorePath` ordering now breaks ties on the name when two paths share a digest, matching CppNix's comparison of `<hash>-<name>` base names. Before, such paths compared equal, so a set of input sources silently dropped one of them. This replaced an earlier local change that accepted duplicate input sources and collapsed them.
- 2026-09-26: The derivation ATerm parser follows CppNix 2.36's parsing rules. Outputs, input derivations, the output names of each input derivation, input sources, and environment keys must be in strictly ascending order without duplicates. An input derivation must list at least one output. Store paths, output names and the platform must not contain escape sequences. Newline, carriage return and tab must be escaped in string fields.
- 2026-09-26: Derivation outputs can be deferred (input addressed, path not yet known), and a parsed derivation may have no outputs, as in CppNix's parser; verification rejects zero outputs and accepts deferred ones with a warning, as CppNix's validation does. Output environment variables are blanked in the masked form for every output when constructing a derivation, but only where present when validating one (CppNix's `maskOutputs`). Parsing no longer adds output environment variables. Derivation outputs on the daemon wire are written with their count, matching CppNix's `derivation::write`.
- 2026-09-27: Worker protocol fixes found by CppNix's worker-protocol fixtures. `Option<StorePath>` reports end of stream as such instead of an absent path, and `Option<CAHash>` reads CppNix's empty string as absent instead of as end of stream. `UnkeyedValidPathInfo` only carries `ultimate`, signatures and CA from protocol 1.16, and its NAR hash is written as Base16. `ValidPathInfo` can be written. Lengths read off the wire no longer size allocations directly. `BuildResult` keeps built outputs by output name (`BuiltOutputs`, `UnkeyedRealisation`), as CppNix 2.36 does, with its string-map encoding for clients without the `realisation-with-path-not-hash` feature.
- 2026-09-29: Derivation string fields follow CppNix 2.35.2's byte handling. The platform, builder, arguments, environment keys and output names are byte strings, so ATerm parsing and writing keep non-UTF-8 bytes and output names that CppNix accepts without validation; JSON serialisation still requires UTF-8. `hash_derivation_modulo` returns `DrvHashModulo`, which keeps a fixed-output derivation's hash under its output name, as CppNix's `DrvHash` does, instead of one digest for all outputs.
- 2026-09-30: Added `cppnix_json`, a new module with CppNix 2.35.2's JSON encodings for store paths, hashes, content addresses, output specifications, deriving paths, path info, narinfo, realisations and build results. The imported `serde` implementations are unchanged.
- Four imported trybuild diagnostic snapshots now reflect the renamed crate location and Rust 1.95.0 wording. The pinned toolchain is supplied by `devenv.nix`.

Apart from the checked arithmetic and division changes above, the imported evaluation algorithms have not been deliberately changed.

## Test status at import

`cargo test --workspace --all-features --no-fail-fast` on rustc 1.100.0-nightly (2026-09-15):

- All tests pass except one: `nix-compat-derive-tests`, test `ui`. It is a trybuild snapshot of compiler error messages. Snix pins Rust 1.95, and the error text differs on newer compilers; on this nightly, one of the compile-fail cases also triggers an internal compiler error. This is a toolchain difference, not a behaviour difference.
- `crates/compat-tests` checks `nix-compat` against the CppNix derivation fixtures in `tests/upstream/`. The gaps it records are listed in that test file.

## Refreshing from upstream

1. Check out the new Snix revision somewhere else.
2. Format its copies with this repository's `rustfmt.toml`, convert TOML indentation, and apply the local crate and identifier rename before comparing, so the diff shows behavior changes rather than branding changes.
3. Diff against this directory, apply the changes, and update the revision above.
4. Run the workspace tests and `crates/compat-tests`. Update the expectation table there if behaviour changed.
