# CppNix architecture and performance evidence

Research date: 2026-10-11. This document records source evidence and design hypotheses. It contains no measured speedups. Implementation priorities and experiments belong to the [performance architecture](performance.md) and [benchmark specification](performance-benchmarks.md).

## Baseline

The repository's `flake.nix` requests CppNix 2.35.2. `flake.lock` resolves `nix-reference` to `2c73b59da29606068c0c98db015dd3a66955525d`. The default workspace's existing ignored `tests/nix-official/` checkout has that exact HEAD; ignored files are not copied into new JJ workspaces. All Nix source links below use that immutable revision. The [qualification lock](../tests/qualification/flake.lock) pins Nixpkgs/NixOS to `714a5f8c4ead6b31148d829288440ed033ccc041` and Home Manager to `7b4c5ec4bedaf1e062bbc1bcaeddbc6bd242aa1b`. The activation sources cited below match those revisions. Each benchmark still records its full fixture lock and root configuration digest.

Version numbers alone are insufficient benchmark provenance. Record binary hashes, source revisions, build flags, configuration, store mode, enabled experimental features, fixture locks, filesystem, and kernel. Benchmark both shipped defaults and equal resource limits. Keep those results separate.

## What CppNix already does

| Area | Verified behavior in 2.35.2 | Consequence for noxi proposals |
| --- | --- | --- |
| Evaluation | `Expr*::eval` methods evaluate AST nodes; thunks defer work until values are forced. CppNix already uses lazy evaluation. [Evaluator source][eval] | Noxi already has a bytecode VM. Measure dispatch, allocation, forcing, and attrset behavior; a VM alone does not establish an advantage. |
| Flake evaluation cache | `eval-cache` defaults to true. Its setting explicitly excludes intermediate results. SQLite stores cached attributes. [Settings][eval-settings], [cache implementation][eval-cache] | Persisting compiled modules or dependency-tracked intermediate results is a candidate extension. Repeating the same locked flake must compare against CppNix's warm cache. |
| Cache identity | `openEvalCache` enables a fingerprint only for pure evaluation. The fingerprint includes locked inputs and relevant source metadata; unlocked inputs cannot provide the same key. [Flake source][flake] | Cache invalidation must account for lock changes, source metadata, evaluation mode, and every external read. A source file hash by itself is insufficient. |
| HTTP | HTTP/2 defaults on. The shared transfer object reuses connections and multiplexes HTTP/2 requests. The default TCP connection limit is 25. HTTP/3 is optional and depends on curl support. [Transfer settings and API][transfer-settings], [curl implementation][transfer] | Rust async networking, pooling, multiplexing, and more connections are existing techniques. Compare scheduling, actual request counts, and network utilization at equal limits. |
| Retry policy | Transfer settings implement exponential backoff, jitter, and rate-limit handling that respects `Retry-After`. [Transfer settings][transfer-settings] | Faster failure reporting must preserve server backoff and retry policy. Measure tail latency and retries under controlled errors. |
| Metadata lookup | CppNix has async substitutable-path queries and async missing-path discovery. [Missing-path source][missing] | A concurrent closure planner primarily closes a current noxi gap. Reducing duplicate lookups, RTTs, and allocations is the comparative hypothesis. |
| Metadata caches | CppNix caches positive and negative narinfo lookups on disk, with default TTLs of 30 days and one hour. Its in-memory path-info cache defaults to 65,536 entries. [Global settings][globals], [store API][store-api-h] | Metadata caching is baseline behavior. Any proposal must test expiration, newly published paths, cache garbage collection, and trust-domain separation. |
| Realisation scheduling | `Worker` deduplicates goals and schedules ready work through an event loop with separate build and substitution slots. The default substitution limit is 16. [Worker][worker], [worker settings][worker-settings] | Parallel dependency execution is existing CppNix behavior. An event-driven ready queue, adaptive resource budgets, and lower coordination overhead are testable noxi changes. |
| Substitution workers | A substitution goal waits with `co_await`, then starts a thread for `copyStorePath`; it wakes the worker when complete. [Substitution source][substitution] | A bounded async network pipeline plus CPU/I/O workers may reduce thread and scheduling overhead. Merely using coroutines is not a new comparison point. |
| NAR copying | `copyStorePath` connects NAR production to ingestion through a source/sink adapter. It checks whether the destination already has the path before starting the copy. [Store API][store-api] | Streaming and avoiding duplicate downloads are baseline expectations. Measure bounded memory, backpressure, and overlapping decode/hash/write stages. |
| NAR validation | Local import hashes the same bytes it restores through `TeeSource`, verifies declared NAR hash and size, and registers the path after validation. Signature checks, path locks, and temporary roots protect the import. [Local store][local-store] | Replacing noxi's full immutable-byte capture with a file spool must preserve those identity and publication conditions. A second read of mutable source data can introduce a race. |
| Store database | CppNix already uses WAL, prepared statements, and transactional `registerValidPaths`. That operation inserts references and rejects cycles before commit. WAL checkpoint configuration amortizes metadata sync work. [Local store][local-store], [SQLite][sqlite] | WAL and transactions also already exist in noxi. Measure lock contention, round trips, batching across imports, reader concurrency, and checkpoint policy before changing database design. |
| Store transports | CppNix provides local, daemon, HTTP cache, local cache, S3, and SSH stores. Remote-store clients have a configurable connection pool, default maximum one daemon connection. [Store types][store-types], [remote-store settings][remote-store] | Connection pooling and store backends are existing architecture. Compare IPC calls and connection churn. New batching protocols need negotiated support and fallback for stock daemons. |

## Architectural candidates worth testing

The following are proposals inferred from the evidence above and the current noxi implementation, rather than claims about achieved behavior.

- Keep evaluation, closure planning, downloading, decompression, verification, materialisation, and metadata publication as separately bounded stages. Let independent objects overlap, while dependencies constrain visibility and build readiness. Measure both end-to-end time and stage queueing. Resource limits must include decoded bytes and open files, not only task counts.
- Replace repeated scans of waiting derivations with dependency counters and a ready queue. Deduplicate shared inputs across requested outputs and concurrent requests. Measure wide DAGs, long chains, diamond graphs, cache hits, cancellation, and failures; a long chain offers little dependency parallelism.
- Introduce batch metadata queries, concurrent closure discovery, and per-cache in-flight request coalescing. Keep cache priority and trust rules explicit. Standard HTTP binary caches still serve individual narinfo files, so a single-request closure manifest requires an optional cooperating cache protocol.
- Extend evaluation reuse in stages: content-addressed parsed/compiled modules, immutable locked-flake output plans, then dependency-tracked intermediate evaluation. Start with pure inputs. Persistent arbitrary live VM heaps bring closure, thunk, cycle, exception, string-context, and evaluator-version concerns. Imported paths, directory membership, symlink targets, file contents, environment access, platform, builtins, and store observations all need invalidation rules where applicable.
- Give unchanged environments a verified plan that skips evaluation and redundant store work. Validate GC roots and store validity before reuse. Equivalent CppNix workloads must receive warm evaluation and fetch caches; native manifest convenience belongs in a separate comparison with equivalent package and service behavior.
- Use read concurrency and a bounded metadata writer queue to shorten time under the current store database mutex. Batch validated imports without delaying small jobs indefinitely. Preserve the on-disk schema and crash behavior when interoperating with CppNix; an independent database requires explicit synchronization and migration rules.
- Remove whole-NAR heap retention where it exists. An immutable temporary file is a practical first step that keeps hashing and ingestion tied to the same bytes. A deeper single-pass staging importer can validate while writing, then publish atomically. Compare peak RSS, written bytes, temporary disk use, syscalls, and failure cleanup on large archives and tiny-file trees.
- Measure filesystem work before selecting hardlinks, reflinks, `io_uring`, packed metadata, or directory caches. Filesystem support and workloads determine the result. NAR ordering, permissions, executable bits, symlinks, and SHA-256 identities must remain compatible.
- Optimize allocator pressure, symbol interning, attrset merges, thunk representations, and VM instructions using evaluator profiles. CppNix's laziness means benchmark output forcing must match exactly. Skipping serialization or leaving a result unforced creates a different task.
- Consider critical-path scheduling and adaptive transfer/CPU/disk limits only after fixed-limit concurrency works. Reducing one request's latency can worsen fairness for concurrent users. Track small-job latency while a large closure occupies the machine.
- Persist derived shell environments only with complete identity and explicit invalidation. Arbitrary shell hooks and service readiness still need execution where their contract requires it. A fast command return is not proof that an environment is usable.

These candidates eliminate work or reduce coordination overhead. Rust's implementation language does not itself predict installation speed. Download throughput, decompression, filesystem metadata operations, package builds, and service startup may dominate different runs.

## Experimental changes with a separate contract

| Proposal | Why it may help | Boundary and required experiments |
| --- | --- | --- |
| Intermediate evaluation database | Reuses unaffected computations after a local edit. | Prove invalidation on changes to contents, directory membership, missing files, symlinks, locks, source metadata, system, evaluator version, and allowed impurity. Compare edits affecting one package against edits affecting most modules. |
| Signed closure manifests or metadata bundles | Reduces many remote metadata requests on high-latency links. | Requires a cooperating cache and a defined freshness/trust contract. Measure against both ordinary caches and CppNix using its normal metadata cache; report protocol advantage separately. |
| Chunked content-addressed storage or delta transfer | Reuses identical bytes across related objects and upgrades. | Changes storage/transport and garbage-collection design. Measure deduplication ratio, server CPU, metadata size, random reads, reassembly, and ordinary NAR import/export compatibility. |
| Content-addressed derivations | Can avoid rebuilds when derivation changes leave outputs identical. | CppNix already exposes experimental `ca-derivations`; compare matching semantics. It does not automatically eliminate fetching, verification, or activation. [Experimental features][experimental] |
| Lazy package materialisation | Defers fetching files until they are read. | Treat as an opt-in store backend, with eager compatible import/export. Measure first command readiness, later misses, offline behavior, kernel and dynamic-loader reads, GC, and failure after activation. Returning before full closure availability is a different completion contract. |
| Frozen module or activation plans | May reuse unaffected system/user configuration computations. | NixOS module evaluation and activation scripts contain dependencies and arbitrary effects. Require observed-input tracking and a declared side-effect contract before incremental execution; do not silently skip scripts. |

Content-addressed derivations, internal CAS storage, and lazily materialised files are distinct proposals. None should be presented as a universal drop-in speedup. The [Nix experimental feature policy][experimental] explicitly permits changes or removal of experimental interfaces.

## Environment and system activation boundaries

NixOS generates activation script fragments in dependency order and updates `/run/current-system`. Its activation options describe filesystem, account, and other effects, and require repeated execution to be idempotent. This work is part of nixpkgs's system activation machinery. A faster evaluator or store can shorten its inputs without automatically accelerating every activation effect. [Pinned NixOS activation source][nixos-activation]

Home Manager activation is an ordered DAG. Its documented write boundary separates checks from side effects. It generates an activation package independently of package realisation. Its implementation also already reuses a profile generation when unchanged in the relevant driver path. Preserve these semantics before experimenting with skipping or parallelizing actions. [Pinned Home Manager source][hm-activation]

Benchmarks should report distinct milestones: requested values evaluated, requested closure realised, profile published, activation completed, and declared services ready. Use disposable VMs for system activation and isolated homes for user activation. Measure no-op switches, one package added, one configuration file changed, service configuration changed, rollback, and restart-required services separately. Package realization results are valid even if noxi cannot yet execute the whole activation workflow, but that unsupported stage cannot produce a comparative speedup.

## Evidence limits

- Source inspection establishes mechanisms, default settings, and candidate bottlenecks. It does not establish their share of wall time on this machine.
- A warm CppNix daemon, its disk caches, and its operating-system page cache can survive individual CLI invocations. Benchmark setup must define each cache explicitly.
- Public cache and network variation can overwhelm a small client improvement. Use a controlled cache fixture for causal comparisons and keep live-network results as a separate realistic workload.
- Local fixture sources, nixpkgs, Home Manager, and CppNix have separate revisions. Record all of them in each result.
- A proposal that changes the cache server, completion milestone, package outputs, build flags, trust requirements, or durability needs its own result category.

[eval]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libexpr/eval.cc
[eval-settings]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libexpr/include/nix/expr/eval-settings.hh
[eval-cache]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libexpr/eval-cache.cc
[flake]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libflake/flake.cc
[transfer-settings]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/include/nix/store/filetransfer.hh
[transfer]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/filetransfer.cc
[missing]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/misc.cc
[globals]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/include/nix/store/globals.hh
[store-api-h]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/include/nix/store/store-api.hh
[worker]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/build/worker.cc
[worker-settings]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/include/nix/store/worker-settings.hh
[substitution]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/build/substitution-goal.cc
[store-api]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/store-api.cc
[local-store]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/local-store.cc
[sqlite]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/sqlite.cc
[store-types]: https://nix.dev/manual/nix/2.35/store/types/index.html
[remote-store]: https://github.com/NixOS/nix/blob/2c73b59da29606068c0c98db015dd3a66955525d/src/libstore/include/nix/store/remote-store.hh
[experimental]: https://nix.dev/manual/nix/2.35/development/experimental-features.html
[nixos-activation]: https://github.com/NixOS/nixpkgs/blob/714a5f8c4ead6b31148d829288440ed033ccc041/nixos/modules/system/activation/activation-script.nix
[hm-activation]: https://github.com/nix-community/home-manager/blob/7b4c5ec4bedaf1e062bbc1bcaeddbc6bd242aa1b/modules/home-environment.nix
