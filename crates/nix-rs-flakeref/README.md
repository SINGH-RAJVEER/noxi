# nix-rs-flakeref

Parsing and formatting of Nix flake references for the nix-rs engine. Initial release `0.1.0` is an incomplete preview with unstable APIs.

```toml
[dependencies]
nix-rs-flakeref = "0.1.0"
```

`nix_rs_flakeref::FlakeRef` represents source references; `FlakeRefOutput` holds source metadata. The library does not fetch sources, resolve registries, maintain lock files, or evaluate flakes. Accepting a reference does not establish that CppNix accepts it or that its source can be fetched. Reference-release parsing and formatting compatibility remain incomplete.

The current workspace fixes selected round trips for forge references, transport queries, explicit source types, and relative paths, and adds Mercurial parsing. Constructing the `Mercurial` variant now requires its repository `url`. Full CppNix formatting equivalence remains open; see [verification](../../docs/verification.md#rust-core-completion-work).

Derived from Snix under GPL-3.0-only. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md).
