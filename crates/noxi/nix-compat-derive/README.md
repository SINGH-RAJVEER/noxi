# noxi-compat-derive

Procedural macros for the Nix wire `NixSerialize` and `NixDeserialize` traits, adapted from Snix's `nix-compat-derive`. Initial release `0.0.1` is an incomplete preview. The Rust library name remains `nix_compat_derive`.

```toml
[dependencies]
nix-compat = { package = "noxi-compat", version = "0.0.1", features = ["daemon"] }
nix-compat-derive = { package = "noxi-compat-derive", version = "0.0.1" }
```

The aliases preserve existing `nix_compat` and `nix_compat_derive` imports and the generated code's default trait paths. Container, variant, and field `#[nix(...)]` attributes control conversions, defaults, and protocol-version fields. See the Rust API documentation for examples.

The macros encode types; they do not implement a daemon or establish protocol compatibility. APIs and compiler diagnostic snapshots may change. Repository builds use a pinned nightly and local Polonius flags; stable packaged builds have not been verified here.

GPL-3.0-only is preserved from the imported source. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md).
