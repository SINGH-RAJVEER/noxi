# noxi OS language

The terms below name the product concepts used by the plan and specifications.

## Language

**noxi OS**:
The NixOS-derived operating system delivered by this project. It uses noxi to evaluate, build, and maintain the installed system.
_Avoid_: GUI distribution, manager package

**noxi engine**:
The Rust package manager that implements the supported Nix behavior used by the OS and its tools.
_Avoid_: frontend, OS

**reference release**:
The pinned CppNix release, or pinned development commit when no release exists, whose observable behavior defines a compatibility qualification run.
_Avoid_: latest Nix, upstream in general

**compatibility matrix**:
The recorded combinations of reference release, platform, feature settings, Nixpkgs revision, and interfaces that a release has passed.
_Avoid_: supported everywhere

**system generation**:
A built NixOS system closure that can be selected for activation or boot and retained for rollback.
_Avoid_: package version

**extension**:
A new noxi behavior that requires an explicit command, setting, or negotiated protocol capability and preserves reference behavior by default. A native surface's own defaults belong to the extension the user selects by using that surface.
_Avoid_: changed default

**native surface**:
A noxi entry point with no CppNix counterpart: the `noxi` command, the OS installer, and the NixBox CLI, TUI, and GUI. Compatibility commands are not native surfaces.
_Avoid_: new CLI (when a compatibility command is meant)

**project manifest**:
A declaration of a project's inputs and named outputs, including environments alongside packages, applications, and checks. Standard flakes are supported project manifests; native manifests are an optional extension.
_Avoid_: environment file when inputs and outputs are meant

**development environment**:
A declaration of a tool closure, shell settings, tasks, and the servers that run with those tools and settings. It may use the dedicated environment interface or be exported by a project manifest, and is distinct from a running environment session.
_Avoid_: shell when the whole environment is meant, system generation

**environment session**:
A running instance of a development environment at a recorded configuration revision, with its own supervised processes and runtime observations.
_Avoid_: environment definition, system service

**development server**:
A long-running process declared in a development environment, including a local database, application server, or worker. It belongs to that environment's session rather than the installed system or a fleet.
_Avoid_: NixOS service, fleet member

**environment task**:
A finite command declared in a development environment, such as initialization, migration, a build, or a test. Its successful completion can be a prerequisite for another task or a development server.
_Avoid_: development server when a finite command is meant

**fleet**:
A set of noxi OS machines managed from a common authoritative configuration, with stable machine identities and explicit administrative ownership. A fleet may contain several independent clusters and machines outside any cluster.
_Avoid_: cluster when the whole managed machine set is meant

**fleet revision**:
The recorded configuration, machine inventory, and locked inputs that define a fleet's desired state at a particular revision.
_Avoid_: current fleet state when desired state is meant

**fleet plan**:
A reviewed desired-state change tied to a fleet revision, an exact target set, per-machine system generations, and service-aware transition rules. It is not a promise of simultaneous or atomic activation across machines.
_Avoid_: fleet generation, global switch

**service topology**:
A declarative description of a distributed service's member relationships, placement, and availability requirements within a fleet. Quorum membership and primary/replica relationships are different kinds of service topology.
_Avoid_: master-slave setup, machine list when relationships are meant
