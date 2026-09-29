#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
checkout_dir="${root_dir}/tests/nix-official"
lock_file="${root_dir}/tests/nix-official.lock"
snapshot_dir="${root_dir}/tests/nix"
snapshot_lock="${root_dir}/tests/nix-snapshot.lock"
remote_url="https://github.com/NixOS/nix.git"
selection="${1:-latest}"

usage() {
	cat <<'EOF'
Usage: scripts/update-nix-tests.sh [latest|master|sync|snapshot|TAG]

latest    Fetch the newest numbered stable Nix release tag and update the lock file.
master    Fetch the current upstream development branch and update the lock file.
sync      Check out the exact commit already recorded in the lock file.
snapshot  Replace tests/nix with the test paths of the checked-out commit and rewrite its lock file.
TAG       Fetch a specific numbered Nix release tag and update the lock file.
EOF
}

# Copy the test paths of the checked-out commit into tests/nix and record their object IDs.
refresh_snapshot() {
	local commit ref version tmp_dir previous
	commit="$(git -C "${checkout_dir}" rev-parse HEAD)"
	if [[ "${commit}" != "$(sed -n 's/^commit=//p' "${lock_file}")" ]]; then
		printf 'Checkout is not at the commit in %s; run sync first\n' "${lock_file}" >&2
		exit 1
	fi
	ref="$(sed -n 's/^ref=//p' "${lock_file}")"
	version="$(git -C "${checkout_dir}" show "${commit}:.version")"

	local -a paths=()
	local name
	for name in COPYING .version nix-meson-build-support; do
		git -C "${checkout_dir}" cat-file -e "${commit}:${name}" && paths+=("${name}")
	done
	while IFS= read -r name; do
		case "${name}" in
			src/lib*-tests | src/lib*-test-support | src/nix-functional-tests | tests/*) paths+=("${name}") ;;
		esac
	done < <(git -C "${checkout_dir}" ls-tree --name-only "${commit}" src/ tests/)

	tmp_dir="$(mktemp -d "${root_dir}/tests/.nix-snapshot.XXXXXX")"
	trap 'rm -rf "${tmp_dir}"' EXIT
	git -C "${checkout_dir}" archive "${commit}" -- "${paths[@]}" | tar -x -C "${tmp_dir}"
	{
		printf '# Provenance of the fixed CppNix test snapshot in tests/nix/.\n'
		printf '# Each object line records the Git object ID of one snapshot path; the commit has identical objects.\n'
		printf 'repository=%s\nversion=%s\nref=%s\ncommit=%s\n' "${remote_url}" "${version}" "${ref}" "${commit}"
		for name in "${paths[@]}"; do
			printf 'object=%s %s\n' "$(git -C "${checkout_dir}" rev-parse "${commit}:${name}")" "${name}"
		done
	} > "${tmp_dir}.lock"

	previous="$(sed -n 's/^object=[0-9a-f]* //p' "${snapshot_lock}" 2>/dev/null | sort)"
	rm -rf "${snapshot_dir}"
	mv "${tmp_dir}" "${snapshot_dir}"
	mv "${tmp_dir}.lock" "${snapshot_lock}"
	trap - EXIT

	comm -13 <(printf '%s\n' "${previous}") <(printf '%s\n' "${paths[@]}" | sort) | sed 's/^/Added snapshot path: /'
	comm -23 <(printf '%s\n' "${previous}") <(printf '%s\n' "${paths[@]}" | sort) | sed 's/^/Removed snapshot path: /'
	bash "${root_dir}/scripts/verify-nix-snapshot.sh" local
}

if [[ "${selection}" == "--help" || "${selection}" == "-h" ]]; then
	usage
	exit 0
fi

if [[ "${selection}" == "snapshot" ]]; then
	if [[ ! -d "${checkout_dir}/.git" || -n "$(git -C "${checkout_dir}" status --porcelain)" ]]; then
		printf 'Missing or modified checkout: %s; run sync first\n' "${checkout_dir}" >&2
		exit 1
	fi
	refresh_snapshot
	exit 0
fi

if [[ -e "${checkout_dir}" && ! -d "${checkout_dir}/.git" ]]; then
	printf 'Refusing to replace non-Git directory: %s\n' "${checkout_dir}" >&2
	exit 1
fi

if [[ ! -d "${checkout_dir}/.git" ]]; then
	mkdir -p "${checkout_dir}"
	git -C "${checkout_dir}" init --quiet
	git -C "${checkout_dir}" remote add origin "${remote_url}"
fi

if [[ -n "$(git -C "${checkout_dir}" status --porcelain)" ]]; then
	printf 'Refusing to overwrite changes in %s\n' "${checkout_dir}" >&2
	exit 1
fi

case "${selection}" in
	latest)
		ref="$(git ls-remote --tags --refs "${remote_url}" | awk '$2 ~ /^refs\/tags\/[0-9]+\.[0-9]+(\.[0-9]+)?$/ {sub("refs/tags/", "", $2); print $2}' | sort -V | tail -n 1)"
		if [[ -z "${ref}" ]]; then
			printf 'No numbered Nix release tags found at %s\n' "${remote_url}" >&2
			exit 1
		fi
		fetch_ref="refs/tags/${ref}"
		;;
	master)
		ref="master"
		fetch_ref="refs/heads/master"
		;;
	sync)
		if [[ ! -f "${lock_file}" ]]; then
			printf 'Missing lock file: %s\n' "${lock_file}" >&2
			exit 1
		fi
		ref="$(sed -n 's/^ref=//p' "${lock_file}")"
		locked_commit="$(sed -n 's/^commit=//p' "${lock_file}")"
		if [[ -z "${ref}" || ! "${locked_commit}" =~ ^[0-9a-f]{40}$ ]]; then
			printf 'Invalid Nix upstream lock file: %s\n' "${lock_file}" >&2
			exit 1
		fi
		fetch_ref="${locked_commit}"
		;;
	*)
		if [[ ! "${selection}" =~ ^[0-9]+\.[0-9]+(\.[0-9]+)?$ ]]; then
			usage >&2
			exit 2
		fi
		ref="${selection}"
		fetch_ref="refs/tags/${ref}"
		;;
esac

git -C "${checkout_dir}" fetch --quiet --depth=1 --filter=blob:none origin "${fetch_ref}"
commit="$(git -C "${checkout_dir}" rev-parse 'FETCH_HEAD^{commit}')"

if [[ "${selection}" == "sync" && "${commit}" != "${locked_commit}" ]]; then
	printf 'Fetched commit does not match lock file: %s\n' "${commit}" >&2
	exit 1
fi

git -C "${checkout_dir}" switch --quiet --detach "${commit}"

for required in COPYING tests/functional src/libstore-tests; do
	if [[ ! -e "${checkout_dir}/${required}" ]]; then
		printf 'Upstream checkout is missing %s\n' "${required}" >&2
		exit 1
	fi
done

if [[ "${selection}" != "sync" ]]; then
	lock_tmp="$(mktemp "${lock_file}.XXXXXX")"
	trap 'rm -f "${lock_tmp}"' EXIT
	printf 'repository=%s\nref=%s\ncommit=%s\n' "${remote_url}" "${ref}" "${commit}" > "${lock_tmp}"
	mv "${lock_tmp}" "${lock_file}"
	trap - EXIT
fi

printf 'Nix upstream %s at %s\n' "${ref}" "${commit}"
printf 'Checkout: %s\n' "${checkout_dir}"
