set indentation := "\t"

# Show available recipes.
default:
	@just --list

# Format the Rust workspace.
fmt:
	cargo fmt --all

# Check Rust formatting without changing files.
fmt-check:
	cargo fmt --all -- --check

# Type-check every workspace crate and feature.
check:
	cargo check --workspace --all-features

# Run every workspace test, including compiler-diagnostic snapshots, then the doctests nextest skips.
test:
	cargo nextest run --workspace --all-features --no-fail-fast
	cargo test --doc --workspace --all-features --no-fail-fast

# Run the workspace tests without the toolchain-sensitive UI snapshot crate.
test-core:
	cargo nextest run --workspace --all-features --no-fail-fast --exclude nix-compat-derive-tests

# Run compatibility checks against the copied CppNix fixtures.
test-fixtures:
	cargo nextest run -p noxi-compat-tests --tests

# Check every shipped dependency license against the accepted set in deny.toml.
licenses:
	cargo deny check licenses

# Regenerate THIRD_PARTY.md for the noxi preview binary from the locked dependency graph, then append the LGPL-2.1 text of the bundled CppNix helper, which cargo-about does not see.
third-party:
	cargo about generate --locked --offline --fail -m crates/noxi-cli/Cargo.toml -c about.toml third-party.hbs -o THIRD_PARTY.md
	{ printf '\n## GNU Lesser General Public License 2.1 (bundled CppNix helper)\n\nThe CppNix helper `imported-drv-to-derivation.nix` is licensed LGPL-2.1-or-later; the text below is the LGPL-2.1 from its reference copy.\n\n~~~text\n'; cat crates/noxi/eval/COPYING; printf '~~~\n'; } >> THIRD_PARTY.md

# Compare the preview evaluator with an explicitly selected CppNix executable.
eval-compare reference report="/tmp/noxi-eval-comparison.json":
	cargo build -p noxi-cli --bin noxi
	cargo build -p noxi-compat-tests --bin differential-eval
	target/debug/differential-eval "{{reference}}" target/debug/noxi "{{report}}"

# Check nix-instantiate's Nixpkgs derivation and output paths against those recorded from the reference.
nixpkgs-paths:
	NOXI_NIXPKGS="$(nix eval --raw --impure --expr '(builtins.getFlake "path:{{justfile_directory()}}").inputs.nixpkgs.outPath')" \
		cargo nextest run -p noxi-commands --test instantiate --run-ignored only -E 'test(matches_reference_nixpkgs_paths)'

# Check the qualification NixOS system, its VM and Home Manager generations, and a standalone Home Manager generation against the derivation paths recorded from the reference.
nixos-paths:
	NOXI_QUALIFICATION_NIXPKGS="$(nix eval --raw --impure --expr '(builtins.getFlake "path:{{justfile_directory()}}/tests/qualification").inputs.nixpkgs.outPath')" \
	NOXI_QUALIFICATION_HOME_MANAGER="$(nix eval --raw --impure --expr '(builtins.getFlake "path:{{justfile_directory()}}/tests/qualification").inputs.home-manager.outPath')" \
		cargo nextest run -p noxi-commands --test instantiate --run-ignored only -E 'test(matches_reference_nixos_paths)'

# Build the pinned CppNix reference release (substituted from cache.nixos.org) at target/nix-reference, and the static busybox the build comparisons use at target/test-busybox.
reference:
	nix build .#nix-reference --out-link target/nix-reference
	nix build .#test-busybox --out-link target/test-busybox

# Build portable release executables before collecting timings.
bench-build:
	cargo build --release -p noxi-cli -p noxi-commands -p noxi-daemon

# Validate benchmark oracles, process accounting, and timeout cleanup.
bench-test:
	uv run --no-project --no-config python -m unittest discover -s benchmarks -p 'test_*.py' -v

# List catalog cases and available subset adapters.
bench-list:
	uv run --no-project --no-config python benchmarks/run.py --list

# Run selected cases; builds and reference fixture preparation are outside timing.
bench reference="target/nix-reference/bin" candidate="target/release" busybox="target/test-busybox" *args:
	uv run --no-project --no-config python benchmarks/run.py --reference-bin "{{reference}}" --candidate-bin "{{candidate}}" --busybox "{{busybox}}" {{args}}

# Compare the preview evaluator with the pinned CppNix reference release.
eval-compare-reference report="target/eval-comparison-reference.json": reference
	just eval-compare target/nix-reference/bin/nix-instantiate "{{report}}"

# Record the pinned reference's executables, settings, features, commands, builtins, protocols and C API in tests/reference-inventory.
reference-inventory: reference
	bash scripts/reference-inventory.sh

# Boot the pinned CppNix reference VM once and keep its disk as the read-only rollback base in target/qualification.
reference-vm-snapshot:
	bash scripts/reference-vm-snapshot.sh snapshot

# Create a disposable copy-on-write overlay of the reference VM base for one test run.
reference-vm-overlay name:
	bash scripts/reference-vm-snapshot.sh overlay "{{name}}"

# Boot a temporary overlay of the reference VM base and check the base is unchanged.
reference-vm-verify:
	bash scripts/reference-vm-snapshot.sh verify

# Boot the pinned reference system, run its rebuild, Home Manager and garbage-collection scenarios, and record every Nix command they run in target/qualification/trace.
reference-trace:
	nix build --out-link target/qualification/trace --print-build-logs "path:{{justfile_directory()}}/tests/qualification#checks.x86_64-linux.trace"

# --no-link keeps the check's output out of ./result.

# Boot a NixOS machine running the pinned reference and check the packaged noxi against it.
vm-test:
	nix build --no-link --print-build-logs ".#checks.$(nix eval --raw --impure --expr builtins.currentSystem).vm"

# Open the VM test's machine in the interactive test driver.
vm:
	nix run ".#checks.$(nix eval --raw --impure --expr builtins.currentSystem).vm.driverInteractive"

# Fetch the latest official stable Nix source and tests; record its exact revision.
nix-tests-update:
	bash scripts/update-nix-tests.sh latest

# Restore the exact official Nix source revision recorded in the lock file.
nix-tests-sync:
	bash scripts/update-nix-tests.sh sync

# Replace tests/nix with the test paths of the checked-out official commit and rewrite its lock file.
nix-snapshot-refresh:
	bash scripts/update-nix-tests.sh snapshot

# Check the fixed test snapshot against its recorded upstream objects; pass "upstream" to fetch the commit too.
nix-snapshot-verify mode="local":
	bash scripts/verify-nix-snapshot.sh "{{mode}}"

# Run the complete development check in the pinned devenv shell.
ci: fmt-check nix-snapshot-verify check test

# Run the complete development check and then the VM test, which boots a machine.
ci-vm: ci vm-test

# Build the provisional replacement package (all engine executables, the reference's names, daemon units) and check its layout.
replacement-package:
	mkdir -p target/qualification
	nix build --out-link target/qualification/replacement .#checks.x86_64-linux.replacement
	nix build --no-link .#checks.x86_64-linux.replacement-layout

# Boot a disposable CppNix VM, swap it to noxi, run one scenario (nixos, home-manager, lifecycle or daemon), return to CppNix, and keep every step's result in target/qualification/MODE-report. A report build is not a gate pass.
qualification-report mode:
	mkdir -p target/qualification
	nix build --out-link "target/qualification/{{mode}}-report" --print-build-logs ".#qualification-{{mode}}-report"
	cat "target/qualification/{{mode}}-report/steps.txt"

# Strict gate: fails unless every step of the scenario passed.
qualification-gate mode: (qualification-report mode)
	nix build --no-link --print-build-logs ".#checks.x86_64-linux.qualification-{{mode}}"

# nixos-rebuild build, dry-activate, test, switch and boot with noxi, a reboot, a second change, a rollback and the return to CppNix (TODO section 7).
qualification-nixos: (qualification-gate "nixos")

# Standalone Home Manager build and switch in classic and flake mode, and Home Manager as a NixOS module, with noxi (TODO section 7).
qualification-home-manager: (qualification-gate "home-manager")

# Install, upgrade, failed upgrade, rollback and removal of noxi as the system's Nix (TODO section 8).
qualification-lifecycle: (qualification-gate "lifecycle")

# Build users, concurrent and leftover builder processes, and the daemon's allowed-users, trusted-users, signature and repair boundaries, each against a CppNix control (TODO section 5).
qualification-daemon: (qualification-gate "daemon")
