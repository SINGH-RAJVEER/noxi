# Benchmarks against CppNix

The runner compares identified release binaries against CppNix 2.35.2 and an optional pre-change noxi binary. It has 39 offline adapters covering subsets of the [154 catalog scenarios](catalog.tsv). The [benchmark specification](../docs/performance-benchmarks.md) defines the full contracts and remaining dimensions. The [architecture plan](../docs/performance.md) maps 32 optimization hypotheses to experiments; the [research appendix](../docs/performance-research.md) records CppNix prior art.

Run everything through `devenv shell`. The Python tools use uv and the standard library. Linux cgroup v2 delegation with `cpu`, `memory`, `io`, `memory.peak`, and `cgroup.kill` is required. The runner finds an already delegated parent, creates its own child groups, and refuses resource-qualified timings if accounting is unavailable.

```sh
devenv shell -- just reference
devenv shell -- just bench-build
devenv shell -- just bench-test
devenv shell -- just bench-list
devenv shell -- just bench target/nix-reference/bin target/release target/test-busybox --case START-01 --case EVAL-04 --case NAR-05 --samples 30 --size 64
```

Reference building and fixture preparation are outside timing. Build fixtures copy the already installed static BusyBox from the host daemon into owned diverted stores. Measured commands use isolated homes, configuration, caches, and diverted stores or owned sockets. Nothing activates a host system or changes its profiles.

To preserve a baseline, run these commands before changing Rust source. Keep its source record beside its binary directory.

```sh
devenv shell -- just bench-build
devenv shell -- mkdir -p target/benchmarks/baseline/bin
devenv shell -- bash -c 'cp --reflink=auto target/release/{noxi,nix-instantiate,nix-store,nix-build,nix-env,nix-collect-garbage,nix-daemon} target/benchmarks/baseline/bin'
devenv shell -- uv run --no-project --no-config python benchmarks/run.py --record-source target/benchmarks/baseline/source.json
```

Rebuild after the change, then add `--baseline-bin target/benchmarks/baseline/bin` to the run. Each matched block contains reference, candidate, and baseline commands, ordered by a recorded seed. Setup and correctness inspection run outside the clock. A failing side invalidates the entire block. A failed preflight prevents timing that case.

For a sequence of optimizations, add preserved artifacts with repeatable `--comparison-bin NAME=DIR`, for example `--comparison-bin original=target/benchmarks/eval-original/bin`. Each participates in the same shuffled block and correctness gate, retains its own source record, and receives a separate comparison with the candidate. Raw samples also support adjacent-step comparisons without rerunning engines in different host conditions.

## Implemented subsets

| Cases | Current work |
| --- | --- |
| START-01, START-03, START-04 | Version dispatch, forced pure evaluation, and a cold store query. |
| EVAL-01 through EVAL-06 | Deterministic literals, attributes, merge/intersection, list sort/fold, shared lists, and repeated imports; strict JSON compared with independent expected values. |
| EVAL-08 | The four derivation paths from `just nixos-paths`, using the qualification flake's locked Nixpkgs and Home Manager sources. Strict JSON must match all four recorded paths and the reference. Source resolution uses host Nix outside timing; the timed evaluator uses a dummy store and performs no builds or activation. `--size` does not alter this fixed workload. |
| FETCH-01, NAR-05 | Writable `builtins.path` imports into empty diverted stores; verify tree identity and CppNix-readable metadata. |
| PLAN-01, DAG-01, DB-06 | CppNix-prepared derivation chains; dry-run planning, serial realization, and output queries. |
| CACHE-01, CACHE-02, CACHE-03, CACHE-06, CACHE-08, CACHE-10 | Signed loopback HTTP/file cache, single/wide/chain closures, injected request latency, a missing first cache, and six compression formats. |
| NAR-01 through NAR-04 | Canonical dumping of preseeded store trees. NAR-03 uses one large file; the other cases use ordinary small-file trees with modes and symlinks. Non-UTF-8 and other catalog dimensions remain open. |
| DB-01, DB-02 | Batched path hash queries and closure traversal in cloned compatible stores. |
| BUILD-01, BUILD-03, BUILD-06, BUILD-07 | Static BusyBox output, file-writing and log-producing builders, and a fixed-output hash mismatch. These measure realization, not evaluation or an end-to-end package install. |
| RPC-01 | Both clients against each server on owned sockets. Client and daemon workers share the accounting group. |
| PROFILE-06 | Rollback between two text-path generations created with `nix-env --set`; check the selected logical target. |
| GC-01, GC-02, GC-05, GC-06 | Dead-path enumeration/deletion, content verification, and optimization of small preseeded stores. |
| FAIL-02, FAIL-03 | Rejected input-addressed signatures and an unavailable narinfo server; require failure without publication or registration. |

`--size` selects file/node/expression count except NAR-03, NAR-05, CACHE-01, and CACHE-08, where it selects payload MiB. `--case` is repeatable. `--suite` selects the catalog's earliest intended suite; it does not expand scale or cache-state matrices. Unselected and unsupported cases remain in `coverage.json`. Catalog status `partial` means a subset adapter exists, not that the complete scenario contract is qualified.

## Output and limits

Runs write `metadata.json`, `samples.jsonl`, `coverage.json`, `summary.json`, `report.md`, raw stdout/stderr, and a checksum manifest under ignored `target/benchmarks/<run-id>/`. Binary and source digests, exact arguments/environment, fixture identity, correctness outcomes, CPU/I/O counters, cgroup memory peaks, per-process `wait4` RSS, and cache requests/body bytes are retained. Version strings do not establish source provenance. A supplied baseline without `source.json` is marked unverified.

For the module workload, use `devenv shell -- just bench target/nix-reference/bin target/release target/test-busybox --case EVAL-08 --samples 30 --timeout 300 --baseline-bin target/benchmarks/baseline/bin`. Profile separately from timing. For example, `devenv shell -- perf record -e cycles:u -c 10000000 -o target/benchmarks/eval.data -- target/release/nix-instantiate --eval --strict --json benchmarks/eval-08.nix --argstr nixpkgs NIXPKGS_SOURCE --argstr homeManager HOME_MANAGER_SOURCE`, with the isolated environment and source paths recorded by the runner. Fixed-period flat cycle sampling works on hosts which reject frequency sampling or call-stack capture. Inspect with `devenv shell -- perf report --stdio --no-children -i target/benchmarks/eval.data`.

The native launcher forks after its own exec, then starts the clock immediately before target exec. This prevents the target's `wait4` RSS from inheriting the Python fixture generator's high-water mark. Python startup and the native fork are outside elapsed time; target startup, native resource-file writing and collector wake/reap latency are included. Cgroup CPU/I/O include the native launcher and descendants. Cgroup peak memory includes file cache; `target_wait4_max_process_rss_bytes` is a separate per-process metric. Launcher RSS retains inherited high-water marks and must not be used as target RSS. RPC memory peaks include daemon startup, while CPU/I/O are deltas across the client command. HTTP server resources sit outside the client group, and response-byte counters cover successful file bodies, excluding headers and error pages.

Every invocation has a fresh home and store state. OS page cache is uncontrolled. Source trees, builders, and HTTP service setup can warm it. No warm-process/cache, contention, VM activation, remote, native-service, phase-tracing, hardware-counter, or end-to-end usability adapter exists yet. Baseline and candidate effects are checked by CppNix outside timing; NAR dumping adapts physical versus logical path arguments to produce the same bytes.

The runner defaults to 30 paired samples and reports medians, paired geometric speedups, bootstrap intervals, and resource/counter medians. Runs below 30 pairs are exploratory. It emits no p95 or aggregate drop-in score. Shared-host timing and one synthetic scale do not qualify a general speed claim.

The TSV columns are `id`, `family`, `scenario`, `sizes`, `states`, `metrics`, `oracle`, `optimizations`, `suite`, `requires`, `comparison`, and `status`. IDs are stable. Comparison lanes distinguish equivalent CppNix work, native workflows requiring an equivalent driver, and experimental storage/fleet studies. Keep unsupported work visible when extending the runner.

The indexed scheduler also has a native diagnostic benchmark. Run `devenv shell -- cargo test --release -p noxi-build ordering_scaling -- --ignored --nocapture`. It emits 30 paired JSON samples at 256, 1,024 and 4,096 nodes for chains and wide graphs. It alternates the old scan and indexed implementations, excludes graph cloning, and checks every returned order against the old implementation. These results isolate ordering cost; they are not CppNix installation ratios.
