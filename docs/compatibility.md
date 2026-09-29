# Drop-in compatibility specification

## Reference and claim

The initial reference release is CppNix 2.35.2 on x86_64-linux, the newest numbered release when it was selected on 2026-09-28. A claim of "100% backwards compatible" applies only to a published [compatibility matrix](compatibility-matrix.md) that names the CPU architecture, operating system, reference release, Nixpkgs revision, enabled experimental features, store configuration, and supported client and daemon pairings. For each row, every behavior exposed by the reference and applicable to that row is required. An untested item is incomplete, not a pass. The matrix must identify any platform the product does not support instead of implying universal support.

The target is a full drop-in replacement, so the inventory includes command behavior and public library consumers, not only the Nix language and file formats. Exact compatibility is the acceptance goal. The repository has not reached it.

## Required behavior

| Area | Compatibility criterion |
|---|---|
| Language | Same accepted syntax, lazy evaluation, value coercion, builtins, string contexts, imports, source positions, and observable error behavior under equivalent settings |
| Derivations and paths | Identical serialization, input graphs, output names, placeholders, and store paths for the same inputs and reference mode |
| Store | Read and update an existing supported store; preserve path validity, reference metadata, roots, profiles, garbage collection, import/export, and crash recovery |
| Builds | Equivalent sandbox restrictions, builders, environment, substitution, remote build behavior, scheduling constraints, cancellation, and logs where observable |
| Binary caches | Compatible NAR, narinfo, hashing, compression, signatures, trust decisions, and substituter behavior |
| Daemon | Compatible worker protocol negotiation and operations; match multi-user trust rules and error handling |
| CLI | Existing `nix` subcommands and legacy commands accept the same flags and inputs and return compatible exit status, stdout, stderr, JSON, and environment effects |
| Configuration | Compatible `nix.conf`, environment settings, flakes and `flake.lock`, profiles, registries, and experimental feature gates |
| Libraries | Public C interface and C++ ABI consumers used by the declared reference and platform must pass a separately recorded interoperability gate |
| NixOS | Same module evaluation outcome, system closure, generation management, and activation semantics for supported configurations |

The Nix manual documents derivation inputs and placeholders as part of the build model, and string contexts as part of dependency tracking. Matching printed values alone cannot establish equivalent build plans. [Derivations](https://nix.dev/manual/nix/2.35/store/derivation/index.html), [string contexts](https://nix.dev/manual/nix/2.28/language/string-context.html)

## Existing store and downgrade safety

The replacement must be tested on a copy of an existing CppNix store before it is allowed to modify a user's store. The gate includes daemon/client combinations, concurrent readers and writers, interrupted registration, garbage collection, cache substitution, and return to the reference implementation. Preserve the original database and path semantics unless a tested reversible migration is specified. A filesystem copy alone does not prove that both engines can safely write to the same store.

### Expected store states

Two starting states are qualified separately, because the reference treats them differently when it opens a local store.

**Empty store.** Opening a directory with no database creates the state recorded in `tests/reference-inventory/empty-store.txt`, which `just reference-inventory` captures from 2.35.2. The recorded state has these parts:

- The directories `nix/store`, `nix/store/.links`, `nix/var/nix/{db,gcroots/per-user,profiles/per-user,temproots}`.
- `db/big-lock` and an 8 MiB `db/reserved` file, which garbage collection deletes to free space when the disk is full.
- `db/schema` containing `10`.
- A WAL-mode SQLite database with the `ValidPaths`, `Refs`, `DerivationOutputs` and `SchemaMigrations` tables, the `IndexReference` and `IndexDerivationOutputs` indexes, the `DeleteSelfRefs` trigger, and the migration `20260309-drop-redundant-indexreferrer` already recorded.

With `ca-derivations` enabled, 2.35.2 also applies `20251017-ca-derivations`. nix-rs must create the same layout, schema text, and recorded migrations, so that 2.35.2 can open a store nix-rs created without migrating it. It must also open a store 2.35.2 created without changing it.

**Copy of an existing CppNix store.** 2.35.2 opens a store as follows:

- It refuses a schema above 10 and refuses schemas below 6.
- It upgrades schemas 6 to 9 in place under the exclusive big lock by adding `ultimate`, `sigs` and `ca` and dropping `FailedPaths`.
- It creates `SchemaMigrations` if it is missing and applies each missing named migration.
- In read-only mode it fails instead of migrating.

nix-rs must accept and upgrade exactly the stores that 2.35.2 accepts and upgrades, with the same resulting schema and migration rows. It must refuse the same stores with an equivalent error. The rules for a store nix-rs has opened are:

- The recorded paths, hashes, registration times, references, derivers, signatures, CA fields, GC roots, profiles, and generation links must stay byte-identical except where the reference would change them.
- nix-rs must never write a migration name or schema version that 2.35.2 does not know. The reference would refuse such a store, which breaks the return path.
- Opening a store and returning to 2.35.2 must need no repair step.

The qualification store is a copy of the reference VM's `/nix` taken from the reference-only snapshot below. It is never the development host's store.

**Reference-only VM snapshot.** Before any nix-rs binary touches a qualification VM, boot the pinned reference VM with CppNix only, shut it down cleanly, and keep its disk image as a read-only base snapshot. Every nix-rs run uses a copy-on-write overlay of that base. Rollback tests compare the result with the base after returning to CppNix, and a failed run is discarded by deleting its overlay. [System tests](verification.md#system-tests)

The system must not start two independent daemons writing the same store. Installation and rollback must select one owner of store mutations at a time. If a future internal store differs from CppNix storage, a migration with integrity verification and reverse operation is required before it can be called a drop-in replacement.

## Extensions

An extension is opt-in through a new command, setting, or negotiated protocol capability. With extensions disabled, it must leave reference file formats, hashes, paths, command output, and daemon messages unchanged. Extension data needed for long-term system maintenance must survive rollback or provide a documented reversal procedure.

Examples may include richer progress events, configuration previews, and GUI-managed edits. The public compatibility commands retain their reference defaults.

[Lua configuration](lua-configuration.md) is an extension of the native surfaces. They default to Lua for user configuration, but compatibility commands never evaluate `.lua` files, and the matrix rows and their evidence are unaffected by it. A new UI is not evidence of compatibility and does not excuse missing legacy behavior.

## Evidence required to close a matrix row

Each row needs a versioned reference fixture set, pass/fail results, tested binaries and dependencies, a complete inventory of the commands and protocols in scope, and end-to-end system tests. Every failing, skipped, or unrun case must remain visible. A single passing Nixpkgs evaluation or boot test does not close the row.
