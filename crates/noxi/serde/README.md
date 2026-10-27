# noxi-serde

Serde conversion between Rust types and `noxi_eval::Value`, adapted from Snix's `snix-serde`. Initial release `0.0.1` is an incomplete preview with unstable APIs.

```toml
[dependencies]
noxi-serde = "0.0.1"
```

`from_str` evaluates a Nix expression in pure, strict mode and deserialises its result. `from_str_with_config` lets the caller configure evaluation. `from_value` deserialises an existing value; `to_value` and `to_string` serialise Rust values to evaluator values and Nix expression text.

This is configuration-data conversion, not a Nixpkgs evaluator or reference store JSON codec. Functions and other evaluator-only values cannot be deserialised as ordinary data; Serde byte values are unsupported, and Nix integers are signed 64-bit. It inherits the evaluator's incomplete language behavior. Enabling I/O through caller configuration does not add builds or fetching.

The imported source retains GPL-3.0-only. See [LICENSE](LICENSE) and [PROVENANCE.md](PROVENANCE.md).
