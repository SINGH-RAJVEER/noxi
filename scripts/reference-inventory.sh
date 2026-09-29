#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
reference="${1:-${root_dir}/target/nix-reference}"
source_dir="${2:-${root_dir}/tests/nix-official}"
out_dir="${root_dir}/tests/reference-inventory"

usage() {
	cat <<'EOF'
Usage: scripts/reference-inventory.sh [REFERENCE_PACKAGE] [SOURCE_CHECKOUT]

Writes tests/reference-inventory/ from the pinned CppNix build (default
target/nix-reference, built by `just reference`) and the matching official
source checkout (default tests/nix-official, restored by `just nix-tests-sync`).
EOF
}

case "${1:-}" in
	--help | -h)
		usage
		exit 0
		;;
esac

if [[ ! -x "${reference}/bin/nix" ]]; then
	echo "missing reference executable: ${reference}/bin/nix (run: just reference)" >&2
	exit 1
fi
if [[ ! -d "${source_dir}/src/libstore" ]]; then
	echo "missing official source checkout: ${source_dir} (run: just nix-tests-sync)" >&2
	exit 1
fi

expected_commit="$(sed -n 's/^commit=//p' "${root_dir}/tests/nix-official.lock")"
actual_commit="$(git -C "${source_dir}" rev-parse HEAD)"
if [[ "${expected_commit}" != "${actual_commit}" ]]; then
	echo "source checkout is at ${actual_commit}, lock records ${expected_commit}" >&2
	exit 1
fi

scratch="$(mktemp -d)"
trap 'rm -rf "${scratch}"' EXIT
mkdir -p "${scratch}/home" "${scratch}/conf"

# The hidden __dump commands must come first on the command line.
# Query the reference with no inherited configuration, so the inventory records
# its built-in defaults rather than this machine's nix.conf.
run_nix() {
	env -i \
		HOME="${scratch}/home" \
		PATH="${reference}/bin" \
		NIX_CONF_DIR="${scratch}/conf" \
		NIX_USER_CONF_FILES="${scratch}/conf/none" \
		NIX_CONFIG= \
		"${reference}/bin/nix" "$@"
}

mkdir -p "${out_dir}"

store_path="$(readlink -f "${reference}")"
version="$(run_nix --version)"

{
	echo "# Files installed by the reference package, relative to its output."
	echo "# ${version} at ${store_path}"
	(cd "${reference}" && find -L . -mindepth 1 \( -type f -o -type l \) -print | sed 's#^\./##' | grep -v '^share/' | LC_ALL=C sort)
	echo "# Shell completions"
	(cd "${reference}" && find -L share -mindepth 1 -type f -print | LC_ALL=C sort)
} >"${out_dir}/files.txt"

# Settings: name, aliases, gating feature, and the default when CppNix documents
# it. Undocumented defaults depend on the build machine (cores, system features,
# paths), so they are recorded as null.
run_nix --extra-experimental-features nix-command config show --json |
	jq -S --arg conf "${scratch}/conf" 'walk(if type == "string" then gsub($conf; "$NIX_CONF_DIR") else . end) | with_entries(.value |= {
		aliases: (.aliases | sort),
		experimentalFeature: .experimentalFeature,
		defaultValue: (if .documentDefault then .defaultValue else null end),
		documentedDefault: .documentDefault
	})' >"${out_dir}/settings.json"

run_nix __dump-xp-features | jq -S 'keys' >"${out_dir}/experimental-features.json"

# The new-style command tree with each command's flags and gating feature. The
# legacy commands parse their own arguments and are not in this dump.
run_nix __dump-cli | jq -S '
	def flags: (.flags // {}) | with_entries(.value |= {
		shortName: (.shortName // null),
		arity: (.arity // 0),
		experimentalFeature: (.["experimental-feature"] // null)
	});
	def walk_commands($prefix):
		(.commands // {}) | to_entries[] |
		(($prefix + [.key]) as $path |
			{key: ($path | join(" ")), value: {
				experimentalFeature: (.value.category["experimental-feature"] // .value["experimental-feature"] // null),
				flags: (.value | flags)
			}},
			(.value | walk_commands($path)));
	{
		globalFlags: (.args | flags),
		commands: ([.args | walk_commands(["nix"])] | from_entries),
		stores: (.stores | keys),
		fetchers: (.fetchers | keys)
	}' >"${out_dir}/cli.json"

run_nix __dump-language | jq -S '
	with_entries(.value |= {args: (.args // null), experimentalFeature: (.["experimental-feature"] // null)})' \
	>"${out_dir}/builtins.json"

# Protocol constants and operation numbers, read from the pinned source.
{
	echo "# Read from ${source_dir##*/} at ${actual_commit}"
	grep -hE '^#define (WORKER_MAGIC_[12]|PROTOCOL_VERSION|MINIMUM_PROTOCOL_VERSION|SERVE_MAGIC_[12]|SERVE_PROTOCOL_VERSION)' \
		"${source_dir}/src/libstore/include/nix/store/worker-protocol.hh" \
		"${source_dir}/src/libstore/include/nix/store/serve-protocol.hh"
	echo "# Worker protocol features"
	grep -hoE 'feature[A-Za-z]+ = "[^"]+"' "${source_dir}/src/libstore/include/nix/store/worker-protocol.hh"
	echo "# Worker protocol operations"
	awk '/^enum struct WorkerProto::Op/,/^};/' "${source_dir}/src/libstore/include/nix/store/worker-protocol.hh" |
		grep -E '^\s+[A-Za-z]+ = [0-9]+' | sed -E 's/^\s+//; s/,.*//'
	echo "# Serve protocol commands"
	awk '/^enum struct ServeProto::Command/,/^};/' "${source_dir}/src/libstore/include/nix/store/serve-protocol.hh" |
		grep -E '^\s+[A-Za-z]+ = [0-9]+' | sed -E 's/^\s+//; s/,.*//'
} >"${out_dir}/protocols.txt"

# Legacy command options, from each command page's synopsis and option list in
# the reference manual. These commands have no machine-readable dump.
{
	echo "# Read from ${source_dir##*/} at ${actual_commit}: doc/manual/source/command-ref"
	cd "${source_dir}/doc/manual/source/command-ref"
	for page in nix-*.md nix-env/*.md nix-store/*.md opt-common.md; do
		name="${page%.md}"
		name="${name/\// --}"
		{
			awk '/^# Synopsis/{f=1;next} /^# /{f=0} f' "${page}" | grep -oE '`-{1,2}[A-Za-z][A-Za-z0-9-]*`' || true
			grep -oE '\[`-{1,2}[A-Za-z][A-Za-z0-9-]*`\]\(#opt-' "${page}" | grep -oE '`[^`]+`' || true
		} | tr -d '`' | LC_ALL=C sort -u | sed "s#^#${name} #"
	done
	cd "${root_dir}"
} >"${out_dir}/legacy-options.txt"

# Public C API functions by library. The C++ headers under src/lib*/include are
# also installed, but have no stable interface to enumerate.
{
	echo "# Read from ${source_dir##*/} at ${actual_commit}"
	for header in "${source_dir}"/src/lib*-c/nix_api_*.h; do
		[[ "${header}" == *_internal.h ]] && continue
		library="$(basename "$(dirname "${header}")")"
		grep -hoE '\bnix_[a-z_0-9]+\s*\(' "${header}" | sed -E 's/\s*\($//' | sed "s#^#${library} #"
	done | LC_ALL=C sort -u
} >"${out_dir}/c-api.txt"

echo "wrote ${out_dir} for ${version}"
