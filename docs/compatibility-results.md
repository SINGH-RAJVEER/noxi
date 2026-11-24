# Compatibility results and harness tiers

Every differential runner and every harness tier writes machine-readable results in one format, `noxi-compat-result/1`, so each row of the [compatibility matrix](compatibility-matrix.md) can point at inspectable evidence. The generated [evidence page](compatibility-evidence.md) counts the kept results per matrix row.

## Tiers

The workspace tests are split into four tiers. Each tier has the same entry point on a developer host, in CI and in a disposable NixOS VM.

| Tier | Entry point | What it runs |
|---|---|---|
| `fast` | `just test-fast` | Format and fixture checks that need no reference executable, no store on disk and no daemon: the unit tests of `noxi-compat`, `noxi-hash`, `noxi-flakeref`, `noxi-serde`, `noxi-compat-derive` and `noxi-events`, and the copied-fixture tests of `noxi-compat-tests` (`upstream_*` except the functional, language and file-transfer runners, plus `fixture_inventory`, `differential_eval`, `derivation_parser_extensions`, `reference_derivation_aterm` and `reference_nars`). |
| `store` | `just test-store` | Everything not in another tier: reference comparisons in isolated diverted stores, builds, caches, the evaluator and the commands. This includes `upstream_lang` and `upstream_functional` under both engines, and the `differential-eval` comparison. Needs `just reference`. |
| `daemon` | `just test-daemon` | `noxi-daemon`, `reference_daemon`, `reference_worker_wire`, `reference_daemon_client` and every other test whose name contains `daemon`. Needs `just reference`. |
| `system` | `just test-system MODE` | One disposable qualification VM scenario (`nixos`, `home-manager`, `lifecycle` or `daemon`) through `just qualification-report MODE`, with its report turned into records. `just compat-qualification` converts reports already committed under `tests/qualification/results/`. |

The `fast`, `store` and `daemon` tiers are nextest profiles in `.config/nextest.toml` (`cargo nextest run --profile TIER`). Their filters partition the workspace; only the toolchain-sensitive `nix-compat-derive-tests` UI snapshots and `#[ignore]` tests fall outside them, and `just test` still runs the former. On 2026-11-25 the profiles selected 2,122 fast, 1,028 store and 65 daemon tests, with no test in two tiers. The default profile's serialization overrides for tests that list roots or collect garbage apply to every profile.

The functional scripts' scratch roots go in `NOXI_FUNCTIONAL_SCRATCH` (default `/tmp`). Git discovery in those scripts, and in other tests that use `TMPDIR`, walks up from the scratch directory, so set both to a directory outside any repository when `/tmp` holds a stray `.git`, for example `NOXI_FUNCTIONAL_SCRATCH=/var/tmp TMPDIR=/var/tmp/noxi`. The functional runner refuses a scratch directory below a `.git`.

`just test` and `just ci` are unchanged and still run the whole workspace. `.github/workflows/compat.yml` runs `just compat-evidence-check` and the fast tier on every push and pull request, and the store and daemon tiers only on manual dispatch. The system tier needs KVM and is not in CI.

In a disposable VM, `nix/harness-tiers.nix` (imported by `nix/qualification-module.nix`, off by default) installs `noxi-harness TIER [DIR]`. It runs the same nextest profile from an archive built on the host with `just harness-archive`, through the same `scripts/compat-results.py run` wrapper, and writes records to `/var/lib/noxi-harness/TIER`. The module copies the source tree, writable, to the absolute path the archive was built in, because the test binaries read fixtures through compile-time paths, and records the revision from `noxi.qualification.harness.revision` because the copy has no Git metadata. No qualification scenario enables it yet, and no tier has been run inside a VM.

## Record directories

`scripts/compat-results.py run --tier TIER --dir DIR -- COMMAND` is the wrapper the recipes use. It:

1. writes `DIR/run.json` with the provenance below, the command and every `NOXI_*`, `CARGO_BUILD_*` and `NEXTEST_*` setting;
2. runs the command with `NOXI_RESULTS_DIR=DIR` and `NOXI_TIER=TIER`, so differential runners write their own records into `DIR`;
3. converts nextest's JUnit report for the profile (`target/nextest/TIER/junit.xml`) into `DIR/nextest.jsonl`, one record per test;
4. writes `DIR/summary.json` (`noxi-compat-summary/1`): counts per matrix row, runner and tier, the failing cases, and each runner's revisions.

A record directory holds `RUNNER.jsonl` files, one JSON object per line, and `blobs/SHA256` files for outputs too large to store inline. Runners that write their own records:

| Runner | File | Cases | Matrix row |
|---|---|---|---|
| `differential-eval` | `differential-eval.jsonl` | the ten pure-evaluation expressions | `nix-language-and-evaluator` |
| `upstream_lang` | `upstream-lang.jsonl` | every copied language fixture, against the live reference | `nix-language-and-evaluator` |
| `upstream_functional` | `upstream-functional.jsonl` | every selected functional script, under both engines | `copied-fixture-inventory` |
| nextest JUnit | `nextest.jsonl` | every test the tier ran | from `tests/compat-results/rows.toml` |
| VM qualification | `qualification.jsonl` | every step of a scenario report | from `rows.toml`'s `[qualification]` table |

Without `NOXI_RESULTS_DIR`, a runner writes beside its existing report: `compat-results/` next to the `differential-eval` report, `records/` in the `upstream_functional` output directory and in the profile directory of the `upstream_lang` reports. The existing JSON reports are unchanged.

## Record format

| Field | Meaning |
|---|---|
| `schema` | `noxi-compat-result/1`. A change that removes or renames a field, or changes a field's meaning, bumps the number. |
| `case` | `RUNNER:ID`, unique within a record directory. |
| `runner` | The runner that wrote the record. |
| `tier` | `fast`, `store`, `daemon` or `system`. |
| `matrix_row` | The [matrix](compatibility-matrix.md) row id the case is evidence for: the row's Area, lower-cased, with each run of other characters replaced by `-` (`scripts/compat-results.py rows` lists them). `null` when no mapping exists. |
| `inventory` | Reference interface inventory entries the case exercises, as `KIND:NAME`, for example `executable:nix-instantiate` or `command:nix eval`. Empty until the inventory mapping in TODO section 1 is done. |
| `run` | Provenance: `platform`, `kernel`, `invocation`, `started`; `noxi.revision`, `noxi.dirty` and `noxi.source` (`git`, or `NOXI_SOURCE_REV` where the tree has no Git metadata), with `noxi.executables`; `reference.release`, `reference.revision` and `reference.repository` from `tests/nix-official.lock`, with `reference.executables`. Executables are given by path, resolved path and SHA-256. |
| `settings` | What both engines shared: `environment`, `configuration`, `experimental_features`, `store`, `inputs` and `timeout_ms`. |
| `engines.reference`, `engines.noxi` | Each engine's capture, or `null` when that engine did not run. |
| `verdict` | `pass`, `fail`, `known-difference` or `unrun`, below. |
| `classification` | The runner's own class, such as `gap`, `anomaly` or a language outcome. |
| `reason` | Why a case is not a pass: the expectation note, the unexpected change, or why it did not run. |
| `differences` | The compared fields that differ between the engines: `exit_code`, `signal`, `timed_out`, `stdout`, `stderr`, `structured_output`, `produced_paths`, `derivations`, `nar`, `store_metadata` and `daemon_exchange`. |
| `test` | Only in nextest records: `binary_id`, `name`, `seconds`, and the captured output of a failure. |

An engine capture has:

| Field | Meaning |
|---|---|
| `argv` | The command line. |
| `exit_code`, `signal`, `timed_out`, `duration_ms` | How the process ended. |
| `stdout`, `stderr` | `size` and `sha256`, plus the bytes as `text` (UTF-8) or `hex`, or a `blob` path relative to the record file when larger than 32 KiB. `stderr` is the string `combined with stdout` when the runner captures both streams together, as the functional runner does with its log. |
| `structured_output` | Stdout parsed as JSON when it is a JSON object or array, otherwise `null`. |
| `store` | A snapshot of the store the case ran against, or `null`: `store_dir`; `paths`, every path in the store directory; `nars`, each path's NAR `sha256` and `size`; `derivations`, every `.drv`'s bytes as a byte field; `metadata`, the `ValidPaths` rows (`path`, `hash`, `nar_size`, `deriver`, `ultimate`, `sigs`, `ca`) with their `references`, registration times excluded. |
| `daemon_exchange` | A runner's normalized daemon exchange; `null` where the case runs no daemon. No runner fills it yet. |

Other runner fields, such as the functional script `outcome`, are added beside these.

### Verdicts

- `pass`: both engines agree on every compared field, or noxi matches the reference and the recorded expectation. For nextest records, the test passed.
- `fail`: a difference or failure no expectation records. This is the only verdict that fails a tier.
- `known-difference`: a difference recorded in the runner's expectation table, a noxi gap or a reference anomaly. It is never a pass.
- `unrun`: the case did not run under one or both engines (a skipped or upstream-disabled case, a C++ consumer suite, a blocked VM step, or a functional script whose reference column was not rerun in this invocation).

## Isolation and captured fields per runner

- `differential-eval` runs both executables with a cleared environment, an empty configuration directory, `NIX_REMOTE=dummy://` and the same timeout. The store fields are `null` because the cases evaluate without a store.
- `upstream_lang` runs the reference `nix-instantiate` and noxi's evaluator with `lang.sh`'s environment, flags and timeout; the store is `dummy://`. The compared bytes are normalized as the runner already normalizes them for its pass decision; the records carry the raw bytes.
- `upstream_functional` runs both engines in turn in the same empty scratch root, so the scripts' diverted stores share one `NIX_STORE_DIR` and the engines' store paths, derivation bytes, NAR digests and `ValidPaths` rows compare directly. Before 2026-11-21 each engine ran in its own random root, so store paths could not be compared. The environment is identical apart from the engine's directory at the front of `PATH`; the configuration is what the script's `common/init.sh` writes. The capture is the combined log, the exit status and the snapshot of `TEST_ROOT/store`. `differences` is informational: the verdict still follows the script outcomes and the expectation table, because logs contain timings and process ids.
- nextest records do not repeat the comparison a test makes internally; they record which test ran, at which revision, with which result.
- VM qualification records carry each step's command, user, exit status, stdout and stderr from the report, and the revision and installed executable digests from the report's `manifest.json`.

## Committed evidence

`just compat-keep DIR NAME` copies a record directory to `tests/compat-results/NAME` and refuses records from a dirty source tree. `just compat-evidence` regenerates [compatibility-evidence.md](compatibility-evidence.md) from `tests/compat-results/rows.toml` and the kept summaries. `just compat-evidence-check`, part of `just ci`, fails when the page is stale, a matrix row has no entry in `rows.toml`, `rows.toml` names a row the matrix lacks, a kept summary counts an unknown row, or an evidence link is broken. A kept result set is evidence for the revision it names only.

### Kept result sets, 2026-11-21

- `2026-11-21-37f8312-fast`: `just test-fast` at the clean revision `37f8312`. 2,117 tests ran: 2,115 passed and 2 failed. The failures are `derivation_parser_extensions::parses_and_round_trips_unchecked_output_names` and `parses_and_round_trips_unchecked_input_output_names`: `calculate_outputs` accepts the output name `drv`, which the tests expect it to reject. They fail the same way on `mvp/integration` at `1d4eccc`, and these changes do not touch the parser. The tests were wrong: the 2.35.2 reference evaluates `derivation { outputs = [ "drv" ]; ... }` to a `-drv` output path and records `["drv"]` as an input derivation's output, so since 2026-11-21 they expect `drv` to be accepted and a new `accepts_drv_as_an_output_name` case checks both forms.
- `2026-11-21-304441f-store-sample`: a sample of the store tier at the clean revision `304441f` against the pinned CppNix 2.35.2, not the whole tier. It covers `upstream_lang` (466 of 467 cases pass against the live reference; the upstream-disabled tail-call case is `unrun`), nine `upstream_functional` scripts under both engines (seven passes, and `add.sh` and `simple.sh` as recorded gaps), and the ten `differential-eval` cases (all pass). For the seven passing scripts, both engines' produced paths, derivation bytes, NAR digests and `ValidPaths` rows are identical; only the logs differ. The nix.conf each engine's script wrote is recorded and identical between the engines.
- `2026-11-21-304441f-daemon`: `just --no-deps test-daemon` at `304441f` against the pinned reference: all 47 daemon-tier tests pass.
- `2026-11-09-b6a0a10-vm-qualification` and `2026-11-09-ed44aff-vm-qualification`: the committed VM qualification reports at those revisions, converted to records. The VMs were not booted again.


### Kept result sets, 2026-11-25

All three ran at the clean revision `84aec12` against the pinned CppNix 2.35.2, with `TMPDIR` below `/var/tmp` and `NOXI_FUNCTIONAL_SCRATCH=/var/tmp`.

- `2026-11-25-84aec12-store`: the whole store tier, `just --no-deps test-store`. All 1,028 tests passed and the `differential-eval` cases match 10/10. Of the 214 functional scripts, 65 pass under both engines, 137 are noxi gaps, 10 are reference anomalies and 2 are unrun. The set is 22 MB, mostly the functional scripts' logs and store snapshots.
- `2026-11-25-84aec12-daemon`: all 65 daemon-tier tests passed.
- `2026-11-25-84aec12-fast`: all 2,122 fast-tier tests passed.

The first whole run, at `610ec54` with the functional roots in `/tmp`, failed only `functional_scripts_match_expectations` (1,026 of 1,027 passed). An empty `/tmp/.git` broke Git discovery for both engines, the expectation table was out of date for ten scripts that now pass, and `eval.sh` timed out because string coercion ignored `max-call-depth`; `main` at `9b7db67` has the same timeout.

`every_protocol_version_matches_the_reference` had failed in 2 of 8 earlier runs at the `FindRoots` count of protocol 1.19. It did not fail again: 20 runs of it alone or in the daemon tier, 45 beside its sibling tests and 198 of protocols 1.18 to 1.20 or 1.21 on a loaded host all passed, and so did 8 runs of its test binary on `main`. A process on the host whose environment names `wire-build-18`, which 1.18 builds and `simultaneous_clients_with_overlapping_builds_and_imports` also builds, raises the count at that token. The recorded failures are therefore consistent with a concurrent test's builder, which the test now avoids by running alone, but this is not proved: a worker or builder left over from the test's own 1.18 session would change the same token.

An earlier run at `84aec12`, while another session compiled on the same host (load average about 33), failed `functional_scripts_match_expectations` because the reference's own `lang.sh` exceeded the 120 s script timeout, and `builds_through_the_daemon_match_the_reference` because the two engines' sequential builds were registered 4 s apart where the test allows 2 s. Both passed in the runs kept above.

Until 2026-11-25 the test environments left read-only stores behind in `TMPDIR`, because `TempDir` cannot remove them. Tests that create stores now use `noxi-test-support`, whose `TempDir` makes the directories it owns writable before removing them, also when the test panics. It follows no symlink and refuses to cross a mount. Each test binary got a fresh `TMPDIR` of its own, for the whole workspace except `upstream_functional`, `upstream_lang` and the derive UI snapshots:

| | Commit | Tests | Left in `TMPDIR` | `reference_worker_wire` alone |
|---|---|---:|---:|---:|
| Before | `528b7aa` | 3,212 passed | 318 directories in 30 binaries, 5,839 MiB | 28 directories, 837 MiB |
| After | this change | 3,214 passed | 0 | 0 |

A test killed by a signal or a nextest timeout still leaves its directory, so give long runs a `TMPDIR` of their own.

A new VM run has not been recorded yet.
