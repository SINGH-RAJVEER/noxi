#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
checkout_dir="${root_dir}/tests/nix-official"
lock_file="${root_dir}/tests/nix-official.lock"
remote_url="https://github.com/NixOS/nix.git"
selection="${1:-latest}"

usage() {
	cat <<'EOF'
Usage: scripts/update-nix-tests.sh [latest|master|sync|TAG]

latest  Fetch the newest numbered stable Nix release tag and update the lock file.
master  Fetch the current upstream development branch and update the lock file.
sync    Check out the exact commit already recorded in the lock file.
TAG     Fetch a specific numbered Nix release tag and update the lock file.
EOF
}

if [[ "${selection}" == "--help" || "${selection}" == "-h" ]]; then
	usage
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
