# nix-rs-build

Experimental local derivation builds for the nix-rs engine, following CppNix 2.35.2. Release `0.1.0` has unstable APIs and narrow reference-build evidence on x86_64-linux. It is not a qualified replacement build engine or a security guarantee for untrusted workloads.

```toml
[dependencies]
nix-rs-build = "0.1.0"
```

`local::LocalBuilder` realises derivations through `nix-rs-store`, prepares the input closure and environment, runs the Linux namespace/chroot sandbox, and checks and registers outputs. It supports structured attributes, reference/size checks, fixed-output hash verification, and compressed or plain build logs. `with_substituters` configures caches to try before building missing outputs and inputs. `options` implements the supported reference derivation-option codecs and feature gates.

Use isolated temporary stores. The sandbox requires working Linux user/mount/PID and related namespace support; syscall filtering currently targets x86 ABIs. Other platforms and architectures are not qualified. The crate has no command entry point, remote-build scheduler, unsandboxed build path, `builtin:` builders, floating content-addressed builds, or complete repair/check mode. Full substitution/fallback semantics, store recovery, and daemon operation remain incomplete.

Repository tests compare selected builds, outputs, metadata, failure diagnostics, structured attributes, and logs against the pinned reference. Those checks do not establish full Nixpkgs builds, NixOS rebuilds, sandbox security, or CppNix compatibility.

GPL-3.0-only. See [LICENSE](LICENSE), [COPYING](COPYING), and [PROVENANCE.md](PROVENANCE.md) for package and reference-source notices.
