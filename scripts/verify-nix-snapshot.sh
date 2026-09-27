#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
snapshot_prefix="tests/upstream/nix"
lock_file="${root_dir}/tests/upstream/nix-snapshot.lock"
mode="${1:-local}"

usage() {
	cat <<'EOF'
Usage: scripts/verify-nix-snapshot.sh [local|upstream]

local     Check that the working-tree snapshot matches the object IDs in the lock file.
upstream  Also fetch the recorded upstream commit and check that it has the same objects.
EOF
}

case "${mode}" in
	local | upstream) ;;
	--help | -h)
		usage
		exit 0
		;;
	*)
		usage >&2
		exit 2
		;;
esac

repository="$(sed -n 's/^repository=//p' "${lock_file}")"
commit="$(sed -n 's/^commit=//p' "${lock_file}")"
if [[ -z "${repository}" || ! "${commit}" =~ ^[0-9a-f]{40}$ ]]; then
	printf 'Invalid snapshot lock file: %s\n' "${lock_file}" >&2
	exit 1
fi

declare -A expected=()
while read -r object path; do
	expected["${path}"]="${object}"
done < <(sed -n 's/^object=//p' "${lock_file}")
if [[ "${#expected[@]}" -eq 0 ]]; then
	printf 'Snapshot lock file records no objects: %s\n' "${lock_file}" >&2
	exit 1
fi

tmp_dir="$(mktemp -d)"
trap 'rm -rf "${tmp_dir}"' EXIT

# Hash the working tree with a private index so neither the index nor the working copy changes.
export GIT_INDEX_FILE="${tmp_dir}/index"
git -C "${root_dir}" read-tree --empty
git -C "${root_dir}" add --all --force -- "${snapshot_prefix}"
snapshot_tree="$(git -C "${root_dir}" write-tree --prefix="${snapshot_prefix}/")"
unset GIT_INDEX_FILE

failures=0

# Every top-level entry and every entry directly under src/ and tests/ must be recorded.
for parent in "" src/ tests/; do
	while IFS= read -r name; do
		path="${parent}${name}"
		if [[ -z "${expected[${path}]+set}" && ! ( "${path}" == src || "${path}" == tests ) ]]; then
			printf 'Unrecorded snapshot path: %s\n' "${path}" >&2
			failures=$((failures + 1))
		fi
	done < <(git -C "${root_dir}" ls-tree --name-only "${snapshot_tree}:${parent}")
done

for path in "${!expected[@]}"; do
	actual="$(git -C "${root_dir}" rev-parse --verify --quiet "${snapshot_tree}:${path}" || true)"
	if [[ "${actual}" != "${expected[${path}]}" ]]; then
		printf 'Snapshot differs from lock at %s: %s\n' "${path}" "${actual:-missing}" >&2
		failures=$((failures + 1))
	fi
done

if [[ "${mode}" == "upstream" ]]; then
	git -C "${tmp_dir}" init --quiet --bare upstream.git
	git -C "${tmp_dir}/upstream.git" fetch --quiet --depth=1 --filter=blob:none "${repository}" "${commit}"
	for path in "${!expected[@]}"; do
		actual="$(git -C "${tmp_dir}/upstream.git" rev-parse --verify --quiet "${commit}:${path}" || true)"
		if [[ "${actual}" != "${expected[${path}]}" ]]; then
			printf 'Upstream %s differs from lock at %s: %s\n' "${commit}" "${path}" "${actual:-missing}" >&2
			failures=$((failures + 1))
		fi
	done
fi

if [[ "${failures}" -ne 0 ]]; then
	printf 'Snapshot verification failed with %d difference(s)\n' "${failures}" >&2
	exit 1
fi

printf 'Snapshot matches %s paths recorded for %s (%s)\n' "${#expected[@]}" "${commit}" "${mode}"
