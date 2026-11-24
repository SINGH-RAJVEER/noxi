# Boots the noxios ISO and checks that noxi ran its boot units and serves the
# store, the noxios branding, the `noxi` live user, the noxios manual and the
# live system's flake. default.nix defines `start_command`, `engine` (the noxi
# package's store path) and `toplevel` (the uninstrumented image's system)
# ahead of this file.

machine = create_machine(start_command)
machine.start()
machine.wait_for_unit("multi-user.target")

with subtest("noxi registered the image's store and ran the channel unit"):
	machine.succeed("systemctl is-active register-nix-paths.service")
	machine.succeed("systemctl is-active nix-channel-init.service")
	machine.succeed("test -n \"$(readlink /nix/var/nix/profiles/system)\"")

with subtest("the nix commands and daemon are noxi's"):
	for command in ["nix", "nix-store", "nix-env", "nix-build", "nix-instantiate", "nix-daemon"]:
		target = machine.succeed(f"readlink -f /run/current-system/sw/bin/{command}").strip()
		assert target.startswith(f"{engine}/"), f"{command} resolves to {target}"
	machine.succeed("su noxi -c 'nix-store -q --hash /run/current-system'")
	pid = machine.succeed("systemctl show -p MainPID --value nix-daemon.service").strip()
	daemon = machine.succeed(f"readlink -f /proc/{pid}/exe").strip()
	assert daemon.startswith(f"{engine}/"), f"nix-daemon runs {daemon}"

with subtest("noxi-daemon.service and noxi-daemon.socket are aliases of the nix-daemon units"):
	machine.succeed("systemctl show -p Id --value noxi-daemon.service | grep -qx nix-daemon.service")
	machine.succeed("systemctl show -p Id --value noxi-daemon.socket | grep -qx nix-daemon.socket")
	machine.succeed("systemctl is-active noxi-daemon.socket")

with subtest("the system is branded noxios and derives from NixOS"):
	machine.succeed("grep -qx 'ID=noxios' /etc/os-release")
	machine.succeed("grep -qx 'ID_LIKE=nixos' /etc/os-release")
	machine.succeed("grep -qx 'NAME=noxios' /etc/os-release")
	machine.succeed("grep -q 'Welcome to noxios' /etc/issue")
	machine.succeed("grep -q 'noxios-help' /etc/issue")
	hostname = machine.succeed("hostname").strip()
	assert hostname == "noxios", f"host name is {hostname}"
	machine.succeed("ls /dev/disk/by-label | grep -qx 'noxios-minimal-[0-9.]*-x86_64'")

with subtest("the live user is noxi, logged in at the console"):
	machine.fail("id nixos")
	machine.succeed("id -nG noxi | tr ' ' '\\n' | grep -qx wheel")
	machine.succeed("su noxi -c 'sudo -n true'")
	machine.succeed("grep -E '^trusted-users = .*\\bnoxi\\b' /etc/nix/nix.conf")
	machine.wait_for_unit("getty@tty1.service")
	machine.wait_until_succeeds("pgrep -u noxi -t tty1")

with subtest("Settings is built in on the text console"):
	machine.succeed("su noxi -c 'noxi settings --help' | grep -q 'Usage: noxios-settings '")
	machine.succeed("su noxi -c 'noxi settings tui --help' | grep -q 'Usage:'")
	machine.fail("command -v noxios-settings-gui")
	machine.fail("systemctl is-active display-manager.service")

with subtest("Settings searches, previews, installs and removes a package as the live user"):
	machine.succeed(r"""su noxi -c 'bash -se' <<'SH'
set -euo pipefail
scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
export NOXIOS_SETTINGS_CONFIG_DIR="$scratch/config"
export XDG_CONFIG_HOME="$scratch/preferences"
export XDG_CACHE_HOME="$scratch/cache"
mkdir -p "$NOXIOS_SETTINGS_CONFIG_DIR" "$scratch/catalog"
cd "$NOXIOS_SETTINGS_CONFIG_DIR"
cat > "$scratch/catalog/flake.nix" <<'NIX'
{
	outputs = { self }: {
		packages.x86_64-linux.settings-test-tool = (derivation {
			name = "settings-test-tool-1.0";
			system = "x86_64-linux";
			builder = "/bin/sh";
		}) // {
			pname = "settings-test-tool";
			version = "1.0";
			meta.description = "Offline Settings test package";
		};
	};
}
NIX
source_path=$(nix --extra-experimental-features nix-command store add-path "$scratch/catalog")
nar_hash=$(nix --extra-experimental-features nix-command hash path "$source_path")
printf '{"root":"root","version":7,"nodes":{"root":{"inputs":{"nixpkgs":"pinned"}},"pinned":{"locked":{"type":"path","path":"%s","narHash":"%s"}}}}\n' \
	"$source_path" "$nar_hash" > flake.lock
printf '{ pkgs, ... }: {\n\timports = [ ];\n}\n' > configuration.nix
cp configuration.nix configuration.before
noxi settings search --json settings-test-tool > search.json
grep -Fq '"attr": "settings-test-tool"' search.json
noxi settings install --dry-run settings-test-tool
cmp configuration.before configuration.nix
test ! -e noxios-settings-system-packages.nix
noxi settings install --no-rebuild --yes settings-test-tool
grep -Fq './noxios-settings-system-packages.nix' configuration.nix
grep -Fq pkgs.settings-test-tool noxios-settings-system-packages.nix
noxi settings list --json > packages.json
grep -Fq '"name": "settings-test-tool"' packages.json
noxi settings remove --no-rebuild --yes settings-test-tool
if grep -Fq pkgs.settings-test-tool noxios-settings-system-packages.nix; then
	echo "the removed package is still in the managed module" >&2
	exit 1
fi

# A small flake export lets Apply exercise a real build and generation change
# without replacing the running installer system.
busybox=
for candidate in /nix/store/*-busybox-*/bin/busybox; do
	if test -x "$candidate"; then
		busybox=${candidate%/bin/busybox}
		break
	fi
done
test -n "$busybox"
cat > flake.nix <<NIX
{
	inputs.nixpkgs.url = "path:$source_path";
	outputs = { self, nixpkgs }: {
		nixosConfigurations.noxios.config.noxios-settings.profile = derivation {
			name = "settings-profile-v1";
			system = "x86_64-linux";
			builder = builtins.appendContext "$busybox/bin/busybox" {
				"$busybox".path = true;
			};
			args = [ "sh" "-c" "echo profile-ready > \$out" ];
		};
	};
}
NIX
rm flake.lock
nix --extra-experimental-features 'nix-command flakes' flake lock
running=$(readlink -f /run/current-system)
profile=/nix/var/nix/profiles/noxios-settings
noxi settings profile setup --no-rebuild --yes
noxi settings apply --yes
first=$(readlink "$profile")
grep -qx profile-ready "$profile"
sed -i s/settings-profile-v1/settings-profile-v2/ flake.nix
noxi settings apply --yes
test "$(readlink "$profile")" != "$first"
noxi settings profile generations
noxi settings profile rollback --to 1
test "$(readlink "$profile")" = "$first"
test "$(readlink -f /run/current-system)" = "$running"
SH""")


with subtest("the noxios manual replaces the NixOS manual"):
	machine.fail("command -v nixos-help")
	machine.fail("test -e /run/current-system/sw/share/doc/nixos")
	machine.succeed("BROWSER=cat noxios-help | grep -q 'noxios manual'")
	machine.succeed("BROWSER=cat noxios-help system | grep -q 'noxios installer image'")
	machine.fail("noxios-help no-such-page")
	machine.succeed("man -w noxios-system")

with subtest("the live system's flake is in /etc/nixos and evaluates offline"):
	for name in ["flake.nix", "flake.lock", "configuration.nix"]:
		machine.succeed(f"test -f /etc/nixos/{name} && test ! -L /etc/nixos/{name} && test -w /etc/nixos/{name}")
	machine.succeed("grep -q 'noxios live system' /etc/nixos/flake.nix")
	label = machine.succeed(
		"nix --extra-experimental-features 'nix-command flakes' eval --raw "
		"/etc/nixos#nixosConfigurations.noxios.config.system.nixos.label"
	).strip()
	version = machine.succeed("cat /run/current-system/nixos-version").strip()
	assert label == version, f"the flake describes {label}, the running system is {version}"
	# The test image adds the driver's instrumentation; the flake describes
	# the image as published, so its system is that image's.
	evaluated = machine.succeed(
		"nix --extra-experimental-features 'nix-command flakes' eval --raw "
		"/etc/nixos#nixosConfigurations.noxios.config.system.build.toplevel.outPath"
	).strip()
	assert evaluated == toplevel, f"the flake's system is {evaluated}, the image's is {toplevel}"

# The checks NixOS's own ISO boot test makes.
machine.succeed("nix store verify --no-trust -r --option experimental-features nix-command /run/current-system")
with subtest("Check whether the channel got installed correctly"):
	machine.succeed("nix-instantiate --dry-run '<nixpkgs>' -A hello")
	machine.succeed("nix-env --dry-run -iA nixos.procps")

machine.shutdown()
