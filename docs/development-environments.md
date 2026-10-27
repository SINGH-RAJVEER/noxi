# Native projects and development environments specification

## Decision and status

The requirement for noxi to be a 100% drop-in replacement for the supported CppNix reference remains unchanged. Every applicable behavior must pass the complete [compatibility gate](compatibility.md) before that claim is made. Standard flakes must retain full support for project inputs, outputs, and development shells. Existing flake projects must work without conversion to another manifest or lock format, including their reference commands, options, output selection, shell hooks, and environment effects.

The dedicated way to define and run development environments is a built-in devenv equivalent on the native `noxi env` interface. It defines tools, shells, tasks, and supervised servers directly, or consumes inputs from an existing flake. Flakes may also export native environment definitions for that interface. Optional native project manifests add a Lua-first way to declare inputs and outputs; they are not required to use flakes or the dedicated environment interface. These routes share the evaluator, input resolver, store, build engine, and environment supervisor. Normal operation must not require an installed `devenv` executable, an external process manager, or a subprocess invoking CppNix.

This is specified, not implemented. The current `noxi` preview only evaluates pure Nix expressions. The repository's existing `devenv.nix` remains its development tooling until the native implementation is qualified. This capability follows the package-manager MVP and depends on qualified input resolution, flake evaluation, store realization, and the planned Lua bridge. It can ship on supported existing systems without requiring the later OS installer or fleet capability.

The design follows the useful separation in the upstream tools. Flakes declare inputs and outputs with a dependency lock graph. Devenv describes tools and shell settings, then explicitly starts supervised processes with readiness and dependency rules. The contracts below are noxi requirements, not a promise of compatibility with every devenv option or command. [Nix 2.35 flake reference](https://nix.dev/manual/nix/2.35/command-ref/new-cli/nix3-flake.html), [Devenv environment basics](https://devenv.sh/basics/), [Devenv processes](https://devenv.sh/processes/)

## Environment entries and shared inputs

| Declaration | Responsibility | Files and outputs |
|---|---|---|
| Standard flake | Preserve reference project inputs, lock relationships, and outputs, including development shells | `flake.nix`, `flake.lock`, and ordinary `devShells`; optional native `environments.<system>.<name>` outputs |
| Dedicated development environment | Define tools, shell behavior, tasks, and servers directly, with declared inputs or an explicit existing input source | `environment.lua` by default or `environment.nix`; standalone `environment.lock` or the selected input source's lock |
| Optional native project manifest | Add native input/output declarations without replacing standard flakes | `project.lua` or `project.nix`; `project.lock`; may export `environments.<system>.<name>` |
| Environment session | Run a selected environment at a recorded revision and report actual process state | Runtime metadata and control socket; never a project output or lock entry |

File names and the native helper API are provisional until the first schema release. The dedicated environment entry contains a schema version, an explicit input authority, and an `environment` function for the default environment or named functions in `environments`. Each function receives resolved inputs and the selected system and returns the same normalized environment schema used by flake exports. An entry may compose tracked modules, such as `shell.lua` or `shell.nix`, rather than define everything inline. Its presence never starts a server automatically.

The dedicated entry either declares its own `inputs`, or names an `inputSource`, such as `./flake.nix` or an optional native project manifest. These choices are mutually exclusive. Standalone inputs use `environment.lock`; a selected input source retains its existing lock and supplies the one resolved graph. Imported environment modules consume that graph and cannot introduce another lock or silently fetch another package set. Language dependency locks, such as `Cargo.lock`, `bun.lock`, or `uv.lock`, retain their own package-manager meaning and are declared as build inputs where required.

Native environment commands accept an explicit environment file, flake, or native project selector. Otherwise they search the current directory and its ancestors for the nearest boundary containing a dedicated environment entry or project manifest. At that boundary a dedicated entry is the preferred environment definition; a flake beside it is a possible declared input source and does not create an ambiguity by its mere presence. Both Lua and Nix dedicated entries require explicit selection when they coexist. Without a dedicated entry, native commands select the flake or native project, diagnosing multiple manifest candidates. An invalid nearer entry is an error, not permission to fall through to a parent's environment. Shell entry defaults to the selected system's `default` environment or development shell; unsupported systems and missing names are errors, with no architecture fallback. Compatibility-command discovery remains exactly the reference behavior.

An ordinary `flake.nix` is a supported environment route without a dedicated file or native project manifest. Its ordinary `devShells` continue to work through reference commands such as `nix develop`, with unchanged semantics. It may also export `environments.<system>.<name>` using the native environment schema, inline or by importing the same modules a dedicated entry uses. If it exports only `devShells`, `noxi env shell` can enter the selected development shell under qualified Nix semantics, but cannot infer a managed server graph from arbitrary shell hooks. Managed server operations require a dedicated declaration or explicit native environment output. Existing hooks still retain their reference behavior, including any user-written process startup; they do not become supervised processes implicitly.

The dedicated interface's frozen-input and explicit-server-start defaults apply to its new native commands. They do not replace the reference lock-update rules or shell behavior of compatibility commands. Native files do not change `nix flake`, `nix develop`, `nix-shell`, `flake.nix`, or `flake.lock` semantics. Qualification must cover these commands with dedicated environment files present, not merely show that native environment commands work.

## Project inputs, outputs, and locking

The optional versioned native project manifest contains `schemaVersion`, optional description, `inputs`, and an `outputs` function. Inputs retain the reference URL, source type, `follows`, transitive override, and `flake = false` concepts supported by the qualified resolver. The outputs function receives resolved inputs, including the project's `self` identity, and returns named values. Dedicated entries use the same input resolver without requiring this project manifest. Native helper functions must preserve lazy Nix values, string contexts, and store dependencies when Lua constructs the result. Evaluation must not eagerly enumerate Nixpkgs just to list output names.

Inputs may resolve ordinary flakes, source-only trees, or another native project's outputs. A native-project input declares `flake = false` and an explicit `manifest` path, such as `project.lua`, within the locked source tree. The resolver locks the tree, loads that selected manifest through the native project module, locks its transitive inputs, and passes its outputs and source metadata to the caller. The native selector is part of the tracked declaration, not an extra reference lock field. A source-only input without a selector stays a source tree; the loader must not guess an entry file. Conflicting selectors for a followed input, paths escaping the locked tree, unsupported manifest schemas, and unresolved native input cycles fail before environment realization.

Standard output names retain their qualified Nix meaning, including `packages`, `apps`, `checks`, `devShells`, modules, and supported system configurations. Native environments use the additional `environments.<system>.<name>` namespace. An environment output contains a typed definition and a realizable tool closure, not a running process or mutable database. Exporting a corresponding `devShells` output is explicit. That export carries the supported tools and shell behavior and does not promise that compatibility commands will manage native servers.

Optional native projects use `project.lock` and standalone dedicated environments use `environment.lock`, both with the reference `flake.lock` dependency-graph encoding. Native schema versions, output metadata, runtime paths, and secret values must not add fields to that reference format. A selected `flake.nix` uses its ordinary `flake.lock`, including when a dedicated entry consumes its inputs. No conversion or duplicate lock is required. Coexisting independent declarations have separate selected authorities; native commands never synchronize their lock files implicitly. Conversion to a native project is optional, explicit, and preserves the original flake files. Matching lock encodings do not make a Lua manifest evaluable by CppNix.

Native `env lock` and `project lock` create missing lock entries while retaining existing resolutions. Native `env update` and `project update` explicitly update named inputs or the whole graph. They report which selected authority and lock they modify, validate the resulting graph, and replace that lock atomically after checking the starting revision. Interrupted writes leave either the prior valid file or the new valid file. Invalid `follows` paths, unresolved cycles, changed source hashes, and unsupported fetchers produce source-located diagnostics. Resolution alone must never execute a project task or server.

Other native project and environment operations use frozen inputs by default. A missing or stale lock fails with the required explicit lock command rather than modifying files while entering a native shell. Compatibility commands keep their qualified reference behavior, including any reference-defined lock effects. Offline native operation uses already available verified sources and closures, and names the missing object when it cannot proceed. Projects cannot silently alter trusted caches, signatures, daemon trust, or engine settings.

The environment plan records the selected environment entry or exported output, its input authority, lock digest, tracked module contents, target system, tool derivations, and source-filtering policy. The development workspace path is a runtime binding; runtime state, secret files, logs, and build outputs are excluded from its captured configuration sources by default. A dirty workspace is allowed, but its actual configuration contents are recorded rather than presented as the last commit. Relative imports and source paths keep the same meaning between planning and realization.

## Environment schema and composition

Lua and Nix declarations normalize to the same versioned environment schema. Equivalent locked inputs and declarations must produce the same tool derivations and normalized task/server graph. Lua remains pure under the [Lua configuration contract](lua-configuration.md). Network access, reading an untracked environment variable, or running an initialization command is runtime work and cannot happen during evaluation.

| Field | Required behavior |
|---|---|
| `schemaVersion` | Select an understood schema; unsupported versions fail before realization or runtime effects |
| `imports` | Compose tracked environment modules with source provenance; import cycles fail |
| `packages` | Ordered package references from resolved inputs; preserve store contexts and reject executable-name collisions without an explicit precedence declaration |
| `env` | Non-secret variables plus typed references to declared runtime values or secrets; reject conflicting definitions |
| `shell` | Shell package, prompt, and explicit entry/exit hooks; the first supported interactive shell is Bash |
| `scripts` | Named commands made available inside the shell and through `env run`; they do not execute on evaluation |
| `tasks` | Finite commands with dependencies, working directory, environment overrides, timeout, and explicit retry policy |
| `processes` | Named long-running servers or workers with command, dependencies, readiness, restart, shutdown, ports, and optional file watching |
| `services` | Versioned adapters that generate ordinary tasks and processes, with explicit package, initialization, configuration, and data paths |
| `state` | Declared mutable data directories and retention behavior, separate from the tool closure and captured source tree |
| `secrets` | Runtime requirements identified by name and provider reference; secret values never enter evaluation or the store |

Composition preserves import order for package and hook lists. Names in `env`, `scripts`, `tasks`, `processes`, and `services` must be unique unless the later importing module declares an explicit replacement of that name. A conflict names both definitions; it must not silently choose the last value. Runtime environment overrides on an individual task or process take precedence over the environment's non-secret variables. References to undefined tasks, processes, secrets, or runtime values fail during planning. The same rules apply across Lua and Nix imports.

Commands use an argument vector by default. A command intentionally written as a shell script selects the declared shell explicitly. The plan resolves executable names against the captured tool closure and generated scripts, not an accidental host `PATH`. Working directories are relative to the project root unless an explicit runtime path is declared; a missing directory is diagnosed before launching that command. Shell entry, task execution, and process execution share the same tool closure and base variables, with documented command-specific overrides.

The runtime inherits a documented small set of user/session variables needed for shell operation, such as user identity, home, and terminal settings. Other host variables require a declared runtime input or an explicit invocation override. Their names and provenance appear in the result. Runtime inputs do not become pure evaluation inputs, and an environment that depends on them must not claim its runtime behavior is reproducible from the lock alone.

For dedicated definitions and native environment outputs, interactive shell hooks run only during explicit shell entry and exit. `env run`, tasks, probes, and servers do not implicitly run interactive hooks or user shell startup files. Entering or exporting these environments must not start their declared server graph. Exiting a shell does not stop an independently started session. Optional native direnv integration exports tools and variables only, and does not authorize executing tasks or starting servers automatically. Ordinary flake development shells and compatibility commands retain their qualified reference hook behavior.

Language helpers may select pinned tools and generate declared scripts or tasks. TypeScript workspace examples use Bun, and Python dependency/environment examples use uv. Helpers must show their generated commands and file effects. Dependency installation or lock updates cannot hide inside pure evaluation. User source code and language build commands can still have runtime side effects, so an environment session is not a build sandbox or a security boundary.

## Server and task lifecycle

The built-in supervisor runs unprivileged and is separate from the Nix store daemon. A server is a local development process; it is not a NixOS service, system generation, remote builder, or fleet workload. Persistent OS services continue to use the [system lifecycle](system.md), and remote service placement continues to use [fleet operations](distributed-computing.md).

Tasks and servers share a validated dependency graph. A dependency names either a task's successful completion, a process's successful start, or a process's readiness. Process dependencies default to readiness and task dependencies default to success. Cycles, dependencies on disabled required nodes, and readiness dependencies without a probe fail before any process starts. A worker without a probe may be a `started` dependency, but is never reported as readiness-verified.

Readiness supports bounded command, TCP, and HTTP probes with explicit interval, per-probe timeout, and overall startup deadline. An HTTP probe declares accepted status codes. Probes continue after startup, with declared consecutive success/failure thresholds; both thresholds default to one. Reaching the failure threshold removes readiness until the success threshold is reached again. A launched PID alone is `running`, not `ready`. Process states are `pending`, `starting`, `running`, `ready`, `blocked`, `failed`, `stopping`, and `stopped`; task completion additionally records `succeeded` or `failed` with exit status. Results distinguish process state from the latest health observation and include its observation time.

`env up` realizes the complete selected closure, binds runtime inputs, checks declared ports and directories, and starts required setup tasks and processes in dependency order. Selecting particular processes includes their transitive prerequisites. Setup tasks run once per startup attempt unless they explicitly declare a validated reusable result with tracked inputs. A failed or timed-out task blocks its dependents and is not treated as completed successfully. Tasks have no automatic retries unless declared; a migration must never be silently repeated after an uncertain interrupted outcome.

`env task` may need server prerequisites, such as a database for an integration test. Its plan names those effects. The invocation stops servers it created when the task finishes, fails, or is canceled, and preserves any preexisting session. It reports an uncertain task outcome after interruption; a later invocation requires an explicit retry rather than assuming the interrupted work never ran.

By default, a startup failure stops everything newly started by that invocation, cancels its remaining tasks, and reports residual effects, including any initialized data. Processes already running before the invocation remain owned by their existing session. An explicit keep-going mode may retain successful independent branches, but reports a partial failure and returns nonzero. Foreground startup waits for all selected readiness conditions, then continues supervising and streaming status. Detached startup returns success only when those conditions hold; it never returns success merely because a supervisor forked.

Restart policies are `never`, `on-failure`, and `always`, with a bounded retry count and backoff. The default is `on-failure` with at most five retries per process start request. A user stop never triggers automatic restart. When a required process exits or loses readiness, the supervisor stops dependent processes in reverse dependency order, marks them blocked, and resumes them only after their prerequisites are satisfied. Dependent tasks are not automatically rerun. Restarting a selected process uses the same dependency and readiness rules and reports which dependents restart with it.

An unrecoverable process failure after startup uses the same default cleanup and keep-going policy as a startup failure. The supervisor records a failed session result even when the original detached client has exited. A process that remains alive but unready has a bounded recovery deadline; expiry is a failure rather than an indefinitely blocked successful session.

Shutdown sends the declared graceful signal to owned process groups, waits the declared deadline, then terminates remaining owned descendants. The default is SIGTERM with a ten-second grace period on the initial Linux target. Dependents stop before dependencies. Foreground Ctrl-C stops a new session started by that invocation. An explicit attach, or foreground `up` reusing a preexisting session, only detaches on Ctrl-C and says the processes remain running. `env down` is idempotent and reports any process it could not stop. Exiting the client that launched a detached session leaves the supervisor running; supervisor failure must trigger a guardian cleanup or leave enough verified ownership information for recovery to diagnose and stop survivors safely.

File watching is opt-in, debounced, and declares watched paths, extensions, and ignored outputs. Build output directories, environment state, and logs are ignored by default. A source change can restart the selected process with its captured environment. Changes to the dedicated entry, input authority, manifest, lock, imported environment modules, or secret-provider settings mark the session stale and require an explicit replan and restart. A watcher cannot switch to an unreviewed package set or rerun a data migration automatically.

## Runtime identity, state, and secrets

Session identity includes the user, canonical workspace root, selected environment entry or project manifest, system, and environment name. Separate Jujutsu workspaces or checkouts have distinct identities even when they share a repository and lock. The first implementation permits one supervisor per identity. An explicit `up` against the same frozen plan can reuse that session; it reports already-running processes and starts only requested missing branches. A different configuration revision requires an explicit restart and cannot silently replace the active plan. Status, logs, attach, and down use the recorded session identity and work even when current source no longer evaluates.

The default persistent state is under `.noxi/state/<project-id>/<system>/<environment>/` in the workspace, with access restricted to the owning user. The project ID distinguishes the canonical workspace and selected entry, so different declarations do not accidentally share a database. An explicitly configured external state directory is also allowed. The control socket lives in a user-private runtime directory. Runtime records contain a session ID, plan digest, supervisor identity, and verified process identities. Control clients authenticate the owning user. A stale PID or reused socket path is not authority to kill an unrelated process; cleanup verifies ownership and process start identity first. Recovery reports survivors it cannot verify as unknown rather than healthy or stopped.

Ports declare a name, protocol, address, and either a fixed value or explicit allocation range. Servers bind to loopback by default. Conflicting fixed ports fail before startup where detectable; a bind race remains a startup failure. Automatic allocation records and injects the chosen value through declared runtime variables, and all dependents use that same value. It never silently changes a requested fixed port or claims that a preflight check reserves a socket. Non-loopback listeners and privileged capabilities must be explicit and shown in the plan; the first release does not elevate privileges automatically.

`env down` preserves mutable service data. A separate `env clean --state` operation lists the exact owned directories to delete, requires an explicit destructive-action acknowledgement, and refuses an active session. It must not follow symlinks out of those directories or delete source, external data, or another workspace's state. Data format upgrades, backups, and incompatible binary downgrades are adapter-specific operations. Reverting a lock or tool closure does not reverse a database migration or restore application data.

The first required service adapter is PostgreSQL. It declares its package revision, data format, local endpoint, initialization task, readiness probe, graceful shutdown, and supported version transitions. Initialization never overwrites an existing data directory; it checks compatibility before starting. Application servers and workers remain expressible as ordinary processes. Other service adapters, including Valkey and container-backed services, need their own versioned contracts and acceptance evidence before being advertised.

Secret providers resolve values only immediately before authorized runtime commands. A configured dotenv file is opt-in and parsed as data, never sourced as executable shell code. Missing required secrets fail before affected processes start. Secret values and untracked secret file contents must be excluded from configuration digests, generated derivations, store objects, plan output, event records, and debug output. Plans record secret names and provider identity. Managed output redacts known values, but arbitrary application output cannot be guaranteed secret-free; full logs are private, bounded, and governed by explicit retention. A secret change requires an explicit runtime restart rather than an input-lock update.

Realized tool closures receive temporary GC roots while shells, commands, tasks, or sessions use them. An optional prepared-environment root has an explicit retention policy. Stopping the final user of a closure removes its session roots, while persistent data remains. A failed preparation must not leave an indefinitely retained root. Garbage collection never treats a mutable state directory as a store output.

## Native commands and operation interface

The command spelling is provisional. All commands accept explicit project, environment, and system selectors where applicable. Output selection uses an exact name rather than evaluating a shell expression supplied by the client.

| Command | Contract |
|---|---|
| `noxi env init` | Create a dedicated Lua environment entry and modules, with standalone inputs or an explicitly selected existing input source; refuse to overwrite declarations |
| `noxi env lock` / `noxi env update [input...]` | Explicitly create or update the dedicated entry's selected input lock and report the authority and diff |
| `noxi project init` | Optionally create a native Lua project and environment template; refuse to overwrite existing declarations |
| `noxi project show` | List inputs and outputs, lock revision, and supported target systems without running environment commands |
| `noxi project lock` / `noxi project update [input...]` | Explicitly create or update the selected input lock and report the diff |
| `noxi project build <output>` / `noxi project run <app>` | Realize a selected package or run a declared application through the shared engine, using frozen inputs |
| `noxi project check` | Validate manifests and declared checks; run declared build checks without starting local server sessions |
| `noxi env list` / `noxi env show` | List environments or inspect tools, variables, tasks, processes, dependencies, state paths, and secret requirements |
| `noxi env plan` | Resolve frozen inputs and validate the environment graph; report realizations and intended runtime effects |
| `noxi env prepare` | Realize the environment closure without running hooks, tasks, or servers |
| `noxi env shell` | Enter the selected shell with tools, variables, and its declared interactive hooks |
| `noxi env run -- <command> [args...]` | Run one foreground command in the environment and propagate its exit status |
| `noxi env task <name>` | Execute the named task and its prerequisites; declare any required process startup in the plan |
| `noxi env up [process...]` | Start and supervise the selected graph; `--detach` leaves a managed background session |
| `noxi env status` / `noxi env logs [process]` | Inspect recorded state and attributed logs; follow mode is explicit |
| `noxi env attach` | Observe a live session without starting or stopping its processes |
| `noxi env restart [process...]` / `noxi env down` | Explicit runtime controls bound to the recorded session |
| `noxi env export` | Export supported shell variables and tool paths without executing hooks or starting processes |
| `noxi env clean --state` | Delete explicitly acknowledged, stopped, environment-owned mutable state |

Pure inspection and planning may fetch declared verified inputs or cache metadata when needed, but do not realize the tool closure, execute hooks, or mutate source or lock files. `project check` and `env prepare` may build derivations. Build checks execute in the qualified build sandbox; runtime integration checks use explicit tasks and sessions in disposable workspaces. Keep these effects visible in command help and structured results.

The project/input module owns manifest selection, output selection, and locked resolution, including the selected input authority of a dedicated entry. The environment planner owns direct-entry evaluation, normalization, validation, and closure planning. The supervisor owns runtime process lifecycle and observed state. These modules call the existing evaluator/store/build interfaces directly and return versioned typed results. The supervisor does not duplicate store scheduling or run user development processes through the privileged Nix daemon.

CLI, TUI, and GUI clients share those operations. Initial qualification may deliver the full headless CLI first; any later graphical environment controls must expose the same frozen plan, process identities, state paths, partial outcomes, and recovery behavior. The [configuration workflow](configuration-workflow.md) applies when a client saves an owned environment declaration. Shell entry and runtime process controls do not rewrite that declaration or create a NixOS generation.

Extend the [structured event contract](logging.md) with project identity, environment name, plan digest, session ID, task/process name, attempt number, observed state, and endpoint metadata. Render resolve, evaluate, plan, realize, initialize, start, ready, restart, stop, and finish as distinct phases. JSON results stay on stdout and events on stderr. In JSON mode runtime child output is an attributed log record and cannot corrupt results. Ordinary `env run` preserves child terminal and standard-stream behavior and propagates signals. Validation and usage failures return 2, operation failures return 1, and successful operations return 0; `env run` and `project run` propagate child status, with signal exits represented by the documented Unix convention. No success result may hide failed readiness or an unknown surviving process.

## Illustrative declarations

These examples show the dedicated entry, ordinary flake route, and optional native project route sharing environment modules. They are design examples, not executable configuration for the current preview. Native helper names depend on the unresolved Lua/Nix bridge.

The preferred standalone `environment.lua` entry needs no flake or native project manifest:

```lua
return {
	schemaVersion = 1,
	inputs = {
		nixpkgs = { url = "github:NixOS/nixpkgs/nixos-unstable" },
	},
	environment = function(args)
		local pkgs = args.inputs.nixpkgs.legacyPackages[args.system]
		return args.importEnvironment("./shell.lua", { pkgs = pkgs })
	end,
}
```

Its native Nix counterpart, `environment.nix`, uses the same `environment.lock`:

```nix
{
	schemaVersion = 1;
	inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
	environment = { inputs, system, ... }:
		import ./shell.nix { pkgs = inputs.nixpkgs.legacyPackages.${system}; };
}
```

To use an existing flake's inputs in either dedicated entry, replace its `inputs` declaration with `inputSource = "./flake.nix"`. The input graph then comes from that flake and `flake.lock`, with no `environment.lock` copy.

A standard `flake.nix` can also expose the shared environment module directly and retain an ordinary development shell:

```nix
{
	inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
	outputs = { self, nixpkgs, ... }:
		let
			system = "x86_64-linux";
			pkgs = nixpkgs.legacyPackages.${system};
			environment = import ./shell.nix { inherit pkgs; };
		in
		{
			devShells.${system}.default = pkgs.mkShell {
				packages = environment.packages;
				inherit (environment.env) APP_ENV;
			};
			environments.${system}.default = environment;
		};
}
```

`nix develop` selects the ordinary development shell under reference semantics. Explicit native `noxi env up` can select the exported environment graph. The custom output does not add server-management behavior to compatibility commands, and reference diagnostics for additional output names remain reference behavior.

An optional `project.lua` can export the same definition:

```lua
return {
	schemaVersion = 1,
	inputs = {
		nixpkgs = { url = "github:NixOS/nixpkgs/nixos-unstable" },
	},
	outputs = function(inputs, project)
		local system = "x86_64-linux"
		local pkgs = inputs.nixpkgs.legacyPackages[system]
		return {
			packages = { [system] = { default = pkgs.hello } },
			environments = {
				[system] = {
					default = project.importEnvironment("./shell.lua", { pkgs = pkgs }),
				},
			},
		}
	end,
}
```

The shared `shell.lua` environment module:

```lua
return function(args)
	return {
		schemaVersion = 1,
		packages = { args.pkgs.python3, args.pkgs.uv, args.pkgs.bun },
		env = { APP_ENV = "development" },
		tasks = {
			check = { command = { "bun", "test" }, timeoutSeconds = 120 },
		},
		processes = {
			web = {
				command = { "python", "-m", "http.server", "8000", "--bind", "127.0.0.1" },
				ports = { http = { protocol = "tcp", address = "127.0.0.1", fixed = 8000 } },
				ready = {
					http = { url = "http://127.0.0.1:8000/", acceptedStatus = { 200 } },
					intervalSeconds = 1,
					probeTimeoutSeconds = 1,
					timeoutSeconds = 30,
				},
				restart = { policy = "on-failure", maxRetries = 5 },
				shutdown = { signal = "SIGTERM", graceSeconds = 10 },
			},
		},
	}
end
```

Its optional native `project.nix` counterpart:

```nix
{
	schemaVersion = 1;
	inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
	outputs = { self, nixpkgs, ... }:
		let
			system = "x86_64-linux";
			pkgs = nixpkgs.legacyPackages.${system};
		in
		{
			packages.${system}.default = pkgs.hello;
			environments.${system}.default = import ./shell.nix { inherit pkgs; };
		};
}
```

The shared `shell.nix` environment module:

```nix
{ pkgs }:
{
	schemaVersion = 1;
	packages = [ pkgs.python3 pkgs.uv pkgs.bun ];
	env.APP_ENV = "development";
	tasks.check = {
		command = [ "bun" "test" ];
		timeoutSeconds = 120;
	};
	processes.web = {
		command = [ "python" "-m" "http.server" "8000" "--bind" "127.0.0.1" ];
		ports.http = { protocol = "tcp"; address = "127.0.0.1"; fixed = 8000; };
		ready = {
			http = { url = "http://127.0.0.1:8000/"; acceptedStatus = [ 200 ]; };
			intervalSeconds = 1;
			probeTimeoutSeconds = 1;
			timeoutSeconds = 30;
		};
		restart = { policy = "on-failure"; maxRetries = 5; };
		shutdown = { signal = "SIGTERM"; graceSeconds = 10; };
	};
}
```

Both language versions use the same locked Nixpkgs revision. The moving input URL does not permit runtime commands to update it.

The dedicated workflow is `noxi env init`, `noxi env lock`, `noxi env plan`, `noxi env shell`, and an explicit `noxi env up`. Existing flake users can continue their ordinary flake workflow and select its shells or exported native environments without creating a native project manifest. A test environment may select another tool closure and process graph from the same input authority. `noxi env down` stops the selected session and preserves its data.

## Acceptance cases and release evidence

| Case | Required evidence |
|---|---|
| Dedicated and flake routes | Standalone dedicated entry, dedicated entry consuming flake inputs, ordinary devShell-only flake, and flake-exported environment work without native project conversion; equivalent definitions yield the same closure and runtime graph |
| Equivalent Lua and Nix projects | Same locked graph, package/tool derivations, normalized environment definitions, and process dependencies |
| Entry discovery | Dedicated Lua/Nix entries, adjacent flakes, optional native manifests, explicit selectors, multiple dedicated candidates, nearest invalid entry, and unsupported systems have the specified outcomes |
| Shared project inputs | Multiple environments and imported modules use the same resolved package graph; native input outputs and transitive locks resolve explicitly; undeclared fetches and nested input updates fail |
| Lock lifecycle | Frozen operation never writes locks; create/update preserves unrelated inputs; stale revision, invalid follows, interrupted replacement, and offline missing sources fail correctly |
| Ordinary flakes and compatibility | Reference flake and shell commands match options, lock effects, outputs, exit status, hooks, and environment effects with native files present; native selection uses `flake.lock` without conversion; arbitrary hooks are not inferred as managed servers |
| Pure evaluation | No hooks, dependency installation, secrets, live filesystem discovery, or process startup during evaluation; source spans survive mixed-language imports |
| Composition | Ordered lists, explicit replacements, duplicate names, import cycles, unknown fields, unsupported schemas, and missing references yield the declared values or diagnostics |
| Shell and command behavior | Captured tools and variables agree across shell, run, tasks, probes, and servers; hooks run only where declared; host PATH cannot choose another executable |
| Task and server dependencies | Initialization success, task failure, dependency cycles, missing probes, process-start versus readiness, and bounded startup timeouts have distinct results |
| Partial startup | Foreground and detached readiness failures return nonzero, clean up newly started processes by default, preserve prior sessions, and report initialized data |
| Restarts and health loss | Retry limits, backoff, blocked dependents, no implicit task replay, and an explicit user stop behave as specified |
| Shutdown and crash recovery | Ctrl-C, detach, repeated down, client exit, supervisor crash, orphaned descendants, stale sockets, and reused PIDs never misreport or stop unrelated processes |
| Watch and revision changes | Source changes debounce and restart only selected processes; output changes do not loop; manifest/lock edits require explicit replan |
| Ports and workspace isolation | Fixed-port conflicts and bind races fail honestly; allocated values reach dependents; separate Jujutsu workspaces do not share sessions, sockets, or default data paths |
| PostgreSQL data lifecycle | Fresh initialization, existing compatible data, incompatible version, interrupted setup, preserved data on down, and guarded cleanup have inspectable outcomes |
| Secrets and runtime inputs | Missing secrets block affected commands; dotenv is never executed; generated artifacts and structured output contain no secret values; runtime-only inputs are labeled |
| Store lifecycle | Realize through noxi, retain active closures through GC, release session roots after shutdown, and recover failed preparation without leaked roots |
| Logs and client parity | Attributed child output, versioned JSON, observed health, partial failures, reconnect, and any shipped TUI/GUI controls agree with the headless operation result |

Run runtime acceptance cases in disposable workspaces, private state directories, and isolated stores on the initial x86_64-linux target. Record engine and environment-schema versions, source and lock revisions, target system, service versions, exact commands, and actual resulting state. Nix/flake behavior uses the pinned reference oracle; native environment behavior uses this specification. A devenv comparison may inform migration tests but does not establish a devenv compatibility claim.

The first environment release requires the dedicated entry/lock workflow, the standard flake environment route, equivalent Lua/Nix environments, shell and command execution, tasks, built-in supervision, PostgreSQL, and all applicable acceptance cases above. Optional native project manifests have their own extension evidence and are never a prerequisite for standard flakes or dedicated environments. Advertise additional shells, platforms, services, and integrations only after separate qualification. The complete 100% drop-in gate remains required; this extension cannot remove, relax, or replace any reference compatibility requirement.

## Decisions before implementation

- Final dedicated entry and optional native manifest names, selector syntax, schema naming, and version policy.
- Lua helper API and lazy value/module bridge, including equivalent environment imports and diagnostics. Use the shared Lua runtime decision rather than embedding a second runtime.
- Packaging the environment library and service adapters, their pinned revision, and how a project requests a supported schema without relying on whichever library is newest.
- Exact defaults, replacement syntax, probe semantics, native-input selector rules, and reference-preserving `devShells` export before code is written.
- Supervisor and guardian implementation, persistent journal format, socket authentication, allocated-port protocol, log retention, and verified crash recovery on Linux.
- The first PostgreSQL package/data-format matrix and explicit transition procedures. Container execution, extra interactive shells, and remote development sessions require follow-up specifications.
