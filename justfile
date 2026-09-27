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

# Fetch the latest official stable Nix source and tests; record its exact revision.
nix-tests-update:
	bash scripts/update-nix-tests.sh latest

# Restore the exact official Nix source revision recorded in the lock file.
nix-tests-sync:
	bash scripts/update-nix-tests.sh sync

# Check the fixed test snapshot against its recorded upstream objects; pass "upstream" to fetch the commit too.
nix-snapshot-verify mode="local":
	bash scripts/verify-nix-snapshot.sh "{{mode}}"

# Run the complete development check in the pinned devenv shell.
ci: fmt-check nix-snapshot-verify check test
