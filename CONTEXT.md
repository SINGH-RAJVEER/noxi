# nix-rs OS language

The terms below name the product concepts used by the plan and specifications.

## Language

**nix-rs OS**:
The NixOS-derived operating system delivered by this project. It uses nix-rs to evaluate, build, and maintain the installed system.
_Avoid_: GUI distribution, manager package

**nix-rs engine**:
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
A new nix-rs behavior that requires an explicit command, setting, or negotiated protocol capability and preserves reference behavior by default. A native surface's own defaults, such as Lua configuration, belong to the extension the user selects by using that surface.
_Avoid_: changed default

**native surface**:
A nix-rs entry point with no CppNix counterpart: the `nixrs` command, the OS installer, and the NixBox CLI, TUI, and GUI. Compatibility commands are not native surfaces.
_Avoid_: new CLI (when a compatibility command is meant)

**Lua configuration**:
A NixOS or Home Manager configuration written in Lua that evaluates to the same modules as its Nix equivalent. It is the default on native surfaces; compatibility commands never read it.
_Avoid_: Lua package set, Nix replacement

**fleet**:
A set of nix-rs OS machines managed from a common authoritative configuration, with stable machine identities and explicit administrative ownership. A fleet may contain several independent clusters and machines outside any cluster.
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
