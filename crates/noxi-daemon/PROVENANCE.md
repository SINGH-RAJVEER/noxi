# noxi-daemon provenance

`noxi-daemon` is maintained for noxi at https://github.com/SINGH-RAJVEER/nix-rs under `crates/noxi-daemon/`. It implements the `nix-daemon` executable and a worker-protocol client in Rust. Its wire format, operation numbers, connection handshake and error reporting follow CppNix 2.35.2, revision `2c73b59da29606068c0c98db015dd3a66955525d`, especially `src/libstore/daemon.cc` (`TunnelLogger`, `performOp`), `src/libstore/worker-protocol*.cc` and `src/libstore/remote-store.cc`, which are LGPL-2.1-or-later. The operation and feature tables are checked against `tests/reference-inventory/protocols.txt`, which is read from that revision.


The `ssh` transport (`src/ssh.rs`), the serve protocol client and server (`src/legacy_ssh.rs`, `src/serve_protocol.rs`) and the build operations machines answer (`src/peer.rs`) follow `ssh.cc`, `legacy-ssh-store.cc`, `ssh-store.cc`, `serve-protocol*.cc` and `opServe` in `nix-store.cc` of the same revision.

No CppNix or Snix source file is copied into this crate. Whether any routine is close enough to its C++ counterpart to count as a translation has not been reviewed line by line; [docs/licensing.md](../../docs/licensing.md#open-items) tracks that question together with the same one for `noxi-build`.

[LICENSE](LICENSE) supplies GPLv3 for this crate. The notices of the linked store, build and compatibility crates remain applicable to distributed binaries.
