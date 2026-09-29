# Verification and release specification

## Reference evidence

The copied CppNix 2.35.2 tests under [tests/nix/](../tests/nix/) are the initial test oracle. The snapshot contains language fixtures, shell tests, NixOS integration definitions, C++ unit test sources, and golden data. It matches tag `2.35.2` (commit `2c73b59`) object for object, and `just nix-snapshot-verify` checks that it stays unchanged. Preserve the copied data and record the upstream revision on each refresh. [Test snapshot](#test-snapshot)

The reference is the newest numbered CppNix release, 2.35.2, as of 2026-09-28. The previous snapshot came from an unreleased 2.36.0 development commit; [the test snapshot notes](#test-snapshot) records the change. Reference executables for fixture comparisons must be the 2.35.2 release.

The full official Nix repository can be fetched into the ignored `tests/nix-official/` checkout with `just nix-tests-update`; `tests/nix-official.lock` records its exact revision. It is used to run CppNix's own build and test suite. It currently names the same commit as the snapshot. Its results do not substitute for running the same cases against nix-rs. When `just nix-tests-update` moves it to a newer tag, do not mix that tag with the fixed 2.35.2 fixture expectations.

`crates/compat-tests/` already classifies the copied ATerm derivation fixtures against vendored `nix-compat`. Its expectation table names unsupported forms, including content-addressed and dynamic derivations. These classified gaps are useful progress markers, not passing compatibility cases.

Every copied fixture file has a classification in `crates/compat-tests/expectations/fixtures.txt`: `table` (a named test lists each file with its expected outcome), `pass`, `gap`, `anomaly`, `unrun`, or `ignore` for placeholders and generator scripts. `cargo test -p nixrs-compat-tests --test fixture_inventory` walks the `src/*-tests/data` trees and `tests/` in the snapshot. It fails on these conditions:

- A file is unclassified.
- A pattern matches nothing.
- A `table` or `pass` entry names a test that does not refer to the fixture's directory.
- A `pass` entry's test does not name each file it claims.

On 2026-09-30 the 1,916 files were classified as follows:

- 1,116 are in per-file tables: 992 language, 58 worker-protocol, 20 path-info and nar-info, 17 store JSON, 14 derivation, 12 realisation and build-result, and 3 fetcher public-key files.
- 44 pass: the malformed NARs, six Git object files, the BLAKE3 and SHA-256 hash JSON files, and 28 shared common and serve protocol files.
- 1 is a reference anomaly: `hash/sha512.json` contains a SHA-256 hash because CppNix 2.35.2's `HashJSON` test constructs SHA-256 for its `sha512` row.
- 752 are unrun.
- 3 are ignored.

The seven shared primitive cases in each of the common and serve protocol fixture groups now decode and re-encode byte for byte in `upstream_shared_protocols`. Their 28 binary and JSON files are classified as passing; realisation and serve-specific protocol fixtures remain unrun, along with store-reference, git hashing data, and every shell and NixOS test outside `lang`.

`upstream_fetcher_public_key` checks the three copied fetcher key JSON fixtures. `simple` and `defaultType` read and write the same JSON, while `noRoundTrip` omits `type` on input and is read as `ssh-ed25519`; writing it makes the type explicit, as the 2.35.2 reference does. These source-fetcher declarations are separate from narinfo trusted verifying keys.

`upstream_git_objects` checks the copied Git blob and SHA-1/SHA-256 tree objects against `nix_compat::git` byte for byte, including their listed modes, names, and child hashes. It also rejects malformed object headers and tree entries. This covers the object encoding in the `git-hashing` fixture group; feature gating and source-accessor integration remain open.

The ATerm parser in `nix-compat` accepts what 2.35.2's `parseDerivation` accepts:

- Outputs, input derivations, each input derivation's output names, input sources, and environment variables may appear in any order. A repeated output keeps the first entry (`emplace`); a repeated input derivation or environment key keeps the last (`insert_or_assign`); repeated output names and input sources collapse into sets. An input derivation may list no outputs. Serialising again writes the canonical sorted form, as CppNix does. `upstream_derivations.rs` checks that 2.35.2's anonymised `hello.drv` and `firefox.drv`, which list inputs sharing a digest out of path order, parse and canonicalise stably.
- Every string field decodes `\n`, `\r` and `\t`, and a backslash before any other byte yields that byte, including in store paths, output names and the platform. Raw newlines, carriage returns and tabs are accepted.
- An empty platform, builder or environment key, and input derivations whose name lacks `.drv`, parse; only derivations built from a `DerivationBuilder` reject them.
- Output names on input-addressed outputs and input derivation references retain non-UTF-8 bytes and parse even when they are empty, reserved, or contain characters that a new derivation cannot use. Fixed-output entries also retain their parsed name. The parser round-trips those names; `DerivationBuilder::build_unverified` still rejects invalid names. Local cases live in `crates/compat-tests/tests/derivation_parser_extensions.rs`.
- Platform, builder command, argument, and environment-key fields retain non-UTF-8 bytes while parsing and serialising ATerm. The same local test file checks a derivation containing non-UTF-8 bytes in all four fields. JSON output keeps its existing UTF-8 field format and returns an error when one of these fields cannot be represented there.

Hash derivation modulo follows 2.35.2's `insert_or_assign`: when two input derivations mask to the same hash, the output names of the later one in path order replace the earlier one's. `crates/compat-tests/tests/reference_hash_modulo.rs` checks this against derivations that the 2.35.2 reference wrote for CppNix issue #16307's graph, stored in `crates/compat-tests/fixtures/hash-modulo-collision/` with the expression that produced them. Merging the output names instead, as `master` does since the #16307 fix, fails that test.

The hash-modulo lookup now distinguishes a regular derivation's single digest from a fixed-output derivation's per-output digest map. A requested fixed output is replaced by its digest and the canonical `out` name; an empty requested set contributes no masked input, and a missing fixed output returns an error. Focused `nix-compat` tests cover the empty and missing-output cases, while the 2.35.2-generated graph checks the ordinary fixed-output path calculation. This is local evidence for these cases, not a complete fixed-output or derivation compatibility result.

Differences from 2.35.2 that remain:

- JSON output cannot represent non-UTF-8 output names and returns an error for them; ATerm retains the original bytes.

The masked-hash, `bad-ca-fixed-path.drv`, and `windows-store-dir.drv` fixtures are absent from 2.35.2, and their tests were removed with the refresh. These checks cover narrow format behavior, not complete derivation or store compatibility.

CppNix 2.35.2's JSON forms for store paths, content addresses, output specifications, and deriving paths live in `nix_compat::cppnix_json`, separate from the imported `serde` implementations, which follow Snix's conventions. `upstream_store_json` decodes each of the 17 fixtures, compares the values CppNix's unit tests construct, and re-encodes 14 of them byte for byte. The other 3 are nested deriving paths that 2.35.2 accepts only with `dynamic-derivations`. Both engines reject them by default, and nix-rs cannot yet accept them with the feature enabled. `cppnix_json::{PathInfo, NarInfo}` implement the path-info and narinfo JSON in all three format versions. `upstream_path_info_json` checks that each of the 20 fixtures decodes to the value CppNix's tests construct, including the self-referencing content-addressed path. It also checks that the 18 fixtures CppNix writes re-encode byte for byte. `cppnix_json` also covers `DrvOutput`, `Realisation`, `UnkeyedRealisation`, and `BuildResult`. `upstream_realisation_json` checks all 12 realisation and build-result fixtures. The checks cover the decoded values, the re-encoded bytes, and realisation fingerprints. They also check that signing a fingerprint with the test key from `realisation.cc` reproduces 2.35.2's ed25519 signatures exactly, and that those signatures verify.

`crates/compat-tests/tests/upstream_worker_protocol.rs` decodes the number of values stated by each JSON fixture at the version CppNix's test uses, and requires byte-identical re-encoding (decoding only, for the lossy reads CppNix also only reads). A count-bound read is necessary because an optional value at EOF may be interpreted as absent rather than as stream termination. It also checks decoded values against those the C++ test constructs. Three fixtures in the snapshot are not referenced by its C++ tests and are marked stale. Fixtures that need protocol feature negotiation, and the handshake, remain gaps.

The pinned nightly previously crashed while compiling the derive UI fixture for an enum whose version ranges leave gaps. `nix-compat-derive` now reports the first uncovered minor version before emitting the async match, preserving a compile-fail regression without treating the compiler crash as expected output. The full `just ci` command passes on the pinned development toolchain.

`crates/compat-tests/tests/upstream_nars.rs` checks that the Rust archive reader rejects the eight malformed NAR fixtures in 2.35.2 with their expected error categories, and that a stalled writer fails a copy. 2.35.2 has no valid NAR fixture, so byte-for-byte archive output has no fixture oracle until a reference binary produces one. This covers archive parsing only; store import/export and cache exchange remain unrun.

`crates/compat-tests/tests/upstream_lang.rs` runs all 467 `parse-*` and `eval-*` cases in the copied `tests/functional/lang/` directory against `nix-rs-eval` with impure I/O and strict evaluation, which corresponds to `nix-instantiate --eval --strict`. It reproduces `lang.sh`'s environment (including the `NIX_PATH` and `HOME` overrides that apply only to `eval-okay` cases), absolute file arguments, working directory, exit statuses, `/pwd` substitution, `.postprocess` script, and expected-file comparisons. Each case runs in a separate process with a 20-second timeout, so hangs and stack overflows are recorded rather than stopping the suite. The test fails if a file in the fixture directory is not a case, a companion of one, or a listed support file, or if a listed support file is missing. Every case's observed outcome is recorded in `crates/compat-tests/expectations/upstream-lang.txt`, with the evaluator's error code for failing cases, and any change in either direction fails the test. A case that still fails for a different reason, such as a newly implemented builtin exposing the next error, therefore also fails the test. After reviewing an intended change, run the test with `NIXRS_COMPAT_BLESS=1` to rewrite the table. Each run writes a JSON report with outputs, exit codes, and timings to `target/tmp/upstream-lang-report.json`.

In a debug build, 104 cases pass. The other outcomes are:

| Outcome | Cases | Main causes |
|---|---|---|
| `error-message` | 278 | The case failed as expected but did not reproduce CppNix's diagnostic text. nix-rs uses a different error renderer, so none match yet. Many failures also have a different cause: the 21 `eval-fail-derivationStrict-*` cases fail because `derivationStrict` is missing, not for the reason under test. |
| `output` | 21 | 12 `parse-okay` cases have no printer equivalent to `--parse`. Of the other 9, 7 print a different value, one prints `builtins.trace` output with an extra type suffix, and one omits a deprecation warning. |
| `unexpected-success` | 21 | The parser accepts duplicate and mixed nested attributes. Evaluation accepts NUL bytes in strings and JSON/TOML input, out-of-range JSON numbers, incomparable `genericClosure` keys, a missing path, and several other invalid operations. |
| `unexpected-failure` | 18 | Missing builtins (`derivation`, `derivationStrict`, `path`, `convertHash`, `parseFlakeRef`, `flakeRefToString`, `break`), a CRLF parse error, `variable not found` errors in `import` and `with` cases, and string context and float handling. |
| `unrun-flags` | 19 | Cases with a `.flags` file need `nix-instantiate` options that have no nix-rs equivalent yet: linting, `--arg`, `-I`, and `--max-call-depth`. |
| `crash` | 5 | Integer overflow panics in the VM, and `toJSON` overflows the stack on deep nesting. |
| `disabled` | 1 | Upstream disables `eval-okay-tail-call-1`. |

Overflow checks are enabled only in debug builds. In release builds the four overflow cases wrap silently instead of panicking, so the table describes debug-profile `cargo test` runs. Neither behavior matches CppNix, which reports an error. The ad hoc checks at the top of `lang.sh`, covering `trace`, `warn`, `addErrorContext`, and `non-eval-fail-bad-drvPath.nix`, are not fixture files and remain unrun. `eval-okay-hash.exp` has no matching case upstream, so it is recorded as a reference anomaly.

`crates/compat-tests/tests/upstream_hashes.rs` checks BLAKE3 digests against three copied CppNix vectors and checks JSON encoding against its copied fixture. The library accepts BLAKE3 hashes, but the reference exposes them behind the `blake3-hashes` experimental feature. No nix-rs command or configuration layer enforces that feature gate yet, so BLAKE3 command compatibility remains unrun.

`crates/compat-tests/tests/reference_hash_formats.rs` records `nix hash convert` results from the pinned 2.35.2 binary for `abc` under MD5, SHA-1, SHA-256, SHA-512, and BLAKE3. It checks hex, nix32, base64, and SRI parsing and output, plus malformed digest rejection. A custom case found that 2.35.2 accepts uppercase hex; `nix-compat` now does too. The reference rejects BLAKE3 when `blake3-hashes` is disabled and accepts it when enabled. The Rust hash library has no feature setting, and nix-rs has no equivalent `nix hash convert` command yet, so that command-level gate remains open.

For each compatibility test, record the reference binary and nix-rs binary, platform, input revision, feature settings, store configuration, command or operation, expected result, actual result, and disposition. Tests that cannot yet run against nix-rs remain unrun. Tests that fail for a known reference issue still need an explicit compatibility decision; do not quietly rewrite expected output.

### Test snapshot

`tests/nix/` is a byte-for-byte copy of these paths from the CppNix 2.35.2 release. The upstream implementation source is not kept.

| Path | Purpose |
|---|---|
| `tests/nix/tests/functional/` | Shell tests, Nix language fixtures, expected output, and helpers |
| `tests/nix/tests/nixos/` | NixOS integration and virtual machine test definitions |
| `tests/nix/tests/installer/`, `filetransfer-retry-backoff/` | Other upstream test groups |
| `tests/nix/src/lib*-tests/` | C++ unit tests and golden data for the evaluator, fetchers, flakes, store, and utilities |
| `tests/nix/src/lib*-test-support/`, `tests/nix/nix-meson-build-support/` | Helpers those suites require |
| `tests/nix/COPYING` | Upstream license text for the copied material |

Relative fixture symlinks are preserved, including one deliberately broken symlink in the language fixtures, and `src/nix-functional-tests` points to the copied functional suite. The snapshot is not a standalone CppNix build: its Meson targets depend on upstream implementation code, so the original suite runs from the separate `tests/nix-official/` checkout.

The snapshot was extracted with `git archive` from tag `2.35.2`, commit `2c73b59da29606068c0c98db015dd3a66955525d`, the newest numbered release on 2026-09-28. `tests/nix-snapshot.lock` records that commit and the Git object ID of each of the 17 copied paths: the top-level files and directories, plus each entry under `src/` and `tests/`. `just nix-snapshot-verify` checks the working tree against those object IDs and rejects any unrecorded path at those levels; `just nix-snapshot-verify upstream` also fetches the commit's trees without file contents and compares them.

An earlier snapshot, introduced in `046abec1`, came from unreleased `master` commit `8f7ed1c1d` (version 2.36.0). The 2.35.2 tag is on the `2.35-maintenance` branch, which left `master` on 2026-06-22. It lacks the masked derivation fixtures, the valid NAR fixtures, 19 language cases, and `tests/fuzzing-engine/` from the earlier snapshot; the paragraphs above give the effect on each compatibility test.

`tests/nix-official.lock` and `tests/nix-snapshot.lock` currently name the same commit. `just nix-tests-update` moves the official checkout to the next numbered tag, or `bash scripts/update-nix-tests.sh master` or a specific tag selects another revision; the updater refuses to overwrite local changes in the checkout. The snapshot stays on 2.35.2 until it is refreshed deliberately. To refresh it, move the official checkout to the new revision, then run `just nix-snapshot-refresh`. It replaces `tests/nix/` with the checkout's `COPYING`, `.version`, `nix-meson-build-support/`, `src/lib*-tests/`, `src/lib*-test-support/`, `src/nix-functional-tests` and everything under `tests/`, rewrites `tests/nix-snapshot.lock`, lists added and removed paths, and runs the local check. Then run `just nix-snapshot-verify upstream` and revise the compatibility matrix and expectation tables. Never edit copied fixtures to make nix-rs pass; record a divergence instead.

## Differential testing

The initial `differential-eval` runner compares ten pure expressions using `nix-instantiate --eval --strict --expr` and `nixrs eval --expr`, with an isolated temporary home and identical controlled environment. It clears inherited variables, sets empty `NIX_PATH` and `NIX_CONFIG`, points Nix's system and user configuration paths at temporary empty locations, uses `NIX_REMOTE=dummy://` and `NIX_STORE_DIR=/nix/store`, and passes the caller's `PATH` to both engines; the report records that path. Every version probe and case has a 20-second timeout, overrideable with `NIXRS_DIFF_TIMEOUT_MS`. A timeout is recorded and makes the case fail. The runner preserves a legacy command symlink because CppNix selects its command from the executable name. Run `just eval-compare /path/to/nix-instantiate /path/to/report.json` in the development shell. The JSON report contains the executable versions, arguments, expressions, exit codes, stdout, stderr, timeout results, and mismatch disposition. Against the pinned reference, `just eval-compare-reference` matched 10/10 on 2026-09-28 using `nix-instantiate (Nix) 2.35.2` at `/nix/store/nskid3yq908g18x6cnvrk2hy96327f3f-nix-2.35.2`, the same path upstream's flake produces at `2c73b59`; the controlled runner also matched 10/10 locally on 2026-09-29. Ten pure expressions are development evidence, not language qualification. The flake preview package also built from a clean local Git clone, and its installed binary evaluated a nested value. This runner covers no store operations; its dummy store does not qualify a copied CppNix store or daemon behavior.

Compare both engines with identical inputs and isolated stores. The comparison must cover:

- Parse and evaluation results, strictness, warnings, errors, traces, source positions, string contexts, and import-from-derivation.
- Serialized derivations, input graphs, store paths, NAR contents, narinfo, cache signatures, and profile links.
- CLI flags, exit status, stdout, stderr, JSON, environment changes, and daemon protocol responses.
- Local and remote builds, sandbox restrictions, substitutions, cancellation, concurrent operations, corruption handling, and garbage collection.
- NixOS module evaluation, `system.build.toplevel`, activation actions, boot entries, generations, and rollback.
- Rebuild event semantics, rendered diagnostics, raw-log access, redaction, and machine-readable output as specified in [logging](logging.md).

Compare bytes where formats or command output require exactness. For build outputs whose content is intrinsically nondeterministic under the reference, compare behavior and invariants without incorrectly treating a changing output hash as proof of an engine defect. Keep these cases identified in the matrix.

The copied shell suite needs an isolated test store and compatible executables before it can validate nix-rs. The copied C++ tests are specifications for Rust ports and format fixtures; they are not Rust test results. NixOS tests require VM execution. [Test snapshot](#test-snapshot)

## System tests

The first system qualification target is defined by `tests/qualification/flake.nix` and its lock file. It pins Nixpkgs/NixOS, Home Manager, and CppNix independently; Home Manager follows the selected Nixpkgs input. `nix eval --raw path:./tests/qualification#nixosConfigurations.reference.config.system.build.vm.drvPath` checks the UEFI QEMU VM definition, and `nix build --out-link target/reference-vm path:./tests/qualification#reference-vm` builds its reference VM package. The VM has the pinned CppNix package, `nix-command` and `flakes`, and a Home Manager module user. The VM has an 8 GiB disk that holds its own CppNix store; it does not share the host's store. `just reference-vm-snapshot` boots it with CppNix only and waits for multi-user mode, the Nix daemon, and Home Manager activation. It then powers the VM off through ACPI and requires a clean shutdown. Finally it flattens the disk into the read-only base `target/qualification/reference-base.qcow2`, with its EFI variables and a manifest of checksums. `just reference-vm-overlay NAME` creates a copy-on-write overlay for one run, and `just reference-vm-verify` boots a temporary overlay and checks that the base is unchanged. The snapshot was taken on 2026-09-29 and verified on 2026-09-30. The base still has to be used for rollback and cross-engine tests before system qualification can pass. The expected empty-store and existing-store behavior is specified in [compatibility](compatibility.md#expected-store-states).

For each release matrix row, run installation and upgrade tests in virtual machines, then qualify claimed hardware targets. Verify first boot, login, networking, a system change through each client, rebuild after reboot, failed evaluation, failed build, stale preview rejection, interrupted switch, rollback, and recovery media. Include a migration test from a reference CppNix installation if existing-system drop-in support is claimed for that row.

For the later OS image, qualify ordinary systemd-boot and Lanzaboote Secure Boot as separate installer paths. Test key enrollment and enforcement on the Secure Boot path, signed generation updates, refusal to boot an unsigned artifact, key loss, failed signing, and recovery. The image must rebuild itself through nix-rs after installation. [Boot security](secure-boot.md), [system lifecycle](system.md)

The [configuration workflow](configuration-workflow.md) requires additional cases for external source edits, staged build versus saved-source mismatch, interrupted journal writes, and desired configuration diverging from the running generation after partial activation.

The [NixBox integration](nixbox-integration.md) must be tested at the engine interface: package lookup uses the selected lock revision, a typed operation produces the expected derivation graph and source change, realization uses the single store owner, and CLI/TUI/GUI receive the same events. A test that only inspects generated Nix text does not prove those behaviors.

Qualify the GPUI-based NixBox GUI in a graphical VM: verify package and option inspection, source-edit fallback, preview and confirmation, live progress during a long build, cancellation, disconnect and restart recovery, distinct pending/running/boot states, generation comparison, rollback, keyboard use, and accessible labels. Run the same operations through the CLI and TUI and compare their plans, events, authorization decisions, and resulting generations. Test headless and recovery workflows without a desktop session.

Run tests for the public C and C++ library interfaces or maintain an explicit list of incompatible consumers. A release with a required incompatible interface cannot claim 100% drop-in behavior for its matrix row. Security checks cover multi-user authorization, trusted users, sandbox escape attempts, cache signature rejection, and secret handling.

## Release gates

The package-manager MVP comes first. It can be described as a 100% drop-in replacement only when the CppNix 2.35.2 compatibility matrix is complete for its declared existing-system target and every applicable command, setting, format, build, store, daemon, cache, and public library interface passes. This includes rebuilding a supported existing NixOS installation with nix-rs, existing-store interoperation, return to the reference implementation, and a safe installation and rollback procedure for the package manager. Publish the tested binaries, source revisions, licenses, reports, signatures, and known limits with the release. OS installation, recovery-media, TUI, and GUI rows remain separate later gates and do not weaken this first compatibility claim.

The later OS release additionally requires:

1. The installed OS builds and activates its next generation using nix-rs, with no CppNix dependency for normal maintenance.
2. Installer, update, rollback, and recovery workflows pass on every advertised platform.
3. CLI, TUI, and GPUI-based NixBox GUI operations pass their shared behavior and accessibility tests.

If a gate is open, continue development and report the specific gap. Neither release can claim behavior that its matrix has not qualified.
