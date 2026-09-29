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

The system must not start two independent daemons writing the same store. Installation and rollback must select one owner of store mutations at a time. If a future internal store differs from CppNix storage, a migration with integrity verification and reverse operation is required before it can be called a drop-in replacement.

## Extensions

An extension is opt-in through a new command, setting, or negotiated protocol capability. With extensions disabled, it must leave reference file formats, hashes, paths, command output, and daemon messages unchanged. Extension data needed for long-term system maintenance must survive rollback or provide a documented reversal procedure.

Examples may include richer progress events, configuration previews, and GUI-managed edits. The public compatibility commands retain their reference defaults.

[Lua configuration](lua-configuration.md) is an extension of the native surfaces. They default to Lua for user configuration, but compatibility commands never evaluate `.lua` files, and the matrix rows and their evidence are unaffected by it. A new UI is not evidence of compatibility and does not excuse missing legacy behavior.

## Evidence required to close a matrix row

Each row needs a versioned reference fixture set, pass/fail results, tested binaries and dependencies, a complete inventory of the commands and protocols in scope, and end-to-end system tests. Every failing, skipped, or unrun case must remain visible. A single passing Nixpkgs evaluation or boot test does not close the row.
