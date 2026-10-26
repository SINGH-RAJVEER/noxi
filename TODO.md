# Package-manager MVP todo

This checklist tracks the first release: nix-rs replacing the selected CppNix release on an existing x86_64-linux NixOS system. The installed operating system, installer, TUI, GPUI NixBox GUI, Lua configuration, native projects and development environments, and opt-in behavior changes come after this gate. [PLAN.md](PLAN.md) defines the product order; [docs/compatibility.md](docs/compatibility.md), [docs/compatibility-matrix.md](docs/compatibility-matrix.md), and [docs/verification.md](docs/verification.md) define what a drop-in claim requires.

Sections 0 through 8 are the package-manager MVP. Ordinary flakes, `devShells`, `nix develop`, `nix shell`, `nix run`, and legacy `nix-shell` stay in that scope, including their reference lock, hook, and environment effects. The dedicated `nixrs env` workflow, built-in server supervision, PostgreSQL adapter, and optional `nixrs project` manifests are later extensions. Their shared engine dependencies belong in the MVP where the reference requires them; their native schemas, commands, and runtime behavior do not. [MVP scope and later gates](docs/product.md#mvp-scope-and-later-gates) records this boundary.

A checked item below means it exists and has focused local evidence in the current working tree. It does not mean the item is committed, published, or qualified for replacement. Keep a failing or unrun test visible until the corresponding behavior passes against a pinned reference.

## 0. Make the baseline reproducible

- [x] Pin the CppNix source commit for the copied snapshot. The first snapshot matched unreleased 2.36.0 `master` commit `8f7ed1c1d`.
- [x] Choose the reference: the newest numbered release, CppNix 2.35.2 (`2c73b59`). The snapshot was refreshed from that tag, and `just nix-snapshot-verify upstream` passes.
- [x] Build the 2.35.2 reference executables and record their store path for differential runs. The `nix-reference` flake package is upstream's build at `/nix/store/nskid3yq908g18x6cnvrk2hy96327f3f-nix-2.35.2`; `just reference` builds it.
- [x] Resolve the derivation divergences from 2.35.2: ATerm ordering, duplicates, escapes and parse-time validation follow `parseDerivation`, and colliding hash-modulo inputs follow `insert_or_assign`, checked against derivations the 2.35.2 reference wrote.
- [x] Match 2.35.2's fixed-output input handling in `nix-compat` ([verification](docs/verification.md)). The lookup distinguishes regular and per-output fixed hashes; empty and missing requested outputs have focused tests, and reference-generated derivations check ordinary output paths. ATerm retains non-UTF-8 platform, builder, argument, environment-key, and output-name bytes, including invalid fixed-output names; JSON remains UTF-8-only for names.
- [x] Pin a Nixpkgs and NixOS revision, a Home Manager revision, the reference feature settings, and the x86_64-linux VM image for the first compatibility row. `tests/qualification/flake.lock` pins the inputs; its UEFI CppNix reference VM built, reached multi-user mode, started Nix and Home Manager, and shut down cleanly. The matrix records the exact revisions and scope.
- [x] Inventory every executable, `nix.conf` setting, experimental feature, protocol version, and public library interface exposed by that reference build. Record what NixOS and Home Manager actually invoke. `just reference-inventory` writes `tests/reference-inventory/` from the reference build and source, and [reference inventory](docs/reference-inventory.md) lists the pinned NixOS and Home Manager call sites, read from their source.
- [ ] Identify public C and C++ library consumers on the pinned target and record their headers, shared libraries, ABI, and packaging requirements before committing to the replacement architecture. Implement and qualify their interfaces in §6; an unsupported required consumer keeps the drop-in gate open.
- [x] Confirm the NixOS and Home Manager call sites by tracing the executables they run in the qualification VM during rebuild, switch, garbage collection, and Home Manager activation. `just reference-trace` audits every `execve` on the pinned reference system through a flake rebuild and switch, Home Manager activation, both maintenance services, a rollback and `nix-collect-garbage -d`; [traced calls](docs/reference-inventory.md#traced-calls) lists the result. Classic-mode rebuilds, remote targets and the standalone `home-manager` command remain untraced.
- [x] Define expected behavior for both an empty store and a copy of an existing CppNix store. Preserve a reference-only VM snapshot for rollback tests. [Expected store states](docs/compatibility.md#expected-store-states) specifies both, with the reference's fresh-store layout recorded in `tests/reference-inventory/empty-store.txt`. `just reference-vm-snapshot` keeps a read-only CppNix-only VM base, and `just reference-vm-verify` boots an overlay of it without changing the base.
- [x] Keep the copied fixture tree unchanged and provide a command to fetch a revision-recorded official Nix checkout with `just nix-tests-update` and `just nix-tests-sync`.
- [x] Provide a pinned nightly development shell and a separate preview flake package. The shell and package builds have been exercised locally.
- [x] Resolve the pinned nightly's rustc internal compiler error in `nix-compat-derive-tests` by diagnosing uncovered enum versions before generating the async match. The UI suite and full `just ci` pass without accepting an ICE as expected output.
- [x] Resolve the long-running `fixtures_match_expectations` worker-protocol test by reading the fixture's stated value count. The full worker-protocol test now completes in normal Cargo test runs.
- [x] Commit the intended working-tree changes in reviewable Jujutsu changes and verify a clean-clone flake build of the `nixrs` preview package.

## 1. Build a differential compatibility runner

- [x] Add an initial ten-case pure-evaluation comparison that records both executable versions, exit codes, stdout, stderr, expressions, timeouts, and mismatches as JSON. The 2.35.2 reference run (`just eval-compare-reference`) passes 10/10 with controlled environment and per-command timeouts; the full language suite is not yet compared against the reference binary.
- [ ] Run reference CppNix and nix-rs in isolated test stores with identical environment, configuration, feature flags, inputs, and timeouts.
- [ ] Capture exit code, stdout, stderr, structured output, produced paths, derivation bytes, NAR bytes, store metadata, and daemon exchanges where applicable.
- [x] Make every copied fixture discoverable and classified as passing, known gap, reference anomaly, or unrun. Fail when an unclassified fixture appears. `crates/compat-tests/expectations/fixtures.txt` classifies all 1,916 copied files, and `fixture_inventory` fails on an unclassified file, a stale pattern, or an unbacked pass claim.
- [ ] Classify the remaining `unrun` fixtures with observed results, one fixture group at a time. The current count is in [verification](docs/verification.md#reference-evidence). Start with the remaining common and serve protocol cases; keep gaps open until the behavior passes.
	- [x] `store-path`, `content-address`, `outputs-spec`, and `derived-path` JSON against `nix-compat`. `nix_compat::cppnix_json` implements 2.35.2's encodings; `upstream_store_json` round-trips 14 of the 17 fixtures byte for byte, and the 3 nested deriving paths need `dynamic-derivations`, which both engines reject by default.
	- [x] `nar-info` and `path-info` JSON against `nix-compat`. `cppnix_json::{PathInfo, NarInfo}` read and write format versions 1 to 3, with and without impure fields; `upstream_path_info_json` decodes all 20 fixtures to the values CppNix's tests build and re-encodes the 18 written ones byte for byte.
	- [x] `realisation` and `build-result` JSON against `nix-compat`. `cppnix_json` adds `DrvOutput`, `Realisation`, `UnkeyedRealisation` with fingerprints, signing and verification, and `BuildResult`. `upstream_realisation_json` matches all 12 fixtures, including the test key's ed25519 signatures byte for byte.
	- [x] Finish `common-protocol` fixture dispositions. Seven shared primitive cases pass byte-for-byte round trips in `upstream_shared_protocols`; six old hash-based drv-output and realisation files have no corresponding 2.35.2 common codec or test and are recorded as anomalies, not passes. The test guards all 20 files and their reference dispositions.
	- [x] Cover the active `serve-protocol` value and handshake fixtures. `cppnix_serve` matches 25 additional copied files across versions 2.1 to 2.8; `upstream_serve_protocol` checks semantics, exact writes, version gates, malformed inputs, and the intentionally read-only 2.6 build result. Six stale hash-based files absent from the pinned C++ tests are recorded as anomalies, not passes. Serve commands and real client/server exchange remain separate gates.
	- [x] `derivation` JSON and derivation output JSON fixtures. `cppnix_json::derivation` reads and writes all 18 copied version-4 derivation and output JSON fixtures byte for byte, including feature-gated output kinds, dynamic inputs, and structured attributes; `upstream_derivation_json` checks their decoded fields and default feature gates.
	- [x] `derivation-options` fixtures (advanced attributes). Covered by `upstream_derivation_options`.
	- [x] `derivation/invariants` fixtures (filling in deferred output paths). `upstream_derivation_resolution` matches the reference, including its no-env-var anomaly.
	- [x] `derivation/try-resolve` fixtures (resolving content-addressed inputs). `upstream_derivation_resolution` resolves all four cases.
	- [x] `store-reference` fixtures. `nix_compat::nixcpp::store_reference` parses all 22 Linux cases and renders the nine round-trip cases byte for byte in `upstream_store_reference`; the Windows-only path is rejected as the Linux reference rejects it.
	- [x] `machines` fixtures. `nix_compat::nixcpp::machines` parses remote builder specifications as `Machine::parseConfig` does; `upstream_machines` covers every case of the pinned test and both copied files.
	- [ ] `dummy-store` and `worker-substitution` fixtures; they need a store implementation.
	- [x] libutil `memory-source-accessor` and `nar-listing` fixtures. `nix_compat::fso::File` reads and writes CppNix's in-memory file tree JSON, and `nar::listing` writes 2.35.2's deep and shallow NAR listings; `upstream_file_listings` matches all four files byte for byte from the reference's example trees.
	- [x] libutil `git` object fixtures. `nix_compat::git` parses and writes the copied blob and SHA-1/SHA-256 trees byte for byte, checks each listed entry, and rejects malformed headers and entries; source-accessor integration and `git-hashing` feature enforcement remain separate gates.
	- [x] Remaining libutil `hash` fixtures. `sha256.json` matches the reference value; `sha512.json` is a reference anomaly because CppNix 2.35.2's `sha512` test constructs SHA-256. Custom tests preserve both outcomes.
	- [x] libfetchers `public-key` JSON. `nix_compat::fetchers::PublicKey` matches both round-trip fixtures and the read-only missing-type fixture, which defaults to `ssh-ed25519`.
- [x] Run every copied language fixture against the Rust evaluator in isolated child processes with timeouts, fail on unclassified fixture files, and record each case's observed outcome so any change fails the test. Eval cases run nix-rs's `nix-instantiate` with `lang.sh`'s arguments and `.flags`. 244 of 467 cases pass; see `crates/compat-tests/expectations/upstream-lang.txt`.
- [ ] Extend the language runner to compare every case and the checks embedded in `lang.sh` against the pinned reference binary, including `--parse` and `.flags` options as their compatibility commands become available. Semantic and diagnostic fixes belong to §3.
- [ ] Adapt upstream functional shell tests to call nix-rs binaries. Do not count tests that still call installed CppNix as nix-rs passes.
	- [x] `lang.sh`'s fixture loop: `upstream_lang` runs every `eval-*` case through nix-rs's `nix-instantiate` with the arguments and `.flags` `lang.sh` uses, and records cases whose arguments the command rejects as `unrun-flags`. Focused command comparisons now cover ordinary/verbose trace and warning behavior from the top of `lang.sh`; its `addErrorContext`, recursive-value trace, abort-warning trace diagnostics and `--parse` checks remain open.
- [ ] Add wire-level tests for all supported daemon protocol versions and feature negotiations, using the copied worker fixtures as an initial corpus.
- [ ] Store machine-readable results with reference revision, platform, feature settings, and command so each row in the compatibility matrix has inspectable evidence.
- [ ] Map every applicable inventory entry, including commands, options, settings, builtins, fetchers, store types, protocol operations, and library interfaces, to test cases and results. Add reference-generated cases where copied fixtures do not cover behavior. Record out-of-scope, reference-disabled, and anomaly decisions explicitly; none is a nix-rs pass by itself.
- [ ] Run the same harness in CI and in disposable NixOS VMs; separate fast format checks from store, daemon, and system tests.

## 2. Finish formats and cryptographic primitives

- [x] Exercise selected hash, derivation, and NAR fixtures against the imported compatibility crates. The masked-output-path fixtures went with the move to 2.35.2.
- [ ] Cover every reference hash encoding and algorithm, including invalid input behavior and experimental feature gates. A custom 2.35.2 `nix hash convert` matrix covers MD5, SHA-1, SHA-256, SHA-512, and BLAKE3 in hex, nix32, base64, and SRI, including malformed inputs. The command-level BLAKE3 feature gate and remaining CLI behavior are open.
- [ ] Complete store-path, content-address, derivation, output-specification, and realisation parsing and serialization for the enabled reference features.
- [x] Generate valid NAR byte oracles with the pinned reference for regular files, executable files, directories, and symlinks. `reference_nars` checks both streaming writers and readers in 16 tests against the four 2.35.2 dumps in `fixtures/nar-reference/`, including binary payloads, empty nodes, nested entries, and a non-UTF-8 filename; the existing malformed-archive tests also pass.
- [x] Complete NAR boundary and resource-limit behavior beyond the selected valid and malformed cases; qualify store import/export and cache compression in §4. `reference_nar_limits` checks 22 boundary archives and the 64-level nesting limit against the reference for both readers, restoring and dumping; store import/export and compression are covered by `reference_store` and `reference_substitute`.
- [x] Implement narinfo parsing and serialization, signatures, trusted-key checks, and cache metadata validation. Narinfo files the 2.35.2 reference writes to `file://` caches, with and without compression, parse and print back byte for byte in `reference_substitute`, and substitution checks their signatures against `trusted-public-keys` and reads `nix-cache-info`. Signed uploads to `file://` caches match the reference, and caches are read with `nix_compat::nixcpp::narinfo`, which fails 34 malformed narinfo files with the reference's messages. The narinfo disk cache is tracked with substitution below.
- [ ] Verify exact bytes where the reference format requires exactness; record intentional nondeterminism separately.

## 3. Complete Nix evaluation and flakes

- [x] Ship a non-conflicting `nixrs` preview binary that evaluates pure `--expr` and `--file` inputs, with focused CLI tests.
- [ ] Close the language gaps recorded by §1: laziness, coercion, equality, missing builtins, paths, imports, string contexts, CppNix-format diagnostics with source locations and traces, and invalid inputs accepted by nix-rs. Match integer-overflow errors and deep-value handling in both debug and release builds rather than accepting panics or silent wrapping.
	- [x] Fix contextless `appendContext`, weak `abort`/`throw` message coercion, byte-wise zero-width `split` progress, and non-UTF-8 `readDir` keys with focused regression tests. Store-context validation, POSIX regex equivalence, diagnostics, and the other language gaps remain open.
	- [ ] Print evaluation errors in CppNix's format with its traces. Done: the `showErrorInfo`/`printCodeLines`/`printTrace` renderer as the default for `nix-instantiate` and the commands that evaluate through it, builtin-call and function-call frames, catchable errors that keep the trace of where they were raised, hidden driver frames, CppNix messages for aborts, throws, type errors with printed values, undefined variables, missing attributes and unexpected arguments with suggestions, CppNix's per-argument builtin checks and order, return-value and element contexts, arithmetic operand checks, deep-force attribute and list-element frames, and CppNix I/O messages. 111 more language cases pass. Remaining: contexts of builtins outside the argument table, attribute-selection, assertion, comparison and derivation frames, equality-assertion messages, parse and compile errors, values in coercion and call errors, and missing-argument errors.
	- [x] Replace integer arithmetic panics and wrapping with evaluation errors for addition, subtraction, multiplication, division, and negation, including builtin/operator parity and division by integer or float zero. All 43 focused arithmetic cases pass in debug and release. The four copied overflow cases now record `error-message E043`, not `pass`; full reference diagnostic rendering remains open.
- [ ] Integrate derivation creation with the store: preserve string-context dependencies and compare `derivationStrict` results, `.drv` bytes, and computed output paths against the reference before attempting full Nixpkgs evaluation.
	- [x] Add opt-in diverted writable evaluation that registers text, copied/filtered sources, and derivations with their database references. Verify captured source hashes before registration, logical filter paths, dependency-output references, and unchanged read-only behavior. This registration milestone is separate from the IFD comparisons below; see [verification](docs/verification.md#rust-core-completion-work).
- [ ] Implement the required builtins and impure evaluation boundary, including filesystem and environment access, fetchers, and import-from-derivation.
	- [x] Register `builtins.warn`, `builtins.traceVerbose` and `scopedImport`, including plain global/`__name` exposure. Correct `seq` forcing so the second argument is not evaluated before the first succeeds. `instantiate` compares sequencing order, warning/trace output, quiet filtering, ANSI filtering, settings precedence, abort behavior and ordinary-file scope shadowing/laziness with CppNix 2.35.2. The copied `eval-okay-import` case now passes.
	- [x] Resolve ordinary expression imports through evaluator I/O, including directories and final symlinks in diverted stores. Preserve lexical ancestor aliases in relative paths and cache keys. Command comparisons match CppNix 2.35.2; library tests cover writable/read-only mappings and virtual source locations.
	- [x] Import registered `.drv` files with recursive output attributes, contextual strings and lazy scopes. Realize path contexts through the selected local/daemon owner, with read-only evaluation reuse, IFD permission/trace settings and deferred-output feature gates. Compare imported values and downstream derivation paths, IFD results/build logs, and `readFile` reference filtering against CppNix 2.35.2; both daemon pairings pass.
	- [ ] Qualify complete import/scoped-import store behavior and import-from-derivation across lazy source mounts, content-addressed/dynamic output builds, separate evaluation/build stores, pure/restricted access rules, failures, recovery and concurrent garbage collection. Warning/error source positions and traces remain in the diagnostic gate.
- [ ] Match source filtering and path-copy behavior because these change derivation inputs and store paths.
- [ ] Parse, resolve, update, and write flake references and `flake.lock` with registry and follows semantics matching the reference.
	- [x] Fix selected flake-reference parse/format round trips for forge refs and hosts, transport queries, Mercurial, plain Git, explicit source types, and relative paths. Full format compatibility, registries, fetching, and locks remain open.
- [ ] Evaluate flake outputs and selected Nixpkgs packages with the same values and derivation graphs as CppNix.
- [x] Evaluate a pinned NixOS configuration and both standalone and module-based Home Manager configurations before claiming they are supported. `just nixos-paths` matches the reference's derivation paths for the qualification VM configuration (without its flake-provided `nix.package`), its VM, its Home Manager module generation, and a standalone Home Manager generation, evaluated through `eval-config.nix`. The flake entry point and flake-mode Home Manager are covered by the flake items.
- [ ] Bring evaluation time close to the reference's. The four derivation paths of `just nixos-paths` take 23.3 s with a release build of nix-rs's `nix-instantiate` and 4.7 s with the reference on the same machine (2026-10-04), with identical results.
- [x] Keep improved evaluator diagnostics behind an opt-in interface when their output would change legacy command behavior. Legacy commands print CppNix-format evaluation errors; `NIXRS_DIAGNOSTICS=codespan` selects the annotated source renderer. The `nixrs` preview CLI keeps the annotated renderer.

## 4. Implement the store, caches, and build engine

- [ ] Define a store interface that supports local and daemon-backed operations without bypassing the single store owner. `nix_rs_store::Store` covers the operations the legacy commands and the evaluator use; `nix-rs-build`'s `LocalBackend` implements it for a local store that builds and substitutes, and the evaluator registers derivations and sources through it. Commands select the store as CppNix's `auto` does. The daemon client implements these operations over the worker protocol; remaining store types and operations still need qualification.
- [ ] Read and validate existing store metadata, references, validity records, roots, and profiles without modifying a user's live store during development.
- [ ] Match fresh-store initialization, supported schema upgrades, named migrations, read-only opens, and unsupported-schema rejection from [expected store states](docs/compatibility.md#expected-store-states). After each supported write or upgrade, require CppNix to reopen the store without repair or nix-rs-only migrations.
- [ ] Implement atomic path registration, NAR import/export, integrity verification, locking, and crash recovery; test concurrent clients and interrupted writes.
- [ ] Implement binary-cache lookup, download, substitution, narinfo and signature verification, upload, and offline behavior. `nix-rs-store` substitutes closures from `file://` and HTTP(S) caches with none, xz, bzip2, zstd, gzip and brotli compression, refuses unsigned paths unless the cache is trusted, and rejects corrupt NARs; `reference_substitute` matches the reference's registered metadata and errors. Uploads, the narinfo disk cache, netrc and TLS settings, `--fallback`, parallel downloads, and offline behavior remain open.
- [ ] Implement derivation realization, dependency scheduling, fixed-output checks, logs, cancellation, and failure propagation. `nix-rs-build`'s `LocalBuilder` realises input-addressed and fixed-output derivations depth first through the library API; `reference_build` compares eleven cases with 2.35.2, including build logs, structured attributes, multiple outputs, input derivations, fixed-output hashes and mismatches, and builder, missing-output, cycle and reference-check failures. `max-build-log-size`, floating content-addressed outputs, substitution, parallel scheduling, cancellation and a command entry point remain open.
- [ ] Implement sandboxed local builds with the reference environment and isolation rules; then add supported remote-builder behavior. The Linux sandbox runs unprivileged builds in a diverted `--store ROOT`, and a probe of its environment, identity, generated `/etc` files, uid and gid maps and directory layout matches the reference byte for byte. Builds without a sandbox, build users, `auto-allocate-uids`, seccomp and network-isolation probes, and remote builders remain open.
- [ ] Implement profile generations, roots, garbage collection, and store optimization with existing-store compatibility tests. `nix-rs-store`'s collector finds the same roots (direct, indirect `--add-root`, profile, temporary and runtime), live and dead paths, and deletions as 2.35.2 on copies of one reference-made store in `reference_gc`, and serves the GC socket. `nix_rs_store::profiles` manages generations as CppNix's `profiles.cc` does, and `nix-collect-garbage` (`-d`, `--delete-older-than`, `--dry-run`, `--max-freed`) matches the reference's output, deletions and remaining files on a local store in `reference_collect_garbage`. Focused local and daemon-client comparisons cover `--gc`, `--delete`, root queries, profile pruning and optimisation. Concurrent maintenance, complete optimisation behavior, and a CppNix client sending roots to the nix-rs GC socket remain open.
	- [x] Route GC, root listing and optimisation through the shared store owner. `reference_daemon_client` compares Rust/reference and Rust/Rust client/daemon pairings with reference/reference, including profile pruning, liveness refusal and duplicate-file inode checks; `reference_nix_store` covers local maintenance and `--query --roots` retention settings. Full store and system qualification remains open.
- [ ] Test a copied CppNix store in both directions: use it with nix-rs, then return it to CppNix without data loss or invalid metadata.

## 5. Implement the daemon and authorization boundary

- [ ] Complete worker-protocol framing, handshakes, version negotiation, operations, responses, errors, and feature negotiation for the pinned reference. The `nix-rs-daemon` preview performs the 1.38 handshake and feature exchange, `SetOptions`, the stderr log and error channel, the read-only, temporary-root, root-listing and missing-path operations, content-addressed and text imports, NAR imports one at a time and in batches, indirect and permanent roots, garbage collection, signatures, store verification without repair, and builds with build activities and log lines; store optimisation, repair, substitutable-path queries, realisations, build-log uploads, other activities, applying a trusted client's settings, and versions below 1.38 remain open.
- [ ] Implement multi-user authorization, trusted users, allowed store operations, daemon configuration, sockets, and service lifecycle.
- [ ] Test reference client with reference daemon, nix-rs client with nix-rs daemon, and both cross-engine pairings where the protocol permits them. `reference_daemon` runs reference clients against both daemons on copies of one store, with identical exit status and output for 17 query commands, an untrusted client's warnings and censored roots, `nix-build` builds that succeed, are already valid or fail, `--out-link` roots, `nix-store --gc` and `--delete` with live, dead and `--max-freed` variants, `nix-collect-garbage` and `nix store gc`, `nix copy` and `nix-store --import` from trusted and untrusted clients with and without signatures, `nix store sign`, and `nix-store --verify` on intact, modified and vanished paths.
	- [x] nix-rs's commands as clients: `reference_daemon_client` runs nix-rs's `nix-store`, `nix-instantiate`, `nix-build` and `nix-env` against a reference daemon and against the nix-rs daemon, with the same output, exit statuses and relayed daemon messages as the reference's commands against a reference daemon, for twelve query, instantiation, build, realisation and profile cases. Additional comparisons cover daemon-backed `nix-collect-garbage` with profile pruning, `nix-store --gc` with live/dead/root queries and `--max-freed`, `--delete` with liveness enforcement, `--query --roots`, and `--optimise`/`--optimize`. GC wire tests cover negotiated and fallback encodings, rejection of unsupported specific-path requests before writing, and connection reuse.
- [ ] Verify simultaneous clients, disconnects, cancellations, daemon restart, malformed messages, and privilege boundaries.
- [ ] Package the daemon and service units expected by the selected NixOS module without altering a live host during qualification.

## 6. Complete the command and configuration contract

- [ ] Provide the `nix` executable and required legacy commands, including `nix-build`, `nix-instantiate`, `nix-store`, `nix-env`, and other names found in the reference inventory. `nix-store --realise` (with `--add-root`, `--dry-run`, `--ignore-unknown`) and `--query` (outputs, requisites, references, referrers, derivers, bindings, hashes and sizes) match the reference's output, errors, roots and missing-path summary in `reference_nix_store`; maintenance also includes GC, root queries and optimisation locally and through a daemon; other operations remain partial or fail explicitly.
	- [x] `nix-instantiate` accepts `--xml --no-location`, `-I`/`--include` ahead of `NIX_PATH`, and `--show-trace`/`--no-show-trace` (traces are not rendered in CppNix's format yet), prints `--json` results unquoted, and no longer prints nix-rs's own lint warnings. `--xml` with locations, `--parse`, the lint flags, `--max-call-depth` and `--extra-experimental-features` remain open.
	- [x] `nix-instantiate --find-file`, which `nixos-rebuild` runs in every mode, and `<...>` lookups follow CppNix's lookup path: `-I` entries, then `nix-path` (which `NIX_PATH` replaces before settings flags apply), then the existing default channel directories, with prefix matching at `/`, symlink resolution, the missing-entry warning, `builtins.findFile`, `builtins.nixPath`, rebinding `__findFile`/`__nixPath`, every builtin as `__name`, and `pure-eval`/`restrict-eval`. `finds_files_in_the_lookup_path` records the reference's results, and `eval-okay-search-path` passes.
	- [ ] Resolve `flake:` lookup path entries through the flake registry, which `<nixpkgs>` needs on flake-built NixOS (`nixpkgs=flake:nixpkgs` on `NIX_PATH`), and download pseudo-URL entries (`channel:`, `https://`). Both are skipped with CppNix's warnings now.
	- [x] `nix-env --set` with store paths, `--rollback`, `--switch-generation`, `--list-generations`, `--delete-generations` and `--switch-profile`, as `nixos-rebuild` switches and rolls back the system profile, match the reference's output, errors and files in `reference_nix_env`.
	- [x] `nix-env --install` and `--uninstall` of store paths and `--query` of the installed elements, which Home Manager runs as the user, build user environments from `manifest.nix` with `builtin:buildenv`, matching the reference's output, generations, manifests and trees locally and through a daemon in `reference_user_env`.
	- [ ] The other `nix-env` operations (`--upgrade`, `--set-flag`, `--file`, the remaining query options) and selecting derivations from Nix expressions.
	- [x] Permit single-derivation `nix-instantiate` selections with an explicit diverted `--store local?root=ROOT`, and support `--eval --read-write-mode`. Read-only diverted reads retain the selected root. Recursive selections, builds, permanent roots, and the remaining commands remain open.
- [ ] Implement command arguments, option aliases, environment variables, `nix.conf`, experimental feature gates, registries, and profiles with matching precedence.
	- [x] Load settings as CppNix 2.35.2 does: `$NIX_CONF_DIR/nix.conf`, user files in reverse order, `NIX_CONFIG`, `include`/`!include`, experimental features applied first, `extra-` appends, aliases, the `flakes`-implies-`fetch-tree` rule, settings flags, and the reference's warnings and errors. `nix_compat::nixcpp::settings` uses a table generated from the reference inventory; `nix-instantiate` uses it and gates `fetchTree` on `fetch-tree`. Acting on most settings, the lint settings, and moving `nix-rs-daemon` onto this loader remain open.
- [ ] Match exit codes, stdout, stderr, JSON fields, log modes, and command side effects; use differential tests for scripts that parse output.
	- [x] Share legacy compound short-flag parsing, common build flags, verbosity filtering, and raw build-output control. Isolated command/store/daemon comparisons cover `-qR`, `-j1`, units, numeric errors, `--`, repeated quiet flags, and failed-build logs. Internal JSON, interactive progress bars, and `--readonly-mode` remain gaps; see [legacy flags and logging](docs/verification.md#legacy-flags-and-logging).
- [ ] Cover core workflows: evaluate, build, develop, shell, run, flake, profile, store, copy, path-info, log, search, and garbage collection.
	- [ ] Qualify `nix develop`, `nix shell`, `nix run`, and legacy `nix-shell` with reference-supported flake and expression inputs, covering selection, lock-file effects, tool paths and variables, shell hooks, child exit status, and signal handling. These commands do not depend on implementing the native environment supervisor.
- [ ] Provide the C and C++ interfaces required by the §0 consumer inventory, including headers, library names, SONAMEs, symbols, link metadata, and installation paths. Rebuild source consumers and run unchanged reference-linked binaries against the replacement libraries; keep the drop-in gate open for any required incompatible consumer.
- [ ] Verify that adjacent Lua configurations, dedicated environment entries, and optional native project manifests leave compatibility discovery, lock effects, output, and environment behavior unchanged. Compatibility commands never evaluate `.lua` files. Any shipped native command or improved diagnostic renderer must be explicitly selected; implementing it, a TUI, or a GUI is later work. Repeat affected compatibility cases when extensions ship.

## 7. Prove NixOS and Home Manager replacement

- [ ] Build a provisional replacement package with the complete executable set and wire it to the selected Nixpkgs `nix.package` and daemon service options only in disposable qualification VM overlays. Verify that migration selects exactly one store-writing daemon; host replacement instructions remain gated on successful qualification.
- [ ] Compare `nixos-rebuild build`, `test`, `boot`, `switch`, and `dry-activate` results, activation effects, boot entries, generations, and failure recovery.
- [ ] Rebuild an existing CppNix-managed NixOS VM with nix-rs, reboot into the resulting generation, make a second change, and roll back.
- [ ] Test standalone Home Manager build and switch, flake mode, and Home Manager as a NixOS module using the same engine.
- [ ] Compare user profiles, activation effects, generations, and rollback for the pinned Home Manager revision.
- [ ] Demonstrate a documented return to CppNix after a failed or successful nix-rs switch, including existing-store reuse.

## 8. Package and qualify the MVP release

- [ ] Make the flake build reproducibly from a clean clone and provide the qualified replacement package, app, checks, daemon, and NixOS integration module.
- [x] Publish the preview command set to crates.io: `nix-rs-eval-builtin-macros` 0.0.1, `nix-rs-eval` 0.2.0 and `nix-rs-cli` 0.1.1 ([distribution](docs/distribution.md#published-crates)).
- [x] Publish the selected additional preview crates with versioned registry dependencies and package-local license/provenance notices. `nix-rs-hash`, `nix-rs-flakeref`, `nix-rs-compat`, `nix-rs-compat-derive`, `nix-rs-serde`, `nix-rs-glue`, and `nix-rs-commands` 0.1.0 passed package/build checks and were confirmed on crates.io on 2026-10-01. Their APIs are unstable; internal harnesses and the in-progress store stay private. Stable Rust APIs and additional crate publication are not prerequisites for the drop-in binary release.
- [ ] Audit imported code and copied test licenses; include attribution and source revisions in release artifacts.
- [ ] Test installation, upgrade, failed upgrade, rollback, and removal in disposable VMs before documenting host installation.
- [ ] Run the applicable compatibility suite against the exact packaged release-profile executables and libraries, not only debug builds or workspace tests. Record artifact hashes and build inputs in the reports, and rerun affected qualification when an artifact changes.
- [ ] Establish performance baselines for evaluation, cache substitution, local and remote builds, daemon startup, memory use, and large rebuild diagnostics against the pinned reference using the packaged release artifacts. Follow the [benchmark protocol](docs/performance-benchmarks.md) and [scenario catalog](benchmarks/catalog.tsv); record hardware, workloads, settings, input revisions, and observed gaps alongside the qualification reports so later improvement claims have a reproducible comparison.
- [ ] Complete the x86_64-linux compatibility matrix with zero applicable gaps or unrun behaviors for the exact declared reference and feature set.
- [ ] Run security checks for daemon authorization, sandbox escape attempts, untrusted caches, signatures, malformed inputs, and secret handling.
- [ ] Publish binaries, source, signatures, test reports, known limits, and a safe migration and rollback procedure. Call the release a 100% drop-in replacement only after every applicable gate passes.

## After the package-manager MVP: Lua configuration

Native surfaces default to Lua for user configuration and still accept Nix, as specified in [Lua configuration](docs/lua-configuration.md). This work is not a drop-in gate, and it must be usable before the installer generates Lua configurations.

- [ ] Choose, pin, and embed a Lua runtime; record its version and license.
- [ ] Define how a Lua configuration maps to a NixOS or Home Manager module: imports, option definitions, module arguments, `lib` helpers such as `mkIf` and `mkForce`, and access to lazy Nix values without forcing `pkgs` or `config`.
- [ ] Carry string contexts and store-path dependencies through Lua values so derivation inputs match the Nix equivalent.
- [ ] Sandbox the runtime: no `io`, `os`, arbitrary `require`, clock, randomness, or network outside tracked inputs; test that each attempt fails with a diagnostic.
- [ ] Implement native configuration discovery: explicit path, Lua only, Nix only, and a stop naming both files when both exist.
- [ ] Report Lua syntax, runtime, and module-system errors with `.lua` locations in the structured diagnostics, including traces that cross Lua and Nix code.
- [ ] Write pinned NixOS configurations in both Lua and Nix and require identical `system.build.toplevel` derivation paths; list any differences from options that embed the configuration source.
- [ ] Do the same for standalone and module-based Home Manager configurations.
- [ ] Test that compatibility commands, including `nixos-rebuild` and `home-manager`, give identical results with and without `.lua` files present, and document how a Lua-configured system is rebuilt.
- [ ] Settle the open questions: Nix modules importing Lua, flake outputs for Lua configurations, Lua engine settings, and a Nix-to-Lua converter with formatter and editor support.

## After the package-manager MVP: development environments

[Native projects and development environments](docs/development-environments.md) specifies standard flake environments and a dedicated built-in devenv equivalent, with optional native input/output manifests. The 100% drop-in gate remains unchanged; native manifest conversion is never required for flakes or dedicated environments. This extension depends on the qualified engine and shared Lua bridge; it can ship on supported existing systems before the OS installer.

- [ ] Settle dedicated environment entry names, schema versions, selector syntax, library packaging, and Lua/Nix environment helper APIs.
- [ ] Implement dedicated environment discovery and direct evaluation, named environments, existing flake input sources, and flake-exported environments; support selecting the MVP-qualified ordinary development shells without manifest conversion.
- [ ] Implement native environment lock create/update with the reference graph encoding and selected input authority, frozen native inputs by default, explicit diffs, stale-source rejection, interrupted-write recovery, and offline diagnostics. Compatibility commands retain their reference lock effects.
- [ ] Normalize equivalent dedicated and flake-exported Lua/Nix environment modules with one resolved input graph, deterministic composition, source provenance, preserved store contexts, and validated task/server graphs.
- [ ] Realize tool closures through nix-rs; support shell entry, command execution, scripts, finite tasks, environment export, and correctly retained/released GC roots.
- [ ] Implement an unprivileged built-in supervisor with dependency-aware startup, bounded readiness probes, restart limits, graceful process-group shutdown, detached sessions, partial-failure cleanup, and attributed logs.
- [ ] Qualify session identity and private runtime records, separate Jujutsu workspace state, stale-plan rejection, authenticated controls, guardian cleanup, and recovery without killing unrelated processes.
- [ ] Implement declared ports, runtime inputs, opt-in file watching, secret providers and dotenv parsing without evaluation/store leakage or implicit lock updates.
- [ ] Qualify a PostgreSQL adapter with explicit initialization, version/data compatibility, readiness, preserved mutable state, and guarded cleanup. Add further adapters only with their own evidence.
- [ ] Expose versioned operations, results, events, and headless inspection/recovery; any shipped TUI/GUI environment controls must share their semantics.
- [ ] Run all [environment acceptance cases](docs/development-environments.md#acceptance-cases-and-release-evidence) in disposable workspaces and stores, publish the exact support matrix, and compare reference flake/shell commands, lock effects, hooks, output, and environment effects with native files present. Keep every 100% drop-in requirement in the compatibility gate.

## After the package-manager MVP: optional native projects

Optional `nixrs project` manifests have their own extension qualification. They can reuse the shared input and environment modules, but neither dedicated environments nor ordinary flakes require them. This work does not block the package-manager MVP or the first environment release.

- [ ] Settle optional native manifest names, project schema versions, output selector syntax, and Lua/Nix project helper APIs.
- [ ] Qualify optional native project discovery, named outputs, lock operations, input dependencies, and target-system validation.
- [ ] Implement `project init`, `show`, `build`, `run`, and `check` through the shared engine with frozen locked inputs and explicit lock updates. Inspection must not run tasks or servers; build checks and runtime integration tasks retain their distinct effects.

## After the package-manager MVP: scoped application builds

[Scoped application builds](docs/scoped-application-builds.md) specifies independent user and machine application profiles that avoid a full system generation for eligible package changes. Project scope reuses native environments. This is an additive extension; reference-compatible profile and NixOS behavior remains in the MVP.

- [ ] Define equivalent Lua/Nix application declarations, locked inputs, typed scopes, profile ownership/paths, executable precedence, and stable operation schemas.
- [ ] Implement eligibility checks and previews with package provenance, dependency work, collisions, and separate plans for changes requiring system or Home Manager activation.
- [ ] Implement scoped build-only, draft, apply, update, removal, and rollback through nix-rs and NixBox, with source/profile revision checks, journal recovery, and GC roots.
- [ ] Qualify user application visibility and machine profile integration, including authority, supported desktop discovery, reboot persistence, and preservation during system rebuilds.
- [ ] Run the scope acceptance cases with equivalent CLI/TUI/GUI results, trace proof of no system build or activation for eligible changes, compatibility checks, and measured build/apply costs.

## After the package-manager MVP: OS image and boot

- [ ] Define the first OS release matrix: architecture, UEFI firmware, installation media, supported filesystems, storage layouts, graphics and headless profiles, and the Nixpkgs baseline. Record unsupported hardware rather than implying universal support.
- [ ] Create a NixOS flake that builds the installed system closure, live installer image, VM image, and recovery image from pinned inputs.
- [ ] Make the installed image use nix-rs for evaluation, realization, substitution, daemon service, profile management, and future rebuilds. Check the runtime closure for unintended CppNix dependencies.
- [ ] Preserve NixOS module semantics, generation activation, boot entries, and rollback. Keep `system.build.toplevel` and the generated activation program as qualified integration points.
- [ ] Define a small default system profile with working networking, account creation, logs, time synchronization, and a documented initial package set; support a headless installation path.
- [ ] Offer a conventional systemd-boot installation and a Lanzaboote Secure Boot installation on supported UEFI systems, with explicit prerequisites and distinct resulting configurations. Follow [the boot security spec](docs/secure-boot.md).
- [ ] Build image artifacts reproducibly in CI, record input revisions, and publish checksums, signatures, and a manifest connecting each image to its source and test results.
- [ ] Boot every advertised image in a VM, then use the installed nix-rs engine to build and activate a second generation after reboot.

## After the package-manager MVP: installer and first boot

- [ ] Design an installer plan that shows the selected disks, partitions, filesystems, encryption, boot path, users, network, and configuration source before any destructive write.
- [ ] Implement guided and manual storage paths with explicit support boundaries. Test preservation of untouched disks, cancellation before writes, interrupted partitioning, and install retry.
- [ ] Generate a Lua configuration, including the hardware module, and a lock file that the installed engine can evaluate unchanged. Keep installer-only state out of the permanent configuration unless it is needed for future rebuilds.
- [ ] Implement boot choice validation: firmware mode, EFI system partition, key state, signing capability, and recovery route. Do not silently downgrade a requested Secure Boot installation.
- [ ] Configure initial users and authentication, secret handling, networking, locale, and hardware-derived settings without placing credentials in the Nix store.
- [ ] Install and verify the bootloader, boot the installed system, confirm the active generation and daemon, and rebuild once from the installed configuration.
- [ ] Provide a text installer or equivalent headless path and recovery media able to mount the system, inspect generations, repair boot state, and restore a known-good configuration.

## After the package-manager MVP: self-maintenance and recovery

- [ ] Implement OS-native `build`, `test`, `boot`, `switch`, and `dry-activate` operations with the distinct NixOS effects specified in [system lifecycle](docs/system.md).
- [ ] Make lifecycle requests bind to a stable target identity, source revision, exact closure, and durable operation identifier so later fleet execution can reuse them without changing single-machine semantics. Remote execution remains gated on the fleet security and recovery work below.
- [ ] Make configuration and lock revisions authoritative for persistent changes. Stage, evaluate, build, save, verify, and activate each change as specified in [the configuration workflow](docs/configuration-workflow.md).
- [ ] Journal source edits and activation state so interrupted operations can report and recover partial outcomes without erasing unrelated edits.
- [ ] Track built, running, and boot-default generations separately. Keep a known-good boot entry until a user-directed retention policy allows collection.
- [ ] Define channels for nix-rs, Nixpkgs, kernel, and installer updates; pin the inputs used for every generation and test upgrade from the preceding supported OS release.
- [ ] Test recovery after a bad configuration, failed build, daemon failure, partial activation, bootloader failure, bad update, and a generation that cannot boot.
- [ ] Document which rollback actions affect system generations and which mutable application data needs a separate backup or migration reversal.

## After the package-manager MVP: NixBox and user interfaces

- [ ] Decide how to version the separate NixBox crates with this repository, record their exact revision and license, and migrate existing CLI/TUI settings and queued operations.
- [ ] Implement typed package, option, flake, system, and store operations backed by nix-rs evaluation and realization, with target-lock-revision resolution and source provenance.
- [ ] Give the OS-native CLI, TUI, and GPUI GUI one versioned operation and event interface. No client should parse human-readable build output to infer engine state.
- [ ] Implement the structured diagnostics, progress, full-log retrieval, JSON mode, redaction, and actual final-state reporting specified in [logging](docs/logging.md).
- [ ] Implement safe structured edits to an application-owned module in the configuration's language, Lua or Nix, with source editing for expressions the interface cannot represent and explicit Save draft, Apply, and Rollback actions.
- [ ] Keep desktop clients unprivileged; authorize privileged actions through a narrow service that checks the reviewed plan and target.
- [ ] Test equivalent operations through CLI, TUI, and GUI, including headless recovery, keyboard use, accessibility labels, long builds, cancellation, reconnect, and restart recovery.

## After the package-manager MVP: OS qualification and release

- [ ] Maintain a hardware and VM test matrix for each advertised architecture, firmware mode, boot option, storage layout, and update path.
- [ ] Test installer image creation, installation, first boot, self-rebuild, reboot into a new generation, rollback, recovery media, and reinstallation without data loss where preservation is claimed.
- [ ] Test the Secure Boot installation and later signed-generation updates separately from conventional systemd-boot. Include key-loss and firmware-reset recovery cases.
- [ ] Run installer and system security reviews for disk selection, key custody, privilege boundaries, untrusted caches, sandboxing, secret handling, and image-signing infrastructure.
- [ ] Publish installation and recovery instructions, supported hardware, known limitations, signed artifacts, source revisions, and test evidence. Do not call an image self-maintaining until it rebuilds and switches using nix-rs after installation.

## After the first OS release: fleets and distributed computing

[Distributed computing](docs/distributed-computing.md) specifies the intended fleet capability. This is not a package-manager MVP gate or implemented support. The single-machine OS contracts must allow this work without promising an atomic fleet switch.

- [ ] Define the typed fleet schema and equivalent Lua/Nix representations for stable identities, inventory revisions, hardware profiles, groups, connections, cluster roles, service topologies, and ownership.
- [ ] Qualify target enrollment, authenticated remote execution, scoped authorization, replay rejection, credential rotation/revocation, and disconnected-operation recovery before permitting remote mutation.
- [ ] Implement immutable fleet plans with frozen target sets, per-machine derivations, tracked inventory inputs, dependency ordering, stale-state rejection, and saved-source verification.
- [ ] Implement durable coordinator/executor journals, topology-transition serialization, stale-coordinator fencing, cancellation boundaries, and resume after lost acknowledgements or partial activation.
- [ ] Qualify canary and health-gated fleet rollouts, shared per-topology availability reservations across concurrent plans, reboot verification, and honest completed/failed/unreachable/blocked/pending results.
- [ ] Implement and qualify kubeadm bootstrap, join, drain, membership changes, certificates, upgrades, and recovery for each advertised stacked or external-etcd topology.
- [ ] Implement and qualify K3s server/agent lifecycle with separate evidence for single-server SQLite, embedded-etcd HA, and each advertised external datastore. Include role-specific token lifetimes, rotation, and snapshot-associated server-token recovery.
- [ ] Implement connected-machine resolution and network policy with explicit API, workload, replication, management, and storage endpoints; qualify chosen CNI/CSI and load-balancing implementations.
- [ ] Qualify etcd membership transitions, learners where supported, quorum-aware maintenance, backup, and loss-of-majority recovery. Keep independent datastore ownership explicit.
- [ ] Choose and qualify the first primary/replica service adapter, including replication lag, promotion authority, fencing, routing, data-loss policy, and divergent-primary recovery.
- [ ] Implement bounded worker-capacity policies and qualified provisioning/enrollment adapters. Test autoscaler/manual-plan conflicts, exhausted capacity, safe scale-in, disruption budgets, and storage constraints.
- [ ] Qualify heterogeneous target evaluation, architecture-correct closure distribution, workload images, and supported device/network/storage integrations; do not infer portability from shared roles.
- [ ] Provide headless fleet inspection, plan/apply/resume, topology and drift views, and correlated structured events with equivalent CLI/TUI/GUI results.
- [ ] Qualify multi-site partitions, local authority policy, reconnect, unknown-state reporting, and recovery without overwriting newer runtime membership.
- [ ] Measure representative 100-, 1,000-, and 10,000-machine tiers with stated hardware, workloads, latency/resource budgets, and failure cases. Label simulations and advertise only demonstrated support limits.
- [ ] Publish exact fleet support combinations, per-cluster and fleet-wide limits, artifacts, security and recovery procedures, unsupported transitions, and release evidence separately from CppNix compatibility qualification.

## Longer-term work after the first OS release

- [ ] Add architectures, firmware modes, filesystems, encrypted-storage layouts, and hardware targets only with explicit build, boot, rollback, and recovery qualification for each new matrix row.
- [ ] Maintain compatibility matrices for later CppNix releases and Nixpkgs/Home Manager revisions. Separate additive nix-rs features from reference behavior with versioned opt-in settings; native-surface defaults such as Lua configuration stay out of compatibility commands.
- [ ] Support explicit migration between ordinary systemd-boot and Lanzaboote Secure Boot, plus signing-key rotation, only after their preview, recovery, and post-reboot verification paths are tested.
- [ ] Treat TPM-bound disk unlocking as a separate installer feature with a stated PCR policy, recovery credential, and tested response to firmware, bootloader, and kernel updates.
- [ ] Define a public extension API for NixBox operations and diagnostics without granting third-party interfaces unrestricted store or root access.
- [ ] Improve evaluation, cache substitution, local and remote builds, daemon startup, memory use, and large rebuild diagnostics using the MVP performance baselines. Repeat comparable measurements against the pinned reference before claiming improvements.
- [ ] Maintain release signing, vulnerability response, provenance records for imported crates, compatibility reports, migration notes, and supported upgrade paths across OS releases.

## Performance architecture and measurement

The [performance plan](docs/performance.md) defines 32 hypotheses and the [catalog](benchmarks/catalog.tsv) defines 154 scenarios across 18 families. The [runner](benchmarks/README.md) has 38 offline subset adapters; 116 scenarios remain planned. Initial O06/O09 changes bound source capture and index dependency edges. Each engine experiment needs affected compatibility checks, and native/experimental work follows its own feature gates.

- [x] Implement the initial paired timing runner, capability/blocker report, isolated state resets, process-tree accounting, timeouts, result schema, and correctness oracles. Repair existing evaluator benchmark validity and forcing checks before using them as evidence.
- [x] Implement and validate offline subsets for expressions, trees, derivation chains, databases, signed caches, builds, RPC pairings, profile rollback, maintenance, and injected failures. Preserve pre-change release binaries for paired comparisons.
- [x] Bound captured source bytes with immutable spooling and replace build-order scans with indexed reverse edges, retaining ready order, dependency inputs and serial realization.
- [ ] Complete the broader fixture dimensions and first ten full baseline contracts named in the performance plan, including warm-cache CACHE-04. Unsupported adapters and dimensions remain explicit.
- [ ] Measure operation-local metadata reuse, immutable source spooling, indexed graph planning, bulk store queries, protocol call reduction, and bounded log handling separately against pre-change nix-rs and pinned CppNix.
- [ ] Qualify bounded parallel realization and shared-goal coordination with resource limits, cross-client policy isolation, roots, cancellation, failure propagation, fairness, and mixed-engine daemon/store evidence.
- [ ] Add real package, standard shell, NixOS, Home Manager, and remote workloads as their compatibility support lands; add equivalent native environment/service comparisons after that extension exists.
- [ ] Investigate evaluator caching/representation, sandbox reuse, allocators/LTO/PGO, filesystem copies, and GC improvements from measured profiles; keep cold/warm and memory regressions visible.
- [ ] Evaluate chunked storage, lazy materialization, delta/metadata protocols, and fleet reuse in separate opt-in research lanes with first-use/full-use, offline, recovery, and compatibility import/export evidence.
