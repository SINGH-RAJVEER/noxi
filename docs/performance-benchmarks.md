# Benchmarks against CppNix

Status: evaluator qualification adapter added 2026-10-25. The [runner](../benchmarks/README.md) implements 39 subsets of the 154 scenarios in [catalog.tsv](../benchmarks/catalog.tsv), including the locked four-path NixOS/Home Manager workload in EVAL-08. This document defines the broader contracts, including dimensions the runner does not yet collect. The [architecture plan](performance.md) names the hypotheses, and the [research appendix](performance-research.md) establishes existing CppNix behavior. Unsupported work remains visible in coverage reports.

## Questions every result must answer

- Does noxi perform the same work and produce the same usable package, shell, profile, or activated generation as the pinned reference?
- Which phase limits completion, and which optimization removes work or shortens the dependency path?
- How do cold setup, repeated setup, changed configuration, failures, concurrent users, and low-resource machines behave?
- Does saved startup time move to first package execution, page faults, lazy download, shell hooks, or service readiness?
- What CPU, memory, network, disk, retained-cache, and recovery costs accompany the elapsed-time change?

## Baselines and artifacts

Use the project-pinned CppNix 2.35.2 release at `2c73b59da29606068c0c98db015dd3a66955525d`. Build it with the existing `just reference` recipe in `devenv shell`; identify the reference binaries explicitly rather than using whichever `nix` appears on PATH. The local official source checkout is optional evidence, not the executable baseline. Preserve `argv[0]` command symlinks because CppNix selects legacy commands through them.

Build candidate binaries with the normal portable release profile and recorded feature set. The existing `release-with-debug` profile can support profiling; record any difference from distributed release settings. Do not time Cargo/Nix builds of the benchmarking tools or their package-manager artifacts inside installation scenarios. Record the packaging expression, source revision, dirty tree digest, Cargo lock digest, compiler, build flags, linked libraries, allocator, binary digests, executable paths, and versions for both engines. A JJ change ID alone is insufficient to identify a mutable working copy.

Each optimization needs two comparisons: candidate versus its pre-change noxi baseline, and candidate versus pinned CppNix. Keep raw values even where a case has no comparable reference command. Use a separate named lane for a newer CppNix release or custom source build; never mix it into the pinned report. Optional native CPU flags or PGO have their own lanes and held-out workloads.

Use `--host-policy PATH.json` to retain a predeclared host validity policy in run metadata. Its numeric fields are `max_memory_full_fraction`, `max_nr_throttled_delta`, and `expected_affinity`. The runner records host PSI, disk and VM counters, all ancestor cgroup memory limits and accounting before the matrix and again in `hardware-after.json`; every invocation retains host stall and throttling deltas outside its timed interval. `host-validity.json` evaluates whole-run system and ancestor memory full-stall fractions, ancestor CPU throttling, counter regressions and affinity against the declaration. Command correctness and host validity remain separate: the process exit status reports correctness, and an invalid lane retains all pairs. Review external activity separately, preserve every pair, and flag an invalid lane as a whole rather than dropping slow observations after seeing the result.

`benchmarks/eval-profile.py` runs the combined EVAL-08 oracle under a separately selected mechanism collector. Modes `stat`, `flat`, `dwarf`, and `lbr` collect user cycles/instructions or fixed-period user cycles; `diagnostics` and `allocation` request explicit counter JSON from the corresponding separately built feature. Each invocation records its binary identity, exact command and isolated environment, original four-path oracle, process-tree resources, and host state before and after. Profiling and instrumented runs are excluded from production timings. On hosts with `perf_event_paranoid=2`, DWARF/LBR collection may require `--user-callchains` in addition to `cycles:u`, since otherwise perf can request prohibited kernel callchains. The wrapper sets this explicitly. A successful short probe establishes permission only; inspect workload sample loss and unwind quality before using caller attribution.

The optional `benchmarks/eval-packages.py` wrapper adds ordinary locked Nixpkgs diagnostics, outside the catalog qualification. `EVAL-PKG-HELLO` forces `hello.drvPath`; `EVAL-PKG-PATHS` forces the same six derivation and output paths recorded independently in `matches_reference_nixpkgs_paths`; `EVAL-PKG-NAMES` forces hello, ripgrep and Git names. These use the root flake's locked Nixpkgs, resolved by host Nix outside timing. Every invocation must match its fixed independent JSON oracle and the pinned reference. Package path evaluation does not build, install or run those packages. Use the same runner options, for example `devenv shell -- taskset -c 0-3,12-15 uv run --no-project --no-config python benchmarks/eval-packages.py --reference-bin target/nix-reference/bin --candidate-bin target/release --case EVAL-PKG-PATHS --samples 30 --timeout 300`.

EVAL-08 defaults to strict evaluation of all four recorded qualification derivation paths together. For attribution, repeat `--eval08-output system`, `--eval08-output vm`, `--eval08-output module`, or `--eval08-output standalone` to create separate shuffled blocks for individual outputs. Each selects the corresponding list element lazily before strict forcing and must independently match its original recorded path and the reference. Split runs do not replace the combined four-output qualification or establish an aggregate speedup; shared evaluation work makes their sum a different workload.

The runner accepts additional preserved artifacts with repeatable `--comparison-bin NAME=DIR`. This puts every optimization stage into the same shuffled, correctness-gated block, so adjacent changes and the cumulative change can be compared under the same host conditions. On heterogeneous CPUs, set the affinity of the entire runner, for example `devenv shell -- taskset -c 0-3,12-15 just bench ...`, and retain its recorded affinity as a distinct resource lane. Never pin just the candidate. The [qualification evaluator measurements](performance.md#qualification-evaluator-measurements-2026-10-25) use this protocol for EVAL-08.

No benchmark may hide a helper engine. If noxi uses CppNix to prepare a derivation outside timing, call the case a realization benchmark. For an end-to-end install, its own timed evaluation/input/store work must produce the result. Record builder/helper executable identities, every store client/server pairing, and time attributable to each engine.

## Family contracts

All families collect elapsed time, process-tree user/system CPU, peak and steady memory where relevant, disk/network bytes, and correctness status. The TSV specifies additional metrics and required capabilities. The following contracts define how to turn a row into a fixture and adapter.

| Family | Fixture and measured work | Correctness and completion oracle |
| --- | --- | --- |
| START | Version/help dispatch, pure CLI launch, store open, and daemon socket-to-first-valid-reply. Separate child exec, configuration parse, and store initialization. | Exact expected success/error behavior and no unexpected store/network activity. Daemon readiness requires an authenticated protocol round trip. |
| EVAL | Valid deterministic expression corpus, generated attribute/list shapes, repeated imports, Nixpkgs package selection, module evaluation, pure cache reuse/invalidation, and import-from-derivation. | Required values forced to the same depth; compare outputs, exit/errors, derivation hashes/paths, string contexts, traces, and relevant effects. Compilation-only cases must be labeled separately. |
| FETCH | Fixed Git repositories/commits, archive server, dirty/local trees, filters, lock graphs, submodules, and credential-separated requests. | Identical fetched tree/NAR digest, logical paths, required lock-file effects, and invalidation after controlled content changes. Generated local Git fixtures have fixed author/timestamps. |
| PLAN | Synthetic chain, diamond, wide/shared graphs; real derivation sets; query-missing and evaluate-to-plan reuse. | Same selected outputs, graph, build/substitute/missing classification, and cost estimates under the same cache state. Planning does not realize outputs unless required evaluation effects do so. |
| CACHE | Local signed HTTP/file cache with deterministic NARs and dependencies, compressed variants, multiple priority caches, controlled latency/errors, and shared client requests. | Same required closure, trusted narinfo, NAR digests, metadata, and usable output. Record server requests and bytes so cache hits cannot hide skipped work. |
| NAR | Fixed generated trees with few large files, many small files, symlinks, modes, non-UTF-8 names, compressible/random data; dump/hash/import/verify/copy. | Byte-identical canonical NAR/hash, expected metadata, valid paths only after verification, and executable bits/symlink targets. Report file count and uncompressed/compressed sizes separately. |
| DB | Seeded compatible SQLite stores containing known paths, references, outputs, signatures, and roots; query, closure walk, registration, contention, and mixed-engine readers/writers. | Same graph/metadata, schema and reference invariants, transaction rollback, and successful CppNix inspection. Hash/normalize DB logical records rather than raw SQLite file bytes. |
| DAG | Static and dynamically discovered dependencies with fixed-duration/synthetic work, shared outputs, simultaneous clients, cancellation, and different resource limits. | Same requested outputs and legal dependency/failure behavior; one publication per deduplicated output; no root, settings, fairness, or policy leakage. Compare critical path with total work. |
| BUILD | Static BusyBox builders for no-op/CPU/I/O/output/refscan/log tests, then locked real source builds, sandbox planning, check/repair, and fixed-output failures. | Same derivations/outputs/NAR metadata and status, logs where required, sandbox behavior, and reference checks. Synthetic builder durations are not claims about real compiler acceleration. |
| RPC | All four CppNix/noxi client/server pairings on isolated sockets, repeated/bulk operations, connection startup/reuse, import framing, slow clients, and credentials. | Protocol/version behavior, responses, logs, trust/settings, frame boundaries, and clean reconnects. Time client-visible completion and account for all server processes. |
| PROFILE | Fixed package/output sets with duplicate names, priorities, collisions, large file manifests, install/upgrade/remove/rollback, and concurrent generations. | Same user environment tree, manifest, generation links, collision behavior, roots, executable selection, and rollback. Include user-environment builder work where reference requires it. |
| ENV | Locked language-tool devShells, `nix shell`, `nix develop`, `nix run`, legacy `nix-shell`, dirty input changes, shell hooks, and native service environments. | First child command sees required tools/environment and exact hook effects. Native service workflows additionally require equivalent dependencies/readiness and clean teardown. |
| SYS | Pinned minimal/workstation/server NixOS configurations and standalone/integrated Home Manager; evaluate/build/switch/test/boot/dry-activate/rollback in disposable VMs. | Same target generation and effects for the mode; activated services healthy, user environment usable, and booted generation verified when boot is the measured milestone. No host activation. |
| GC | Seeded rooted/unrooted closures, hardlinks, many generations, verification/repair/store optimize, concurrent imports, and root-registration races. | Identical live/dead sets and supported byte accounting, preserved live outputs, valid roots, and no premature publication/deletion. Include maintenance's impact on foreground installs. |
| MEM | Repeated evaluator/daemon/environment sessions, heap retention, cache eviction, request bursts, RSS/PSS process accounting, worker recycling, and resource limits. | No unbounded growth, no retained cross-user values/secrets, same outputs, and clean disposal. Capture cgroup peak memory so forked workers are included. |
| FAIL | Controlled source mutation, wrong hash/signature, truncation, timeout, disk full, process death, cancellation, changed trust, stale misses, and restart. | Expected failure/error/effects, no invalid registered output, safe cleanup, retry/recovery correctness, and other subscribers unaffected as specified. Recovery time counts to usable success. |
| REMOTE | Qualified SSH/serve builders and stores with fixed capabilities, queued load, network limits, shared closures, reconnection, and builder failure. | Same build selection constraints, verified input/output closures, authentication, logs/status, and no missing work delegated to an unmeasured helper. |
| EXT | Optional chunk/delta/lazy store prototypes, owned metadata endpoints, and fleet fanout with explicit target identities. | Ordinary NAR export/import plus fully used artifacts, byte/hash/trust correctness, offline and failure behavior. Separate from drop-in ratios; label simulations and prototype limitations. |

## Workload and fixture design

Use synthetic fixtures to isolate costs and real fixtures to validate relevance. Synthetic trees and graphs record generator version, seed, shape, payload digest, path/file/edge counts, executable/symlink counts, and uncompressed/compressed bytes. Shared dependencies need an exact overlap fraction. Fix canonical ordering and builder environment. A benchmark that emits an unforced lazy value does not measure evaluation of its contents.

Real suites pin Nixpkgs, NixOS, Home Manager, flake locks, service versions, and root configuration digests. Start with `hello`, a medium tool bundle, and a larger compiler/language environment. Measure NAR bytes and closure path counts from the pinned definitions rather than asserting stable sizes for package names. Standard shell fixtures cover Rust, Python, Node/TypeScript, mixed tools, and C/C++; use bun and uv where fixture dependency management applies. Builders and package invocations use absolute fixture paths.

Reuse `tests/qualification/` for reference system inputs and VM infrastructure, `crates/compat-tests/tests/reference_build.rs` for diverted-store/static-builder techniques, command reference suites for profiles/daemon/GC, and the differential evaluator for output comparison. Those existing tests remain correctness tests. Future timing adapters must extract stable setup and oracle logic without silently including fixture generation or reference expectation generation in measured work. Do not alter imported `tests/nix/` benchmarks to support project experiments.

Before timing, validate fixtures and run both adapters once. Reject parse errors, invalid expressions, unforced expected work, missing output selection, and unintended disabled features. The current Criterion evaluator helper discards its result and one generated merge expression is malformed; these checks must catch such mistakes. A deliberate failure case has a specified failure oracle and cannot enter a success-workflow average.

## States that must stay distinct

| State | Reset and measurement contract |
| --- | --- |
| Fresh user | New HOME/XDG/config/cache directories, identical fixture settings, no previous input or eval cache. Required credentials supplied from isolated fixture configuration. |
| Cold store | Clone the same seed lacking the selected outputs for each sample. Never let a warmup install turn the measured install into a no-op. |
| Partial store | Seed exactly 25%, 50%, or 90% of the requested closure by recorded identity; report overlap by both path count and bytes. |
| Warm store | Seed the whole required closure; still time validity checks, hooks, profile work, and the command milestone. Assert no unexpected downloads/builds. |
| Cold application cache | New engine metadata/eval/input cache, with store and OS page-cache state separately recorded. |
| Warm application cache | Populate that engine's own cache with the declared operation; reuse within its expiry and verify hits. Report population time and retained bytes separately. |
| Cold process | New CLI/daemon/evaluator worker processes, with other cache states unchanged. |
| Warm process | Reuse a declared coordinator/daemon/worker; include memory/idle cost, worker recycling, and repeated requests. CppNix uses its qualified persistent daemon plus its own disk caches. |
| Cold OS page cache | Fresh matched VM boots or documented cache reset inside disposable guests; report whether the cache server is cold too. Do not drop caches on the developer's host. |
| Warm OS page cache | Controlled untimed reads or repeat operations against immutable seeds. This differs from warm store and warm application cache. |
| Changed source | Modify one declared file/input/argument or a missing path/directory entry after warm population, verify fresh expected results, and measure invalidation work. |
| Offline | Disable guest/fixture network after required population; require the same advertised offline behavior and record any needed fallback. |

Logical store paths must remain identical, typically `/nix/store`. Use separate physical diverted roots where supported, or matched VM snapshots with the same mounted paths. Separate mutable homes, roots, profiles, sockets, and caches for each engine; never share a writable store unless mixed-engine access is the case being measured. Seed cloning, snapshot restore, fixture-server startup, and untimed validation happen outside the measured region. Record clone method and host page-cache effects; copy-on-write snapshots have write amplification that needs a separate measurement lane.

## Execution protocol

The planned runner uses explicit argument arrays, fixed environment maps, monotonic clocks, and a process-tree timeout; do not concatenate shell strings from case data. Python orchestration, if selected, uses uv for dependencies/venvs. Add required tools to the devenv shell when the runner is implemented. Criterion can cover Rust internals; a small pinned CppNix source benchmark adapter or equivalent CLI workload must compare the same work. A Rust-only microbenchmark is a candidate diagnostic, not a CppNix speed ratio.

1. Freeze the suite, artifacts, workload/config digests, hardware/resource budget, decision rule, sample count, and seeded A/B order before collecting samples.
2. Preflight executable versions/digests, daemon pairing, available command/options/features, resource accounting, fixture validity, and the complete correctness oracle. Mark unsupported capability rows `blocked`, with reason; do not quietly omit them or count them as faster failures.
3. Prepare matched isolated state for each A/B sample. For mutations, restore seed state before every sample. For read-only warm microcases, run the declared warmup, then measure in seeded randomized paired blocks.
4. Execute the operation under an isolated cgroup or equivalent process-tree collector. Measure foreground process, daemon and its children, builders, relevant supervisor/services, and any helper processes. Cache server resource use is reported separately and included when comparing a whole owned-cache architecture.
5. Capture timestamped phase/milestone events and result outputs. Timeouts kill the whole owned process tree, preserve the partial state report, and clean up declared roots/VMs without touching external processes.
6. Verify result artifacts and effects outside timing against fixed expected values or the reference result, including closure digests and final readiness. In performance cases for polling-based health, probe schedule and delay are part of the declared measurement.
7. Keep every sample, expected/unexpected failure, timeout, outlier, and exclusion reason. Pairwise ratios exist only for matching successful work; errors remain visible in coverage/failure reports.
8. Aggregate paired samples, store raw results and traces, and render phase charts/resource plots. A rerun due to environmental interference needs recorded evidence and a fresh paired block, not selective replacement of slow samples.

[Hyperfine](https://github.com/sharkdp/hyperfine) supports warmup/preparation and JSON export and is suitable for some read-only command cases. Stateful installs still need this runner's restore, oracle, resource, and paired-order rules. [Criterion's comparison groups](https://bheisler.github.io/criterion.rs/book/user_guide/comparing_functions.html) can compare equivalent inputs at multiple sizes; keep microbench throughput separate from end-to-end workflow timing.

## Phase and resource accounting

Emit `resolve`, `snapshot`, `parse`, `compile`, `evaluate`, `plan`, `metadata`, `download`, `decompress`, `hash`, `extract`, `register`, `queue`, `sandbox`, `builder`, `profile`, `shell-hook`, `activation`, and `readiness` spans where supported. Overlapped spans must not be added as if sequential. Report critical-path elapsed time, total worker busy time, queue waits, and uninstrumented remainder. Keep a reference adapter's coarse phases honest rather than pretending they have identical internal instrumentation.

Mandatory metrics are wall time to the declared milestone, user/system CPU, process-tree peak memory, exit/failure status, output/closure digests, input/output/network bytes, cache states, and benchmark metadata. Additional metrics include:

- Allocated bytes and counts, retained heap/cache bytes, PSS/RSS over time, page faults, instructions, cycles, cache misses, context switches, and syscalls in diagnostic runs.
- Narinfo/NAR requests, DNS/TLS/connect counts, cache-hit/negative-hit/invalidation counts, retries, compressed/uncompressed bytes, and wasted speculative transfers.
- Graph vertices/edges visited, repeated derivation parses, SQL calls, transaction sizes/commit latency, lock waits, checkpoints, fsyncs, and filesystem operation counts.
- Build slots occupied, queue wait per goal, critical-path work, deduplicated requests, service health-probe time, activation restart count, and user-visible logs/events.
- Fixture preparation time, cache population cost, retained disk footprint, and cleanup/recovery time in separate columns. Amortized service experiments also report total cost per session over a fixed request count.

Portable production timings use tracing/profilers off. Use separate matched diagnostic runs for allocation tracking, perf, syscall tracing, or verbose events, and quantify instrumentation overhead if phase claims depend on them. Forked/shared-process memory cannot be summarized by the parent RSS alone.

## Hardware and matrix selection

Start with x86_64-linux because that is the qualified baseline. Record CPU/microcode, topology, frequency/governor/turbo policy, RAM, kernel, libc, filesystem/mount options, disk model, cgroup limits, VM/KVM settings, background load, NUMA placement, and server hardware. aarch64 and other OS/filesystem targets are separate qualification rows.

Use a small explicit matrix rather than the full Cartesian product of every catalog dimension:

- One single-core/low-memory guest, one ordinary 4-8-core machine, and one high-core/fast-disk machine for CPU/coordination scaling. Example resource tiers are 1 core/2 GiB, 4 cores/8 GiB, and 16 cores/32 GiB; exact hardware still belongs in each report.
- SSD/NVMe, constrained virtual block device, and tmpfs diagnostic lanes. tmpfs exposes CPU/metadata cost but cannot establish durable install speed.
- Controlled cache networks at 0, 20, and 100 ms RTT and 10, 100, and 1,000 Mbit/s; fault lanes add jitter/loss/server throttling inside owned fixtures/VMs. Run public internet caches only as supplemental observational results.
- Concurrent clients at 1, 4, 16, and 64, with CPU/memory/transfer caps fixed. Expose fairness and p95 only with enough repetitions.
- `max-jobs=1` versus a declared multi-job setting, equal `cores`, sandbox and trust settings, cache priority, durability, experimental features, and retry/TTL settings. Report both shipped-default and matched-resource lanes when defaults differ.

Use pairwise dimension coverage for ordinary suites and explicit combinations for known interactions such as cold store + high RTT + many tiny paths, GC + concurrent registration, and high parallelism + low memory. Each report includes the selected matrix and excluded dimensions.

## Statistical and acceptance rules

For a benchmark pair, `speedup = CppNix elapsed / noxi elapsed`; values above 1 favor noxi. Candidate change ratios use the same formula with pre-change noxi as the numerator. Pair samples under matched conditions. Publish absolute medians, paired log-ratio estimates and 95% bootstrap confidence intervals, dispersion, sample counts, CPU/memory/bytes, and raw samples. Do not average speedup ratios arithmetically across unrelated cases.

Default planned collection is 30 paired samples for short offline cases and at least 10 paired samples for expensive build/VM cases. Ten expensive samples are exploratory and do not support tail-latency claims. For p95 under contention, collect at least 200 completed operations across multiple fresh runs, report uncertainty, and keep errors/timeouts separate. Autocorrelated requests are resampled by run/block, not falsely treated as independent. Continue collection only under a predeclared uncertainty/sample budget, not until a desired significant result appears.

Choose a primary case group and resource limits for each optimization before measurement. Hundreds of catalog comparisons create false positives; confirmation uses new runs and held-out real workloads. Predeclare an absolute noise floor per machine and scenario. Initial regression policy flags a greater-than-5% median slowdown whose paired 95% interval excludes no change, provided its absolute delta exceeds that floor. This is a proposed policy to calibrate with baseline variance, not a universal guarantee. Flag peak/steady memory increases above 10% or any quota breach for review; a latency win cannot conceal resource exhaustion.

An optimization can be accepted only when its primary workflow improves by its declared useful amount, relevant correctness/failure checks pass, and sentinel cold/warm/low-resource workflows meet their budgets. Keep beneficial narrow policies scoped if a broad default regresses other workloads. Summaries may use a declared fixed-workload geometric mean, but retain every per-case result and never combine unsupported, native, or experimental cases into a drop-in score.

## Completion milestones

| Workflow | Required elapsed-time endpoint |
| --- | --- |
| Package install | Requested profile/generation updated and a declared package command executes successfully. Also record realization-only and profile-only time. |
| Tool shell | A child inside the requested shell prints a validated environment/tool marker after required hooks. Also measure a representative first tool invocation. |
| Development stack | Tool closure ready, required initialization complete, every declared service dependency and health probe passes. A process PID is not readiness. |
| System build | Expected `system.build.toplevel` realized and verified, before activation. |
| System switch/test | Requested generation/mode completed activation and declared services/user session effects pass health checks. |
| Boot generation | Mode-specific command succeeds; separately time reboot until the selected generation and required services are verified. Boot command completion alone cannot count as the reboot. |
| Rollback | Selected prior generation restored and healthy under the declared mode; no claim that application data was reverted. |
| Recovery | Failure injected, operation restarted/retried, and the same requested usable result verified, including cleanup and root consistency. |

For native `noxi env`, compare a CppNix-backed workflow with identical locked packages, tasks, hooks, and an explicit pinned unprivileged supervisor implementing the same dependency/readiness work. Fix its launcher and include its runtime cost. Also compare realization and shell entry with equivalent standard CppNix commands separately. A minimal `nix develop` invocation is not an equivalent baseline for a ready multi-service stack. The reference driver/supervisor contract is a prerequisite before native rows are executable.

## Result record and artifact layout

The JSONL schema is versioned. The following table defines the full intended record. The initial runner records identities, arguments, fixtures, correctness, process-tree resources and cache counters; its metadata explicitly lists missing phase, hardware-counter and other measurements. Arrays and nested records avoid escaping shell commands into strings.

| Field group | Required fields |
| --- | --- |
| Identity | `schema_version`, `run_id`, `case_id`, `variant_id`, `sample_index`, `pair_id`, `order_seed`, UTC start time. |
| Artifacts | Engine role/version/revision/tree digest, binary path/digest, lock digests, feature/build settings, helper/client/server artifact records. |
| Workload | Generator/seed, input/config/output-selector digests, graph/tree sizes, logical paths, resource settings, timeout, milestone. |
| State | HOME/cache/store/profile seed IDs, page-cache/process/application-cache states, snapshot/clone method, changed-input record. |
| Execution | Exact argv/env redacted of secrets, process/cgroup IDs, elapsed/CPU/memory/I/O, counters, phase spans, observed milestone. |
| Correctness | Expected/actual status and output/effect digests, oracle version, comparison outcome, reason and evidence on failure. |
| Coverage | `planned`, `blocked`, `running`, `passed`, `mismatch`, `failed`, `timed_out`, or `invalid_sample`; blocker/exclusion reason, cleanup result. A passed benchmark sample does not establish full compatibility. |

Write `metadata.json`, `samples.jsonl`, `summary.json`, coverage and methodology Markdown, and optional traces under `target/benchmarks/<run-id>/`. Record artifact checksums in a manifest. Store large raw traces/stores/VM images as declared external artifacts, with retention rules and no credentials. Release evidence references immutable reports. Any aggregate report links to coverage so missing modern commands, remote builds, or native services are visible.

## Intended suites and implementation order

| Suite | Intended scope | Execution policy |
| --- | --- | --- |
| PR | Short offline START/EVAL/PLAN/DB/NAR checks and targeted cases for the changed mechanism. | Time budget around 10 minutes; noisy shared CI is advisory. Run correctness first. |
| Nightly | Local signed cache, build/graph/RPC/profile cases, concurrency/failure sweeps, memory retention. | Dedicated runner, selected dimension matrix, raw artifacts retained. |
| Release | Packaged binaries, all supported comparable families, real devShell/package workloads, disposable NixOS/Home Manager VMs and recovery. | Dedicated hardware, frozen inputs, uncertainty/report review; unsupported rows remain blockers for their advertised scope. |
| Research | Allocator/PGO studies, new storage/transports, lazy first/full use, future fleet tiers. | Explicit optional policy/backend and resource accounting, no drop-in aggregate. |

Implement runner validity tests before optimizing the engine: reject fast failures and invalid lazy fixtures, verify reset per mutation sample, preserve expected failure cases, include child/server CPU/memory, kill timeouts, and report unsupported adapters. Then implement deterministic fixtures, preflight/oracles, and baseline adapters by family. Extend existing related tests when implementation changes require it; the catalog is not a demand for hundreds of repetitive smoke tests.

The initial runner produces reports with blocked coverage and counters. The catalog gives the later suite breadth; it does not require every size/cache/network combination on every PR. See the [runner instructions](../benchmarks/README.md) for commands, exact implemented subsets, artifact layout and measurement limits.

## Initial local results

These are synthetic component results from the `performance-plan` workspace on 2026-10-11, on an AMD Ryzen AI 9 HX 370 with 24 available CPUs. The host was shared, affinity/frequency were uncontrolled, page cache was not reset, and build settings used one job and one core. They establish local mechanism evidence, not release qualification or an end-to-end install/system-setup speed claim.

`target/benchmarks/optimizations-64-v3/` contains 30 matched blocks per case across CppNix 2.35.2, preserved pre-change noxi, and the candidate. All 540 timed commands passed their oracles. The runner records exact artifacts, hardware, raw samples, output logs, resource/counter medians and bootstrap intervals. Candidate source digest was `6bd7b0b212c5bf0848a48eaaa7a59c4e613033974abadbdb5134503b8ac2693a`; pre-change source digest was `978ed6a21422577904ec68363403dec8a125cb390a429f4c8769bd01349d6940`.

| Component at scale 64 | CppNix median ms | Pre-change noxi median ms | Candidate median ms |
| --- | --- | --- | --- |
| Version dispatch | 18.652 | 1.321 | 1.295 |
| Forced generated list sort/fold | 19.719 | 2.965 | 3.005 |
| Derivation-chain dry-run | 18.294 | 4.254 | 4.384 |
| Recursive import, 64 MiB payload | 120.486 | 254.715 | 271.122 |
| Batched path hash queries | 24.128 | 6.545 | 6.694 |
| Serial 64-derivation realization | 453.143 | 388.157 | 390.813 |

The import spool reduced median target `wait4` RSS from 74.31 MiB to 17.93 MiB, about 76%. Cgroup peak memory stayed roughly flat, 131.96 MiB versus 133.53 MiB, because it includes the spool's file cache. The candidate had a higher elapsed-time median. Its paired geometric speedup against the old buffer was 1.027 with a 95% interval of 0.941 to 1.130, so this run does not establish a latency improvement or regression. CppNix still completed this import much faster. Spooling is currently a bound on heap growth, with extra temporary-file work, and must not be advertised as an installation speedup.

The 64-build workflow showed no clear benefit from indexed ordering: paired candidate/pre-change speedup 1.000, interval 0.956 to 1.043. The native diagnostic isolates ordering from sandbox/store work. `target/benchmarks/ordering-scaling.log`, `ordering-scaling-summary.json`, and `ordering-scaling-metadata.json` preserve 30 paired scan/indexed samples for chains and wide graphs at 256, 1,024 and 4,096 nodes. Every returned order matched the old implementation; cloning and checking were outside timing.

| Native ordering diagnostic | Old scan median ms | Indexed median ms | Paired speedup | 95% interval |
| --- | --- | --- | --- | --- |
| 4,096-node chain | 92.681 | 5.775 | 15.64 | 15.40 to 15.87 |
| 4,096 independent nodes | 9.788 | 2.707 | 3.60 | 3.57 to 3.63 |

These native ratios compare Rust algorithms, not CppNix commands. They justify removing the quadratic scan; real build latency remains dominated by other work at the tested scale. The next measured experiments should remove duplicated metadata queries and repeated source-ingestion passes, then test bounded parallel realization under the required compatibility and failure contracts.

The earlier `optimizations-64` collector inherited the Python fixture generator's RSS high-water mark. Its RSS fields are invalid and are superseded by `optimizations-64-v3`. A regression test now keeps a 96 MiB parent allocation alive while checking a small target's independent RSS. Do not use launcher RSS as target RSS.
