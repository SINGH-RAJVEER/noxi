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
The pinned CppNix release whose observable behavior defines a compatibility qualification run.
_Avoid_: latest Nix, upstream in general

**compatibility matrix**:
The recorded combinations of reference release, platform, feature settings, Nixpkgs revision, and interfaces that a release has passed.
_Avoid_: supported everywhere

**system generation**:
A built NixOS system closure that can be selected for activation or boot and retained for rollback.
_Avoid_: package version

**extension**:
A new nix-rs behavior that requires an explicit command, setting, or negotiated protocol capability and preserves reference behavior by default.
_Avoid_: changed default
