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

# Run every workspace test, including compiler-diagnostic snapshots.
test:
	cargo test --workspace --all-features --no-fail-fast

# Run the workspace tests without the toolchain-sensitive UI snapshot crate.
test-core:
	cargo test --workspace --all-features --no-fail-fast --exclude nix-compat-derive-tests

# Run compatibility checks against the copied CppNix fixtures.
test-fixtures:
	cargo test -p nixrs-compat-tests --tests

# Compare the preview evaluator with an explicitly selected CppNix executable.
eval-compare reference report="/tmp/nix-rs-eval-comparison.json":
	cargo build -p nix-rs-cli --bin nixrs
	cargo build -p nixrs-compat-tests --bin differential-eval
	target/debug/differential-eval "{{reference}}" target/debug/nixrs "{{report}}"

# Check nix-instantiate's Nixpkgs derivation and output paths against those recorded from the reference.
nixpkgs-paths:
	NIXRS_NIXPKGS="$(nix eval --raw --impure --expr '(builtins.getFlake "path:{{justfile_directory()}}").inputs.nixpkgs.outPath')" \
		cargo test -p nix-rs-commands --test instantiate -- --ignored

# Build the pinned CppNix reference release (substituted from cache.nixos.org) at target/nix-reference.
reference:
	nix build .#nix-reference --out-link target/nix-reference

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

# --no-link keeps the check's output out of ./result.

# Boot a NixOS machine running the pinned reference and check the packaged nixrs against it.
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
