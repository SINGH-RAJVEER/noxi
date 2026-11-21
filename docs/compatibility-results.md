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

The `fast`, `store` and `daemon` tiers are nextest profiles in `.config/nextest.toml` (`cargo nextest run --profile TIER`). Their filters partition the workspace; only the toolchain-sensitive `nix-compat-derive-tests` UI snapshots and `#[ignore]` tests fall outside them, and `just test` still runs the former. On 2026-11-21 the profiles selected 2,117 fast, 957 store and 47 daemon tests, with no test in two tiers. The default profile's serialization overrides for garbage-collection tests apply to every profile.

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
