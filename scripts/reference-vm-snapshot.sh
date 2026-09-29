#!/usr/bin/env bash
set -euo pipefail

root_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
out_dir="${root_dir}/target/qualification"
boot_timeout="${NIXRS_VM_BOOT_TIMEOUT:-600}"
manifest="${out_dir}/reference-base.manifest"

usage() {
	cat <<'EOF'
Usage: scripts/reference-vm-snapshot.sh [snapshot|overlay NAME|verify]

snapshot      Build the pinned CppNix reference VM, boot it once with CppNix only,
              shut it down through ACPI, and flatten its disk into the read-only
              base target/qualification/reference-base.qcow2.
overlay NAME  Create target/qualification/NAME.qcow2, a copy-on-write overlay of
              the base, plus a copy of the base's EFI variables. Delete both to
              discard a run.
verify        Boot a temporary overlay of the base to multi-user mode, power it
              off, delete the overlay, and check the base is unchanged.
EOF
}

# The EXIT trap runs after a function returns, so these stay global.
scratch=""
qemu_pid=""
cleanup() {
	if [[ -n "${qemu_pid}" ]]; then
		kill "${qemu_pid}" 2>/dev/null || true
	fi
	if [[ -n "${scratch}" ]]; then
		rm -rf "${scratch}"
	fi
}
trap cleanup EXIT

wait_for_log() {
	local log="$1" pattern="$2" timeout="$3" what="$4"
	local waited=0
	until grep -qE "${pattern}" "${log}" 2>/dev/null; do
		if ! kill -0 "${qemu_pid}" 2>/dev/null; then
			echo "reference VM exited before ${what}; see ${log}" >&2
			exit 1
		fi
		if ((waited >= timeout)); then
			echo "reference VM did not reach ${what} in ${timeout}s; see ${log}" >&2
			exit 1
		fi
		sleep 2
		waited=$((waited + 2))
	done
}

# Boot VM with DISK and EFI, wait for multi-user mode and Home Manager
# activation, then power off through ACPI and require a clean shutdown.
boot_and_power_off() {
	local vm="$1" disk="$2" efi="$3" log="$4"
	local monitor="${scratch}/monitor.sock"
	rm -f "${log}" "${monitor}"

	NIX_DISK_IMAGE="${disk}" NIX_EFI_VARS="${efi}" \
		QEMU_OPTS="-display none -serial file:${log} -monitor unix:${monitor},server,nowait" \
		"${vm}/bin/run-nixrs-reference-vm" &
	qemu_pid=$!

	# The console log has colour escapes between words, hence the loose patterns.
	wait_for_log "${log}" 'Reached target .*Multi-User System' "${boot_timeout}" "multi-user mode"
	wait_for_log "${log}" 'Started .*Nix Daemon' 60 "the Nix daemon"
	wait_for_log "${log}" 'Finished .*Home Manager environment for qualification' 120 "Home Manager activation"

	printf 'system_powerdown\n' | socat - "UNIX-CONNECT:${monitor}" >/dev/null
	local waited=0
	while kill -0 "${qemu_pid}" 2>/dev/null; do
		if ((waited >= 180)); then
			echo "reference VM did not power off; see ${log}" >&2
			exit 1
		fi
		sleep 2
		waited=$((waited + 2))
	done
	wait "${qemu_pid}" || true
	qemu_pid=""
	if ! grep -qE 'reboot: Power down' "${log}"; then
		echo "reference VM stopped without a clean power-off; see ${log}" >&2
		exit 1
	fi
}

check_base() {
	if [[ ! -f "${manifest}" ]]; then
		echo "missing ${manifest}; run: just reference-vm-snapshot" >&2
		exit 1
	fi
	local expected actual
	expected="$(sed -n 's/^base_sha256=//p' "${manifest}")"
	actual="$(sha256sum "${out_dir}/reference-base.qcow2" | cut -d' ' -f1)"
	if [[ "${expected}" != "${actual}" ]]; then
		echo "reference base changed since its snapshot; recreate it" >&2
		exit 1
	fi
}

snapshot() {
	mkdir -p "${out_dir}"
	local vm qemu_img
	vm="$(nix build --no-link --print-out-paths "path:${root_dir}/tests/qualification#reference-vm")"
	scratch="$(mktemp -d)"
	boot_and_power_off "${vm}" "${scratch}/reference.qcow2" "${scratch}/reference-efi-vars.fd" \
		"${out_dir}/reference-boot.log"

	qemu_img="$(grep -oE '/nix/store/[^ ]+/bin/qemu-img' "${vm}/bin/run-nixrs-reference-vm" | head -n1)"
	rm -f "${out_dir}/reference-base.qcow2" "${out_dir}/reference-efi-vars.fd"
	"${qemu_img}" convert -O qcow2 "${scratch}/reference.qcow2" "${out_dir}/reference-base.qcow2"
	cp "${scratch}/reference-efi-vars.fd" "${out_dir}/reference-efi-vars.fd"
	chmod 0444 "${out_dir}/reference-base.qcow2" "${out_dir}/reference-efi-vars.fd"
	{
		echo "vm=${vm}"
		echo "qemu_img=${qemu_img}"
		echo "created=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
		echo "base_sha256=$(sha256sum "${out_dir}/reference-base.qcow2" | cut -d' ' -f1)"
		echo "efi_vars_sha256=$(sha256sum "${out_dir}/reference-efi-vars.fd" | cut -d' ' -f1)"
	} >"${manifest}"
	echo "wrote ${out_dir}/reference-base.qcow2"
}

overlay() {
	local name="$1"
	check_base
	local qemu_img
	qemu_img="$(sed -n 's/^qemu_img=//p' "${manifest}")"
	"${qemu_img}" create -q -f qcow2 -b "${out_dir}/reference-base.qcow2" -F qcow2 "${out_dir}/${name}.qcow2"
	cp "${out_dir}/reference-efi-vars.fd" "${out_dir}/${name}-efi-vars.fd"
	chmod 0644 "${out_dir}/${name}-efi-vars.fd"
	echo "NIX_DISK_IMAGE=${out_dir}/${name}.qcow2 NIX_EFI_VARS=${out_dir}/${name}-efi-vars.fd"
}

verify() {
	check_base
	local vm name="verify-$$"
	vm="$(sed -n 's/^vm=//p' "${manifest}")"
	scratch="$(mktemp -d)"
	overlay "${name}" >/dev/null
	boot_and_power_off "${vm}" "${out_dir}/${name}.qcow2" "${out_dir}/${name}-efi-vars.fd" \
		"${out_dir}/reference-verify.log"
	rm -f "${out_dir}/${name}.qcow2" "${out_dir}/${name}-efi-vars.fd"
	check_base
	echo "reference base booted from an overlay and is unchanged"
}

case "${1:-snapshot}" in
	snapshot) snapshot ;;
	overlay)
		[[ -n "${2:-}" ]] || {
			usage >&2
			exit 2
		}
		overlay "$2"
		;;
	verify) verify ;;
	--help | -h) usage ;;
	*)
		usage >&2
		exit 2
		;;
esac
