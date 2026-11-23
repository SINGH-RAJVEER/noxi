# Performance architecture and optimization plan

## Opt-in evaluator diagnostics, 2026-11-13

The evaluator's `eval-diagnostics` Cargo feature records executed opcodes and adjacent opcode pairs, frame scheduling and entry events, thunk construction and first/cached forcing, capture-length histograms, instrumented constructor allocation events and runtime layouts. It is absent from default builds. Build the command with `--features noxi-eval/eval-diagnostics` and set `NOXI_EVAL_DIAGNOSTICS` to a JSON output path. Snapshots accumulate on the evaluation thread across VM instances until `noxi_eval::perf_counters::reset()`; each completed evaluation atomically writes the cumulative snapshot. Separate threads and processes require separate output paths. The counters intentionally alter execution cost and must not be enabled in production timing comparisons.

Capture-buffer allocation counts are construction-site estimates. Instrumented thunk allocation payloads exclude reference-count headers and allocator overhead. These counters are not total allocator traffic or live-heap measurements. Derived capture clones and allocations outside the instrumented constructors are not included.

The independent command feature `eval-allocation-diagnostics` wraps the same production mimalloc allocator with allocation-free atomic accounting. Set `NOXI_EVAL_ALLOCATION_DIAGNOSTICS` to report successful/failed allocations and reallocations, requested allocation/deallocation bytes, and current/peak requested live bytes from process startup through CLI return, before report serialization. Requested bytes exclude allocator metadata, fragmentation, retained pages and temporary realloc copying storage. Concurrent background threads can make the snapshot fields nontransactional. Run this allocation-only build separately from opcode diagnostics and production timings.

The earlier 2026-11-13 work recorded its starting source and full dirty patch under `target/eval-20261113/{baseline,preservation}/`. New comparisons use a freshly built baseline with recorded source and executable hashes. The split EVAL-08 adapter retains the independently recorded path for each selected output; evaluating a single output is a different diagnostic workload from the four-output qualification and is reported separately.

The starting EVAL-08 diagnostic execution matched the four-path oracle and counted 245,306,061 logical opcodes, 70,584,198 bytecode frame entries and 36,679,134 thunk constructor allocations. Of 40,154,978 instrumented finalized capture arrays, 28,679,243 contain one or two static values. Instrumented capture-buffer payload totals are cumulative allocation estimates, not retained memory. These measurements motivate eliminating temporary suspensions before enlarging all capture representations with inline storage. A managed heap or VM-lifetime arena is not retained: cumulative traffic does not identify the retained graph, and returned values can outlive the VM that produced them.

Two production experiments are being compared independently. Boxing the rare error payload in `ContextMessage` reduces scheduler `Frame` size from 200 to 144 bytes while leaving `BytecodeFrame` at 128 bytes. The baseline workload never entered `ContextMessage`, so it adds no error-payload allocation on that execution. Three exploratory pairs do not establish a wall-time benefit for this change alone: paired speedup is 1.003x with interval [0.988, 1.012]. The immediate-force experiment targets 17,435,602 adjacent `ThunkSuspended`/`Force` executions. It enters the same bytecode and captured environment with a force continuation but no memoization cell for the unshared temporary. Deferred recursive captures and runtime observers keep the original path; a result that is another thunk reconstitutes the evaluated wrapper and uses the existing forcing path. Both experiments remain subject to final compatibility and representative performance checks.

User-only DWARF sampling requires `--user-callchains` in addition to `cycles:u` on this host. Omitting it requests disallowed kernel callchains. The resulting baseline profile matched the oracle, but 623 of 625 sampled memory-copy stacks still lacked a caller. Two resolved samples lead to frame-vector movement; they do not establish that all copies have that cause. LBR branch-stack sampling is unsupported by the available PMU. Exact profiler commands, errors and reports remain under `target/eval-20261113/`.

The first ten-block final lane was interrupted after memory pressure exceeded its declared host-validity threshold. Its completed and partial samples remain under `target/eval-20261113/final-eval08-10/` with `INVALID-HOST.json` and are excluded from performance qualification. An ancestor agent slice applies `memory.high` even though the leaf cgroup is uncapped. Inspecting only the leaf misses this limit. Clean pages from owned build artifacts can be advised out without modifying files or global host settings; final comparisons require a fresh quiet lane, with ancestor pressure and limits recorded, rather than selecting fast pairs from the interrupted run.

Status: evaluator hotspot measurements updated 2026-11-12; representative qualification measurements below remain separately dated; the current-main diagnosis and the evaluator cost reductions below are exploratory five-pair runs. The [runner](../benchmarks/README.md) supports 39 subsets of the 154 catalog scenarios. O06 bounds immutable source capture with an 8 MiB spool threshold; O09 indexes dependency edges while retaining serial realization and ready ordering. EVAL-08 now covers the qualification NixOS and Home Manager derivation paths. The remaining hypotheses and unimplemented parts of those hypotheses remain proposed work. The [benchmark specification](performance-benchmarks.md), [scenario catalog](../benchmarks/catalog.tsv), and [CppNix research](performance-research.md) define the evidence required before advertising an optimization.

## Evaluator attribute and context costs, 2026-11-12

Attribute updates now carry binding positions alongside the sorted merge, preserving the right operand's position even when it is absent. Chained merges retain these positions by offset; literal layouts provide source spans through their validated sorted permutation. This avoids constructing a new position hash map and looking up each merged key in both operands. Attribute sets without shared literal layouts use binary search for their first four lookups and build a hash index only when further lookups need it. Shared literal indexes retain their immediate initialization. Successful `addErrorContext` boundaries no longer traverse and format the outer trace stack; the suspended outer frames are counted when an error actually needs a context message.

The diagnostic [hotspot wrapper](../benchmarks/eval-hotspots.py) reuses the ordinary runner's independent expected JSON, reference comparison, shuffled matched blocks, isolated homes, dummy stores, process-tree accounting and bootstrap intervals. `EVAL-HOT-MERGE` performs 4,000 updates of a 256-binding literal with two subsequent selections and returns `9018000`. `EVAL-HOT-CONTEXT` returns `42` through 1,000 successful nested contexts. These cases are synthetic diagnostics outside the qualification catalog. CLI startup is included.

The preserved candidate is `target/agent-perf/c3/bin/nix-instantiate`, built with `CARGO_BUILD_JOBS=4 CARGO_TARGET_DIR=target/agent-perf cargo build --profile release-with-debug -p noxi-commands --bin nix-instantiate`. This profile inherits portable release optimization, fat LTO and one codegen unit, and retains debug information. The preserved base and intermediate artifacts under `target/agent-perf/{base,c2}/bin` came from this work session; their exact source snapshots were not recorded, so their comparisons are diagnostic rather than fully qualified ablations. The runner records executable hashes. The candidate's source record is adjacent to its binary directory.

Both runs use 30 matched blocks, seed 1729, and affinity `0-3,12-15` on the Ryzen AI 9 HX 370. All 120 merge and 90 context timed invocations passed their output oracles, as did their preflights. OS page cache, frequency and other host activity were uncontrolled. The merge timings below use a fresh run after profiling completed; these shared-host measurements are evidence for these specific diagnostic workloads, not a general evaluator qualification. Medians are shown below; speedups are paired geometric means with 95% bootstrap intervals.

| Workload / engine | Wall ms | CPU ms | Peak RSS MiB | Candidate speedup over engine |
| --- | ---: | ---: | ---: | --- |
| Merge / preserved base | 46.838 | 46.290 | 98.61 | 2.090x [2.000, 2.185] |
| Merge / before deferred indexes | 27.799 | 27.120 | 71.21 | 1.212x [1.190, 1.237] |
| Merge / CppNix 2.35.2 | 28.372 | 28.274 | 80.41 | 1.241x [1.197, 1.293] |
| Merge / candidate | 22.783 | 22.599 | 58.17 | n/a |
| Context / preserved base | 41.032 | 40.049 | 23.01 | 12.208x [11.434, 12.917] |
| Context / CppNix 2.35.2 | 15.762 | 16.053 | 49.20 | 4.793x [4.498, 5.068] |
| Context / candidate | 3.266 | 3.327 | 21.27 | n/a |

The fixed four-path EVAL-08 workload also passed all 15 timed invocations and three preflights in `target/agent-perf/final-eval08/`. This exploratory five-pair run does not establish a representative improvement: the candidate median is 19.518 s versus 21.684 s for the preserved base, but its 26.691 s and 49.503 s outliers produce a paired geometric speedup of 0.860x [0.577, 1.156]. CppNix's median is 3.789 s, leaving the candidate 5.15 times slower by median elapsed time. Median CPU times are 18.592 s candidate, 19.966 s base and 4.345 s CppNix; median peak RSS is 2,958.33 MiB, 3,059.07 MiB and 996.00 MiB respectively. Shared-host variation and the limited sample count prevent a general speed claim. Beating CppNix on representative NixOS/Home Manager evaluation remains open.

Raw records, environments, executable identities and output digests are in `target/agent-perf/final-{merge-clean,context,eval08}/`. Reproduce after preserving the desired artifacts:

```sh
devenv shell -- taskset -c 0-3,12-15 uv run --no-project --no-config python benchmarks/eval-hotspots.py --reference-bin target/nix-reference/bin --candidate-bin target/agent-perf/c3/bin --baseline-bin target/agent-perf/base/bin --comparison-bin before-index=target/agent-perf/c2/bin --case EVAL-HOT-MERGE --samples 30 --size 4000 --output target/agent-perf/reproduce-merge
devenv shell -- taskset -c 0-3,12-15 uv run --no-project --no-config python benchmarks/eval-hotspots.py --reference-bin target/nix-reference/bin --candidate-bin target/agent-perf/c3/bin --baseline-bin target/agent-perf/base/bin --case EVAL-HOT-CONTEXT --samples 30 --size 1000 --output target/agent-perf/reproduce-context
devenv shell -- taskset -c 0-3,12-15 uv run --no-project --no-config python benchmarks/run.py --reference-bin target/nix-reference/bin --candidate-bin target/agent-perf/c3/bin --baseline-bin target/agent-perf/base/bin --case EVAL-08 --samples 5 --timeout 300 --output target/agent-perf/reproduce-eval08
```

Flat `cycles:u` profiles identified the removed work. In the preserved base's merge profile, position-map insertion accounted for 13.33%, position-map lookup for 6.92%, and lookup-index initialization for 10.30% of samples. After position changes, index initialization grew to 20.39% of a smaller total, motivating deferred indexes. In the final profile it no longer appears among the leading entries; cloning the binding slice accounts for 24.62%, and collecting binding positions for 11.65%. In the base's successful-context profile, trace construction alone accounts for 16.60%, with another 14.29% in codemap span lookup and 6.88% in formatting. These short profiles support the mechanism; they do not replace matched timings. Reports and raw profiles are under `target/agent-perf/{base,c2,final}-merge*` and `target/agent-perf/base-context*`.

A separate candidate EVAL-08 profile, collected after timing with the same affinity and a fresh isolated environment, matched all four paths and recorded 8,228 samples with zero loss. Its largest entries are bytecode execution at 15.47%, libc memory movement at 8.13%, lambda execution at 7.37%, capture population at 4.33%, and value cloning at 3.45%. Attribute lookup is 1.06%. The remaining representative gap therefore needs broader VM and value-management work. Raw data and the report are `target/agent-perf/final-eval08.data` and `target/agent-perf/final-eval08-profile.txt`.

Correctness validation passes 533 evaluator tests plus one doctest with `devenv shell -- env CARGO_BUILD_JOBS=4 cargo test -p noxi-eval --quiet`. Added tests cover span provenance across balanced and skewed updates, KV operands, absent right-side positions, repeated updates, literal source-order permutations, and lookups before and after index creation. Direct comparisons against the pinned reference also match exit status, stdout and stderr for lazy successful contexts, nested abort contexts under `--show-trace`, and caught throws.

## Evaluator success-path diagnostics round, 2026-11-13

This round removes diagnostic work that ran on successful evaluation and keeps the two experiments recorded above. All measurements use the four-path EVAL-08 expression (`benchmarks/eval-08.nix`) under the runner's isolated environment (dummy store, empty `NIX_PATH`, fresh home), affinity `0-3,12-15` on the AMD Ryzen AI 9 HX 370, and `release-with-debug` builds (portable release, fat LTO, one codegen unit, debug information) from `HEAD` `67d460f` plus the uncommitted working tree. Every run's strict JSON output had digest `31328840df22d3ff`, byte-identical to pinned CppNix 2.35.2's output for the same command.

Changes are cumulative. Instruction counts are single `perf stat -e instructions:u` runs, which stay stable while wall time varies with host load. Other agents edited command and store crates between builds; the eval crate dominates this workload, but the last step's count includes their concurrent edits.

| Artifact | Change | EVAL-08 user instructions | Step |
| --- | --- | ---: | ---: |
| `target/agent-perf/base` | starting tree | 190.28 billion | baseline |
| `c1` | Count the outer traces of an `addErrorContext` boundary only when a context message is needed, not on every successful boundary | 173.08 billion | -9.0% |
| `c3` | `//` carries binding positions by offset instead of building a position hash map; deferred lookup indexes (see 2026-11-12 section) | 165.29 billion | -4.5% |
| `c4` | Box the cold `ContextMessage` error (scheduler `Frame` 200 to 144 bytes); enter a thunk constructed for an immediate `Force` directly without a memoization cell | 154.22 billion | -6.7% |
| `c5` | Share compiled regexes through `Rc` instead of cloning them, since a cloned `regex::Regex` starts with an empty match cache; cache each file's line starts for CppNix-style positions, which `derivationStrict` requests for every derivation's `name` | 146.76 billion | -4.8% |

The cumulative instruction reduction is 22.9%. A pinned CppNix invocation used about 41 billion instructions in earlier measurements, so noxi still executes about 3.6 times as many.

Ten shuffled matched blocks then compared CppNix, the starting tree and `c5` (seed 1729; 30 invocations, all matching). Memory pressure (`some avg10`) stayed between 1.4% and 4.2% during the run; compilation by other agents was not running but host activity was otherwise uncontrolled. Medians:

| Engine | Wall s | User CPU s | Peak RSS MiB |
| --- | ---: | ---: | ---: |
| CppNix 2.35.2 | 3.195 | 3.845 | 968.8 |
| Starting tree | 18.475 | 16.925 | 3,062.1 |
| `c5` | 15.200 | 13.765 | 2,953.2 |

`c5` is 1.226 times as fast as the starting tree (paired geometric mean, 95% bootstrap interval [1.212, 1.247]) and uses 3.6% less peak RSS. It remains 4.76 times slower than CppNix ([4.71, 4.81]) and uses 3.05 times its RSS. Ten blocks are fewer than the runner's 30-pair qualification default; the interval is narrow, but this is a diagnostic lane rather than a general speed claim. The near-reference gate remains open.

Validation at `c5`: 544 evaluator unit tests and the doctest (also with `eval-diagnostics`, adding five counter tests), the language runner with `NOXI_REFERENCE_BIN` (466 of 467 fixtures pass, one disabled, 466 exact live reference matches, all outcomes match the expectations file), and `just nixos-paths` all pass. `c4` passed the same suites before the last step.

Profiling method: the host rejects kernel callchains, so stacks came from a gdb-driven sampler (periodic `SIGUSR1` interrupts with `thread apply all bt`) and breakpoint backtraces on hot symbols. These samples are biased toward page-fault and syscall boundaries and were used only for attribution; flat `cycles:u` profiles and instruction counts decided changes. Before the first change, 7.4% of sampled stacks were trace construction under `addErrorContext`, all from one call site.

Remaining success-path diagnostic costs and candidates, not changed in this round:

- `builtins.genericClosure` prints every element with `ValuePrinter`, and both compared elements at every binary-search step, before it can know whether an error occurs (about 1% of cycles in `ValuePrinter` and string literal printing). CppNix prints in its `catch` handlers, after forcing. Deferring the print needs a lazily rendered trace entry and changes noxi's output in error cases toward CppNix's, so it needs its own fixtures.
- ATerm escaping in `nix-compat` runs an Aho-Corasick search per derivation string while hashing (about 1.1% of cycles). A scan for bytes needing escaping before the search is a candidate outside this crate's ownership.
- The flat profile is now led by bytecode dispatch (about 16%), the frame loop (7%), memory copying (6%), capture population (5%) and value cloning (5%). These need representation changes, such as smaller frames, compact captures and fewer generator frames, rather than more success-path fixes.

Artifacts: binaries in `target/agent-perf/{base,c1,c3,c4,c5}/bin`, test logs `target/agent-perf/nixos-paths-c{4,5}.log`, and the language runner reports under `target/agent-perf/tmp/`.

## Qualification evaluator measurements, 2026-10-25

EVAL-08 evaluates the exact four derivation paths from `just nixos-paths`: the qualification NixOS system, its VM, its Home Manager module generation, and a standalone Home Manager generation. Nixpkgs is locked at `e94cb152ed51bd6e24eb4a41f1460252beb52cd2` and Home Manager at `7b4c5ec4bedaf1e062bbc1bcaeddbc6bd242aa1b`. Every engine must independently match all four recorded paths as strict JSON and then match the pinned reference. Host Nix resolves the source paths outside timing; each timed engine evaluates the modules itself against a read-only dummy store.

The final run, `target/benchmarks/eval-hotpaths-final-30`, contains 30 shuffled matched blocks with five release artifacts, plus one preflight per artifact. All 155 invocations passed. Builds used the normal portable release profile through `devenv shell -- just bench-build`. All engines inherited affinity `0-3,12-15` on an AMD Ryzen AI 9 HX 370, four physical faster cores with their SMT siblings. This is a fixed-resource lane: those cores advertise 5.16 GHz versus 3.29 GHz for the other cores. The governor remained `powersave`; OS page cache and other host activity were uncontrolled. These are fresh CLI processes and isolated application state.

Wall and process-tree CPU times are medians. RSS and cgroup memory columns are medians of each invocation's peak. Cgroup memory includes charged page cache and is distinct from target process RSS. Step speedups are paired geometric means with 2,000-resample bootstrap intervals; the final column is the ratio of wall-time medians.

| Engine / cumulative change | Wall s | CPU s | Peak RSS MiB | Cgroup peak MiB | Speedup over preceding noxi, 95% CI | Wall / CppNix |
| --- | ---: | ---: | ---: | ---: | --- | ---: |
| CppNix 2.35.2 | 3.186 | 4.143 | 979.4 | 967.0 | n/a | 1.000x |
| Original noxi, `569e1b3` | 22.927 | 22.839 | 4,694.8 | 4,697.7 | baseline | 7.197x |
| Direct captures, `04469d5` | 21.737 | 21.656 | 4,612.4 | 4,615.4 | 1.053x [1.044, 1.060] | 6.824x |
| Memoized forcing, `2fe6949` | 20.450 | 20.377 | 4,612.7 | 4,615.3 | 1.065x [1.058, 1.075] | 6.419x |
| Shared mapped-call bytecode, `772d833` + `a77b06f` | 19.526 | 19.419 | 3,240.5 | 3,241.0 | 1.047x [1.041, 1.054] | 6.129x |

The cumulative paired speedup is 1.175x, 95% interval [1.170, 1.180]. The wall-time medians differ by 3.401 s, a 14.8% reduction. Median peak RSS fell by 31.0%; the largest observed target RSS was 4,695.1 MiB for the original and 3,241.2 MiB for the final candidate. Evaluation still takes 6.1 times the reference's median elapsed time, so TODO section 3's near-reference gate remains open. These measurements qualify this four-path workload in this resource lane.

The changes address costs identified by flat `cycles:u` profiling:

- Directly install populated captures when constructing VM thunks and closures, reserve the known capture count, and remove redundant reference-count operations. This avoids allocating a capture vector only to replace it immediately.
- Read fully memoized thunk values directly in bytecode and builtin force/try-force requests. Suspended thunks, blackholes, and evaluated thunks pointing to another thunk retain the normal forcing path.
- Share one generated application lambda per invocation of `map`, `mapAttrs`, `genList`, and `zipAttrsWith`. Each resulting thunk keeps its own argument and callee captures. Empty results return before allocating application bytecode. The ordinary suspended-thunk machinery, source spans, and error types remain unchanged.

After timing, fresh controlled profiles of the preserved original and final artifacts used the same isolated environment and affinity. `perf record -e cycles:u -c 10000000` collected 10,756 and 9,211 samples respectively, with zero lost samples; both profiling invocations also matched the four-path oracle. The visible libc allocation/free/reallocation routines account for approximately 22% of original samples and 19% of final samples. In the final profile, bytecode dispatch accounts for 9.71%, generator dispatch for 8.20%, and thunk forcing for 5.11%. These fractions describe a smaller total cycle count, approximately 107.56 billion original versus 92.11 billion final, and do not replace the 30-block timing estimate. Further evaluation-time work needs to address the remaining VM/generator and value-management costs. Raw data, flat reports and exact commands/environments are in `target/benchmarks/profiles/controlled-{original,candidate}.{data,txt,json}`; `target/benchmarks/profile-eval.py` reproduces them. The host rejected frequency sampling and call-stack capture, so these are fixed-period, user-space flat profiles.

The reference-generated `eval-okay-shared-mapped-call-captures` fixture checks empty-result laziness and independent captured values for all four builtins. Full CI passed 2,726 nextest tests with five skipped and all doctests; all twelve benchmark-runner tests passed. Both `just nixos-paths` and `just nixpkgs-paths` passed unchanged after every optimization stage.

The original release and each intermediate artifact, source identities, exact arguments and isolated environments, fixture digest, CPU/I/O counters, target RSS, cgroup memory, stdout/stderr, and manifest are retained under `target/benchmarks/`. The final manifest's 470 files and every measured binary digest were verified. `eval-hotpaths-final-30-adjacent-summary.json` and `analyze-eval.py` retain the adjacent-stage analysis outside the sealed run directory. Preliminary `eval-baseline`, `eval-captures`, and `eval-forced` runs were exploratory; the partial `eval-hotpaths-30` and `eval-hotpaths-bigcores-30` matrices were stopped to control affinity and remove empty-result allocation respectively. Their samples are not pooled with the final run.

Reproduce the final matrix after preserving artifacts as described in the benchmark README:

```sh
devenv shell -- taskset -c 0-3,12-15 just bench target/nix-reference/bin target/benchmarks/eval-final/bin target/test-busybox --baseline-bin target/benchmarks/eval-forced-baseline/bin --comparison-bin original=target/benchmarks/eval-original/bin --comparison-bin captures=target/benchmarks/eval-captures-baseline/bin --case EVAL-08 --samples 30 --timeout 300 --output target/benchmarks/eval-hotpaths-repeat
```

## Second evaluator round, 2026-10-26

The preserved round-one release in `target/benchmarks/eval-final/bin` is the baseline for this round (19.526 s median wall time and 3,240.5 MiB median peak target RSS in the preceding controlled run). Builtin values now share immutable metadata with copy-on-write partial arguments; completing an application copies only captured arguments when necessary. The `eval-okay-shared-builtin-partials` reference fixture checks independent reuse of those captures. Non-async builtin bodies also register a synchronous entry point: fourteen type predicates, list operations and attribute lookups can bypass generator construction when all strict arguments are ready. Suspended arguments retain the existing generator and forcing order; lazy arguments remain untouched. Macro tests cover both paths, catchable propagation and captured state. No error types changed.

The exploratory runs `round2-shared-5` and `round2-shared-3` were interrupted because concurrent worktree builds caused substantial host memory pressure and swapping. Their completed invocations matched the independent path oracle, but their timings are not used to claim an improvement. New performance claims require a fresh fixed-affinity matched comparison.

The bytecode dispatcher now continues directly after a partial builtin application or successful synchronous call, avoiding a frame push/pop, another outer dispatch and a source-span lookup. Suspending calls and errors retain the caller frame and its original span. A release differential check covers 46 success, error, catchable, partial-application and control-flow expressions, preserving exit status, stdout and full trace stderr against the preceding artifact.

## Current merged evaluator diagnosis, 2026-10-26

A fresh portable release of main `a7548847` was compared with the preserved round-one release and pinned CppNix 2.35.2 on EVAL-08. The same fixed affinity `0-3,12-15`, locked inputs, read-only dummy store and independent four-path JSON oracle were used. The host passed a 20-second quiet-window check before collection; maximum full memory PSI remained 0.00% throughout. No compilation overlapped the timed run. Application state was isolated, but OS page cache and unrelated host activity were not controlled. These five shuffled matched blocks are diagnostic evidence, not a replacement for the earlier 30-block qualification or a general package-management score. All fifteen timed invocations and three preflights matched the oracle.

| Engine | Median wall s | Median process-tree CPU s | Median peak target RSS MiB |
| --- | ---: | ---: | ---: |
| CppNix 2.35.2 | 3.267 | 4.226 | 966.4 |
| Preserved round-one noxi | 20.064 | 19.952 | 3,240.2 |
| Current main, `a7548847` | 22.878 | 22.776 | 3,324.0 |

Current main takes 7.00 times the reference's elapsed time and 3.44 times its peak RSS in this small run. It also takes 14.0% longer than the preserved noxi release and uses 2.6% more peak RSS. Current-main wall times span 22.558-23.332 s; reference times span 3.227-3.272 s. The earlier 19.526 s result does not describe the newly merged tree. Median physical disk reads and writes were zero for both engines, and noxi's CPU time is nearly equal to elapsed time: the measured delay is evaluation work, rather than downloads, building or an idle process waiting on those services.

Fixed-period user-space `cycles:u` profiles were collected separately from timing for all three artifacts. Call stacks were unavailable under the host's `perf_event_paranoid=2` policy, so these are flat, exclusive symbol samples, not inclusive call-tree costs. The current, preceding and reference profiles contain approximately 109.84, 95.36 and 19.12 billion sampled-event cycles respectively, with zero lost samples. Function boundaries and inlining differ between artifacts; percentages alone cannot isolate the causal effect of an individual commit.

The current profile assigns 18.99% to libc allocation/free symbols, 10.28% to bytecode dispatch symbols, 7.60% to generator driving/resumption, 3.81% to bytecode operand decoding, 3.97% to thunk forcing, 2.59% to capture population, 3.39% to value-cloning symbols and 2.39% to libc memory copying. Generator re-enqueueing adds 2.55%. These paths support the primary diagnosis: the evaluator performs substantial scheduling, ownership and data-management work per lazy language operation. Parser/compiler symbol groups account for 5.04% of this flat profile, so parser replacement is a lower-priority hypothesis for this workload.

The source explains the mechanisms. `vm/generators.rs` represents resumable work as `Gen<VMRequest, VMResponse, Pin<Box<dyn Future<...>>>>`; forcing an unready value can re-enqueue one frame, allocate another generator and return through the outer VM loop. `Thunk` uses `Rc<RefCell<ThunkRepr>>` and a separate `Rc<Upvalues>` containing vectors of captured values. Shared attribute-set updates can clone their backing maps. The pinned CppNix source instead forces a thunk through its expression and environment pointers, batches value allocation in `EvalMemory::allocValue`, and uses compact interned symbols and attribute bindings. This is an implementation and representation gap; these profiles do not establish an inherent Rust-versus-C++ cost or prove that a VM is necessarily slower than an AST evaluator.

The memory policies also differ. The default noxi command enables `impure`, not the evaluator's optional `no_leak` feature. Its context-free `NixString` allocations are deliberately not freed, allowing pointer-copy clones; `Rc` alone cannot collect reference cycles in the lazy value graph. Enabling `no_leak` is not an established fix: it changes string cloning/deallocation costs and requires its own matched measurement. A separate CppNix `NIX_SHOW_STATS` invocation, excluded from timing, reports 19,360,824 values, 9,713,630 environments and 3,026,115 attribute sets containing 23,899,850 attributes. Its value and attribute entries are each 16 bytes. Four GC cycles manage about 1,073.4 MiB of total GC allocations, leaving a 720.3 MiB GC heap after the final collection. This illustrates how per-object overhead and reclamation matter at this scale; there is no allocation-stack or retained-heap breakdown yet assigning noxi's extra memory to specific object classes.

Recent diagnostics add measurable work on successful evaluation too. `FrameTrace` cloning and dropping account for 3.20% of current samples, and source-span lookup for 2.59%; the frame loop clones trace metadata at each transition, generator suspension clones it again, and function calls retain parent frames for accurate traces. The old artifact has no `FrameTrace` clone/drop symbols. These are credible contributors to the observed regression, but this comparison includes many other semantic changes and compiler/inlining differences. It does not attribute the full 14.0% slowdown to diagnostics. Trace rendering remains necessary for compatibility; the experiment to prioritize is a cheaper representation and deferred materialization that preserves the same errors and call sites.

The next experiments should measure compact frame/continuation state and additional synchronous forcing paths, allocation count and retained bytes by object class, and trace bookkeeping separated from error materialization. Attribute-map sharing and compact capture storage are candidates after that attribution. Swapping an allocator or removing the measured libc allocation/free time alone cannot close this gap: eliminating that 18.99% symbol bucket entirely would yield only about 1.23 times the speed. New optimizations still require per-change comparison with both preserved noxi and pinned CppNix, exact diagnostics, and unchanged `just nixos-paths` and `just nixpkgs-paths` results.

Raw diagnostic artifacts are in `target/performance-diagnosis-2026-10-26/`: `matched-5/` contains samples, manifests, environments, correctness results and hardware; `main/` preserves the executable and source identity; `host-state.jsonl` records the quiet-window/pressure checks; `profiles/` contains the three perf captures, symbol reports, summarized groups and reference allocation statistics. `diagnostic-summary.json` records the measured medians. No evaluator implementation was changed for this diagnosis. A recovery copy of these artifacts is preserved at `/home/rajveer/.local/state/noxi-performance-diagnosis/2026-10-26-a7548847`.

## Evaluator cost reductions, 2026-10-27

The changes below address the mechanisms the diagnosis named. Each is its own commit, checked against its predecessor by user-space instruction counts (`perf stat -e instructions:u`), which stayed stable while wall time varied with unrelated host load. Every candidate also ran a differential against the merged-main artifact. That differential covers 362 language fixtures and 34 extra expressions (errors, catchable errors, `tryEval`, assertions, infinite recursion, `with`, selection and deep-force traces), each with and without `--show-trace`, and compares exit status, stdout and stderr. All outputs were identical for every kept change. The workspace tests, doctests, `just nixos-paths` and `just nixpkgs-paths` pass at the final revision.

| Change | EVAL-08 effect |
| --- | --- |
| Move a generator's trace into the running frame instead of cloning it per transition; skip the clone for trace-less bytecode frames | with the next change, 5.4% fewer instructions |
| Record each suspended thunk's span at compile time instead of a binary search per thunk | included above |
| Force a plain suspended bytecode thunk by entering its code directly under a `Force` frame, which stands in for the `force` generator one for one: same frame indices, spans, error chains, traces and blackhole | 17.9% fewer instructions |
| Allocate with mimalloc in `noxi` and the `noxi-commands` executables | 8.2% fewer instructions, 5% lower peak RSS |
| Box lambda formals and release chunk spare capacity; a lambda allocation drops from 320 to 240 bytes | peak live heap about 140 MiB lower |
| Store static upvalues as a boxed slice and share captured `with` stacks; `Rc<Upvalues>` drops from 64 to 48 bytes | peak live heap about 130 MiB lower |
| Fat LTO with one codegen unit for release builds | 4.7% fewer instructions |
| Test for a catchable error inline before tracing a result | 0.6% fewer instructions |
| Share the attribute-position map of sets built from one literal when their positions match, and stop deep-copying it with the set | about 110 MiB lower peak RSS, 1% fewer instructions |

Together the user-space instruction count fell from 231.4 to 158.2 billion. Rebuilding the merged-main baseline and the pre-mimalloc revision with the newer nightly gave the same instruction counts, so the toolchain update in the same period does not account for these differences.

A five-pair matched run with the repository runner then compared the final revision `1018a06d` with the preserved round-one release and pinned CppNix 2.35.2, using the same affinity, inputs, dummy store and oracle as the diagnosis. The host was not quiet; the CPU times below are less affected by that than wall times. All fifteen timed invocations matched the oracle.

| Engine | Median wall s | Median process-tree CPU s | Median peak target RSS MiB |
| --- | ---: | ---: | ---: |
| CppNix 2.35.2 | 3.318 | 5.117 | 994.1 |
| Preserved round-one noxi | 20.246 | 20.082 | 3,240.5 |
| Final revision, `1018a06d` | 15.170 | 15.088 | 2,937.3 |

The final revision is 1.35 times as fast as round one (95% interval 1.31-1.39), which reverses the merged tree's 14% regression, and uses 9% less peak RSS. It still takes 4.57 times CppNix's elapsed time and 2.95 times its peak RSS.

Three experiments were rejected. The `no_leak` feature raised peak RSS to 3.98 GB because it also disables string interning, and it ran more instructions, so leaked context-free strings are not the memory problem. Sharing captured `with` stacks alone gained under 1%; it was kept only as part of the compact upvalue representation. A synchronous fast path for `==` between evaluated scalars saved 1.2% of instructions, below the 2% bar for this round.

The remaining gap is architectural. Builtins, comparisons, equality and string coercion still run as boxed generators. A thunk cell takes 56 bytes, and about 9 million are live at peak, against CppNix's 16-byte values. `//` copies its maps, as CppNix also does. `Rc` cannot collect reference cycles without a collector. A live-size histogram at peak attributes about 480 MiB to thunk cells, 330 MiB to upvalues, 230 MiB to lambdas and 590 MiB to allocations of a kilobyte or more, which include source text and attribute tables. These need representation changes, not further local fixes.

Raw artifacts are in `target/performance-tip-2026-10-27/`: `bin/` holds the measured executables, `matched-5/` the runner samples and report, `summary.txt` the medians, and `differential/` the differential and instruction-count scripts. A recovery copy is at `/home/rajveer/.local/state/noxi-performance-tip/2026-10-27-1018a06d`.

## Evaluator CPU workstream, 2026-10-27

The `perf/eval-cpu` baseline reproduces EVAL-08 at 158.416 billion user-space instructions and 2,970,506 KiB median peak RSS across two invocations. Fixed-period `perf record -e cycles:u -c 10000000` collected 7,162 samples with none lost. The flat profile assigns 14.48% to bytecode dispatch, 5.47% to generator driving, 2.01% to operand decoding and 1.28% to dropping frame traces. Temporary builtin counters identify `map` and `mapAttrs` as the most frequent full builtin calls, at 421,627 and 398,652 respectively. Counter instrumentation is excluded from the measured binaries.

Thirty more builtin bodies now register synchronous entry points, including `map`, `mapAttrs` and `genList`, whose generated thunks retain the original call span. An explicit VM continuation forces their strict arguments from left to right before calling the body. Catchable arguments retain the generated wrapper's propagation and `#[catch]` behavior. The combined change uses 155.148 billion instructions, a 2.06% reduction, with 2,986,918 KiB median peak RSS. Both invocations retain the required `f4e14079455b2d17` output digest. The 518 evaluator and builtin-macro tests, doctests and the full baseline differential pass, including exit status, stdout and full trace stderr.

Evaluated scalar equality and comparison, numeric/string addition, path/string concatenation and scalar string coercion now run directly without generator frames. Equality and comparison also avoid allocating their work vectors for scalar arguments inside builtin calls. Suspended values, aggregate comparisons, imports and failures retain the original generator paths. This step uses 149.092 billion instructions, 3.90% below the synchronous-builtin revision and 5.89% below the original baseline, with 2,935,974 KiB median peak RSS. Both EVAL-08 digests, all 518 tests, doctests and the full baseline differential pass.

Bytecode frames now lend their trace to error construction instead of cloning it on entry. Attribute-name span lookup is deferred until a type error, and single-byte vu128 operands bypass the general decoder. This step uses 141.545 billion instructions, another 5.06% reduction and 10.65% below baseline, with 2,947,432 KiB median peak RSS. Both digests, the 518 tests, doctests and the full differential pass. A separate CppNix invocation uses 41.005 billion instructions and 1,002,056 KiB peak RSS, so the instruction gap is still 3.45 times at this stage.

Five-pair instruction profiles of START-03 and EVAL-01 through EVAL-06 pass all reference oracles for the baseline and first two revisions. At size 64, the synchronous revision's small-case instruction changes range from a 0.10% reduction to a 0.24% increase. The evaluated-operation revision reduces EVAL-04 and EVAL-06 by about 1.8%; its largest small-case increase is 0.17%. These startup-scale changes are recorded rather than rounded into a blanket no-regression claim.

The ready-argument expansion alone saved only 0.3%; adding the forcing continuation without the span-aware mapped builtins saved 1.5%. Neither intermediate qualifies as a standalone improvement. These shared-host runs gate instruction counts and RSS; their wall times do not establish a speedup. Artifacts, flat reports and preserved executables are under `target/perf-cpu/` in the CPU worktree.

### Pinned fixture recheck, 2026-10-28

The shared orchestration scripts now read immutable fixture copies under `target/perf-orchestration/fixtures`. Two more invocations of the committed bytecode-frame revision use 141.454 billion median instructions and 2,933,192 KiB median peak RSS. A fresh baseline invocation uses 158.459 billion instructions and 2,967,524 KiB RSS; CppNix uses 41.010 billion instructions and 1,012,516 KiB RSS. All four evaluations retain `f4e14079455b2d17`. The prescribed `tip-bin` reference and the preserved baseline executable have identical SHA-256 hashes, and their pinned-fixture differential outputs match byte-for-byte. An earlier invocation against a temporarily renamed qualification hostname is excluded from these comparisons.

An experiment consuming list and attribute operands instead of cloning their `Rc` owners uses 139.591 billion median instructions, only 1.32% below the committed revision, and 2,944,516 KiB median peak RSS. It fails the 2% instruction gate without a memory improvement and has been removed. Its patch and executable remain in `target/perf-cpu/` for inspection.

The final small-case matrix contains five samples for each of six executables on START-03 and EVAL-01 through EVAL-06 at size 64. All 252 preflight and sample oracles pass. Relative to baseline, the committed revision reduces median instructions by 3.17% on EVAL-04 and 2.50% on EVAL-06; its largest increase is 0.074% on EVAL-01. The ownership experiment changes these small cases by less than 0.09% relative to the committed revision. These instruction profiles include the original commands in `final-small/samples.jsonl`; their wrapper-inclusive wall times are not speedup evidence.
## Evaluator memory representation, 2026-10-27

A fresh three-run EVAL-08 baseline at `3012fb5` used a median 158.445 billion user-space instructions and 2,985,744 KiB peak RSS. Every run produced digest `f4e14079455b2d17`. Measurements use the orchestration script's isolated dummy-store environment and the shared heavy-job lock; wall time is not a decision metric on this shared host.

A separate temporary counting allocator around mimalloc measured 2,664,149,140 bytes of peak requested live allocations. It snapshots live size bins whenever the high-water mark crosses another 4 MiB. The last snapshot was 2,663,383,131 bytes: the 56-byte bin containing thunk cells held 9,140,997 allocations and 488.2 MiB; the 48-byte bin containing upvalue headers held 7,258,050 allocations and 332.2 MiB; the 248-byte bin containing lambdas held 1,007,698 allocations and 238.3 MiB. Reported bins of at least 1 KiB accounted for another 606.5 MiB. These are allocation-size bins, not allocation-stack attribution. Layout checks confirmed a 16-byte `Value`, 32-byte `ThunkRepr`, 32-byte `Upvalues`, and 232-byte `Lambda`, before reference-count headers and the thunk's borrow flag. The instrumented executable matched the output digest but is excluded from instruction and RSS comparisons. At command return, 2,629,335,153 requested bytes remained live, so retention also needs attention.

The compiler now constructs a function literal's closure directly in positions where it immediately forces the literal: a call's callee and a function's returned expression. Recursive closures retain the ordinary force operation and finalisation. Lazy positions keep the suspended wrapper, preserving unforced function previews. Three EVAL-08 runs reduced median instructions from 158.445 to 152.920 billion, a 3.5% saving. Median RSS was effectively unchanged at 2,983,400 KiB. All output/error/trace differential files matched the baseline byte-for-byte, all 519 evaluator tests and the doctest passed, and all 90 timed EVAL-01 through EVAL-06 samples matched their oracles. Smaller-workload instruction medians did not regress; EVAL-04 saved 6.7% at size 1,024. These are instruction and memory diagnostics, not a wall-time qualification.

A fresh three-run pinned CppNix comparison produced the same digest with median 41.011 billion instructions and 1,018,852 KiB peak RSS. After the compiler change noxi still uses 3.73 times the instructions and 2.93 times the RSS, so the memory target remains open.

One representation experiment was rejected. Moving suspension spans into shared bytecode and retaining that bytecode in blackholes reduced the thunk allocation request from 56 to 48 bytes. The full output/error/trace differential, 518 evaluator tests and the doctest passed, but median EVAL-08 instructions rose to 163.036 billion, 2.9% above baseline, while RSS barely changed at 2,973,444 KiB. The production thunk representation remains unchanged.

Unconditionally constructing function literals as closures was also rejected. It saved 5.2% of EVAL-08 instructions, from 158.445 to 150.253 billion, and reduced median RSS to 2,961,572 KiB, but changed unforced functions in error previews from `«thunk»` to a lambda with its name and position. Function construction can bypass a wrapper only where the compiler already forces that literal.

Raw counting-allocator source, its patch and executable, baseline measurements, rejected patches, test logs, and differential outputs are retained under `target/perf-memory/` in the `perf/eval-memory` worktree. The baseline differential is `target/diff-baseline/`.

## Evaluator capture ownership experiments, 2026-10-28

Suspended thunks now own their captures directly and move them into an owning VM frame on forcing. Function calls retain shared `Rc<Upvalues>` captures. This removes the separate capture-header allocation for suspended thunks without adding one when forcing them. In three EVAL-08 runs, median instructions fell from the compiler commit's 152.920 to 151.811 billion, and median RSS fell from 2,983,400 to 2,851,272 KiB: 0.7% fewer instructions and 129 MiB less RSS. Against the original `3012fb5` baseline, the branch saves 4.2% of instructions and 4.5% of RSS. All three runs produced digest `f4e14079455b2d17`; the full pinned-fixture differential was byte-identical, 520 evaluator tests and the doctest passed, and `just nixos-paths` and `just nixpkgs-paths` passed. All 90 timed EVAL-01 through EVAL-06 samples matched their oracles; smaller-case instruction changes versus the compiler commit span -1.94% to +0.06%. Artifacts are `target/perf-memory/owned-frame*`.

The measured branch tip still uses 3.70 times the pinned CppNix instruction count and 2.80 times its RSS. The 1 GiB memory target remains open; reclaiming unreachable value cycles is the next substantial opportunity. The earlier live-heap measurement found most requested bytes still allocated at command return, and reference counting alone cannot reclaim those cycles.

Storing captures inside suspended thunk cells and allocating `Rc<Upvalues>` only on forcing reduced median EVAL-08 RSS from 2,983,400 to 2,855,796 KiB, about 125 MiB. It raised instructions from 152.920 to 154.376 billion, however, and regressed EVAL-04 by 1.45% and EVAL-06 by 0.98%. This version was rejected despite passing the exact differential, 520 evaluator tests, the doctest and both path checks. Its patch, binary and measurements are under `target/perf-memory/inline-upvalues*`.

The baseline already avoids suspended wrappers for constants and known local references, and moves a forced thunk's captures into its executing frame. A regression test now checks that the thunk cell does not keep those captures alive after the frame releases them. Ordinary imports cache their compiled result, and imported ASTs are dropped after compilation; `scopedImport` deliberately compiles each scope separately. Evaluation results can contain values reused in later evaluations, so an arena freed when the VM returns would invalidate those values. An arena design needs owners that outlive the VM or a collector.

After the main checkout's rename, differential scripts read pinned fixture copies. The refreshed reference directory is `target/diff-pinned-baseline/`; earlier `target/diff-baseline/` results contain different absolute source paths. Both contain 2,376 exit/stdout/stderr files.

## Store substitution costs, 2026-10-27

The store workstream starts at `3012fb5`. Its release binaries, source identities, profiles and comparison runs are retained under `target/store-perf/` and `target/benchmarks/store-*` in the implementation worktree. These five-pair runs are exploratory measurements on a shared host. Instruction counts decide whether to retain a change; wall times describe these particular loopback fixtures.

The apparent fixed HTTP transfer cost was predominantly content-address verification. A `strace -f -tt -T` capture starts the first HTTP request about 19 ms after exec. The flat user-cycle profile assigns 58.1% to `Rewriter::drain`, 31.6% to its byte comparisons and 9.3% to SHA-256. The old rewriter searches for a store hash with a map lookup at every byte offset. Substring search for one pattern and Aho–Corasick for multiple equal-length patterns preserve leftmost nonoverlapping matches, absolute offsets and matches spanning writes. The test compares every chunk size against the original search rule.

On CACHE-01, the change reduces user instructions from 2,654,304,287 to 206,315,211, a 92.2% reduction. Peak RSS measured with `/run/current-system/sw/bin/time -f %M` changes from 14,340 to 14,252 KiB. The matched five-pair run reduces the preserved baseline's median from 615 ms to 226 ms; CppNix takes 158 ms. Correctness passes for CACHE-01, all six CACHE-08 formats, both CACHE-10 transports and NAR-05. The store/build tests and doctests pass, as do all 70 command tests, with two optional skips.

HTTP startup, certificate loading and an async runtime are not responsible for the dominant CPU cost in this fixture. A separate request cost remains: repeated narinfo responses incur roughly 41 ms waits between headers and body. Planning and realization issue three metadata requests for a single path, compared with one in CppNix. Decompression already streams into NAR restore; it does not buffer a complete downloaded NAR.

HTTP narinfo lookups now share positive and negative results between cache handles, with the configured TTLs; transport and parse errors remain retryable. Planning prefetches independent HTTP metadata within the substitution-job and HTTP-connection limits. File caches retain live metadata reads and skip prefetch. CACHE-02 narinfo requests fall from 96 to 32, and user instructions fall from 16,114,445 to 14,627,423, a 9.2% reduction. Its separate direct probes take 4.05 s before and 1.20 s after. Parallel workers raise peak RSS from 14,052 to 58,372 KiB; the matched CppNix run uses about 77 MiB. This is a concurrency cost, not a memory reduction over the previous serial implementation.

The HTTP connection limit follows CppNix's zero convention: `http-connections = 0` removes that limit, while `max-substitution-jobs` still bounds work and is clamped to at least one. The configuration test covers both direct settings and parsed configuration.

The `store-metadata` five-pair run passes all cache correctness checks. Candidate versus CppNix medians are 2,752 versus 4,435 ms for CACHE-03, 1,455 versus 2,747 ms for CACHE-06, and 1,170 versus 1,668 ms for CACHE-10/http. CACHE-10/file remains faster than CppNix, 15 versus 39 ms. CACHE-08 is near parity for none and XZ, and faster for the other four formats in this run. The host was busy, especially during CACHE-01 and CACHE-02; the retained instruction-count improvement is more reliable than their elapsed ratios.

An HTTP/1.0 correctness failure was reproduced independently: repeated fresh commands sometimes fetch narinfo but fail before requesting the NAR. The pinned ureq protocol code does not treat a length-delimited HTTP/1.0 response as implicitly closing the connection. After detecting such a response without explicit keep-alive, subsequent requests avoid the stale pooled connection and request explicit closure. HTTP/1.1 pooling remains enabled. Six fresh substitutions on each protocol pass, and the metadata test covers clone sharing, expiry, disabled caching and reading the NAR body on both protocols. All 70 command tests and 19 substitution/store-concurrency compatibility tests pass, with their optional skips.

NAR import now hashes the validated archive while restoring it, then reuses its SHA-256 and length for content addressing and registration. Other requested hash algorithms retain their separate digest pass. This removes two filesystem serialization/hash passes for SHA-256 archive imports without changing source capture or evaluator code. NAR-05 user instructions fall from 269,589,939 to 181,709,575, a 32.6% reduction; peak RSS changes from 39,152 to 38,696 KiB. The archive test checks SHA-256, SHA-512, symlinks and bytes following the archive. Store/build tests (23 passed, one optional skip), 20 compatibility tests, 70 command tests, doctests and both NixOS/Nixpkgs path checks pass.

An additional experiment processed incoming rewrite buffers directly instead of copying each chunk into the pending buffer. CACHE-01 instructions changed from 206,213,656 to 206,018,157, only 0.1%, with no clear memory benefit. It was rejected under the 2% instruction gate. The retained substring-search implementation still buffers chunks; its much larger search improvement is independent of this experiment.

### Immutable import captures, 2026-10-30

The local import path now carries the source capture's SHA-256 into registration instead of hashing the same snapshot again during restore. Capture owns its bytes privately and exposes no mutation after hashing. The NAR parser still validates the archive, and the digest is reused only when its consumed length matches the capture; trailing bytes use the restored tree's digest. Ordinary reader imports and remote imports retain their independent verification. The CLI's `LocalBackend` forwards the capture explicitly; the initial experiment omitted that forwarding and saved no instructions on the actual CLI, so it was corrected before retention.

The existing 8 MiB limit is a spill threshold. Small captures stay in memory; the 32 MiB NAR-05 fixture spills to a private temporary file. Source replacement after capture and rejection of a wrong expected hash retain their previous behavior.

The unlocked release probes reduce NAR-05 user instructions from 181,713,611 to 95,280,270, a 47.6% reduction on top of the restore-hash change. Peak RSS is essentially unchanged, 38,932 versus 38,720 KiB; the matched CppNix probe uses 128,180 KiB. In `target/benchmarks/store-capture-gate`, five paired samples give preserved-baseline, candidate and CppNix medians of 130.9, 82.3 and 108.0 ms, respectively, with a paired 1.31x ratio against CppNix. These shared-host timings are exploratory.

All 56 store/build/glue tests, 70 command tests, 20 store/substitution compatibility tests and affected doctests pass. One command test produced a harness leak warning during the full run and passed cleanly on its own. All 792 language differential evaluations match the preserved orchestrator tip. Six fresh 32 MiB substitutions on each of HTTP/1.0 and HTTP/1.1 pass at this release, each with one narinfo GET and one NAR GET. The HTTP/1.0 fix remains in the earlier cache-metadata commit; no TCP delayed-ACK changes are included here.

### Branch-tip comparison, 2026-10-30

The code at `70d32c3` passes all 36 selected benchmark variants in `target/benchmarks/store-branch-tip`. The preserved binaries and matching source identity are in `target/store-perf/tip-release`. Measurements run unlocked under the revised brief. These five-pair shared-host results are exploratory; the orchestrator's quiet-host run remains the wall-clock qualification. Times below are medians; the ratio is the paired geometric mean of CppNix time divided by candidate time, so it can differ from the ratio of the two medians.

| Case | Before ms | Tip ms | CppNix ms | CppNix / tip | 95% interval |
| --- | ---: | ---: | ---: | ---: | --- |
| CACHE-01 | 618.8 | 135.1 | 171.8 | 1.212x | 1.054 to 1.394 |
| CACHE-02 | 4082.4 | 220.4 | 1742.0 | 8.062x | 7.521 to 8.986 |
| CACHE-03 | 8455.9 | 4078.6 | 5655.6 | 1.280x | 0.734 to 1.905 |
| CACHE-06 | 4116.8 | 1478.9 | 2754.1 | 1.953x | 1.792 to 2.201 |
| CACHE-08/br | 761.0 | 140.4 | 164.8 | 1.090x | 0.958 to 1.243 |
| CACHE-08/bzip2 | 1919.4 | 2158.0 | 2333.4 | 1.085x | 0.868 to 1.326 |
| CACHE-08/gzip | 3138.7 | 2545.9 | 3107.3 | 1.036x | 0.365 to 2.355 |
| CACHE-08/none | 478.9 | 154.6 | 185.0 | 1.430x | 1.010 to 2.440 |
| CACHE-08/xz | 720.1 | 345.7 | 250.9 | 0.732x | 0.514 to 1.142 |
| CACHE-08/zstd | 860.0 | 258.9 | 264.9 | 2.107x | 0.990 to 4.611 |
| CACHE-10/file | 16.4 | 17.1 | 36.6 | 2.196x | 2.139 to 2.258 |
| CACHE-10/http | 4095.8 | 1208.2 | 1639.8 | 1.368x | 1.336 to 1.405 |
| NAR-05 | 151.6 | 84.9 | 86.9 | 1.181x | 1.051 to 1.310 |

The selected START, DB, GC, BUILD, RPC, PROFILE, FETCH, PLAN, NAR-01..04 and CACHE-10/file cases retain point estimates above CppNix in this run. NAR-03 user instructions remain effectively unchanged, 5,566,257 before versus 5,572,230 after. XZ remains below parity in the point estimate, with a wide interval spanning parity. Its flat profile assigns 63.0% of user cycles to the XZ reader and 31.9% to SHA-256; the decoder's byte-at-a-time CRC64 loop is the next CPU target. Streaming decompression already feeds restore directly. TCP delayed-ACK work belongs to the daemon/build workstream and is not included in this branch.

The final NixOS path gate passes in 106.4 s. The Nixpkgs path gate passes in 22.4 s after resolving the same pinned input through the Git-filtered worktree. The original `just nixpkgs-paths` input lookup traversed generated `target/debug` files and was stopped when the filesystem filled; the fallback runs the identical `matches_reference_nixpkgs_paths` test with the Git-resolved `NOXI_NIXPKGS`. No reference expectations were changed. After measurements, 53 byte-identical raw benchmark outputs were hard-linked, preserving every artifact path and byte while freeing 1.66 GiB.

## Consolidated evaluator comparison, 2026-11-01

Three matched samples compared the consolidated release binary with the `59b7dd58` evaluator baseline and pinned CppNix 2.35.2 on EVAL-08, `hello.drvPath`, a six-path Nixpkgs projection, and a package-name projection. Exit status, standard output, and standard error matched across engines for every completed invocation; the JSON outputs parsed successfully. The EVAL-08 output digest was `f4e14079455b2d17`.

Median EVAL-08 user-space instructions were 133,898,214,382 for the baseline, 133,938,062,287 for the consolidated binary, and 41,150,156,443 for CppNix. Median peak RSS was 2,884,216 KiB, 2,884,392 KiB, and 1,018,352 KiB respectively. On the other three workloads, the consolidated binary's median instruction counts were within 0.3% of the baseline. These measurements check the integration against its baseline; they do not establish that noxi is faster or uses less memory than CppNix.

The raw counters, outputs, binary hashes, and host-pressure records are retained in `target/consolidation-evidence/2026-11-01-measure` and `target/consolidation-evidence/2026-11-01-quiet.jsonl`. Earlier attempts were aborted when memory pressure exceeded the guard and are not included in these samples.

The separate combined evaluator experiment used a median 2,868,971,683 instructions for `hello` against 2,665,228,607 for the consolidated binary over three completed samples, an increase of 7.6%. Its wider comparison stopped when memory pressure exceeded the guard, so it remains unqualified and is not included in the consolidated binary. Its partial counters and pressure record are retained under `target/consolidation-evidence/2026-11-01-experiment` and `target/consolidation-evidence/2026-11-01-experiment-quiet.jsonl`.

## What should become faster

Measure time until an installed package actually runs, a development shell executes its first command, development services pass readiness checks, and a system generation completes activation. Download completion, derivation evaluation, and a CLI returning early are intermediate milestones.

The strongest architectural change is to share a validated operation plan and deduplicate work across evaluation, closure discovery, substitution, building, and environment creation. Then overlap independent work within explicit CPU, memory, network, disk, and build-user limits. A faster interpreter alone cannot remove network latency, filesystem metadata writes, compilation, shell hooks, or service restarts.

For an operation whose evaluation consumes 20% of elapsed time, even eliminating evaluation leaves at most a 1.25x overall improvement if the other work stays fixed. Phase attribution and the dependency graph must guide investment. Proposed targets below are engineering goals, not forecasts.

The first reference remains CppNix 2.35.2 at `2c73b59da29606068c0c98db015dd3a66955525d`, verified against `flake.lock`. Nixpkgs, NixOS, and Home Manager use the separately pinned [qualification inputs](../tests/qualification/flake.lock). A newer reference belongs in a separate report; do not silently move this baseline.

## Existing architecture and the comparison boundary

| Area | Current noxi evidence | Consequence for the plan |
| --- | --- | --- |
| Evaluator | [Evaluation](../crates/noxi/eval/src/lib.rs), compiler/VM, and [thunks](../crates/noxi/eval/src/value/thunk.rs) use lazy evaluation and `Rc<RefCell<...>>` values. The `no_leak` feature is explicitly a temporary memory option in [Cargo.toml](../crates/noxi/eval/Cargo.toml). | Benchmark the current VM before proposing JIT compilation or concurrent thunk forcing. Long-lived evaluator processes need bounded retention and user isolation first. |
| Source ingestion | [CapturedImport](../crates/noxi-glue/src/store.rs) now hashes into an immutable spool that spills beyond 8 MiB, then ingests those same bytes. | The bounded-capture portion of O06 is implemented. Snapshot reuse and removing repeated ingestion passes remain open. See the [initial measurements](performance-benchmarks.md#initial-local-results) for lower target RSS, extra file-cache work and unproven latency gains. |
| Realization planning | [LocalBackend](../crates/noxi-build/src/backend.rs) now uses reverse edges and dependency counters to compute topological order; realization remains sequential. [LocalBuilder](../crates/noxi-build/src/local.rs) also recursively realizes inputs. | The scan removal implements part of O09. A shared goal graph, parse reuse and bounded parallel execution remain open. Preserve reference failure propagation and settings. |
| Substitution | [BinaryCache](../crates/noxi-store/src/binary_cache.rs) already reuses an HTTP agent and streams NAR decompression. Its closure substitution visits references recursively and serially; [query_missing](../crates/noxi-build/src/missing.rs) independently queries cache metadata. | Share metadata and in-flight requests between planning and realization. Target serial dependency discovery, duplicated requests, and bounded overlap; do not claim existing NAR streaming as new. |
| Store metadata | [LocalStore](../crates/noxi-store/src/local.rs) uses a mutex around one SQLite connection. `register_valid_paths` already batches a transaction and uses prepared statements; [database opening](../crates/noxi-store/src/db.rs) already supports WAL. [Store::compute_closure](../crates/noxi-store/src/api.rs) queries one path at a time. | Measure query counts, lock waits, and transaction sizes before adding reader pools, bulk closure queries, or a metadata cache. WAL and transactions are existing behavior. |
| Daemon | [server](../crates/noxi-daemon/src/server.rs) forks per client and opens its store in the child. The [client](../crates/noxi-daemon/src/client.rs) already retains one connection. | Shared scheduling across clients would require a coordinator and isolated workers, not a mechanical conversion to async functions. Cross-engine protocol pairings remain required. |
| Measurements | [Criterion evaluation benches](../crates/noxi/eval/benches/eval.rs), the differential evaluator, reference command tests, and VM qualification already exist. | Reuse their fixtures and oracles. Criterion currently discards evaluation results, and the small/large merge expression lacks a semicolon after `c = 3`; check validity and forced results before treating these benches as evidence. This plan does not repair them. |

CppNix already has lazy evaluation, a persistent flake evaluation cache, shared HTTP transfer machinery, parallel substitutions, a goal scheduler, SQLite WAL, transaction batching, and NAR streaming. The [research appendix](performance-research.md) links the pinned implementation. Rust, async I/O, content addressing, and parallelism do not establish a performance advantage by themselves.

Compatibility commands must retain Nix expression semantics, store paths, NAR bytes, signatures, settings, locking, GC roots, sandbox isolation, daemon protocol behavior, profiles, shell hooks, errors, and activation contracts. Optimization cannot mean disabling checks or doing less work than CppNix. Native environments and experimental storage transports have separate qualification and explicit opt-in behavior.

## Proposed operation architecture

```mermaid
flowchart TD
	A[Compatible CLI or native client] --> B[Locked inputs and source snapshot]
	B --> C[Evaluation workers and validated caches]
	C --> D[Immutable operation plan]
	D --> E[Shared goal coordinator]
	E --> F[Metadata and transfer workers]
	E --> G[Sandboxed local or remote builders]
	F --> H[Hash verification and staged imports]
	G --> H
	H --> I[Store commit and GC roots]
	I --> J[Profile or shell environment]
	I --> K[Existing system activation]
	J --> L[Native unprivileged service supervisor]
```

This is a candidate design. Keep compatible synchronous adapters while introducing internal operation-scoped bulk APIs and worker queues. Do not make the entire `Store` trait async or add `Arc` to every evaluator value as a prerequisite.

- A plan records selected outputs, exact input identities, derivation graph, required store paths, metadata observations, operation settings, and effects that still need execution. Record provenance and a schema version; never persist credentials in plans.
- Plans are reusable only while their dependencies and policy remain valid. Recheck path validity, trust, permissions, roots, and mutable inputs at execution. A plan is not authorization and cannot promise that all substitutes remain available.
- The coordinator deduplicates compatible goals by output identity and relevant settings. Two requests with different repair/check modes, trust policy, credentials, sandbox requirements, or output selections cannot blindly share a result.
- Worker pools keep CPU hashing/decompression, blocking filesystem/SQLite work, network I/O, evaluation, and build processes under separate limits. A queue carries an estimated memory cost as well as a job count.
- One cancelled subscriber must not kill work still needed by another. The final subscriber releases or cancels work according to its state; temp roots stay live until publication or cleanup. Recover from coordinator loss without declaring staged paths valid.
- Store publication retains atomic visibility and reference validation. Limit batch size and commit delay so a large closure does not starve interactive queries or enlarge crash recovery indefinitely.
- Separate trusted store operations from evaluation and user runtime services. A long-lived evaluator handles immutable pure inputs in isolated workers, with quotas and recycling. A development service supervisor remains unprivileged.

## Optimization inventory

Each row states the full hypothesis. Benchmark identifiers refer to [catalog.tsv](../benchmarks/catalog.tsv); `partial` rows have subset adapters. `P1` means an early experiment after baseline tooling, `P2` means later work justified by profiles, and `P3` means exploratory or additive work. These are performance priorities, not a replacement for [MVP gates](../TODO.md).

| ID | Candidate shift and expected mechanism | Priority and owner | Benchmark families | Conditions that can reject it |
| --- | --- | --- | --- | --- |
| O01 | Load settings and command modules only when needed; keep `--version` and pure evaluation off the store/network path. | P1, commands/CLI | START, EVAL | Changes settings discovery, diagnostics, or misses real cold-start costs. |
| O02 | Cache parsed/compiled immutable inputs; optimize measured VM dispatch and hot builtins. Compare bytecode specialization or JIT only after VM profiles show a CPU bottleneck. | P2, eval | EVAL, MEM | Compilation costs exceed savings, cache versioning fails, or error spans/laziness change. |
| O03 | Intern attribute names and immutable strings; compare compact values, arena lifetimes, and representation changes against current `Rc` ownership. | P2, eval | EVAL, MEM | Extra retained memory, aliasing bugs, recursion behavior changes, or cold workloads regress. |
| O04 | Persist dependency-tracked results for immutable pure evaluation; invalidate affected entries rather than reevaluate every output. Cache successful immutable values and required derivation metadata first. | P2, eval/glue | EVAL, FETCH, FAIL | An input read is untracked, effects disappear, error replay differs, or cache validation costs more than evaluation. |
| O05 | Reuse bounded evaluator workers across requests; parallelize independent output evaluations with isolated heaps before considering shared-thunk concurrency. | P2, eval/daemon | EVAL, RPC, MEM | `Rc` values cannot cross workers, repeated imports multiply memory, state leaks across users, or workers grow without bound. |
| O06 | Capture source bytes once into a bounded memory buffer or immutable spool; reuse content-verified snapshots and filtered manifests. | P1, glue/store | FETCH, NAR, MEM, FAIL | Hashing and ingestion observe different bytes, directory filters or executable bits change, or warm reuse trusts only timestamps. |
| O07 | Resolve locked input graphs once; coalesce identical Git/archive fetches and reuse verified trees. Pipeline independent locked fetches. | P2, fetcher/input module | FETCH, ENV | Dirty-tree, `follows`, credential, submodule, registry, TTL, or lock-update semantics change. Fetchers must exist and pass compatibility first. |
| O08 | Pass an immutable typed plan through evaluate, query-missing, realize, profile, and shell stages; avoid recomputing derivations and closures. | P1, glue/build/commands | PLAN, ENV, SYS | A stale observation skips revalidation, import-from-derivation effects change, or plan serialization costs more than the duplicated work. |
| O09 | Index reverse graph edges and dependency counters; remove the waiting-map scan and repeated derivation parsing from scheduling. | P1, build | PLAN, DAG | Incremental/dynamic dependencies become incorrect, memory use exceeds the graph budget, or ready/failure ordering diverges from required semantics. |
| O10 | Execute ready build and substitution goals concurrently with bounded resource queues; deduplicate shared dependencies across clients. Experiment with critical-path prioritization only where ordering is permitted. | P1, build/daemon | DAG, BUILD, CACHE, FAIL | Ignores `max-jobs`, `cores`, keep-going, fairness, build-user limits, output locks, or reference diagnostics. `cores` is a builder hint, not a reliable host reservation. |
| O11 | Share HTTP agents and bounded narinfo caches with positive/negative TTLs; reuse connections and credentials safely, respect cache priority and server backoff. | P1, store/cache | CACHE, RPC, FAIL | Stale misses hide new outputs, trusted keys change without revalidation, or racing mirrors change required selection/errors. |
| O12 | Fetch independent closure metadata concurrently and share it between planning and substitution; deduplicate identical in-flight narinfo/NAR requests. | P1, store/build | CACHE, PLAN, DAG | Standard caches do not provide a bulk endpoint, speculative requests waste bandwidth, or closure cycles and missing references are mishandled. Use standard per-path requests by default. |
| O13 | Overlap NAR transfer, decompression, hashing, reference checks where required, and staged extraction with byte-budgeted buffers. | P1, store/NAR | NAR, CACHE, MEM, FAIL | A path becomes visible before integrity checks, pipeline stages duplicate full archives, or concurrency overloads disk and increases tail latency. |
| O14 | Tune codec workers/buffers and use supported optimized hashing implementations; preserve wire-declared algorithms and canonical NAR traversal. | P2, hash/store/build | NAR, BUILD | Gains require changing existing cache compression, hash algorithms, signatures, or NAR bytes. More codec workers may reduce install throughput under CPU limits. |
| O15 | Add measured bulk metadata queries, transaction chunking, and read connections around a controlled write path using the compatible SQLite schema. | P1, store | DB, RPC, GC | Existing WAL/batching already removes the cost, snapshot inconsistency appears, writer starvation grows, or mixed CppNix access breaks. |
| O16 | Cache immutable path metadata and parsed derivations within operations; use bounded shared caches only with cross-process invalidation or fresh validity checks. | P2, store/build | DB, PLAN, GC, FAIL | GC, repair, signature changes, or another engine makes entries stale. A daemon-local epoch cannot detect CppNix writers by itself. |
| O17 | Use existing protocol bulk operations where available, retain connections, and pool independent ordered connections for concurrent requests. Negotiate new bulk operations only as extensions. | P1, daemon/client | RPC, DB, NAR | Interleaves frames on a sequential legacy stream, reuses the wrong client options, or breaks any CppNix client/server pairing. |
| O18 | Add a long-lived goal coordinator and bounded isolated workers to replace repeated per-client planning. Compare the cost and isolation of processes versus threads. | P2, daemon | START, RPC, DAG, MEM, FAIL | A crash affects all clients, a user shares another's policy or credentials, or unsafe fork-after-threads enters the design. Keep sandbox process launch isolated. |
| O19 | Cache immutable sandbox input manifests and reduce repeated closure/mount planning; use kernel-supported copy/mount operations behind the same isolation contract. | P2, build/sandbox | BUILD, DB, FAIL | Reuses mutable namespaces or build directories, changes allowed paths/network, or setup savings vanish on real builds. |
| O20 | Reuse authenticated remote connections, send only missing verified inputs, and schedule against measured queue/transfer/build costs within reference constraints. | P2, build/remote | REMOTE, DAG | Remote support is incomplete, scheduler policy changes observable constraints, or predicting a faster builder increases transfer cost and failure risk. |
| O21 | Reuse sorted profile manifests and merge selected output trees once; retain collision/priority handling and atomic generations. | P2, buildenv/profiles | PROFILE, ENV | Skips user-environment builders or hooks, changes collisions/priorities, or invalidates rollback roots. Native fast paths need matching artifacts. |
| O22 | Cache validated shell environment capture and realized closures; entry still runs the required shell hooks and child setup. | P2, commands/environment planner | ENV, FETCH, FAIL | Caches a side effect, restores secrets, omits hooks, misses changed shell packages, or changes TTY/signal behavior. |
| O23 | For native `noxi env`, reuse a frozen plan and start independent tasks/services concurrently using explicit dependency and readiness rules. | P2, native environment supervisor | ENV, MEM, FAIL | Says ready before health checks, skips initialization/tasks, races service data, or misattributes benefits to the package manager when the comparator lacks equivalent services. |
| O24 | Reuse evaluation/realization plans for system and Home Manager operations; optimize their pre-activation preparation and preserve generated activation order. | P2, system operations | SYS, PLAN | Skips activation scripts or required restarts. Service migration and activation remain semantic lower bounds. |
| O25 | Batch closure/GC queries, use indexed reachability and bounded parallel deletion only under the existing root/lock protocol. | P2, store/GC | GC, DB, FAIL | Deletes a newly rooted path, mishandles hardlinks, or exposes inconsistent roots to another engine. |
| O26 | Compare filesystem reflinks, copy-range support, and deduplication indexes for import/copy/optimization, retaining canonical modes and metadata. | P2, store/filesystem | NAR, GC, BUILD | Assumes filesystem support, changes hardlink accounting, relies on unsafe source mutation, or copy-on-write later amplifies disk use. |
| O27 | Bound progress/log queues and batch rendering; offer tracing spans off the hot path while preserving compatible output. | P1, logging/commands/daemon | START, RPC, BUILD, SYS | Drops required logs, changes output ordering, or lets a slow client block unrelated builds. |
| O28 | Compare allocators, codegen units, LTO, PGO, and CPU dispatch using portable release artifacts and representative training inputs. | P2, packaging/eval | START, EVAL, NAR, MEM | Uses debug versus release comparisons, overfits PGO to measured workloads, increases binary/startup costs, or loses supported CPUs. |
| O29 | Explore an optional chunked content store with verified NAR import/export and ordinary path materialization; reduce repeated content transfer and storage. | P3, experimental store | EXT, NAR, GC | Requires rewriting the shared store, alters paths/NAR interoperability, or spends more on indexing and materialization than it saves. Nix already has content-addressed paths; chunking is the proposed difference. |
| O30 | Explore an opt-in lazy materialization backend for immutable read workloads. Measure first use, all-use completion, page faults, offline access, and missing-content recovery. | P3, experimental store | EXT, ENV, SYS | A quick shell start hides later fetches, boot needs unavailable network data, or it cannot support mmap, executable files, permissions, and ordinary store verification. |
| O31 | Explore negotiated chunk/delta transfer against an owned cache, retaining hashes, signatures, and standard-cache fallback. | P3, experimental cache | EXT, CACHE, REMOTE | No compatible endpoint exists, encode/index CPU outweighs bytes saved, or delta failure cannot fall back safely. Report separately from drop-in substitution. |
| O32 | Reuse identical per-platform plans/closures across fleet targets, distribute verified artifacts, and bound activation batches with health gates. | P3, fleet operations | EXT, REMOTE, SYS | Treats heterogeneous targets as identical, hides per-machine failure, or claims distributed activation is atomic. This follows single-machine OS qualification. |

## Cache correctness before cache speed

O04, O07, O08, O16, and O22 need different cache keys and invalidation rules. Do not build one global cache whose key is only a flake revision.

| Cached object | Required identity and validation | Never infer validity from |
| --- | --- | --- |
| Parsed or compiled source | Content digest, logical source identity for paths/spans, parser/compiler version, evaluator feature settings. | mtime alone, path name, or a watcher event stream. |
| Pure evaluated result | Evaluator/build semantics version, source graph, selected output, system, overlays/module arguments, settings, search path, observed reads including missing paths/directory listings and indirect imports, and required string context. | A lock hash without the root source, dirty working tree, or argument identity. |
| Impure evaluation | Default to operation-local reuse. Persistent reuse requires explicit tracking of every external dependency and effect; time, environment reads, network observations, traces, and import-from-derivation prevent a blanket pure-result policy. | Calling an expression pure because its imports are locked. |
| Narinfo/closure metadata | Cache URI and credential scope, store path, trust policy/key version, expiry, error class, and path validity. Bound negative entries and distinguish absence from transient failure. | A previous 404 forever or another user's authenticated request. |
| Realization plan | Evaluation identity plus requested outputs, build/repair/check settings, observations and expiry; recheck local validity, trust, available substitutes, and permission before executing. | A persisted ready flag or an old daemon instance ID. |
| Captured shell environment | All derivation and shell outputs, environment-capture inputs, shell/version, relevant settings, and closure validity. Filter secrets; required runtime hooks still execute. | A shell name, unchanged flake lock, or PATH alone. |
| Native service plan | Environment identity, task/service definitions, session/data identities, executable closure, declared readiness dependencies, and observed runtime state. | Another session's process IDs or yesterday's readiness result. |

Content-addressed cache entries can be immutable, but their reachability, permissions, trust, and referenced store objects still change. File watchers are invalidation hints. Content checks or a validated filesystem snapshot establish identity. Bound bytes retained, not only entry count, and expose hit/miss/invalidation counters.

## Measurement goals and stop rules

After collecting baselines, select targets per workload and machine. Initial candidate goals are at least a 20% reduction in a confirmed CPU/metadata bottleneck or a 2x reduction in a repeated pure-evaluation workload. They do not promise a 2x package install or system switch. A cold install limited by compressed bytes divided by network throughput will improve only if bytes fall or unnecessary round trips disappear.

Every accepted experiment must satisfy these conditions:

- Pass the affected differential and failure oracles before counting any timed sample as a success.
- Show the mechanism in counters, such as fewer narinfo requests, derivation reads, allocations, graph visits, SQL calls, or lock waits. Measure the optimized phase and at least one user workflow.
- Compare with both the pre-change noxi artifact and pinned CppNix, using identical work and resource settings. Report milliseconds saved, ratios, memory, CPU, bytes, and disk effects.
- Use the predeclared decision rule in the benchmark spec. Small/noisy gains, cold-path regressions, or displaced latency into first package use are reasons to reject or narrow the feature.
- Include short-lived and long-lived behavior. A persistent daemon that makes the second request faster but accumulates memory is incomplete.
- Retain an off switch for persistent caches, scheduling policies, and experimental backends until their recovery and interoperability evidence is complete.

## Work sequence

| Step | Deliverable | Exit evidence |
| --- | --- | --- |
| 0 | This architecture plan, sourced CppNix audit, scenario catalog, benchmark protocol, links from product/development/verification docs. | Catalog and cross-reference validation. No performance result claimed. |
| 1 | A differential timing runner with isolated homes/stores, versioned result records, capability discovery, deterministic fixtures, timeouts, process-tree accounting, and cleanup. Repair the existing evaluator benchmark validity/forcing checks. | Runner rejects fast failures and mismatched work; paired START/EVAL/DB/NAR/PLAN cases produce reproducible reports. |
| 2 | Offline synthetic fixtures and local signed cache server, controlled faults/latency, process profiling spans/counters, and cross-engine daemon pairings. | CACHE/DAG/BUILD/RPC/PROFILE baseline matrix with correctness digests and byte/request counts. |
| 3 | O06, O08, O09, O11, O12, O13, O15, O17, and O27 as individual measured experiments. Start with repeated metadata work and indexed graph planning, then concurrency. | Ablation reports versus pre-change noxi and CppNix; affected tests and crash/recovery cases pass. |
| 4 | O10 bounded shared goals and O18 coordinator experiment after single-operation scheduling and roots are reliable. | Contention, fairness, cancellation, output-lock, GC, client isolation, and mixed-engine evidence. |
| 5 | Real locked Nixpkgs/NixOS/Home Manager and standard shell workflows as capabilities land. Consider evaluator caching/representation and O19/O20/O21/O22/O24 only from phase profiles. | Repeatable package-use, shell-command, and activated-system milestones, with unsupported cases visible. |
| 6 | Native environment readiness benchmarks once that extension exists; service startup experiments and memory-soak evidence. | Equivalent tools/services/hooks, verified readiness and teardown. Native results remain separate from CppNix parity results. |
| 7 | Experimental chunk store/lazy materialization/delta transfer and fleet reuse, each with its own decision record and opt-in protocol/backend. | Full-use and offline/recovery results, compatibility export/import, demonstrated limits, and no migration of the default store. |

Performance measurement starts during the MVP. Removing unnecessary work can happen before the OS release when affected compatibility passes; performance targets do not replace the drop-in gate. The broader optimization program remains separately tracked. Do not defer instrumentation until after every feature is finished, and do not move missing fetchers, modern shell commands, or remote builders into performance work to avoid their compatibility requirements.

## First implementation tasks

- Build the paired runner and promote ten baseline scenarios: START-01, EVAL-01, EVAL-04, PLAN-01, CACHE-01, CACHE-04, NAR-01, DB-01, DAG-01, and RPC-01. A capability blocker stays explicit until implemented; select feasible offline cases first.
- Capture current graph visit counts, metadata requests, staged/import bytes, SQLite lock/commit time, and process-tree memory. Separate profiling runs from headline timings.
- Replace waiting-map scans with indexed graph edges, preserving serial execution initially. This isolates O09 from O10.
- Share operation-local narinfo and derivation metadata between query-missing and realization without changing TTL or cross-user policy. Compare O08/O11/O12 independently before persistent caching.
- Add bounded immutable spooling for large captured imports and compare RSS, temporary bytes, and elapsed time. Preserve source mutation and failed-hash behavior.
- Extend the scheduler only after these measurements show its critical path. Keep one-job behavior as a comparison case and qualify concurrent failure propagation explicitly.

The implementation backlog and benchmark acceptance rules live alongside this document. The first implementation includes the paired runner, generated offline fixtures, repaired evaluator benchmarks, and the bounded-capture/indexed-ordering portions of O06/O09. Source capture hashes as it writes and rewinds the immutable spool for ingestion; it never rereads the source after verification. Tests cover both flat and recursive captures above and below the spill threshold after source replacement. Indexed ordering matches the prior scan across generated DAGs, ready ties, empty graphs and cycles. System/environment activation, worker coordination, parallel realization, warm-cache matrices and real-workload qualification remain open.

Ancestor resource snapshots also retain local memory events and memory.stat counters. Reports include per-ancestor page fault, reclaim scan/steal and CPU throttled microsecond deltas alongside cap events and PSI. These observations help attribute stalls; they do not qualify an invalid lane or establish a performance improvement.

## Round-two continuation resource lane, 2026-11-13

The current checkout is authoritative. The earlier `target/eval-20261113` and historical `target/agent-perf/{base,c1,c3,c4,c5}` artifacts are absent in this continuation; their documented measurements remain historical and cannot identify the current executable. Fresh preservation is under `target/eval-round2-20261113/`, including a copy of every existing JJ-tracked file, a per-file hash manifest and the complete starting dirty patch. This copy precedes further evaluator changes and preserves unrelated work without changing JJ history.

The new `round2-cap-event-free-observe-psi` lane is declared before collecting samples in `policy.json`. Affinity is `0-3,12-15`. Full memory PSI is retained as an outcome rather than filtered, because allocation and reclaim can be intrinsic costs of evaluation. Any observed ancestor `memory.events` high, max, oom or oom_kill delta, missing required event accounting or CPU throttling invalidates the entire lane. Builds, tests, counters and profiling run outside production timing. Whole-run and per-process leaf-plus-ancestor snapshots retain faults, reclaim, PSI and throttle deltas. This lane is separate from the earlier strict PSI lane; no samples are pooled and slow pairs are never discarded. Absence of cap events does not prove absence of all external contention.

The source audit also found an import before `from __future__ import annotations` in the process collector, which prevented Python from importing the runner. The import now follows the future directive. Host policy validation rejects unknown pressure policy names and missing configured memory-event counters on cgroups exposing the memory controller.

The mechanism collector preserves the final command symlink in argv[0], so profiling the pinned multi-call `nix-instantiate` invokes the legacy evaluator rather than resolving it to the modern `nix` command. Production and mechanism builds remain separate. The initial collector validation passed 28 tests; later run records identify subsequent collector revisions.

## Validated round-two fusion ablation, 2026-11-13

The current evaluator is retained after a fresh portable release comparison against an otherwise identical fusion-disabled copy. Thirty shuffled matched blocks, seed 1729 and affinity `0-3,12-15`, passed the original independent four-path EVAL-08 oracle for all 93 invocations. The paired speedup is 1.033483x, 95% bootstrap interval [1.026026, 1.041798]. This qualifies the incremental fusion benefit on this host/workload. It does not re-establish historical cumulative improvements whose artifacts are absent.

| Engine | Median wall s | Median target user CPU s | Median target RSS MiB | User instructions, billion | User cycles, billion |
| --- | ---: | ---: | ---: | ---: | ---: |
| CppNix 2.35.2 | 3.643775 | 3.872513 | 995.195 | 41.003254 | 19.071981 |
| Unfused frozen noxi | 17.036013 | 13.926195 | 2950.498 | 147.927574 | 70.333870 |
| Current frozen noxi | 16.498309 | 13.438047 | 2942.424 | 141.781333 | 65.143332 |

Instruction and cycle columns are medians of three separate `perf stat` mechanism runs per production binary, not paired timing samples. Counters were fully scheduled. Fusion reduces instructions by 4.155% and cycles by 7.380%. Median production target RSS falls by only 0.274%. The candidate remains 4.528 times slower than CppNix by median elapsed time and uses 2.957 times its RSS. Overall faster-than-CppNix evaluation is not demonstrated.

The cap-event-free observe-PSI policy passed for combined, split and ordinary lanes, with no rejected high/max/oom/oom_kill events, CPU throttling or affinity violations. Combined whole-run full memory PSI was 3.3119%, so this run would fail a 1% strict PSI threshold. That earlier policy was not retroactively relaxed: this separate lane was declared before collection and preserves every slow pair. The target leaf records a median 566,324.5 page faults, 46.5 major faults, 1,143.5 direct-reclaim scanned pages and 1,017 direct-reclaim stolen pages for current noxi, compared with 565,139, 63.5, 1,184 and 1,092.5 for unfused noxi. These show fault/reclaim work without cap events; they do not exclude external host memory pressure or prove that all PSI is intrinsic allocation cost. Leaf and ancestor raw snapshots, finite high/max limits, CPU throttled microseconds and event deltas remain in the sample records.

### Separate output and ordinary-package checks

| Workload | Pairs | Unfused wall ms | Current wall ms | CppNix wall ms | Fusion speedup and 95% interval |
| --- | ---: | ---: | ---: | ---: | --- |
| EVAL-08/module | 5 | 6778.695 | 6775.547 | 1704.488 | 1.0138x [0.9965, 1.0338] |
| EVAL-08/standalone | 5 | 4849.793 | 4698.808 | 1149.086 | 1.0337x [1.0142, 1.0614] |
| EVAL-08/system | 5 | 12856.466 | 12402.682 | 2655.954 | 1.0233x [1.0092, 1.0345] |
| EVAL-08/vm | 5 | 13138.111 | 12789.095 | 2730.540 | 1.0321x [1.0198, 1.0444] |
| EVAL-PKG-HELLO/default | 30 | 296.040 | 291.400 | 122.709 | 1.0151x [1.0099, 1.0208] |
| EVAL-PKG-NAMES/default | 30 | 182.754 | 182.169 | 78.267 | 1.0033x [0.9999, 1.0065] |
| EVAL-PKG-PATHS/default | 30 | 766.880 | 753.453 | 261.979 | 1.0159x [1.0119, 1.0195] |

Each split output passed its independently recorded derivation path, with 72 total invocations. Five pairs per output are exploratory and are not pooled with the combined lane. The module interval includes no change. Ordinary hello, six derivation/output paths and three package names passed 279 invocations across 30 blocks per case. The exact root `flake.lock` Nixpkgs input is fetched directly outside timing, rather than traversing the workspace with its target artifacts.

### Mechanisms and allocation

| Process allocation measurement | Unfused | Current |
| --- | ---: | ---: |
| `allocation_count` | 200,576,865 | 183,324,426 |
| `allocation_requested_bytes` | 16,042,705,656 | 14,662,510,536 |
| `reallocation_count` | 13,188,934 | 13,188,934 |
| `peak_live_requested_bytes` | 2,738,705,386 | 2,738,705,386 |
| `current_live_requested_bytes` | 2,714,468,215 | 2,714,468,215 |

The allocation-only mimalloc builds report 17,252,439 fewer successful allocations (8.6014%) and 1,380,195,120 fewer requested allocation bytes. Both builds report zero allocation/reallocation failures. Peak and final requested live bytes are identical, showing a temporary traffic reduction rather than a retained-heap reduction. Bytes exclude allocator metadata, fragmentation and retained pages. Four individual outputs also passed allocation oracles; allocation and opcode builds never contribute production timing samples.

Both opcode builds execute 245,306,061 logical opcodes and 70,584,198 bytecode entries. The current build records 17,435,602 immediate-force fusions and 183,163 evaluated fallback wrappers, exactly explaining the 17,252,439 avoided allocation cells. Suspended constructor counts fall from 36,679,132 to 19,243,530. Current layouts are `Value=16`, `Upvalues=32`, `FrameUpvalues=32`, `ThunkRepr=56`, `FrameTrace=48`, `BytecodeFrame=112`, `Frame=128` and `Error=184` bytes. Historical 144/128-byte scheduler/bytecode frame figures above describe earlier source, not this snapshot. Cold error boxing is preserved but was not independently wall-time ablated in this continuation.

Twenty-one hardware-counter collections, ten allocation collections and two opcode collections passed their full/split output and report-schema checks. Every retained collector manifest and all six binary hashes verified. The complete split instruction/cycle and allocation reports are under `stat-{current,unfused,reference}-{system,vm,module,standalone}` and `allocation-{current,unfused}-{system,vm,module,standalone}`.

### Split mechanism detail

| Output | Current / unfused instructions, billion | Instruction reduction | Current / unfused cycles, billion | Allocation count reduction | Current RSS MiB | Current peak requested live MiB |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| system | 109.862 / 114.335 | 3.912% | 48.932 / 51.783 | 7.908% | 2236.223 | 1973.837 |
| vm | 114.856 / 119.407 | 3.812% | 51.402 / 53.836 | 8.083% | 2247.035 | 1982.068 |
| module | 64.419 / 66.417 | 3.009% | 28.013 / 28.615 | 5.725% | 1368.168 | 1187.092 |
| standalone | 45.882 / 47.320 | 3.040% | 19.652 / 20.511 | 6.293% | 1104.324 | 927.614 |

Split hardware counts are single diagnostic runs per engine/output; cycles can reflect host frequency and scheduling. RSS comes from the five-pair production split lane, while requested live bytes come from the separate allocator build. These measurements are not combined into an aggregate score.

### Compatibility, identity and provenance

The frozen current source passed 544 evaluator tests and one doctest; with diagnostics enabled it passed the same 544 tests, five counter tests and the doctest. The language harness passed 466 of 467 fixtures with one disabled and 466 exact live reference matches. Ten focused production cases additionally match CppNix and unfused noxi byte-for-byte in exit status, stdout and stderr: lazy contexts, nested traced errors, caught throws, recursion, recursive captures, shared list/attribute identity, function identity, returned functions and unused aborts. Collector validation passed 28 tests. No commit, push, publication or host activation was performed.

The copy of 3,593 JJ-tracked files and starting dirty patch is preserved under `target/eval-round2-20261113/`. The preservation manifest hash is `aae412f006171e28e63490dfc1b57abdd3955db770d0b1c448cb8a97c7c5e95d`. The evaluator files in the final checkout are byte-identical to that preserved evaluator source. Later sibling command/store fixes are outside the frozen shared configuration; these results do not qualify a newly rebuilt final integrated command binary. Source IDs below hash the complete frozen regular-file manifests. Per-file records, compiler details, Cargo features, commands and linked libraries are adjacent to each artifact. Builds use `cargo build --locked --release -p noxi-commands --bin nix-instantiate`, two jobs, portable release fat LTO and one codegen unit.

| Artifact | Source manifest SHA256 | Binary SHA256 |
| --- | --- | --- |
| `current-production` | `205ae6171d49baeb3473faa08a3fe3d959fccd7936bcf8e609beff446ece211a` | `0987795a6c61a0786167c8e55b67d1ab81353dc3542b5b4d7a09b35ee1fff32b` |
| `current-allocation` | `205ae6171d49baeb3473faa08a3fe3d959fccd7936bcf8e609beff446ece211a` | `ef2162e2a7f2e917be43864d0fbbfcfaba10cf1d5dc6fa3ea615651381362429` |
| `current-diagnostics` | `205ae6171d49baeb3473faa08a3fe3d959fccd7936bcf8e609beff446ece211a` | `b8451f3ac150011fc90cd12016a2366dc69a7dbdf04bdaf0fdd38abbc633e6a7` |
| `unfused-production` | `6088c4b60331a448be5aca4f76a63c3ec4d3e0cf0efab714e64dc0b67097b2d5` | `233540ef177ace2ff8e4f0c5170cf0579f7b24a2d8a3c0a4fbe2780e2ba6cde8` |
| `unfused-allocation` | `6088c4b60331a448be5aca4f76a63c3ec4d3e0cf0efab714e64dc0b67097b2d5` | `1a304cefc6e202a7bd7dfa6802e0b7d2134c2b72a0dcfccabd8225657ab35807` |
| `unfused-diagnostics` | `6088c4b60331a448be5aca4f76a63c3ec4d3e0cf0efab714e64dc0b67097b2d5` | `8ebdbe5577ad71ef89866e98653ed9189af7596eb345cdfb97bf702ca8108e90` |

CppNix is the pinned 2.35.2 artifact at declared source revision `2c73b59da29606068c0c98db015dd3a66955525d`; its measured binary SHA256 is `a1fee5522ad985bc016990d72d3769cf6b40579dd84166124fff00814a77eb79`, identical across lanes. Version alone is not used as binary identity. The qualification Nixpkgs and Home Manager source locks and original independent four-path oracle are unchanged.

Rejected attempts remain preserved. `INVALID-unfused-production-cache-reuse` caught an identical cached binary before timing; explicitly refreshing source-input mtimes forced the correct independent local compilation. `split-5` failed all reference preflights because legacy `nix-instantiate` has no `--apply`; the corrected split adapter uses lazy `builtins.elemAt` in `--expr`. `ordinary-30` was stopped before any sample because root path-flake source resolution traversed target artifacts; `ordinary-locked-30` uses the exact locked fetchTree input. None of these attempts is pooled with passing lanes. Earlier invalid strict-PSI evidence referenced above is absent from this checkout and has not been reconstructed.

No arena or managed-heap design is retained: cumulative allocation traffic does not establish object lifetime, and returned values can outlive a VM. Enlarging every capture with inline storage and deferring genericClosure diagnostic printing remain unmeasured designs, not implemented performance claims. The remaining gap includes about 3.46 times CppNix user instructions and a roughly threefold RSS footprint; the validated temporary-cell reduction alone does not close it.

## Attribute selection and allocation follow-up, 2026-11-18

Two evaluator changes were tested together. The VM moved owned attribute sets and lists off the stack and deferred diagnostic span lookup until a type conversion failed. This let list concatenation reuse uniquely owned buffers. Attribute sets took their backing reference directly on consumption and final destruction, removing the empty placeholder allocation while preserving iterative destruction.

The evaluator tests passed. Exact stdout, stderr, and independent output checks passed for three matched rounds per workload with release builds and pinned CppNix 2.35.2. Each cell shows median elapsed time and peak evaluator process RSS.

| Workload | CppNix | Frozen noxi | Patched noxi |
| --- | --- | --- | --- |
| EVAL-08, NixOS and Home Manager | 3.316 s / 993 MiB | 15.672 s / 2,952 MiB | 16.131 s / 2,952 MiB |
| EVAL-PKG-PATHS, six locked Nixpkgs paths | 0.263 s / 165 MiB | 0.776 s / 373 MiB | 0.764 s / 373 MiB |

The NixOS median rose 2.9% and peak RSS changed by less than 1 MiB. The package projection's paired timing interval includes parity with frozen noxi. These are exploratory shared-host runs without a predeclared host policy. They do not attribute the NixOS slowdown to either change individually. The evaluator changes were reverted. The patch and binary are under `target/perf-attrs-20261118`. The matched reports are under `target/benchmarks/20261117T230208.625077Z` and `target/benchmarks/20261117T230429.393055Z`.

### Forced loads and capture layout trial

The next trial combined two changes. The VM handled an immediately following force during a local or captured value load without a separate dispatch. Captures stored the shared dynamic `with` stack as one pointer, reusing a snapshot's existing vector allocation. The evaluator passed 545 tests and one doctest. Three matched rounds on each workload passed exact output and independent path checks.

| Workload | CppNix 2.35.2 | Frozen noxi | Trial noxi |
| --- | --- | --- | --- |
| EVAL-08, NixOS and Home Manager | 3.465 s / 994 MiB | 18.335 s / 2,931 MiB | 18.091 s / 2,850 MiB |
| EVAL-PKG-PATHS, six locked Nixpkgs paths | 0.292 s / 165 MiB | 1.058 s / 340 MiB | 1.083 s / 330 MiB |

Peak evaluator RSS fell 80 MiB on EVAL-08 and 10 MiB on the Nixpkgs projection. Both paired timing intervals include parity with frozen noxi. These shared-host results do not isolate the contribution of either change. The trial patch, binaries, flat baseline profile and raw reports are under `target/perf-continuation`.

### String interner growth

The interner now rehashes stored entries by string contents, matching insertion and lookup. Previously growth rehashed entries by their pointer addresses and could allocate a second backing block for an existing string. The regression test reproduces the duplicate allocation after growth and passes with the fix. All 546 evaluator tests and the doctest pass.

| Workload | CppNix 2.35.2 | Frozen noxi | Interner fix |
| --- | --- | --- | --- |
| EVAL-08, NixOS and Home Manager | 3.412 s / 993 MiB | 17.805 s / 2,931 MiB | 17.916 s / 2,928 MiB |
| EVAL-PKG-PATHS, six locked Nixpkgs paths | 0.293 s / 164 MiB | 1.134 s / 340 MiB | 1.142 s / 339 MiB |

These three exact-output rounds per workload isolate the interner fix from the earlier VM and capture changes. Peak RSS falls by 3.5 MiB on EVAL-08 and less than 1 MiB on the package projection. The small RSS change accounts for little of the gap to CppNix, and elapsed time did not improve in this run. Reports and build artifacts are under `target/perf-continuation`.

### Retention after VM teardown

The opt-in `eval-diagnostics` report samples approximately one thunk in 1,024 by construction kind with weak references, capped at 65,536 samples per thread. A weak reference keeps its allocation header available for observation but does not keep the thunk payload alive. A snapshot counts samples with a surviving strong reference. A surviving sample can still belong to a live returned value on workloads that return a lazy graph.

The oracle-checked EVAL-08 diagnostic snapshot was taken after the VM dropped its roots. Of 18,619 thunks sampled at suspended construction, 8,811 still had strong references (47.3%). None of the 171 sampled thunks constructed in the evaluated state survived. Sampling did not hit its cap. The counter records construction kind, so the surviving suspended constructions may have since been forced. The snapshot indicates substantial retention beyond VM-owned roots, but does not identify which edges retain those thunks.

A second oracle-checked run reproduced the same survivor count and distinguished their current states. Of the 8,811 survivors, 6,082 were still suspended and 2,729 had been evaluated. The bounded, non-forcing scan found a closed path back to the sampled thunk in 13 of 490 probes. Eleven witnesses passed through an attribute set, ten through a list, three through closure captures, and one through a dynamic `with` capture. A witness can involve multiple edge families. The scan reached its depth or visit limit in 393 probes, so these counts establish that the direct-edge collector misses some cycles, without estimating the prevalence of each family. The report is under `target/perf-continuation/retention-edges-eval08`.

The witness mask uses bit values 1 for static captures, 2 for dynamic captures, 4 for memoized payloads, 8 for attributes, 16 for lists, and 32 for closures. The scan reports one witnessed path per probe.

The first whole-graph census with string reclamation enabled passed the EVAL-08 oracle and classified 10,177,029 nodes and 28,723,218 edges with no count mismatches or graph truncation. It proved 255,114 reached thunks collectible, including 255,046 outside the bounded trial's reach. Only 330 of the 8,811 survivor roots were proved collectible. The remaining roots were either externally rooted or reachable from a node with unexplained owners. A forward scan cannot tell whether the missing owners are themselves part of an unreachable cycle. The largest proved collectible component contained 141,301 thunks. The census itself is diagnostic and excluded from timing and memory comparisons.

`eval-profile.py --mode diagnostics --ownership-census --ownership-census-dense` tests sampling coverage by increasing the creation sample rate from one in 1,024 to one in 64. The recorded retention cap and census truncation fields indicate whether the resulting graph or sample set reached a configured limit.

The denser run passed the EVAL-08 oracle without truncation or count mismatches. It reached 10,408,533 nodes and proved 400,234 thunks collectible, including 398,402 outside the bounded trial's reach. It proved 7,281 of 141,487 sampled survivors collectible, up from 330 of 8,811 at the lower sampling rate. More seeds increased the proven collectible set, but many retained thunks still have unexplained owners. The report is under `target/perf-continuation/dense-census-eval08`.

The next speed experiment inlines the hot capture read and keeps diagnostic construction for a missing stack slot out of that path. It uses the same bytecode, stack bounds check, and error metadata. This targets the out-of-line capture reader identified in the flat cycle profile. The production comparison keeps string reclamation disabled on both noxi builds to isolate the capture change.

Three exact-output EVAL-08 rounds reduced median elapsed time from 17.197 s to 16.866 s, with a paired speed ratio interval of 1.009–1.022. Peak RSS stayed at about 2,568 MiB. The locked package projection's timing and peak RSS intervals both include parity. Reports are under `target/perf-continuation/capture-decode-eval08` and `target/perf-continuation/capture-decode-packages`.

With capture inlining present in both noxi builds, turning on reclamation reduced EVAL-08 peak RSS from 2,568 MiB to 2,308 MiB in three exact-output rounds. Its paired elapsed-time interval includes parity. The package projection had no clear difference in either metric. The reclamation-enabled artifact is the control for the next collector and call-path trials. Raw results are under `target/perf-continuation/capture-plus-reclaim-eval08` and `target/perf-continuation/capture-plus-reclaim-packages`.

The ownership census also supports `--ownership-census-all` to retain a weak sample for each thunk allocation, with a hard limit of 22 million records. This diagnostic mode distinguishes missing thunk seeds from unmodeled ownership. Its own memory use is excluded from production measurements.

The all-thunk EVAL-08 census passed the four-path oracle with no truncation or count mismatches. It proved 3,770,812 of 9,102,001 surviving thunks originally constructed suspended collectible. Of those, 3,707,804 fell outside the bounded trial. The largest component alone contained 3,555,974 thunks. Compared with the dense census, recording every allocation exposed a substantially larger collectable component by bringing more owners into the graph. The post-VM census does not establish when those nodes became collectible, and its estimated working memory was 958 MiB. A production collector needs to account for its own memory and CPU costs. The report is under `target/perf-continuation/full-census-eval08`.

The same census on the locked package projection proved 874,024 of 941,573 surviving thunks originally constructed suspended collectible after VM teardown. One component contained 872,337 thunks. The run passed the six-path oracle with zero count mismatches or truncation. The report is under `target/perf-continuation/full-census-packages`.

`eval-profile.py --workload EVAL-PKG-PATHS` reuses the locked package adapter and its six-path oracle. The flat profiles passed that oracle with zero lost samples. On noxi, bytecode execution accounted for 10.42% of samples, lambda execution 4.43%, and capture population 3.38%. Copying attribute bindings and initializing an attribute lookup index accounted for 3.25% and 2.48% respectively. The profiles are under `target/perf-continuation/capture-decode-package-flat` and `target/perf-continuation/cppnix-package-flat`.

The small-attribute-set trial used binary search for sets with four through fifteen bindings, avoiding a separate hash index for those sets. Sets with fewer than four bindings still used linear search. The release comparison enabled string reclamation on both noxi builds to isolate this lookup change.

The trial was rejected. Three exact-output rounds against the capture-inlining control gave a paired speed ratio of 0.993 (0.984 to 1.001) on EVAL-08 and 0.972 (0.953 to 0.983) on the package projection. The production lookup is unchanged. Reports are under `target/perf-continuation/small-attrs-eval08` and `target/perf-continuation/small-attrs-packages`.

Setting `NOXI_EVAL_OWNERSHIP_CENSUS=1` alongside `NOXI_EVAL_DIAGNOSTICS` adds a read-only `ownership_census` object to the report. Starting from every surviving sample, it records each reachable thunk cell, attribute set, list, closure, shared capture set and `with` stack as a node, together with its strong count and the references found inside the explored graph. Unexplained references come from owners the census cannot traverse, such as the VM, evaluator caches, builtin partial applications, native thunks and lambdas. Those nodes and everything they reach are live. The rest are owned only by one another and are reported as collectible, by type, sharing, connecting edge kinds and component size, including how many collectible thunks lie in components beyond the bounded trial: more than 32 thunks, more than 256 internal references, or a shared container. Opaque owners and limits can only make a node look live, so the counts are lower bounds. The census defaults to 28,000,000 nodes and 112,000,000 edges, adjustable with `NOXI_EVAL_OWNERSHIP_CENSUS_MAX_NODES` and `NOXI_EVAL_OWNERSHIP_CENSUS_MAX_EDGES`. It reports its own memory as `estimated_peak_bytes` and its running time as `explore_ms` and `analyze_ms`. It does not force, mutate or release values, and its runtime and memory are excluded from production timings.

The same run counted 332,878,208 requested backing bytes for strings allocated without context and another 12,128,140 bytes for strings that lost their context. These paths deliberately retain backing allocations under the default string policy. The combined 329 MiB excludes allocator overhead and captures only the instrumented evaluator thread. The separate report is under `target/perf-continuation/retention-eval08`; its runtime is excluded from production timings.

### Inline captures and direct string construction

The evaluator stores a single static capture inline. The VM constructs that case without allocating a vector. Capture sets with a dynamic `with` stack keep sharing that stack. Interpolation, coercion, and concatenation construct their final string from bytes directly, eliminating the temporary context-free string they previously constructed first.

| Workload | CppNix 2.35.2 | Interner fix | Inline captures and direct strings |
| --- | --- | --- | --- |
| EVAL-08, NixOS and Home Manager | 3.590 s / 988 MiB | 17.200 s / 2,927 MiB | 17.443 s / 2,644 MiB |
| EVAL-PKG-PATHS, six locked Nixpkgs paths | 0.316 s / 164 MiB | 1.094 s / 331 MiB | 1.050 s / 305 MiB |

Three exact-output rounds per workload reduced peak evaluator RSS by 283 MiB on EVAL-08 and 26 MiB on the package projection. The paired EVAL-08 timing interval includes parity. These runs measure the two changes together. Raw comparisons are under `target/perf-continuation/inline-eval08` and `target/perf-continuation/inline-packages`.

### Bounded cycle-collection experiment

The opt-in `eval-cycle-collection` feature checks a bounded queue of weak candidates at VM frame boundaries. It traces direct thunk and uniquely owned closure capture edges, subtracts observed internal owners from the reference counts, and preserves every node reachable from an external owner. The trial leaves shared containers and native captures conservatively rooted by their uncounted owners. Focused tests cover self and mutual cycles, duplicate edges, a live external alias, an opaque native capture, a borrow conflict, and a recursive closure invoked in a second VM.

Three exact-output rounds against the same build without the collector reduced EVAL-08 peak RSS from 2,643 MiB to 2,625 MiB, with a timing interval spanning parity. Peak RSS on the six-package projection moved from 319 MiB to 320 MiB, also with a timing interval spanning parity. The collector remains opt-in. The direct-edge trial reaches too little of the retained graph to close the gap. Reports are under `target/perf-continuation/cycle-eval08` and `target/perf-continuation/cycle-packages`.

The next trial followed nested attribute sets, lists, and dynamic captures only when their backing allocation had one owner. A shared-container regression test verifies that the collector preserves the cycle while an external owner exists. Against the later string-context baseline, the expanded collector saved 13 MiB of peak RSS on EVAL-08 and 2 MiB on the package projection. EVAL-08 median time increased from 17.146 s to 18.195 s. All three rounds on each workload passed exact output checks. The collector remains experimental. Reports are under `target/perf-continuation/owned-edges-eval08` and `target/perf-continuation/owned-edges-packages`.

### Destroying string contexts

String teardown now drops its boxed dependency context before freeing the raw backing block. The production build has the experimental collector off. Compared with the previous production build, EVAL-08 peak RSS fell from 2,645 MiB to 2,568 MiB and the package projection fell from 323 MiB to 315 MiB. Both paired timing intervals include parity. The patched evaluator passed 546 tests and the doctest, and all three rounds on each workload passed the independent path and exact-output checks.

In these rounds CppNix 2.35.2 used 993 MiB and 3.445 s on EVAL-08, while noxi used 2,568 MiB and 17.907 s. On the package projection CppNix used 164 MiB and 0.292 s, while noxi used 315 MiB and 0.924 s. Reports are under `target/perf-continuation/context-drop-eval08` and `target/perf-continuation/context-drop-packages`.

### Uninterned string reclamation and VM dispatch trial

`NOXI_RECLAIM_STRINGS=1` opts into reference counting for uninterned string allocations. Short interned strings remain permanent and the existing `no_leak` feature keeps its own behavior. The recorded string layout distinguishes counted allocations, whose header has a reference count, from allocations using the original ownership policy. Benchmark runs with `--reclaim-strings` pass this setting only to the candidate, allowing the same binary to serve as the disabled control.

To compare evaluator changes with reclamation enabled in both noxi builds, add `--reclaim-strings-role baseline`. The option may also name a configured comparison role, and the runner rejects the CppNix reference role. The recorded process environment includes the setting for each enabled role.

The candidate binary also includes VM instruction fusions that run without a runtime observer and defers formatting scoped error hints until trace construction. Comparing reclamation on and off in that binary isolates the runtime setting. Comparing either mode with the previous production binary also includes the VM changes and the cost of adding the string ownership check to default builds.

| Workload | CppNix 2.35.2 | Previous noxi | Reclamation disabled | Reclamation enabled |
| --- | --- | --- | --- | --- |
| EVAL-08 | 3.555 s / 987 MiB | 16.571 s / 2,568 MiB | 17.751 s / 2,568 MiB | 17.017 s / 2,308 MiB |
| EVAL-PKG-PATHS | 0.314 s / 164 MiB | 1.031 s / 299 MiB | 1.078 s / 296 MiB | 1.022 s / 300 MiB |

All three rounds on each workload passed exact output and independent path checks. Reclamation reduced EVAL-08 peak RSS by 260 MiB against the same binary with reclamation disabled. The enabled build still took longer than previous noxi on EVAL-08. The package projection's peak RSS increased by about 5 MiB with reclamation enabled against the same binary with reclamation disabled. Reports are under `target/perf-continuation/reclaim-strings-eval08` and `target/perf-continuation/reclaim-strings-packages`. These exploratory results leave both workloads above CppNix on both metrics.

The fused evaluator passed 561 evaluator tests, five diagnostic tests, and one doctest with diagnostics and cycle collection compiled in. A separate EVAL-08 diagnostic build passed the four-path oracle and recorded identical logical opcode and opcode-pair maps to the pre-fusion build. This validates counting and the observed output. The temporary patch is preserved at `target/perf-continuation/fused-diag/vm-fusions.patch`, while the next comparison builds reclamation without it.

The benchmark summary reports `paired_peak_rss_ratio` and `peak_rss_ratio_ci95` when every successful elapsed-time pair also reports positive target process RSS. Ratios above one favor the candidate. An incomplete RSS metric leaves both RSS ratio fields absent, so the timing cohort cannot silently be replaced by a smaller memory cohort.

Removing the new fusions and enabling reclamation recovered parity with the previous noxi build on EVAL-08 while reducing its peak RSS from 2,561 MiB to 2,307 MiB. Median elapsed times were 16.222 s and 16.264 s respectively. The 95% paired time ratio interval was 0.991–1.018, and the peak RSS ratio interval was 1.110–1.115. The locked package projection showed no clear change in either metric from the previous noxi build. CppNix remained ahead on both metrics on both workloads. Each workload had three exact-output rounds. The new RSS statistic can be recomputed from the raw sample files in `target/perf-continuation/reclaim-only-eval08` and `target/perf-continuation/reclaim-only-packages` without modifying the original reports.

Separate flat user-cycle profiles of the reclamation-only binary and CppNix 2.35.2 passed the same EVAL-08 oracle with zero dropped samples. The noxi profile assigned 18.08% of samples to bytecode execution, 9.54% to lambda execution, and 6.43% to capture population and its closure. The profiles are under `target/perf-continuation/reclaim-only-latest-flat` and `target/perf-continuation/cppnix-latest-flat`. The collector accepts `--reclaim-strings` to set the mode explicitly in the profiled process and record it in metadata.

Evidence and reproduction commands are in `target/eval-round2-20261113/{build-artifact.py,remaining.py,integrity.json}`, the three timing lane directories, preserved source directories, `semantic-differential/report.json`, and test logs. Independent review is requested through the parent as a fresh follow-up; these measurements are not a review approval.

Independent qualification review follow-up: the host validator now requires configured memory-event counters and CPU throttling accounting for every expected target accounting path. Missing dictionaries, paths and null counters invalidate the lane. All 29 collector tests pass, including the focused missing-target-data regression. Revalidation of the unchanged saved observations passes for combined EVAL-08 (93 invocations), ordinary workloads (279), and actual split-expression measurements (72). Original reports and manifests remain unchanged; `qualification-review-fix.json` and each lane's `host-validity-review-fix.json` record the corrected-validator verdict. The failed `split-5` preflight remains excluded even though its host observations pass. No benchmark was rerun and no samples were added or removed.

## Evaluator scheduling and trace materialization, 2026-11-18

This round starts from `075a5e7`, with frozen base binaries under `target/perf-lanes/base`. Each step was measured with three `eval-profile.py --mode stat` runs per workload under the shared heavy-job lock. Every run passed its oracle. The table gives median user-space instructions and peak RSS. The steps are cumulative.

| Step | EVAL-08 instructions, billion | EVAL-08 peak RSS MiB | EVAL-PKG-PATHS instructions, billion | EVAL-PKG-PATHS peak RSS MiB |
| --- | ---: | ---: | ---: | ---: |
| Base `075a5e7` | 150.450 | 2,567 | 8.198 | 306 |
| Direct frame transitions | 149.547 (-0.60%) | 2,565 | 8.176 (-0.27%) | 309 |
| Child frames entered without a push | 145.562 (-3.25%) | 2,567 | 8.070 (-1.56%) | 312 |
| Deferred `genericClosure` printing | 135.340 (-10.04%) | 2,355 | 7.999 (-2.43%), two runs | 298 |
| Immediate force finished in place | 133.913 (-10.99%) | 2,355 | 7.965 (-2.84%) | 300 |

Direct frame transitions keep calls, forces and returns inside the bytecode loop when the next frame is bytecode, or a `Frame::Force` to finish on the way. Each transition does what one outer-loop iteration does: cycle-collection tick, pop, `reasonable_span`, `current` and `call_depth`. The transitions alone saved only 0.6% of instructions. The outer loop's high cycle share did not correspond to instruction count. The next step builds a closure call's frame, a forced suspended thunk's frame and an immediately forced expression's frame, then enters it without pushing and popping the 120-byte `Frame`. The span of a fused immediate force is looked up from the suspended parent frame only when the result is itself a thunk. A trace-less running frame no longer drops its previous trace through out-of-line drop glue. Runs with an observer keep every outer-loop transition. An EVAL-08 diagnostics build counted 10,889,359 `bytecode_enter` and 59,931,412 `bytecode_enter_direct`, which total the 70,820,771 bytecode entries counted earlier. Its 27,241,618 `force_finish_direct` equal the earlier `force_finish`. The opcode and thunk counters were unchanged.

An instruction-sampled profile then showed `genericClosure` printing elements for trace frames on every iteration and binary-search step, whether or not an error occurred. Those prints accounted for about 7% of EVAL-08 instructions. Under the default string policy they also retained about 210 MiB of strings. CppNix adds these frames as an error passes, so its trace shows an element in its state at the time of the error. The builtin now stores the comparison frames as values in its trace, `BuiltinContexts::Printed`, and the VM prints them when a trace is materialized. The element and operator frames are printed only in their error branches. The comparison frames are cleared after each binary search, including when the key was already present. Previously they persisted into the next element's errors. Earlier noxi printed `«thunk»` where CppNix 2.35.2 prints the value the comparison or operator call had forced. The new `generic_closure_prints_elements_when_an_error_passes` test in `noxi-commands/tests/language_boundaries.rs` compares those cases byte for byte with CppNix. One divergence remains from before: when forcing an element throws, CppNix adds `in genericClosure element «thunk»` and noxi does not.

When the forced value is neither a thunk nor catchable, finishing a fused immediate force would only pop and push back the value. That frame is now dropped in place.

Paired exact-output timing compared the final build with the base binary and CppNix 2.35.2. EVAL-08 used three pairs and EVAL-PKG-PATHS used ten. Ratios above one favor the candidate. Both runs are exploratory.

| Workload | Base | Candidate | CppNix | Speedup vs base, 95% interval | Peak RSS ratio vs base, 95% interval |
| --- | --- | --- | --- | --- | --- |
| EVAL-08 | 17.466 s / 2,567 MiB | 16.052 s / 2,356 MiB | 3.404 s / 991 MiB | 1.077 (1.060 to 1.090) | 1.090 (1.089 to 1.090) |
| EVAL-PKG-PATHS | 0.995 s / 304 MiB | 0.962 s / 300 MiB | 0.316 s / 165 MiB | 1.026 (1.009 to 1.043) | 1.010 (1.007 to 1.011) |

The evaluator passed 554 tests and the doctest, and the upstream language suite passed 466 of 466 fixtures with exact live reference matches. The language boundary tests also passed. Builds, patches, stat runs, profiles, diagnostics counters and paired reports are under `target/perf-vm-1` in the lane worktree. Each `bin-*` directory holds the patch it was built from.
## Real flake system evaluation, 2026-11-18

This lane measured the user's NixOS flake (`nix eval --raw --no-write-lock-file --option eval-cache false path:~/.config/nixos#nixosConfigurations.nixos.config.system.build.toplevel.drvPath`) with frozen noxi 075a5e7 and host CppNix 2.35.6. Every run held the heavy lock. The host also ran other lanes, so single-run numbers carry noise. Both evaluators return `/nix/store/h9j8xmn9dbsxfzx0csfxmak30n6dabpj-nixos-system-nixos-26.11.20261113.d5cc683.drv`.

### Phase split

The evaluation boundary is the first 100 ms sample with more than 200 MiB RSS. Cold runs evicted the input trees' file pages with `posix_fadvise(DONTNEED)` before each run. Warm runs read them first. Each noxi cell is the median of three runs. The CppNix column is one run.

| Phase | noxi warm | noxi cold | Candidate warm | Candidate cold | CppNix |
| --- | --- | --- | --- | --- | --- |
| Lock, fetch and input hashing | 2.62 s | 11.68 s | 1.41 s | 2.72 s | about 1 s |
| Evaluation and derivation writing | 40.0 s | 42.0 s | 40.5 s | 43.1 s | about 11.6 s |
| Total elapsed | 42.59 s | 53.67 s | 41.88 s | 45.81 s | 12.62 s (one run) |
| Process CPU (task-clock) | 36.22 s | 41.00 s | 35.88 s | 38.95 s | 13.18 s |
| System CPU | 6.92 s | 8.99 s | 6.56 s | 8.73 s | 1.2 s |
| Minor page faults | 1.37 M | 1.40 M | 1.37 M | 1.39 M | 0.44 M |
| Peak RSS | 5,329 MiB | 5,337 MiB | 5,332 MiB | 5,329 MiB | 1,694 MiB |
| `write` bytes (`wchar`) | 427 MiB | 427 MiB | 80 MiB | 80 MiB | 106 MiB |
| `write` system calls (`syscw`) | 1,066,247 | 1,066,247 | 47,544 | 47,544 | about 132,000 |
| Bytes sent to the daemon | 328 MiB | 328 MiB | not traced | not traced | 105 MiB |

The `write` rows are not comparable across the two implementations. CppNix sends its daemon requests with `write`, so its 106 MiB is mostly daemon traffic. noxi sends them with `sendto`, which these counters omit. For noxi the `write` rows are the spill file, and its daemon traffic is the separate row, taken from one traced run of each implementation. The trial does not touch the daemon path.

Parse time could not be separated from evaluation without the diagnostics build. Before evaluation starts, noxi's RSS stays under 112 MiB. All of the peak therefore belongs to evaluation.

### Causes

Locked inputs were hashed again on every evaluation. `get_accessor_unchecked` already resolves a final input with a NAR hash to its valid store path, as CppNix does. `mount_input` then dumped that whole store path as a NAR into a `SourceCapture` to recompute the hash. Above 8 MiB the capture spills to an unbuffered `O_TMPFILE` in `/tmp`, on the root filesystem. For nixpkgs alone that meant reading 94,708 files. A traced run made 1,064,694 `write` calls to the spill file. CppNix reads about 64 MiB of nixpkgs, only the files evaluation needs. Cold, the dump of the nixpkgs tree takes about 11 s. Warm, it takes 0.5–1.6 s.

Derivation registration sends a full `AddToStore` request to the daemon for every derivation. A traced noxi run sent 59,241 of them, 328 MiB, each a synchronous round trip. CppNix sent 58,736 `IsValidPath` and 58,722 `AddTempRoot` requests, 4,360 `AddToStore` requests, and 105 MiB in total. The noxi daemon worker used 3.3–4.9 s of CPU against 1.3 s for CppNix.

Most system time comes from first-touch page faults. Minor faults match peak RSS divided by 4 KiB. `mmap` and `madvise` calls number only in the hundreds. Running the base binary with `MIMALLOC_ALLOW_LARGE_OS_PAGES=1` left faults unchanged (1,411,937), and `AnonHugePages` stayed at 0 kB. Transparent huge pages are in `madvise` mode on this host.

CppNix statistics put this workload at 1.46 times the thunks of EVAL-08 and 1.90 times its collected bytes. It has 5.1 times the `derivationStrict` calls (57,551 against 11,372) and 2.7 times the `pkgs/top-level/stage.nix` instantiations (92 against 34). noxi's peak RSS grows by roughly the same factor as CppNix's from EVAL-08 to this flake. Most of the extra RSS therefore scales with the amount of evaluation. One store-side suspect is that `KnownPaths` keeps every unique derivation for the whole evaluation. That would grow with the derivation count, but it is an unmeasured estimate of 150–250 MiB. Only one nixpkgs revision is evaluated.

### Locked input trial

The trial skips the dump when an input is final, carries a NAR hash, is unfiltered, and its content-addressed store path is valid and is the mount's physical root. It keeps the per-evaluation hash cache and the fetcher cache entries. A read-only evaluation still mounts the tree. On the real flake the trial cut pre-evaluation time by 8.96 s cold and 1.21 s warm. Median elapsed time fell by 7.86 s cold (paired deltas −12.78, −3.04 and −5.74 s). Warm, it fell by 0.71 s, and those paired deltas (−1.69, −0.99 and +0.76 s) include zero. Bytes written fell by 347 MiB and write calls by 1.02 million. Peak RSS did not change.

| Workload | Frozen noxi | Trial noxi |
| --- | --- | --- |
| EVAL-08, instructions / peak RSS / elapsed | 150.463 G / 2,566.7 MiB / 18.58 s | 150.610 G / 2,566.7 MiB / 18.29 s |
| EVAL-PKG-PATHS, instructions / peak RSS / elapsed | 8.201 G / 296.8 MiB / 1.25 s | 8.197 G / 296.9 MiB / 1.19 s |

All six gate runs per workload passed the oracle with identical stdout. Instruction and RSS changes stay within 0.1%. Neither workload fetches flake inputs, so a neutral result is expected. Three paired exact-output rounds against frozen noxi agree. EVAL-08 had a speedup of 1.000 (95% interval 0.991–1.015) and a peak RSS ratio of 0.9999 (0.9991–1.0009). EVAL-PKG-PATHS had 0.993 (0.953–1.015) and 0.9997 (0.9985–1.0006). Those reports are under `target/perf-real-1/confirm`. A stack-traced candidate run spilled to a temporary file only three times. Each spill came from `capture_import` through `import::add_path`, a path import. No input tree is dumped any more. The evaluator tests pass: 554 plus one doctest. Three `noxi-glue` integration tests fail identically with and without the change. They are `explicit_cli_overrides_are_sticky_and_match_reference_lock_bytes`, `github_gitlab_and_sourcehut_apis_lock_offline_with_reference_attributes` and `lfs_batch_and_offline_cache_match_reference`. The change is kept uncommitted. The patch, binaries and gate reports are under `target/perf-real-1`.

### Remaining fixes, ranked

1. Check derivation validity before writing. In `register_derivation` and `register_build_derivation`, send `IsValidPath` and `AddTempRoot`, and skip `AddToStore` when the path is valid, as CppNix does. That removes about 55,000 `AddToStore` requests and about 220 MiB of socket traffic. The elapsed gain is estimated at 1–3 s.
2. Lower evaluator RSS. This is evaluator work for other lanes. It accounts for about 3.6 GiB above CppNix, and each GiB costs about 262,000 page faults. Eval-phase system time is 6.5–7 s against about 1 s for CppNix.
3. Back the allocator with transparent huge pages. This is allocator work for other lanes. The environment knob had no effect, so it needs a build-level arena or `madvise` change. The saving is estimated at up to about 5 s of system time.
4. Retain less derivation data. `KnownPaths` could keep each derivation's hash modulo and outputs and reread the store on demand. The saving is estimated at 150–250 MiB.
5. Buffer spilled captures. Path imports through `capture_import` still spill unbuffered, three times per evaluation with the trial applied. They should be wrapped in a buffer. The trial's whole `write` volume of 80 MiB and 47,544 calls bounds their cost. The saving is estimated at under 1 s warm.
## Attribute-set construction and layered updates, 2026-11-18

Opt-in attribute counters (`target/perf-attrs-1/diag-counters.patch`, not kept in the source) counted attribute-set operations on the frozen base. The package projection performed 26,140 non-trivial `//` updates that copied 3.88 million bindings. Updates whose left operand had more than eight times as many bindings as the right accounted for 57% of the copies. 19,207 updates shared no names between operands. 52 sets with 16,384 or more bindings received 1.27 million of the 1.45 million hash-index entries built. EVAL-08 performed 551,983 updates that copied 23.5 million bindings, 51% of them in the same lopsided shape, and built 8.75 million index entries. Neither operand of `//` was ever uniquely owned. A flat instruction profile of the package projection showed `merge_bindings` at 1.51%, its per-binding position callback at 1.45%, binding-slice copying at 1.49%, index construction at 1.30%, and sorting of `(name, value)` vectors at 1.56%. Linker code folding attributes the sort to literal construction, but the counters showed that path sorted only 112,000 bindings. The sorting comes from `builtins.mapAttrs` and `builtins.listToAttrs`, which collected through a hash map and sorted the result.

The kept change has four parts:

- `mapAttrs` maps the input's sorted bindings in place by reference. It no longer copies a shared input, hashes, or sorts, and the result shares the input's lookup index.
- `listToAttrs` collects bindings in list order and stable-sorts them, keeping the first binding of each name. Lists derived from existing sets are already sorted, so this sort finishes in linear time.
- `removeAttrs` builds the filtered bindings directly, without re-sorting.
- Updates copy positions in runs instead of calling back per binding.

The layered part handles updates whose right operand is a non-empty set with less than a quarter as many bindings as the left. Such an update returns a layered set that references both operands and copies nothing. Lookups check the small top set first, then the base, so the base keeps the hash index it already built. Iteration merges the two sorted binding slices with the same right-biased precedence as the flat merge. A name in the top set reports its position from the top set, even when that position is absent. A further small update merges into the top set while it remains under a quarter of the base. Otherwise, and whenever a layered set is the right operand, the update flattens. The distinct-name count is computed on first use. A test compares iteration order, lengths, keys, lookups and owned iteration against flat merges across chained updates.

Three stat runs per build, under the shared lock, all passed the exact-output oracles:

| Workload | Base instructions:u | Candidate instructions:u | Base peak RSS | Candidate peak RSS |
| --- | ---: | ---: | ---: | ---: |
| EVAL-PKG-PATHS | 8,196.4M | 7,575.5M (-7.6%) | 297.4 MiB | 248.5 MiB |
| EVAL-08 | 150,438.8M | 147,579.7M (-1.9%) | 2,582.5 MiB | 2,337.6 MiB |

Values are medians. Single-run screening attributed about 4.4% (package projection) and 1.55% (EVAL-08) of the instruction reduction to the builtins alone. Run-based position copying added about 1.6% and 0.3%. The first layered trial used an eight-times threshold and counted shared names when the set was constructed. It saved 0.25% of package-projection instructions and cost 0.29% more on EVAL-08. Counting lazily, the same trial saved 1.2% on the package projection and was level on EVAL-08. Compared with the build without layering, the two versions reduced single-run peak RSS by 14% and 19% on the package projection and by 9% and 10% on EVAL-08. A four-times threshold was 0.4% and 0.1% cheaper again than eight times.

The cycle-collection experiment walks a uniquely owned set's values; it now walks a layered set's base and top as separately owned sets. Evaluator tests pass with no features and with `eval-diagnostics`, `eval-cycle-collection`, `no_leak` and all features. A differential run compared exit status, stdout and stderr against the frozen base for 21 expressions, with and without `--show-trace`, and found them byte-identical. The expressions cover missing attributes on layered sets, `listToAttrs`, `removeAttrs` and `mapAttrs` errors, and iteration, comparison, JSON and builtins over layered sets. A file-based variant compared `unsafeGetAttrPos` for base, overridden, unpositioned and chained layer names and was also identical. Both noxi builds still differ from CppNix in that check, before and after this change. CppNix reports positions for `listToAttrs` names and keeps positions through `removeAttrs`.

Three paired exact-output rounds against the frozen base passed on both workloads. EVAL-08 median elapsed time fell from 17.890 s to 17.795 s, a paired speed ratio of 1.017 (1.005 to 1.023). The package projection fell from 0.971 s to 0.870 s, a ratio of 1.142 (1.087 to 1.250). Median peak RSS of the timed samples was 2,321 MiB against 2,566 MiB on EVAL-08, and 249 MiB against 296 MiB on the package projection. CppNix 2.35.2 took 3.600 s and 0.263 s. These are exploratory shared-host runs. Reports are under `target/perf-attrs-1/paired-eval08` and `target/perf-attrs-1/paired-pkg` in the lane worktree, and each trial's patch is beside them.
## Evaluator heap footprint, 2026-11-18

String reclamation is now the default. Setting `NOXI_RECLAIM_STRINGS=0` restores the original policy, in which uninterned context-free strings are shared by pointer and never freed. On the frozen `075a5e7` binary, three `eval-profile.py --mode stat` runs per setting gave the following medians.

| Workload | Reclamation off | Reclamation on |
| --- | --- | --- |
| EVAL-08 | 150.460 billion instructions / 2,568 MiB | 150.263 billion / 2,307 MiB |
| EVAL-PKG-PATHS | 8.198 billion / 307 MiB | 8.200 billion / 304 MiB |

The benchmark `--reclaim-strings` options now repeat the default for builds that include this change. Comparisons against such a build pass no reclamation option to either role.

The `eval-allocation-diagnostics` report now also lists live requested bytes by allocation size. Sizes up to 1 KiB have exact bins and larger sizes have power-of-two bins. The `peak_snapshot_bins` field copies the bins whenever live bytes pass the previous copy by 4 MiB, so it is within 4 MiB of the peak; `final_live_bins` is taken at command return. At the peak of the base EVAL-08 run with reclamation, 2,118 MiB were requested and live. The largest bins were 64 bytes (9.6 million thunk cells, 586 MiB), 240 bytes (1.02 million `Lambda` allocations, 233 MiB), 32 bytes (5.8 million, mostly closures and two-value capture arrays, 178 MiB) and 72 bytes (1.4 million, 98 MiB). Blocks of at least 128 KiB held another 410 MiB.

Requested bytes overstate what a smaller object saves. With the allocator the evaluator links, mimalloc 0.1.52 using its default v3 sources, measured block footprints are 32 bytes for requests up to 32, 48 for 40, 64 for 48 through 64, 192 for 176, 224 for 192 and 208, and 256 for 224 through 256. Shrinking the thunk cell from 64 to 56 bytes therefore saves no memory, which also explains the earlier 56-to-48-byte suspension-span trial. A thunk cell must be 40 bytes or less to use a smaller block.

### Best candidate, below the memory target: smaller lambdas

A `Lambda` is now 160 bytes, down from 224, so with its reference counts it uses a 192-byte block instead of a 256-byte one. Function and parameter names are interned strings of one word instead of an inline `SmolStr` and a heap `String`. The selected-attribute path and the addition fallback spans share one optional box. The upvalue count and the chunk's last operation index are 32-bit. A chunk's attribute-key spans are boxed, because most chunks build no attribute set. Messages, traces and `toXML` output use the same names as before. An intermediate 176-byte `Lambda` still took a 224-byte block: with default reclamation it measured 150.627 billion EVAL-08 instructions and 2,277 MiB. Four `vm/mod.rs` lines changed: two debug assertions widen the upvalue count, and two accessors replace the former fields for addition spans and attribute spans.

The release build with both changes passed 554 evaluator tests and the doctest. Every run passed its oracle. Self-built `075a5e7` with reclamation enabled measured 150.322 billion EVAL-08 instructions and 2,307 MiB, matching the frozen binary.

| Workload | Frozen base | Smaller lambdas with default reclamation |
| --- | --- | --- |
| EVAL-08, stat medians | 150.460 billion instructions / 2,568 MiB | 150.815 billion (+0.24%) / 2,252 MiB (-12.3%) |
| EVAL-PKG-PATHS, stat medians | 8.198 billion / 307 MiB | 8.239 billion (+0.50%) / 286 MiB (-6.8%) |

Three exact-output paired rounds per workload against the frozen base gave an EVAL-08 median of 17.029 s against 17.534 s. The paired speed ratio was 1.027 (1.024 to 1.031), and the peak RSS ratio was 1.139 (1.139 to 1.139). On the package projection, three rounds gave a speed ratio interval of 0.761 to 1.071 on a loaded host. Ten further exact-output rounds gave a median of 1.060 s against 1.067 s, a speed ratio of 1.008 (0.986 to 1.031), and a peak RSS reduction from 294 MiB to 285 MiB. CppNix 2.35.2 took 3.471 s and 987 MiB on EVAL-08, and 0.285 s and 164 MiB on the package projection. With `NOXI_RECLAIM_STRINGS=0`, the same build passed the EVAL-08 oracle with 150.938 billion instructions and 2,510 MiB. The 15% EVAL-08 memory target is not met, so the change is left uncommitted as a candidate rather than kept under the lane rule. Reports are under `target/perf-mem/paired-F-eval08`, `target/perf-mem/paired-F-packages` and `target/perf-mem/paired-F-packages-10` in the `perf-mem-1` worktree, and stat runs are under `target/perf-mem/stat`.

### Rejected: 16-byte captures and a 32-byte thunk state

Static capture arrays were stored behind one tagged pointer that keeps lengths up to 15 in its low bits, and the blackhole state replaced its two optional spans with a flag. This made `Upvalues` 16 bytes and `ThunkRepr` 32 bytes, so the thunk cell requested 56 bytes. With string reclamation, EVAL-08 RSS was unchanged at 2,304 MiB because 56-byte requests use 64-byte blocks. The first version copied each capture vector into a new allocation, adding 27.6 million allocations and 3.4% instructions. Reusing the vector's boxed slice restored the allocation count. The final version still cost 1.26 billion EVAL-08 instructions (0.8%), mostly in capture construction, destruction and indexing. The patch is `target/perf-mem/bins/D/patch.diff`.

These representations only save memory together with a thunk cell of 40 bytes. That would need a custom reference-counted cell: one word holding a 32-bit strong count and a 32-bit borrow state, followed by the 32-byte state. It would keep the `Weak` count only for diagnostic and cycle-collection builds. At 9.4 million live cells this saves about 144 MiB of footprint, but the capture cost leaves almost no room within a 1% instruction budget.

### Rejected: whole-graph cycle collection

The trial-deletion collector from `target/perf-lanes/inputs/graph-gc.patch` in the main checkout was built from `075a5e7` with `--features noxi-eval/eval-cycle-collection` after a one-line borrow fix in its table growth. It passed 566 evaluator tests, also with `NOXI_EVAL_CYCLE_COLLECTION=stress` and an interval of 16. In one traced EVAL-08 run with reclamation, the oracle passed. Four collections walked 2.8 to 9.8 million nodes each, released 762,572 thunks in total and spent 6.6 s marking and scanning. The table grew to at most 138 MB. There were no survivors or aborted collections. The run used 167.389 billion instructions (+11.4%) and 2,297 MiB, 10 MiB less than the control. Most of the graph the census found collectible after VM teardown is still live at these safe points. The collector was not adopted. The patch and trace are `target/perf-mem/bins/GC/patch.diff` and `target/perf-mem/gc-trace.jsonl`.

## Derivation registration traffic, 2026-11-18

This lane applies the first remaining fix from the real flake section above. The base is a release build of ab557d5. The workload is the same flake command, run warm, with every run under the heavy lock.

### Change

`register_derivation` and `register_build_derivation` now share `write_derivation` in `crates/noxi-glue/src/store.rs`. It follows CppNix's `writeDerivation`. It sends `AddTempRoot`, then `IsValidPath`, and serializes and sends the derivation only when the path is not valid. A trace of CppNix shows the same order. Taking the root before the check means a collection cannot delete a reused derivation during the evaluation. The evaluator store also remembers the derivations it has registered. Each one already holds a temporary root for the life of the process, so a repeat registration sends no request. Read-only evaluation still makes no store request. A traced CppNix `nix eval --read-only` sends none either. Evaluation has no repair mode, since `nix eval` rejects `--repair`.

### Results

On two small derivations against the system daemon, the candidate sends exactly CppNix's requests. New derivations cost two each of `AddTempRoot`, `IsValidPath` and `AddToStore`, 1,701 bytes. Already valid ones cost two each of `AddTempRoot` and `IsValidPath`, 648 bytes. The base sent two `AddToStore` requests either way. Derivations written into fresh `local?root=` stores by the base and the candidate compare byte-identical.

A second trial, kept as `target/perf-real-2/trial-no-dedupe.patch`, drops the set of registered derivations. Every registration then sends `AddTempRoot` and `IsValidPath`, as CppNix does.

| Real flake, warm | Base | Candidate | Trial without the set |
| --- | --- | --- | --- |
| Elapsed, runs | 38.83, 37.36, 36.51, 40.43, 36.26, 36.06 s | 33.30, 33.25, 34.18, 33.81, 33.25, 33.96 s | 35.68, 38.49, 32.92 s |
| Elapsed, median | 36.94 s | 33.56 s | 35.68 s |
| User CPU, median | 26.09 s | 24.20 s | 24.63 s |
| System CPU, median | 7.07 s | 7.28 s | 6.78 s |
| Peak RSS, median | 5,139 MiB | 5,141 MiB | 5,132 MiB |

The runs alternated base and candidate in rounds one to three, and base, trial and candidate in rounds four to six. The six paired candidate deltas are −5.53, −4.11, −2.33, −6.62, −3.01 and −2.10 s, a median of −3.56 s. The trial's paired deltas are −4.75, +2.23 and −3.14 s, so its gain is noisier and its interval includes zero. All 16 runs, including a warm-up and two traced runs, return `/nix/store/h9j8xmn9dbsxfzx0csfxmak30n6dabpj-nixos-system-nixos-26.11.20261113.d5cc683.drv`. Candidate user CPU fell 1.89 s and system CPU rose 0.21 s, so process CPU fell about 1.7 s. The rest of the gain is time spent waiting on daemon round trips.

| Daemon requests | Base (earlier trace) | Candidate | Trial without the set | CppNix (earlier trace) |
| --- | --- | --- | --- | --- |
| `AddToStore` | 59,241 | 1,391 | 1,391 | 4,360 |
| `IsValidPath` | not counted | 12,977 | 52,441 | 58,736 |
| `AddTempRoot` | not counted | 12,898 | 52,362 | 58,722 |
| Bytes sent | 328 MiB | 70.0 MiB | 76.8 MiB | 105 MiB |

The flake registers about 52,000 derivations, but only about 12,900 distinct paths. The set skips the repeats. The base column comes from the earlier lane's trace of 075a5e7, which differs from ab557d5 only in fetching. The base runs had already written every derivation, so the remaining `AddToStore` requests should be path imports and text files. The trace records only opcodes, so this is not confirmed.

The set assumes that a temporary root lasts for the whole evaluation. Through the daemon, a root lasts only as long as the connection. `DaemonStore::with_conn` drops the connection after a transport error and opens a new one on the next request. Every traced run made exactly one connection. After a reconnect, the roots taken on the old connection are gone in the base, the trial and CppNix alike. Only a repeat registration would take such a root again, and the set skips that.

| Workload, single stat run | Base | Candidate |
| --- | --- | --- |
| EVAL-08, instructions / peak RSS / elapsed | 133.901 G / 2,351.0 MiB / 17.49 s | 133.927 G / 2,354.9 MiB / 16.91 s |
| EVAL-PKG-PATHS, instructions / peak RSS / elapsed | 7.9605 G / 295.7 MiB / 1.22 s | 7.9606 G / 292.4 MiB / 1.14 s |

Both oracles pass. Instructions change by under 0.02% and peak RSS by under 0.2% on EVAL-08. Both workloads use a dummy store, so no derivation is registered there.

`cargo test -p noxi-glue` passes 33 tests. The same three tests fail as in the earlier lane: `explicit_cli_overrides_are_sticky_and_match_reference_lock_bytes`, `github_gitlab_and_sourcehut_apis_lock_offline_with_reference_attributes` and `lfs_batch_and_offline_cache_match_reference`. The `noxi-commands` targets `generated_derivation_import` (3), `reference_flake_cli` (21) and `reference_flakes` (1) pass, with ignored tests included. `instantiate` passes 26 of 27. `appended_string_context_validation_matches_the_reference` fails the same way on the base. noxi prints the context key `/bad` where the reference prints `"/bad"`. The change is kept uncommitted. Binaries, traces and the opcode counter are under `target/perf-real-2`.
## Generator round trips and capture buffers, 2026-11-19

This round starts from `4860eab`, rebuilt in the lane worktree as the base. Each step was screened with one `eval-profile.py --mode stat` run per workload under the shared heavy-job lock; user-space instruction counts repeat within about 0.02%. Every run passed its oracle. The steps are cumulative.

An instruction-sampled profile of the base ranked `populate_upvalues` (5.66% of samples), generator round trips (`run_generator`, 4.24%) and `finish_force` (1.59%) after the bytecode loop. About a fifth of `run_generator` was the recursion budget request scanning the whole frame stack for `deep_force` generators. Temporary counters in an `eval-diagnostics` build counted 15.4 million generator requests per EVAL-08 run after the first step. One avoided request saved about 290 instructions.

| Step | EVAL-08 instructions, million | EVAL-PKG-PATHS instructions, million |
| --- | ---: | ---: |
| Base `4860eab`, median of 7 | 131,236.1 | 7,378.9 |
| Evaluated builtin arguments skip their trace context | 130,703.4 (-0.41%) | 7,371.6 |
| Native equality without a request, budget scan only under a deep force, register-sized capture reads | 127,433.1 (-2.90%) | 7,294.0 |
| A thunk argument is forced with its context set and cleared in one request | 127,132.7 (-3.13%) | 7,295.0 |
| Deferred captures flagged while read; `toString` and `map` skip unread frames | 126,922.0 (-3.29%) | 7,289.8 |
| `removeAttrs`, `match`, `split` and positioned argument checks use the combined request | 125,462.1 (-4.40%) | 7,232.2 |
| Returned frames give their capture buffers to a pool | 124,185.0 (-5.37%) | 7,204.1 |
| `genericClosure` orders natively comparable keys without a context frame, median of 5 and 7 | 123,966.2 (-5.54%) | 7,203.7 (-2.37%) |

A builtin's argument check used to set a context frame such as `while evaluating the first argument passed to builtins.length`, force the argument, and clear the frame, at two generator round trips and a vector, string and box per argument. Forcing an evaluated argument that is not catchable cannot fail, so `generators::ready_argument` now returns it at once. A thunk argument is forced by one `ForceInContext` request: the VM sets the frame as a `BuiltinContexts::Static` hint, built into a `Trace` only when a trace is materialized, and re-enqueues the builtin in the `AwaitingValueInContext` state, which clears the frame when the forced value arrives unless it is catchable. A catchable argument still sets the frame the old way. The builtin macros and the hand-written checks share `force_argument_in_context`. The macro change also applies to the builtins `noxi-glue` defines, without editing that crate. A forced thunk argument still allocates one box for the static frame: `FrameTrace` is 48 bytes and `Frame` 120, and an inline slot would grow both.

`check_equality` answers operands the VM would compare natively without suspending, which removed 2.2 million requests per EVAL-08 run. `deep_force` generators hold a counted guard, so a recursion budget request scans the frame stack only while one exists. Capture reads return `Result<Value, usize>`, which fits in two registers, instead of an `EvalResult` moved through memory, and clone thunk captures inline. Removing only the capture read change from the third step raised EVAL-08 to 128,197.8 million instructions, so that change alone saved 1,065 million. The fused immediate force no longer walks the new captures a second time to look for deferred ones: `populate_upvalues` reports them as it reads them.

Frames that return through the bytecode loop give a capture buffer they own to a pool of at most 32 buffers for each count from two to nine. `populate_upvalues` fills a pooled buffer in place instead of allocating one. This saved 1,277 million instructions on EVAL-08, where the diagnostics build counted 27.3 million capture buffers per run.

A ceiling probe replaced the binary span searches of `Op::Call`'s closure path and `Op::Force` with the frame's span. It saved 629 million instructions (0.49%), so a faster span lookup was not pursued in this round; the lookup lives in `chunk.rs`. The remaining requests per EVAL-08 run are dominated by real forces (2.7 million), lambda entries (1.15 million), calls (0.87 million), recursion budget requests from `nix_eq` (0.6 million), span requests of builtins that take a span (0.6 million) and `derivationStrict` contexts in `noxi-glue`.

The evaluator passed 555 tests and the doctest, and 565 tests with `eval-diagnostics` and `eval-cycle-collection`. The upstream language suite and the language boundary, generated boundary, generated forcing and instantiate tests passed, except `appended_string_context_validation_matches_the_reference`, which already fails on `4860eab`. A differential run compared exit status, stdout and stderr of the base and the final build for 72 expressions, with and without `--show-trace`, and found them byte-identical. The expressions cover thrown, aborted and mistyped thunk arguments, arguments under `tryEval` and `addErrorContext`, `map` with unevaluated and non-function arguments, `removeAttrs`, `match`, `split`, `replaceStrings`, `toString`, `elem`, `genList` and `genericClosure` with mixed and non-scalar keys. A second grid ran 24 equality and deep-force expressions at `max-call-depth` 60, half of them overflowing, and matched the base in all 48 runs.

Peak RSS was measured in interleaved base and candidate runs, because the base's own peak drifted between 234.7 and 254.2 MiB on the package projection across sessions. EVAL-08 measured 2,010.8 and 2,012.0 MiB for the base and 2,016.4 and 2,016.8 MiB for the candidate. The package projection measured 249.2 to 254.2 MiB for the base and 253.7 to 254.0 MiB for the candidate. User-space cycles fell less than instructions: about 2.3% on EVAL-08 in the interleaved pairs.

Paired exact-output timing compared the final build with the base binary and CppNix 2.35.2. Ratios above one favor the candidate. Both runs are exploratory.

| Workload | Pairs | Base | Candidate | CppNix | Speedup vs base, 95% interval | Peak RSS ratio vs base, 95% interval |
| --- | ---: | --- | --- | --- | --- | --- |
| EVAL-08, first run | 3 | 30.392 s / 2,009 MiB | 29.482 s / 2,015 MiB | 6.259 s / 986 MiB | 1.028 (1.020 to 1.033) | 0.997 (0.997 to 0.999) |
| EVAL-08, second run | 3 | 29.994 s / 2,012 MiB | 28.934 s / 2,015 MiB | 6.437 s / 975 MiB | 1.030 (1.022 to 1.037) | 0.999 (0.998 to 1.000) |
| EVAL-PKG-PATHS | 10 | 1.565 s / 253 MiB | 1.545 s / 252 MiB | 0.538 s / 165 MiB | 1.007 (0.992 to 1.021) | 1.001 (1.000 to 1.002) |

All three binaries ran about twice as long as in earlier rounds and as in this round's stat runs, CppNix included. The cause was not established: the runs recorded the `powersave` CPU governor and no cgroup throttling, and the load average fell from 1.25 to 1.05 during the second EVAL-08 run. The paired ratios compare the binaries under the same conditions. The EVAL-08 interval excludes parity in both runs; the package projection's includes it.


The kept source, rebuilt after formatting, measured 123,948.0 million EVAL-08 instructions in one run with its oracle passing.

Builds, patches, stat runs, profiles, diagnostics counters, the differential and paired reports are under `target/perf-vm-2` in the lane worktree. Each candidate directory holds the patch it was built from.

## Allocator page faults and transparent huge pages, 2026-11-19

This lane looks at kernel time spent on first-touch page faults. The base is a release build of f858f14, at `target/perf-alloc-1/base/bin`. Every run held the heavy lock. The host runs THP `enabled=madvise` and `defrag=madvise`. Every mTHP size except 2 MiB, which inherits, is `never`. The hugetlb pool is empty.

### Why `MIMALLOC_ALLOW_LARGE_OS_PAGES=1` did nothing

mimalloc 3.3.2 (libmimalloc-sys 0.1.49) already calls `madvise(MADV_HUGEPAGE)` on each 1 GiB arena it reserves, because `allow_thp` defaults to 1. A trace of base EVAL-08 shows three such calls. `allow_large_os_pages` only adds a first try with `MAP_HUGETLB`, which fails with ENOMEM and falls back to the same mmap and madvise. The options mimalloc printed with `MIMALLOC_VERBOSE=1` confirm this.

Each first touch of a 2 MiB region in an madvised arena tries a huge page. Under `defrag=madvise` that try does direct compaction. Physical memory on this host was fragmented for most of the day. One base EVAL-08 run caused 1,585 compaction stalls, 1,583 of them failed, 165 M pages were scanned for migration and 990 THP faults fell back. AnonHugePages peaked at 4 MiB, or 124 MiB with the env var. The process then faulted 4 KiB pages anyway. Syscalls account for about 0.1 s (`strace -c`), so nearly all of the 1.8 to 2.8 s of system time went to faults costing 5 to 9 µs each.

A microbenchmark, `target/perf-alloc-1/spike/fault-bench.c`, faulted 2 GiB in a loop. Plain 4 KiB faults cost 0.30 to 0.34 s of system time, or 0.6 µs each. `MADV_POPULATE_WRITE` cost 0.25 to 0.28 s. With `MADV_HUGEPAGE` on the same fragmented host, it cost 1.43 to 2.31 s.

### Change

`crates/noxi-commands/Cargo.toml` enables mimalloc's `no_thp` feature. It compiles out only the `madvise(MADV_HUGEPAGE)` call; `allow_thp` stays 1. This matters: `MIMALLOC_ALLOW_THP=0` would also call `prctl(PR_SET_THP_DISABLE)`, which spawned builders inherit across exec. A trace of the candidate shows no `MADV_HUGEPAGE`, no prctl and 0 compaction stalls. Instructions:u change by +0.01% on EVAL-08 and 0% on EVAL-PKG-PATHS.

### Results

The outcome depends on fragmentation, so each row is labelled by the base run's minor faults. About 500 K on EVAL-08, or about 1 M on the flake, means THP failing ("fragmented"). About 8 K or 25 K means THP working. In between is mixed. Medians are shown.

| Workload, regime | Base elapsed / user / sys / faults / peak RSS | Candidate elapsed / user / sys / faults / peak RSS |
| --- | --- | --- |
| EVAL-08, fragmented, wait4, 3 pairs | 15.54 s / 13.57 s / 1.91 s / 499 K / 2,013.5 MiB | 14.00 s / 13.34 s / 0.60 s / 524 K / 2,009.3 MiB |
| EVAL-08, fragmented, stat, 3 pairs | 16.62 s / 131.234 G instructions / 2.82 s sys / 2,012.3 MiB | 14.04 s / 131.251 G instructions / 0.57 s sys / 2,009.8 MiB |
| EVAL-08, THP working, wait4, 3 pairs | 13.05 s / 12.78 s / 0.20 s / 8.5 K / 2,069.9 MiB | 13.46 s / 12.86 s / 0.52 s / 543 K / 2,009.1 MiB |
| EVAL-PKG-PATHS, fragmented, wait4, 3 pairs | 0.963 s / 0.724 s / 0.233 s / 28.0 K / 257.4 MiB | 0.824 s / 0.718 s / 0.101 s / 59.4 K / 240.2 MiB |
| EVAL-PKG-PATHS, fragmented, stat, 3 pairs | 0.894 s / 7.377 G instructions / 257.8 MiB | 0.823 s / 7.377 G instructions / 240.6 MiB |
| EVAL-PKG-PATHS, THP working, paired run, 3 pairs | 0.692 s / 0.666 s / 0.026 s / 274.2 MiB | 0.762 s / 0.685 s / 0.066 s / 242.1 MiB |
| User flake, fragmented, 3 pairs | 58.83 s / 49.17 s / 7.84 s / 1,040 K / 4,036.6 MiB | 53.57 s / 49.49 s / 2.54 s / 1,075 K / 4,034.8 MiB |
| `nix-instantiate --eval -E 1`, 20 pairs | 6 ms / 282 faults / 17.3 MiB | 6 ms / 401 faults / 17.2 MiB |

The paired `run.py` EVAL-08 run, with a fragmented base at 3.59 s sys, gives a candidate speedup of 1.099, interval 1.091 to 1.105. Absolute times were about twice the others because another lane was compiling. The paired `eval-packages.py` run came after a host restart had defragmented memory. It gives 0.895, interval 0.818 to 0.980, against the base, and both pass all 9 samples.

The user's flake, after the restart, gave six more pairs. All return `/nix/store/h9j8xmn9dbsxfzx0csfxmak30n6dabpj-nixos-system-nixos-26.11.20261113.d5cc683.drv`. Each delta is candidate minus base, as elapsed / user+sys CPU, labelled by base faults:

| Base faults | Delta elapsed | Delta CPU |
| --- | --- | --- |
| 25 K | −1.76 s | +2.68 s |
| 286 K | −2.24 s | −1.68 s |
| 316 K, candidate first | +6.46 s | +6.07 s |
| 200 K | +0.72 s | +0.76 s |

The 316 K pair had 29.5 s of candidate user CPU against about 23.3 s in every other recent run, which looks like host noise. These pairs are inconclusive. Whenever THP works even partly, the base matches or beats the candidate on CPU.

On EVAL-08 the candidate is stable: 13.4 to 14.1 s with about 0.55 s sys in both regimes. The base ranges from 12.9 to 16.6 s, plus one 88.7 s wall-clock stall on 18.6 s of CPU. The candidate never raises peak RSS. Base THP backing costs 3% more RSS on EVAL-08 and 13% on EVAL-PKG-PATHS. The candidate does not reduce fault counts; each fault becomes cheaper because the failed compaction is gone.

### Decision

The change is a conditional keep, left uncommitted with its patch at `target/perf-alloc-1/cand/no_thp.patch`. It meets the bar on a fragmented host, which was this host's state for most of the day and returned within two flake runs of a restart. It fails the 1% elapsed bar while THP succeeds: EVAL-08 is 3% slower and EVAL-PKG-PATHS is 0.895x.

Rejected spikes:
- `MIMALLOC_ALLOW_LARGE_OS_PAGES=1`: no-op, as explained above.
- `MIMALLOC_ALLOW_THP=0`: matches the candidate's timing (EVAL-08 sys 2.6 s to 0.61 s, elapsed 16.4 s to 14.2 s) but sets `PR_SET_THP_DISABLE` for the process and every child.

Not built: `MADV_POPULATE_WRITE` batching through `mi_manage_memory` commit callbacks. At 0.6 µs a fault it would save about 0.1 s. Also not built: a runtime probe that stops madvising new arenas once faults stop being backed by huge pages.

`cargo test -p noxi-eval -j 4` passes, 556 tests. Binaries, traces and logs are under `target/perf-alloc-1`.
## Thunk cell size, 2026-11-19

This lane follows the custom thunk cell plan from the heap footprint section above. The base is a self-built release `nix-instantiate` of a8c9fdc. On that base `ThunkRepr` was 40 bytes: a suspended thunk held its lambda, 24 bytes of captures and its span. `Rc<RefCell<ThunkRepr>>` therefore requested 64 bytes, with a strong count, a weak count and a borrow flag ahead of the state.

### Change

`value/thunk/cell.rs` adds `RcCell<T>`, a reference-counted cell with one header word: a 32-bit strong count and a 32-bit borrow state. The weak count exists only in builds with `eval-diagnostics` or `eval-cycle-collection`, the only ones that hold weak references to thunks. Release builds therefore use 8 bytes of header, and a thunk with a 32-byte state requests 40 bytes, which mimalloc places in a 48-byte block. The cell checks borrows exactly as `RefCell` does and panics with the same messages. `Ref::map` and `RefMut::map` behave the same, and `Debug` prints `RefCell { value: .. }` as before. Count overflows abort as in `Rc`. Unit tests cover nested and conflicting borrows, panicking projections, a value whose drop borrows the cell again after `replace`, a panicking drop, `try_unwrap` with outstanding weak handles, and weak upgrades that fail during the value's drop.

`ThunkRepr` becomes 32 bytes without trial D's 16-byte captures, which cost 0.8% of instructions:

- A suspended thunk no longer stores its span. It reads it from its lambda through `Lambda::thunk_span`, which falls back to the first span of the chunk. VM blueprints already recorded the span in `Lambda::suspended_span`. This change also records it for constant thunks without captures in the compiler, for imports and derivation builtins through `Thunk::new_suspended`, and for the shared lazy-call lambdas of builtins, whose span is the first span of their chunk. `Thunk::new_suspended` sets the span on a lambda it owns alone. A shared lambda must already record it: debug builds assert this, and release builds keep the lambda's span. No current caller passes a shared lambda.
- The `Debug` form of a thunk keeps its `RefCell { value: .. }` wrapper. The suspended state no longer shows a `span` field, and the blackhole shows `origin`.
- A blackhole stores its forcing span and one optional pair of origin spans instead of two independent optional spans.
- `Captures` in `upvalues.rs` is `#[repr(u8)]`. Without it, rustc encodes the capture tag in the `Value` tag of the single-capture variant at offset 8, and the blackhole cannot be placed around that niche, so `ThunkRepr` stays 40 bytes. With an explicit tag at offset 0, the other states fit after it.

A compile-time assertion keeps `ThunkRepr` at 32 bytes and the release cell header at one word.

### Results

Three `eval-profile.py --mode stat` runs per binary gave the following medians. All oracles passed.

| Workload | Base | Thunk cell | Change |
| --- | --- | --- | --- |
| EVAL-08 instructions:u | 123.952 G | 123.702 G | -0.20% |
| EVAL-08 peak RSS | 2,071.5 MiB | 1,928.5 MiB | -6.9% |
| EVAL-PKG-PATHS instructions:u | 7.206 G | 7.210 G | +0.06% |
| EVAL-PKG-PATHS peak RSS | 272.8 MiB | 255.0 MiB | -6.5% |

The EVAL-08 saving of 143 MiB matches the 144 MiB expected from about 9.4 million live cells moving from 64- to 48-byte blocks. The first two candidate stat runs overlapped debug test builds of this lane. Their elapsed times of 63 and 35 s reflect that contention, and their instruction counts and RSS match the third, quiet run.

Ten exact-output paired rounds on EVAL-08 gave medians of 12.168 s against 12.332 s, a speed ratio of 1.001 (0.983 to 1.019), and a median peak RSS of 1,926.0 MiB against 2,070.3 MiB. An earlier three-round set, run while page reclaim was active on the host, gave 0.919 (0.775 to 1.111). Ten rounds on the package projection gave 1.074 s against 1.132 s, a speed ratio of 1.028 (0.940 to 1.119), and peak RSS from 267.0 MiB to 252.6 MiB. Every sample passed its exact-output check. CppNix 2.35.2 took 3.154 s on EVAL-08 and 0.420 s on the package projection.

`cargo test -p noxi-eval` passes 564 tests. With `eval-diagnostics,eval-cycle-collection` it passes 577. `upstream_lang` passes. The `noxi-commands` targets `language_boundaries`, `generated_boundaries`, `generated_forcing` and `instantiate` pass 34 of 35, and `appended_string_context_validation_matches_the_reference` fails as on main. Miri was not run, because no local toolchain includes it. The change is kept uncommitted. Binaries, stat runs, paired reports and the patch are under `target/perf-mem-2` in the lane worktree.

## EVAL-08 frontend, 2026-11-23

This lane looked for the cost of the Home Manager outputs of EVAL-08. The base is e2f1840 and the reference is CppNix 2.35.2, both on NixOS under WSL2. The engines do the same Nix work: 8,141,456 lambda calls against 8,141,466 on the combined case, and the same files parsed once each. The base takes 3.02, 3.60 and 3.49 times the instructions of CppNix on the combined, module and standalone outputs.

### Where the frontend cost is

Replaying the 3,907 files the module output imports (41.3 MB) with LTO off takes 32.0 G of a 65.4 G instruction run: 12.9 G to parse, 2.0 G for lints, 4.0 G for the duplicate check and 13.0 G to lower to bytecode. `hackage-packages.nix` is 40.3% of that source. CppNix imports it in 2.61 G instructions and noxi in 10.86 G, which is 10%, 19% and 28% of the combined, module and standalone gaps. About 2% of its packages are evaluated. On the combined case the bytecode loop, captures and scheduling are a larger share (41.0% of instructions by symbol), and this lane does not change them.

### Changes

- `check_duplicates` no longer revisits attribute sets it already checked through their parent binding. The parent's check sees every definition of the nested set in one tree, so the first error does not change. Nodes are keyed by interned names, and dotted paths are built only for errors.
- `parser_lints::Settings::check` returns early when the path and URL lints are ignored and a green tree scan finds no `or` identifier.
- `compile_contents` moves the source text into the source map instead of copying it twice, and drops the parse tree once the file is compiled.

### Results

Three interleaved `eval-profile.py --mode stat` runs per binary gave the following medians, for the three changes together. All oracles passed.

| Output | Base instructions:u | Changed | Change |
| --- | --- | --- | --- |
| Combined | 123.646 G | 120.947 G | -2.18% |
| Home Manager module | 58.788 G | 56.145 G | -4.50% |
| Home Manager standalone | 41.718 G | 40.117 G | -3.84% |

Elapsed time could not be separated from noise on this host. Peak RSS fell from 987 to 967 MB on the module output and from 780 to 749 MB on the standalone output, and did not change on the combined case.

Not landed:

- Dropping a per-token `String` in the rnix 0.14.0 tokenizer brings the totals to -3.24%, -6.58% and -5.75%. It needs a release of rnix, because a `[patch]` section does not reach crates.io consumers.
- Emitting function bytecode on the first call, after an eager pass that keeps the order of compile errors, is designed and not built. Lowering bounds it at 13.0 G of 65.4 G on the module output.
- An in-frame select of one static name changed +0.05%, -0.02% and -0.17%, and was rejected.
