# Rebuild diagnostics and CLI logging specification

## What we are fixing

The current host's `nixos-rebuild` launcher invokes the Python implementation of `nixos-rebuild-ng`. NixOS release notes describe it as a Python rewrite of the earlier rebuild command. The Nix package manager it drives has a C++ implementation, including its daemon. Nix already offers `raw`, `bar`, `bar-with-logs`, and `internal-json` log formats. The criticism here is about the usefulness of the end-to-end rebuild experience, not a claim that every Nix log is Python or that Nix has no structured output. [NixOS 25.05 release notes](https://github.com/NixOS/nixpkgs/blob/master/nixos/doc/manual/release-notes/rl-2505.section.md), [NixOS rebuild module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/installer/tools/tools.nix), [Nix daemon source](https://github.com/NixOS/nix/blob/master/src/libstore/daemon.cc), [Nix log formats](https://nix.dev/manual/nix/2.35/command-ref/nix-build.html)

The design intent is blunt: a rebuild must stop dumping shitty walls of text and calling that useful feedback. A wall of build noise followed by "command failed" is bullshit. When something breaks, the user needs the failing phase, the actual cause, the relevant source or package, the system state left behind, and a concrete next step. Progress must say what the machine is doing. Noise belongs in the full log, not in the user's face by default.

Rust is an implementation choice, not a guarantee of good diagnostics. The standard is the practical clarity of a good Rust compiler error: a precise location when one exists, a short explanation, a cause chain, and a trustworthy suggestion. The engine must produce that information as structured data before the CLI renders it.

## Two CLI contracts

The OS-native command, provisionally `noxi`, uses the diagnostic behavior in this document for all operations: evaluation, builds, fetches, cache substitution, store maintenance, system rebuilds, activation, and rollback. The TUI and GUI consume the same event stream and diagnostic objects.

Compatibility commands such as `nix`, `nix-build`, and `nixos-rebuild` must retain their declared reference output, exit status, and log-format behavior under the [drop-in contract](compatibility.md). They may offer the improved renderer through an explicit new flag or setting. The distribution may advertise `noxi system switch` as its preferred interactive rebuild command, but cannot silently change the output of a compatibility command and still claim byte-for-byte CLI parity. This distinction resolves the tension between better logs and 100% backwards compatibility.

### Compatibility log formats

The compatibility commands render CppNix's logger contract, not the event schema below. `nix_compat::log::logger` ports CppNix 2.35.2's three loggers behind one activity API: producers start activities, report results (progress, expected totals, build log lines, phases) and log messages, and the selected `--log-format` renders them.

- `raw` and `raw-with-logs` are CppNix's simple logger: an activity prints its text followed by `...` when the verbosity admits it, and build log lines print only with logs.
- `internal-json` writes one `@nix` object per call, with the keys in nlohmann's sorted order, ANSI codes kept, invalid UTF-8 replaced and ids of the form `(pid << 32) + n`. Errors carry `raw_msg` and null `file`, `line` and `column`.
- `bar` and `bar-with-logs` are a port of `progress-bar.cc`. On a terminal (standard error is a TTY, `TERM` is set and not `dumb`, and neither `NO_COLOR` nor `NOCOLOR` is set) a timer thread redraws the `[N/M built, ...]` status and the newest visible activity, truncated to the terminal width; log lines clear the status line first. Redirected, the bar prints like the raw logger, and with logs each line is `NAME> LINE`.
- `-Q` selects `raw`, `nix` starts with `bar`, and `multiline` and `multiline-with-logs` are rejected because 2.35.2 has no such formats.
- As in CppNix, the bar stops (clearing its line and showing the cursor) before an error is printed, before `nix run`, `nix shell`, `nix develop` and `nix fmt` execute their program, before `nix edit` runs the editor, for the whole of `nix repl`, before `nix eval` prints, before `--print-out-paths`, and when the command thread ends or panics. Other output to standard out written while the bar draws clears the status line first (`write_stdout`).

The local build backend reports CppNix's worker progress: `realise`, `builds` and `copy-paths` activities, an update for each goal created, started and finished, a `build` activity per derivation whose output is split into lines as `BuildLog` splits it, and builder `@nix` `setPhase` and `msg` lines handled as `handleJSONLogMessage` does. Binary-cache substitution reports `substitute`, `copy-path` and `query-path-info` activities, and `nix store prefetch-file` reports its `file-transfer`. `reference_log_format` compares all of this with the reference; [verification](verification.md#legacy-flags-and-logging) lists the normalisations and remaining gaps.

Through a daemon the same activities cross the worker protocol. A daemon worker installs CppNix's `TunnelLogger` as the process's `logger::LogSink`, so the builds, substitutions and messages of its operations go to the client as `STDERR_START_ACTIVITY`, `STDERR_STOP_ACTIVITY`, `STDERR_RESULT` and `STDERR_NEXT` messages, queued until the operation starts work and dropped for clients older than 1.20, whose activities become `text...` lines as in CppNix. `BuildPaths` runs the local backend's worker (`noxi_build::backend::Worker`), and `BuildPathsWithResults`, `BuildDerivation`, `EnsurePath` and substituting `QueryValidPaths` report the same activities. The client hands each message to its own logger with the daemon's ids, parents, types, levels and typed fields, as `processStderr` does, so every `--log-format` renders a daemon's work as it renders local work.

## Rebuild phases and visible progress

An OS-native rebuild has one operation ID and these ordered phases. Each phase emits start, progress, completion, or failure events. An already satisfied phase says so without pretending work occurred.

| Phase | What the user should see | Failure must identify |
|---|---|---|
| Resolve | Selected configuration, flake or file, lock revision, target system, and requested action | Missing input, lock conflict, unavailable source, or unsupported target |
| Evaluate | Current configuration file or module path when useful, evaluated system target, and option diagnostics | The exact source span or definition chain when known |
| Plan | Resulting derivation and intended generation; changed packages and services when knowable | Why a plan cannot be produced; unknown changes stay marked unknown |
| Fetch and substitute | Cache source, artifact identity, transferred bytes, and trust result | Network error, missing artifact, hash mismatch, or rejected signature |
| Build | Active derivations, completed count, cache hits, current build step, and elapsed time | The failed derivation, builder exit status, meaningful log excerpt, and full log location |
| Prepare activation | Target closure, current generation, proposed boot default, and selected `build`, `test`, `boot`, `switch`, or `dry-activate` action | Permission error, stale preview, missing closure, or conflicting operation |
| Activate | Bootloader, mounts, units, users, and other changes as they are applied | The exact failed step and whether running state or boot default changed |
| Verify and finish | New running generation, boot default, prior generation, elapsed time, and rollback command | Any failed post-switch check and the actual final state |

The CLI must never show a made-up percentage. If the build graph is incomplete or dynamic, use counts for known tasks and an indeterminate indicator. A cache hit is reported as a cache hit, not as a build. A remote builder's host and failing stage must remain visible. The normal view updates in place on a terminal and emits stable, line-oriented phase changes when output is redirected or running in CI.

`build`, `test`, `boot`, `switch`, and `dry-activate` keep their [NixOS meanings](system.md). For `build`, stop after realization and say no activation occurred. For `boot`, say the running system is unchanged. For `test`, say the boot default is unchanged. For `dry-activate`, show intended changes and say none were applied. The final message must report observed state, not the action the program hoped to perform.

## Structured event contract

Every producer, including evaluator, store, builder, daemon, and activation code, writes events with at least `schema_version`, `operation_id`, `event_id`, `parent_event_id` when applicable, monotonic sequence, timestamp, phase, kind, severity, and a stable machine-readable code. Relevant events also carry source path and span, option name, derivation path, store path, builder host, current and target generation, bytes or task counts, and an optional full-log reference. Sensitive values are redacted before persistence or rendering.

An error object includes a concise summary, the triggering operation, one or more typed causes, the best known source span, affected artifact, impact on running and boot state, and a suggested action when the engine can justify one. Suggestions must come from known facts such as a misspelled option with a verified match or a named failed service. Do not fabricate a fix when the cause is unknown. A diagnostic code links the summary to detailed documentation.

Structured events are the source of truth. The pretty terminal output, TUI, GUI, and versioned JSON output render the same events. They must not infer phase or cause by regexing human-readable lines. External builder output may be unstructured; capture it as attached raw output and identify which derivation produced it.

The JSON mode emits one versioned object per line on stderr, with stable field names and event ordering within an operation. Structured command results remain on stdout. The existing Nix `internal-json` mode belongs to the compatibility command behavior; the new event schema has its own version and compatibility policy. Nix's manual describes `internal-json` as structured but warns that its message text can change between releases. [Nix log formats](https://nix.dev/manual/nix/2.35/command-ref/nix-build.html)

## Fleet operation events

The later [fleet operations](distributed-computing.md#large-fleet-execution-and-visibility) use a parent operation and attributable child operations. Relevant events add fleet revision, stable machine identity, cluster/topology identity, batch or step identifier, coordinator authority epoch when used, observation time, and service health. Report drain, membership change, provisioning, reboot/reconnect, fencing, and replication checks as typed steps, not unexplained build output.

Ordering is defined within each producer stream and its coordinator-recorded event stream. An aggregate sequence is receipt order, not a global causal order across machines. Durable cursors and deduplication identifiers must let clients reconnect without treating a repeated event as a repeated mutation. Large fleets require filtered/paginated results, bounded normal output, and backpressure; fetching status must not fetch every full log.

Final results distinguish completed, failed, unreachable, blocked, and not-started targets, saved desired revision, actual per-machine generations, and verified runtime membership. Quorum loss, replication lag, insufficient capacity, expired authority, and failed traffic checks retain their specific diagnostics. A count of successful activations is not a percentage of service availability or proof of fleet convergence.

## Human-facing output rules

- In a TTY, show one compact status area with current phase, active jobs, elapsed time, and useful counters. Do not redraw the entire terminal for every build line.
- On failure, stop progress animation and print one diagnostic block. Show the source span or artifact, cause chain, state impact, one justified next step, and how to open the full log.
- Collapse repeated warnings by code and source, then show the count. Do not hide distinct failures under one generic summary.
- Keep successful builds quiet enough that a user can spot the one failing derivation. The full raw builder log remains available on demand during and after the operation.
- Use color only as a supplement to words and symbols. Respect `NO_COLOR`, non-TTY output, terminal width, and screen-reader-friendly text. Never rely on animation to convey a state change.
- On cancellation, name the canceled operation and say which child builds stopped, which continued, and whether any system mutation had begun.
- Never print credentials, signing keys, authorization tokens, or secret-bearing environment values in either normal or debug output.

The full log must record every relevant subprocess line with its producer and timestamp. Store it under a documented retention policy and print its location at failure. Verbose and debug modes may show raw output live, but normal mode shows only a bounded excerpt with enough context to identify the failure. A missing full log is itself an error to report.

## Status lines

Progress uses cargo's shape: a verb right-aligned to 12 columns, then what it applies to. Multi-line text continues aligned under the first line. Every operation ends with `Finished ACTION in TIME`, `Failed ACTION after TIME`, or `Stopped ACTION after TIME` (ended before changing anything). Durations read as `340ms`, `2.81s`, `1m 42s` or `1h 03m`, and sizes use binary units (`412.0 MiB`).

```text
   Resolving flake /etc/nixos#host (lock 3f9a1c2)
  Evaluating nixos-system-host-25.11 (2.8s)
 Substituted 214 paths from cache.nixos.org (412.0 MiB in 31s)
    Building openssl-3.4.1, nginx-1.27.3 [2/5]
    Finished nixos-system-host-25.11 in 2m 14s
```

## Terminal palette

Colors follow rustc and cargo, so their meanings are already familiar. No other colors are used, and nothing sets a background.

| Element | Style |
|---|---|
| `error` and error codes, primary underline of an error | bold bright red |
| `warning` | bold bright yellow |
| `note` | bold bright green |
| `help` | bold bright cyan |
| Gutters, `-->`, `\|`, line numbers, secondary underlines | bold bright blue |
| Status verbs | bold green; bold red for `Failed`, `Stopped` and `Cancelled`; bold yellow for work that finished with warnings |
| Package and artifact names | bold |
| Store hashes, paths and timings | dim |
| Per-build line prefixes when output cannot be redrawn | a stable color from a hash of the build name, from cyan, magenta, blue, green, bright magenta and bright cyan; never red or yellow |

Color is on when the stream is a terminal. `NO_COLOR` (any non-empty value) or `TERM=dumb` turns it off, and `CLICOLOR_FORCE` (non-empty, not `0`) turns it on even when redirected. `COLUMNS` overrides the detected width, and rendering assumes at least 40 columns.

## Diagnostic codes

Every diagnostic has a stable code: a letter for the area and four digits. `R` is resolving and updating sources, `E` evaluation, `F` fetching, `S` substitution, `B` building, `A` activation and verification, and `D` the daemon. A published code never changes meaning, and a retired code stays reserved. `noxi explain CODE` prints the summary and explanation. The table lives in `crates/noxi-events/src/codes.rs`.

## Per-build output streams

Builds that run in parallel never share a stream. Every activity has an id, and all of its output is split into whole lines before it is rendered or logged. Two builds can therefore never interleave within a line. On a terminal, each running build will get its own live lane. When output cannot be redrawn, each line is prefixed with its build in that build's color, and `--group-logs` will print each build as one contiguous block when it finishes. Each build's full output is also written to its own log file under the operation's directory.

## Example diagnostic shape

Diagnostics are rendered by `annotate-snippets`, the renderer rustc uses.

```text
error[E0001]: undefined variable 'pkgz'
 --> /etc/nixos/configuration.nix:3:34
  |
1 | { pkgs, ... }:
  |   ---- did you mean this argument?
2 | {
3 |   environment.systemPackages = [ pkgz.git ];
  |                                  ^^^^ not defined here
  |
  = note: evaluation stopped before any build or system change
help: a variable with a similar name exists
  |
3 -   environment.systemPackages = [ pkgz.git ];
3 +   environment.systemPackages = [ pkgs.git ];
  |
```

A build failure names the derivation and the step it failed in, and shows an excerpt of its log with the failing lines underlined:

```text
error[B0001]: build of `openssl-3.4.1` failed in buildPhase
    --> /nix/var/log/noxi/ops/1/builds/openssl-3.4.1.log:4411:1
     |
4410 | CC crypto/evp/e_aes.o
4411 | crypto/evp/e_aes.c:812:5: error: implicit declaration of function 'foo'
     | ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
     |
     = note: builder exited with status 2 after 1m 42s
     = note: required by `nginx-1.27.3` -> `nixos-system-host-25.11`
     = help: see the full log with `noxi logs openssl-3.4.1`
```

The renderer produces both shapes today. `noxi system` reports its failures with it. Mapping evaluator errors and build failures from the engine into these shapes is still open. An activation failure must state whether the boot default moved and which services changed before the failure. Never replace that information with an opaque traceback or a thousand lines of dependency output.

## Acceptance tests

Test the renderer and underlying events separately. Fixture cases must include a Nix syntax error, invalid option, conflicting definitions, unavailable flake input, hash mismatch, untrusted cache signature, sandbox denial, failed derivation, remote build failure, daemon authorization failure, failed service activation, stale preview, cancellation, and recovery after interrupted output. For each case, assert the error code, phase, source or artifact, cause, next action when justified, actual system state, and full-log reference.

Run the same operation in TTY, redirected, `NO_COLOR`, narrow terminal, debug, and JSON modes. Assert that stdout remains usable by scripts, JSON parses as its declared version, sensitive values never appear, and large builder logs do not flood normal output. Compare compatibility-command output separately to the reference release; improvements in the OS-native renderer cannot excuse a compatibility regression.

Fleet cases add interleaved child events, reconnect with duplicate delivery, filtered large-fleet output, unreachable targets, stopped batches, lost quorum, blocked drain, denied promotion, exhausted capacity, and stale-coordinator rejection. Assert truthful per-target state and service diagnostics without fabricated aggregate progress or secret-bearing enrollment data.
