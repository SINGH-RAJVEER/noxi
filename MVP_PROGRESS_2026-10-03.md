# Package-manager MVP progress, 2026-10-03

The package-manager MVP is approximately **35% complete**, with a planning range of **30–40%**. This is an engineering judgment about delivering the complete replacement, not a measured CppNix compatibility percentage or a prediction of remaining calendar time. The current implementation has useful store, cache, builder and daemon components, but cannot replace system Nix.

The scope is the first deliverable in [PLAN.md](PLAN.md): a drop-in replacement for CppNix 2.35.2 on the pinned x86_64-linux NixOS target. The installer, new OS image, Lua configuration, NixBox TUI/GUI and fleet work follow that MVP. NixOS and Home Manager replacement testing is part of this first gate. [Scope and acceptance](TODO.md), [matrix](docs/compatibility-matrix.md)

## How the estimate was made

The following weights are explicit planning assumptions. Component completion estimates include behavior and qualification, rather than merely the existence of Rust code. They are deliberately coarse; no measured task-duration model exists in this audit.

| Area | Share of MVP | Estimated completion | Contribution |
|---|---:|---:|---:|
| Reference baseline and reproducibility | 5% | 90% | 4.5 points |
| Differential runner and evidence coverage | 10% | 40% | 4 points |
| Formats and cryptographic primitives | 10% | 70% | 7 points |
| Evaluation, source fetchers and flakes | 20% | 35% | 7 points |
| Store, caches and build engine | 20% | 45% | 9 points |
| Daemon, clients and authorization | 10% | 35% | 3.5 points |
| Commands, effective configuration and public libraries | 10% | 10% | 1 point |
| NixOS/Home Manager replacement and release qualification | 15% | 0% | 0 points |
| Total | 100% | | 36 points |

The weighted result rounds to approximately 35%. The estimate is sensitive to how much work complete command compatibility, diagnostics, public library consumers and system qualification require. The reference VM and preview packaging exist, but they have not demonstrated the final replacement outcomes counted in the last row.

The package-manager portion of `TODO.md`, before the first after-MVP heading and including nested items, has 38 checked entries and 62 open entries. That is a 38% checklist completion rate. It is not an effort-weighted metric: some checked entries are narrow fixtures, while individual open entries cover entire subsystems. Several unchecked entries contain substantial implemented work. [Checklist](TODO.md)

## Fresh verification

The source changed during the audit. Successful tests below describe the artifacts compiled around 15:08 IST on 2026-10-03. A later rebuild around 15:11 IST encountered incomplete concurrent printer changes; a final targeted type-check passed after those edits progressed. The Jujutsu parent was `7aebb972`; the working change was dirty and mutable. This report does not claim a clean or frozen final checkout, and the language results do not cover subsequent printer edits.

| Check | Result | Limit |
|---|---|---|
| `devenv shell -- cargo nextest run -p nixrs-compat-tests --test reference_store --test reference_build --test reference_substitute --test reference_gc --test reference_daemon --no-fail-fast` | 43 passed, 2 skipped | Temporary stores, library calls and CppNix clients. The optional network substitution and Nixpkgs hello tests were skipped in this run. |
| Fixture inventory, in the combined language/inventory run | 3 passed | Classification and backing-test checks; classification is not a pass for the classified behavior. |
| `devenv shell -- cargo nextest run -p nixrs-compat-tests --test upstream_lang --test fixture_inventory --no-fail-fast` | Language harness failed its recorded-outcome check | Three outcomes improved relative to the expectation table, detailed below. |
| Freshly built `nix-instantiate`, six direct comparisons with pinned CppNix | 3 exact matches, 3 mismatches | Small targeted sample, not representative coverage. |
| Later inventory rebuild | Compile failed | `PrintOptions` and `ValuePrinter` exports were missing, and `Builtin::has_partials` was unavailable in the sampled concurrent edit state. |
| Final `devenv shell -- cargo check -p nix-rs-commands` | Passed | The transient compile errors above were resolved by the concurrent edits. This is type-check evidence, not a fresh language-suite run. |

Backend run log: `/tmp/nix-rs-progress-reference-tests.log`. The later compile failure is in `/tmp/nix-rs-progress-inventory.log`, and the final passing type-check is in `/tmp/nix-rs-progress-final-check.log`. Generated evaluator evidence is in [the language report](target/tmp/upstream-lang-report.json), and the direct command sample is in [the command comparison](target/mvp-progress-command-comparison.json). These ignored build artifacts and temporary logs can be replaced by subsequent runs.

No full workspace CI, release-profile suite, VM lifecycle test or registry verification was run in this audit. No live store or daemon was replaced, and no implementation or expectation files were edited by this audit.

## Evaluator comparison

The copied language oracle is pinned to CppNix commit `2c73b59da29606068c0c98db015dd3a66955525d`. The local official checkout's HEAD matches that commit. The 467-case run compares against copied expected outputs and diagnostics; it does not execute a fresh reference process for every case. The separate six-case sample does execute both commands. [Runner](crates/compat-tests/tests/upstream_lang.rs), [reference lock](tests/nix-official.lock)

| Observed outcome | Cases |
|---|---:|
| Exact pass | 125 |
| Expected failure with different diagnostic | 307 |
| Different successful output | 25 |
| Unexpected failure | 5 |
| Crash | 2 |
| Unsupported flags | 2 |
| Reference-disabled | 1 |
| Total | 467 |

Exact language-fixture passes are **26.8%**, or 125/467. All 125 are successful-evaluation cases. The `eval-okay` subset has 144 cases, so 125/144, or **86.8%**, pass exactly. This indicates much better ordinary evaluation progress than the all-case rate alone suggests. It does not establish complete language semantics: successful output can miss effects, and error cases can fail for the wrong reason. Thirteen parse-success cases have no matching AST printer. [Observed report](target/tmp/upstream-lang-report.json)

The three expectation differences were improvements: `eval-okay-attrs6` and `eval-okay-overrides` moved from output mismatch to pass; `eval-fail-set-override` moved from unexpected success to a controlled error with a mismatching diagnostic. The table was not blessed during this audit. The harness remains red until the implementation and recorded outcomes are reviewed together.

The two crashes are `eval-fail-toJSON-stack-overflow` and `eval-fail-derivation-structuredAttrs-stack-overflow`, both SIGABRT after stack overflow. Missing successful behavior includes `addDrvOutputDependencies`, `parseFlakeRef`, `flakeRefToString`, `__nixPath`, and scoped import overrides. Two home-path cases reject `--impure`. Other mismatches include POSIX regex behavior, path existence, attribute source positions, trace formatting and warnings. [Observed report](target/tmp/upstream-lang-report.json)

The targeted live sample matched arithmetic, attribute override and one POSIX-class regex expression. With `flakes` enabled, CppNix's `parseFlakeRef` returned a structured reference and nix-rs failed because the builtin is absent. `getFlake` remains an explicit Rust stub; both commands failed in the sample's dummy store, for different reasons. Invalid JSON failed in both engines but with different diagnostic text and traces. [Sample](target/mvp-progress-command-comparison.json), [fetcher stubs](crates/nix-rs-glue/src/lib.rs)

## What has progressed

- Reproducible CppNix provenance, inventory, locked qualification inputs and reference VM infrastructure are in place. [Inventory](docs/reference-inventory.md), [qualification](tests/qualification/flake.nix)
- Hashes, NAR, store paths, ATerm/JSON derivations, narinfo, signatures and protocol values have substantial byte-level fixture coverage. Feature-enabled forms and integration remain separate gates. [Compatibility tests](crates/compat-tests/tests), [format evidence](docs/verification.md)
- The Rust store can register and exchange paths with CppNix in diverted stores; GC roots, live/dead paths and deletions have reference comparisons. [Store tests](crates/compat-tests/tests/reference_store.rs), [GC tests](crates/compat-tests/tests/reference_gc.rs)
- Local sandbox builds cover dependencies, multiple outputs, fixed-output checks, structured attributes, logs and selected failures. [Build tests](crates/compat-tests/tests/reference_build.rs)
- Cache substitution covers local and HTTP caches, signatures and compressed NARs. Signed `file://` uploads now exist, including key rotation and failed-publication cases. [Cache implementation](crates/nix-rs-store/src/binary_cache.rs), [cache tests](crates/compat-tests/tests/reference_substitute.rs)
- CppNix clients can query, import, build, collect garbage, sign and verify through the Rust daemon in the covered cases. This is useful interoperability evidence. [Daemon tests](crates/compat-tests/tests/reference_daemon.rs)

## Main shortcomings and next work

1. Connect Rust evaluation, instantiation, realization and a build command into one tested workflow. Current build tests largely start with derivations created by CppNix, and daemon tests use CppNix clients. Fetchers and `getFlake` are explicit stubs. Full flake input resolution, locking, registries and output workflows are absent. [Glue](crates/nix-rs-glue/src/lib.rs), [build harness](crates/compat-tests/tests/reference_build.rs)
2. Finish command coverage. Only a partial `nix-instantiate` compatibility executable exists, alongside the `nixrs` pure-evaluation preview. The reference exposes 12 executables and 96 modern command entries. There is no packaged replacement daemon executable or service integration. [Commands](crates/nix-rs-commands/Cargo.toml), [daemon manifest](crates/nix-rs-daemon/Cargo.toml), [reference inventory](docs/reference-inventory.md)
3. Apply accepted client settings. Daemon `SetOptions` consumes build jobs, fallback, silent timeout, cores, keep-failed and substitution fields without applying them; trusted overrides mostly produce debug logs. Tests need to compare their effects, rather than just handshakes and warnings. [Implementation](crates/nix-rs-daemon/src/connection.rs)
4. Complete protocol behavior. The Rust daemon advertises worker 1.38 against reference 1.39; eight active operation variants are absent, and older-version acceptance exceeds actual older-operation support. Negotiated working subsets do not qualify full wire compatibility. [Detailed review](CPPNIX_GAP_REVIEW_2026-10-03.md), [reference protocol inventory](tests/reference-inventory/protocols.txt)
5. Close evaluator crash and semantic gaps, then reproduce diagnostics and parse output. The 307 diagnostic mismatches require inspection because some encounter a different error than the reference case tests. [Language report](target/tmp/upstream-lang-report.json)
6. Qualify production store/build behavior: multi-user build-user allocation, concurrent and interrupted writes/builds, cancellation, profiles, optimisation, remote builders, settings and complete cache behavior. Existing sandbox evidence uses diverted unprivileged stores. [Detailed review](CPPNIX_GAP_REVIEW_2026-10-03.md), [MVP gates](TODO.md)
7. Inventory and support required C/C++ consumers, then run packaged replacement tests for NixOS rebuild, Home Manager, daemon ownership handoff, reboot, rollback and return to CppNix. These acceptance gates have no completed replacement evidence. [Compatibility contract](docs/compatibility.md), [matrix](docs/compatibility-matrix.md)

The priority is a connected Rust package-manager workflow and functional settings, while stabilizing the ongoing evaluator changes. After that, broaden the behavior comparisons and qualify the packaged engine in VM overlays. Publishing individual preview crates or increasing unit-test counts does not close those gates.

## Documentation drift

`TODO.md` and the matrix still state 122 language passes; one verification paragraph states 111, while the expectation table records 123 and this sampled working tree produced 125. Some store/cache descriptions also predate GC, the daemon and signed local uploads. The recorded fixture inventory lists 691 unrun files out of 1,916, but per-file tables contain both passes and gaps and cannot all be counted as passing. These figures should be refreshed in the owning implementation work; this audit preserves that work and records the discrepancies here. [TODO](TODO.md), [matrix](docs/compatibility-matrix.md), [verification](docs/verification.md), [expectation table](crates/compat-tests/expectations/upstream-lang.txt)

The companion [CppNix gap review](CPPNIX_GAP_REVIEW_2026-10-03.md) records the store/build/daemon source comparison in more detail, including a link to the official pinned protocol header.
