| [`noxi-system`](https://crates.io/crates/noxi-system) | 0.0.1 | GPL-3.0-only | Native `noxi flake`, `noxi system` and `noxi snapshot` operations |
# Flake and crate distribution path

## Current package boundary

The first installable artifact is the `noxi` preview command. The repository flake exposes `packages.x86_64-linux.noxi`, `packages.x86_64-linux.default`, and an app that runs that command. `cargo install --path crates/noxi-cli --locked` installs the same binary from a checkout. It currently evaluates a pure expression with `noxi eval --expr EXPRESSION` or `noxi eval --file PATH`. The CLI deeply forces the result before printing it, matching the strict evaluator mode used by the initial differential runner. File evaluation reports the source path in diagnostics, but the pure evaluator cannot perform store-backed or impure operations. Its name deliberately differs from `nix`, and it does not own the store or daemon. It cannot replace `nix.package`. Since 2026-11-12 the same binary also offers the native `noxi flake update`, `noxi system` and `noxi snapshot` commands through the `noxi-system` crate, which links `noxi-commands` and therefore the store, build and flake crates; they run the compatibility engine in-process, use the system store the configured way, and have offline tests but no VM qualification. The `noxi-cli` crate is planned for crates.io publication.

A NixOS user may add the preview flake package to `environment.systemPackages`, or a Home Manager user may add it to `home.packages`. Both methods install `noxi` alongside CppNix. Neither changes the daemon or the `nix` command. Host replacement remains gated on the replacement checks below. The qualification-only module is for disposable VM tests.

## Binary cache

The project uses `https://noxios.cachix.org` with public signing key `noxios.cachix.org-1:sDINeeBObCrgzbHv5/VcgY/hkoNHRli9j44F+fbU9LE=`. The root flake advertises this cache through `nixConfig`; accept its settings when prompted, or pass `--accept-flake-config` to use them. The repository's `devenv.nix` also pulls from `noxios`.

To configure the cache for other projects, run `cachix use noxios`, or add the following settings to your NixOS configuration and rebuild:

```nix
nix.settings = {
	extra-substituters = [ "https://noxios.cachix.org" ];
	extra-trusted-public-keys = [ "noxios.cachix.org-1:sDINeeBObCrgzbHv5/VcgY/hkoNHRli9j44F+fbU9LE=" ];
};
```

Run `devenv shell -- just cache-push` to build and upload both `.#noxi` and `.#noxi-replacement` for x86_64-linux. Uploads require a Cachix token with write access to `noxios`, supplied through `CACHIX_AUTH_TOKEN` or the Cachix client's local credential configuration. Keep tokens outside the repository. The recipe stops if either package fails to build. Public downloads require no token, but the Nix daemon must trust the cache's signing key and allow its substituter.

Cachix stores Nix build outputs and their runtime dependencies. It does not cache Cargo's local `target/` directory, and publishing the replacement package does not qualify it for host installation. The Nix package version follows `crates/noxi-cli/Cargo.toml`; changing that crate's version also changes the Nix output names.

## Continuous integration and automatic releases

`.github/workflows/publish.yml` validates the release automation on pushes and pull requests. Every push builds both x86_64-linux package variants and uploads their closures to `noxios` after validation passes. A manual workflow dispatch can retry an upload. Builds use at most two cores and one concurrent Nix derivation to limit memory use. This caches the preview and replacement package; it does not build or deploy a host's NixOS configuration.

After a successful cache upload on the default branch, currently `main`, CI checks every publishable workspace crate against crates.io. Versions already present are skipped. Missing versions of existing crates must increment the highest published stable version by exactly one patch, including when that highest version was yanked. Yanked versions cannot be overwritten. New publishable crates are included at their initial version; mark internal crates `publish = false`. Non-default branches and pull requests do not publish crates.

Push a version bump in the affected crate's `Cargo.toml`, update dependent crates' version requirements when they require that release, regenerate `Cargo.lock`, and update the changelog and release documentation. CI does not edit versions or create bump commits. Cargo packages and verifies the selected crates together with `--locked --all-features`, publishes them in dependency order, and the script confirms each version through the registry API. Published versions are immutable: later source changes need another patch bump. A partially completed release can be retried; already published versions are skipped. Crates.io publication is serialized and never cancels a running publication.

Configure these GitHub Actions repository secrets before enabling uploads:

- `CACHIX_AUTH_TOKEN`: a token with write access to the `noxios` cache. The public signing key alone cannot authorize uploads.
- `CARGO_REGISTRY_TOKEN`: a crates.io token with publish access to every publishable workspace crate, including any newly introduced crates.

The workflow fails explicitly if required credentials are missing or rejected. It needs only read access to repository contents. Dependency actions are pinned to commits. Pull requests exercise the release tests without receiving publishing secrets. The workflow uses JSON flow syntax, which is valid YAML without space indentation.

Run `devenv shell -- just ci-release-check` for workflow linting and release-planner tests. Run `devenv shell -- just release-plan` for a read-only comparison with the live registry. Only the script's explicit `--publish` mode uploads crates; running a plan does not publish anything.

## Additional platforms

Only x86_64-linux currently has flake package outputs and a replacement qualification target. The [additional-platform checklist](../TODO.md#after-the-package-manager-mvp-additional-package-manager-platforms) tracks AArch64 Linux and Intel/Apple Silicon Darwin after the package-manager MVP, independently of later OS hardware support. Preview builds and installation can be qualified separately from the full engine.

ARM Linux needs target-aware build settings, AArch64 sandbox syscall rules, and native build/store/daemon and NixOS/Home Manager recovery evidence. Darwin additionally needs a platform-specific sandbox and execution backend, daemon peer-credential handling, launchd integration, and macOS NAR filename case handling. Each target needs its own pinned compatibility row and packaged-artifact evidence before replacement installation is recommended. Adding a flake system entry or compiling the preview does not establish engine support.

## Crates and release status

The `noxi` 0.0.1 release published `noxi-hash`, `noxi-eval-builtin-macros`, `noxi-compat-derive`, `noxi-flakeref`, `noxi-compat`, `noxi-eval`, and `noxi-store`, and 0.0.2 was their first patch release. The 0.0.3 release of 2026-11-12 moves those seven crates to 0.0.3 and publishes every other crate in the workspace for the first time: `noxi-serde`, `noxi-glue`, `noxi-build`, `noxi-flake`, `noxi-daemon`, `noxi-commands` and `noxi-cli` at 0.0.2, and the new `noxi-system` at 0.0.1.

The crates below are intended for publication on crates.io under the SINGH-RAJVEER account. Upstream owns `nix-compat` and `nix-compat-derive`. This project's format crates are published as `noxi-compat` and `noxi-compat-derive`, retaining Rust library names `nix_compat` and `nix_compat_derive`.

| Crate | Version | License | Contents |
|---|---|---|---|
| [`noxi-eval-builtin-macros`](https://crates.io/crates/noxi-eval-builtin-macros) | 0.0.3 | GPL-3.0-only | Procedural macros for defining builtins, derived from `snix-eval-builtin-macros` |
| [`noxi-eval`](https://crates.io/crates/noxi-eval) | 0.0.4 | GPL-3.0-only | Nix language evaluator, derived from `snix-eval` |
| [`noxi-cli`](https://crates.io/crates/noxi-cli) | 0.0.2 | Apache-2.0 | The `noxi` preview command |
| [`noxi-hash`](https://crates.io/crates/noxi-hash) | 0.0.3 | GPL-3.0-only | Hash and Nix base32 formats |
| [`noxi-flakeref`](https://crates.io/crates/noxi-flakeref) | 0.0.3 | GPL-3.0-only | Flake reference parser and formatter |
| [`noxi-compat`](https://crates.io/crates/noxi-compat) | 0.0.4 | GPL-3.0-only | Nix formats and protocol values |
| [`noxi-compat-derive`](https://crates.io/crates/noxi-compat-derive) | 0.0.3 | GPL-3.0-only | Wire derive macros |
| [`noxi-serde`](https://crates.io/crates/noxi-serde) | 0.0.2 | GPL-3.0-only | Bidirectional serde bridge |
| [`noxi-glue`](https://crates.io/crates/noxi-glue) | 0.0.3 | GPL-3.0-only | Read-only store-aware evaluation on Unix |
| [`noxi-commands`](https://crates.io/crates/noxi-commands) | 0.0.3 | GPL-3.0-only | CppNix-compatible legacy and modern command implementations |
| [`noxi-store`](https://crates.io/crates/noxi-store) | 0.0.4 | GPL-3.0-only | Experimental local store and cache substitution |
| [`noxi-build`](https://crates.io/crates/noxi-build) | 0.0.3 | GPL-3.0-only | Experimental local sandboxed derivation builder |
| [`noxi-daemon`](https://crates.io/crates/noxi-daemon) | 0.0.3 | GPL-3.0-only | Nix worker protocol daemon |
| [`noxi-flake`](https://crates.io/crates/noxi-flake) | 0.0.2 | GPL-3.0-only | Flake graph and operations |
| [`noxi-system`](https://crates.io/crates/noxi-system) | 0.0.1 | GPL-3.0-only | Native `noxi flake`, `noxi system` and `noxi snapshot` operations |

Imported code retains Snix's GPL-3.0 license, with package-local text and provenance; bundled LGPL Nix material has its own notices and license text. The CLI's own source is Apache-2.0, but the `noxi` binary links `noxi-eval`, so a distributed binary must meet GPL-3.0 terms. Published versions are permanent: a version can be yanked but its source stays public. Record each release in [CHANGELOG.md](../CHANGELOG.md), bump changed crates and versioned path dependencies, package and verify the archives, then publish the selected packages in dependency order. Reuse an unchanged dependency's existing registry release rather than attempting to upload the same version again. These are preview crates: their APIs are not stable, and publication does not close the drop-in release gate.

Each bump of an existing crate must increment its patch version by exactly one, or `0.0.1`. Keep its major and minor versions unchanged and do not skip patch numbers. New crates use their selected initial version. Leave unchanged crates at their existing releases, and raise a consumer's dependency minimum when it needs functionality introduced by a new patch. Historical release versions remain unchanged.

The `noxi-hash` crate contains hash and Nix base32 code extracted from the compatibility library and retains GPL-3.0-only. The library reexports its former paths, so the split does not require downstream source changes.

The `noxi-flakeref` crate contains extracted flake reference parsing and formatting under the same license. The compatibility library's `flakeref` feature still exposes `nix_compat::flakeref`.

### Release scope and verification

Nine new versions were published on 2026-10-01 after all-feature package verification, archive/license/provenance inspection, and the workspace test suite. The registry API confirmed all ten versions from that release existed, were not yanked, and belonged to SINGH-RAJVEER. The unchanged builtin-macro crate remains at its existing 0.0.1 release. Internal compatibility and derive test crates remain private. Store and build publication was excluded from that release, not permanently prohibited.

Six versions were published on 2026-10-02: evaluator 0.2.1, compatibility 0.1.1, glue 0.1.1, commands 0.1.1, and first releases of store and build at 0.1.0. The crates.io API confirms all six are present, not yanked, and match the local package SHA-256 checksums below. Unchanged CLI, hash, flake-reference, Serde, and macro crates reuse their published releases. Glue and commands require the new evaluator methods, and build requires compatibility 0.1.1's downstream placeholder and corrected feature gate.

The upload used a separate Jujutsu release workspace frozen from checkpoint `f90a0278`, with package-notice and formatting corrections. All-feature package verification and license/provenance checks passed, as did 28 focused local-store, build, substitution, and derivation-option comparisons, with two optional tests ignored. Later garbage-collection and selective-input-output changes remain under `Unreleased`; continued development was not silently included in these immutable registry versions. [Verification](verification.md#crate-release-checks)

| Package | Uploaded archive SHA-256 |
|---|---|
| `noxi-compat` 0.1.1 | `6bec113dfb559d98155af332bb249cd5f116bc041a2f2931eee397a47f2dd0bf` |
| `noxi-eval` 0.2.1 | `1945e5afc33d1f99713308c8f31c13537501b47e5b03fcf786c9541ab9d046d2` |
| `noxi-store` 0.1.0 | `cfb89e07f166bbde39531ebe5deff622c4b4424d6cd14518832faf9cdc1f0444` |
| `noxi-glue` 0.1.1 | `bc3b7ec5dc9e7ca6fa26692e7fb849f2be809b8e9994f3b87ba79668324df1a4` |
| `noxi-build` 0.1.0 | `7900ac39e5046cc654d97f78137425252832ee0578979d344abc6e31f6ceea97` |
| `noxi-commands` 0.1.1 | `b51e5aec8aefa2bea0376ae006f130e017f4277ca677257b8312bff39b251982` |

The new store and build crates are experimental library previews for isolated temporary stores on the tested x86_64-linux target. Store includes local metadata/NAR handling and selected local/HTTP(S) cache substitution. Build includes the local sandbox path, output checks, structured attributes, logs, and configured substitution. Neither supplies a complete command/daemon replacement or qualifies live-store maintenance, sandbox security, NixOS rebuilds, remote scheduling, or other architectures.

The occupied compatibility package names are replaced without changing internal import paths. Consumers can use `nix-compat = { package = "noxi-compat", version = "0.1.1" }` and the corresponding derive-macro alias. Package archives carry their license text and source provenance. All APIs are previews; registry publication does not close the drop-in release gate.

`cargo install noxi-commands` installs `nix-instantiate`, unlike the non-conflicting `noxi` preview. Use `cargo install noxi-commands --root /path/to/isolated-prefix` and call that executable by absolute path; do not replace or shadow the system's CppNix during development.

## Replacement milestones

| Milestone | Work required | Evidence before advancing |
|---|---|---|
| Installable preview | Reproducible flake build, local Cargo installation, versioned binary, non-conflicting command name | Build the flake package and run the installed binary; test its actual supported command and failure status |
| Nix command compatibility | Implement the `nix` command family and legacy executables, flags, output formats, environment behavior, `nix.conf`, feature gates, profiles, registries, and exit codes | Differential command inventory against the pinned CppNix release, including scripts invoked by Nixpkgs and Home Manager |
| Evaluation and flakes | Complete lazy Nix evaluation, builtins, string contexts, imports, fetchers, source filtering, flake references, `flake.lock`, registry resolution, output selection, and import-from-derivation | Compare fixed Nixpkgs and Home Manager revisions, resulting values, derivation graphs, errors, and flake lock files with the reference |
| Store and builds | Implement existing-store metadata, NAR and narinfo exchange, signatures, substitution, sandboxed local and remote builds, roots, garbage collection, profiles, and crash recovery | Exercise copied stores, concurrent clients, interrupted writes, cache trust, and return to CppNix on isolated test systems |
| Daemon and system integration | Implement worker protocol negotiation and operations, multi-user authorization, `nix-daemon`, service/socket behavior, and every executable NixOS expects from `nix.package` | Test reference and noxi clients against each supported daemon pairing; rebuild and roll back an existing NixOS VM using noxi as `nix.package` |
| Home Manager | Support standalone `home-manager build` and `switch`, its flake mode, and Home Manager as a NixOS module through the same engine | Compare built user profiles, activation, generations, and failure recovery on a pinned Home Manager revision |
| Distribution and optional crate release | Publish the complete flake package; separately publish additional versioned Rust crates only if selected, with resolvable dependencies, source licenses, and documented API stability | Build from a clean clone, `cargo package` each selected crate, verify clean installation and upgrade, and run the exact packaged release-profile artifacts through the compatibility matrix before publication |

These milestones are qualification areas, not a strict implementation order. Follow the [plan's work sequence](../PLAN.md#work-sequence): connect minimal command, evaluation, and store operations first, then extend them through builds and the daemon before completing all workflows. Discover public C and C++ library consumers during baseline work so ABI and packaging requirements are known before the replacement architecture is fixed. Additional Rust crate publication is optional for the binary release; it does not require a stable Rust API promise.

The matrix in [compatibility](compatibility-matrix.md) remains the release gate. A command shim that delegates to CppNix, or a binary placed earlier on `PATH`, does not establish engine compatibility. Provisional replacement packages may be wired to `nix.package` in disposable qualification VM overlays to obtain rebuild and rollback evidence, with one store-writing daemon at a time. Before publishing a replacement module or recommending host deployment, the package must provide the expected executable set and daemon, preserve the existing store, and pass a tested rollback to CppNix.

## Installation path

For the preview package, a NixOS flake can include the noxi input and add `inputs.noxi.packages.${pkgs.system}.noxi` to `environment.systemPackages`. A Home Manager flake can add the same package to `home.packages`. Run `noxi --version` and `noxi eval --expr '1 + 2'` to inspect the preview. The system's existing `nix` and `nix-daemon` stay in place.

The replacement stage will package noxi for the NixOS `nix.package` option and verify the `nix-daemon` service wiring in the selected Nixpkgs revision. Home Manager may run standalone or inside NixOS, so both paths need independent qualification. The target CppNix release, Nixpkgs revision, Home Manager revision, platform, and feature settings must be pinned together before any drop-in claim. The current copied fixture tree is the CppNix 2.35.2 release. [Test snapshot](verification.md#test-snapshot)

The NixOS package option and daemon wiring are defined in [Nixpkgs' Nix module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/config/nix.nix) and [daemon module](https://github.com/NixOS/nixpkgs/blob/master/nixos/modules/services/system/nix-daemon.nix). Home Manager documents its [standalone and NixOS-module installation paths](https://nix-community.github.io/home-manager/installation.html) and [flake update and switch commands](https://nix-community.github.io/home-manager/usage/updating.html). These upstream interfaces are the integration targets, not evidence that noxi supports them today.

## Local engine commands under development

The current source workspace also builds `nix-instantiate`, `nix-build`, `nix-store` and a standalone `nix-daemon`. Build them with `cargo build -p noxi-commands -p noxi-daemon` inside the pinned development shell, then invoke `target/debug/COMMAND` by absolute path. Select an existing diverted directory with `--store local?root=ABSOLUTE_ROOT`. For the daemon, `NIX_DAEMON_SOCKET_PATH` selects an absolute Unix socket; by default it lives under the diverted state directory. Client commands in this workspace support local diverted stores; they do not yet connect to the Rust daemon themselves.

`nix-build --no-out-link --expr EXPRESSION` evaluates and realizes selected derivation outputs. Without `--no-out-link`, it creates permanent result roots. `nix-store` provides query flags for hash, size, references, referrers, deriver, outputs and requisites, plus local realization, dump, verify, optimise, GC and deletion. This is a subset of the legacy interface. The flake's default package still installs only the separate `noxi` preview. The provisional VM package and qualification module below expose the engine commands for testing. The new functionality is unreleased.

## Provisional VM replacement package

`packages.x86_64-linux.noxi-replacement` builds the preview, commands and daemon crates together from the locked source and nightly toolchain. The default package remains the non-conflicting preview. `checks.x86_64-linux.replacement` builds the replacement, and `replacement-layout` compares its executable set with the Rust build and CppNix and exercises the packaged evaluators. After merging `mvp/integration`, the package installs eleven real executables under `libexec/noxi`, exposed through `bin` symlinks: `noxi`, `nix`, `nix-build`, `nix-instantiate`, `nix-store`, `nix-env`, `nix-collect-garbage`, `nix-daemon`, `nix-shell`, `nix-copy-closure` and `nix-hash`. Only `nix-channel` and `nix-prefetch-url` remain explicit exit-127 placeholders. They do not delegate to CppNix.

Since the 2026-11-09 integration (`b6a0a106` on top of merge `04abbfd3`), the package installs thirteen real executables: the eleven above plus the native `nix-channel` and `nix-prefetch-url` from `noxi-commands`. No reference command name is a placeholder any more; the placeholder mechanism is kept for future gaps. `nix-channel --update`, `--rollback` and `--list-generations` still fail explicitly inside the executable, so installing it does not qualify channel workflows. The layout check runs both commands from the package, requires a `file://` prefetch to print CppNix 2.35.2's hash and store path, and checks an empty `nix-channel --list`.

`overlays.qualification` supplies the package and `nixosModules.qualification` sets `nix.package` and the single `nix-daemon` service when `noxi.qualification.enable` is true. Import them only in disposable VM test overlays. The module retains the reference daemon arguments and socket contract so unsupported integration remains observable. Until 2026-11-09 it disabled build-time `nix.conf` validation while configuration inspection was unqualified. It now keeps Nixpkgs' default `nix.checkConfig`, so every qualification generation's `nix.conf` is validated by the candidate's own `nix config show` while the system builds; the guest also probes `nix config show --json` directly. No host migration or qualified replacement is provided.

The package installs `THIRD_PARTY.md` and the workspace `LICENSE`, `COPYING`, `NOTICE` and `PROVENANCE.md` files that exist under `share/doc/noxi`, preserving their relative paths. The preview notice was imported from `mvp/license-audit`; it covers the preview dependency graph only. The larger commands/daemon dependency notices and the audit's open licensing decisions remain release blockers. A root `LICENSE` or `NOTICE` is installed if present; neither is invented by the package builder. See [licensing](licensing.md).

The current reports use the pinned `tests/qualification` inputs. `just qualification-report MODE` runs `nixos`, `home-manager`, `lifecycle` or `daemon` and preserves each command, exit status, stdout and stderr. `just qualification-gate MODE` rejects any failed or blocked step. The lifecycle mode replaces the older package-profile `distribution` fixture with system installation, upgrade, deliberately failed upgrade, rollback and removal. Both candidate package variants still contain the same engine binaries, so this fixture cannot qualify an upgrade between engine releases. The saved reference closure provides recovery inside the VM.

The [integrated qualification at `44d9bd7`](verification.md#integrated-vm-qualification-at-44d9bd7) passed clean-clone preview/replacement/layout builds, replacement and binary-package reproducibility checks and byte comparison of all eleven installed engine executables. All three strict VM gates failed. The real modern executable now runs, but flake builds reject `--no-write-lock-file`, configuration inspection is unsupported and daemon startup still rejects `--daemon`. Every mode completed return to CppNix, reboot, daemon identity, full store verification and existing-store reuse. The [dated manifest and reports](../tests/qualification/results/2026-10-26-44d9bd7/manifest.json) retain exact errors and limitations alongside the older results. The [prioritized engine gaps](verification.md#prioritized-engine-gaps-blocking-integrated-replacement) identify the next implementation work.

The [2026-11-09 rounds](verification.md#disposable-vm-qualification-2026-11-09) supersede those 44d9bd7 failures for current main without erasing them. With the integrated candidate `/nix/store/zkg76l4dbcxbfa04zw6xl92hk8spnpyp-noxi-replacement-provisional-0.1.1` from `b6a0a106`, the daemon starts and every classic NixOS, Home Manager and lifecycle activation succeeds; the lifecycle strict gate passes. The nixos and home-manager strict gates still fail on flake installable parsing, and both package variants still share identical binaries. This remains a disposable-VM package with no host installation instructions.

## Recorded package and VM milestone c60e3e3

The later [8dbc158 qualification](verification.md#recorded-vm-qualification-at-8dbc158) records a clean-clone build, byte-identical rebuilt engine executables, real offline classic/flake controls, system lifecycle probes and successful CppNix recovery/store verification in all three modes. Its strict gates fail. These results describe the pre-integration executable set; subsequent integration must rerun the affected package and VM checks.

The [qualification evidence](verification.md#recorded-vm-qualification-at-c60e3e3) records the committed provisional package and VM infrastructure at `c60e3e3`. A clean Git clone built the preview, replacement package and executable-layout check. The tested replacement artifact is `/nix/store/9smxwm90h4pm4zkwcjscxb5yv6saam3v-nixrs-replacement-provisional-0.1.1`; its report includes executable SHA-256 hashes. Later concurrent packaging and qualification refactors were preserved and are not covered by those runs.

This milestone includes daemon service/socket units and tmpfiles rules, the qualification-only overlay/module, private bootable test disks and `just` recipes. The VM module supplies target-version metadata only to Nixpkgs' configuration formatter and skips its modern-CLI configuration check so the guest can expose unsupported commands. These test accommodations do not qualify the package for host deployment.

All three strict VM gates failed. The Rust daemon rejects `--daemon`; the missing modern `nix` command blocks every flake rebuild action and flake-mode Home Manager. Classic Home Manager reaches evaluation but fails to register derivation inputs through the unavailable daemon. System reboot into a noxi-built generation, a second system change and system rollback remain blocked. Return to CppNix after failed prebuilt activation passed in the NixOS fixture, including full store-content verification.

The distribution fixture passed profile installation, selection of an upgrade fixture, preservation after a failed upgrade, rollback and an empty-profile removal assertion. Both generations use the same engine binaries. The removal selector matched no manifest entries, so this does not qualify a real release-to-release upgrade or manifest-based uninstall. Home Manager and distribution full-store checks reached the hash phase but timed out at 180 seconds; they remain incomplete. The [manifest and reports](../tests/qualification/results/2026-10-25-c60e3e3/manifest.json) preserve every exit status and these limits.

Reproduce the milestone in an isolated checkout at `c60e3e3`, using `devenv shell -- just replacement-package` and the `qualification-nixos`, `qualification-home-manager` and `qualification-distribution` recipes. `qualification-report MODE` retains observations despite engine gaps; `qualification-gate MODE` rejects failed or blocked probes. The full replacement and publication gates remain open. No host installation procedure, release publication or push was performed.

## Push-only version update, 2026-11-13

This source update bumps `noxi-build` to 0.0.3, `noxi-commands` to 0.0.3, `noxi-compat` to 0.0.4, `noxi-compat-tests` to 0.0.3, `noxi-daemon` to 0.0.3, `noxi-eval` to 0.0.4, `noxi-glue` to 0.0.3, `noxi-store` to 0.0.4. The test-only `noxi-compat-tests` remains unpublished (`publish = false`); its inherited version is overridden explicitly, as is `noxi-build`, leaving the workspace version and unrelated packages unchanged. Internal dependency requirements follow the changed packages. This update is committed and pushed to main only; no crates.io publication, release tags or host activation is performed. Frozen performance artifacts retain their recorded scope: these version changes produce a new source hash and do not qualify the new source for performance.


## Flake output interface

The root flake targets `x86_64-linux`. `nix build` and `nix run` select the preview; `nix run .#noxi` selects it explicitly. Every compatibility executable listed by the replacement package also has a named app, for example `nix run .#nix -- --version`. These apps run the provisional engine and retain the qualification limitations above.

`nix develop` provides the development tools from `devenv.nix`; `devenv shell` remains the repository workflow. `nix fmt` uses Nixpkgs' Nix formatter. `overlays.default` adds the preview as `pkgs.noxi`; `overlays.qualification` and `nixosModules.qualification` retain the disposable VM replacement interface.

Existing packages include the preview, replacement, pinned reference, static test builder and four qualification reports. Existing checks include both packages, executable layout, reference VM and four strict qualification gates. The NixOS and Home Manager configurations remain qualification fixtures. Evaluating these outputs does not establish that their builds or VM gates pass, and does not activate a host.
