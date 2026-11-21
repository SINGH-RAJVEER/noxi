# Runs the harness tiers (docs/compatibility-results.md) inside a disposable
# NixOS qualification VM with the same entry points as the host: the
# nextest profiles in .config/nextest.toml and scripts/compat-results.py.
#
# The test binaries come from a nextest archive built on the host
# (`just harness-archive`). They read fixtures through paths fixed at
# compile time, so `noxi-harness` first copies the source tree, writable, to
# the absolute path the archive was built in. With the option disabled the
# module changes nothing.
#
# In the VM: `noxi-harness TIER [DIR]` writes noxi-compat-result/1 records
# and a summary to DIR (default /var/lib/noxi-harness/TIER); copy that
# directory out of the VM and keep it with `scripts/compat-results.py keep`.
{ config, lib, pkgs, ... }:
let
	cfg = config.noxi.qualification.harness;
	python = "${pkgs.python3}/bin/python3";
	harness = pkgs.writeShellApplication {
		name = "noxi-harness";
		runtimeInputs = [ pkgs.cargo-nextest pkgs.coreutils pkgs.git pkgs.bash cfg.reference ];
		text = ''
			tier=''${1:?usage: noxi-harness TIER [DIR]}
			dir=''${2:-/var/lib/noxi-harness/$tier}
			workspace=${lib.escapeShellArg cfg.workspacePath}
			if [ ! -e "$workspace/Cargo.toml" ]; then
				mkdir -p "$workspace"
				cp -r --no-preserve=mode,ownership ${cfg.source}/. "$workspace"
			fi
			cd "$workspace"
			export NOXI_SOURCE_REV=${lib.escapeShellArg cfg.revision}
			export NOXI_SOURCE_DIRTY=0
			export NOXI_REFERENCE_BIN=${cfg.reference}/bin
			exec ${python} scripts/compat-results.py run --tier "$tier" --dir "$dir" \
				--junit "$workspace/target/nextest/$tier/junit.xml" -- \
				cargo-nextest nextest run --archive-file ${cfg.archive} \
				--workspace-remap "$workspace" --profile "$tier" --no-fail-fast
		'';
	};
in
{
	options.noxi.qualification.harness = {
		enable = lib.mkEnableOption "the noxi harness tiers in a disposable qualification VM";
		archive = lib.mkOption {
			type = lib.types.path;
			description = "Nextest archive of the workspace tests, from `just harness-archive`.";
		};
		source = lib.mkOption {
			type = lib.types.path;
			description = "The source tree the archive was built from, at revision `revision`.";
		};
		revision = lib.mkOption {
			type = lib.types.str;
			description = "The noxi Git revision of `source`, recorded in every result.";
		};
		workspacePath = lib.mkOption {
			type = lib.types.str;
			description = "The absolute workspace path the archive was built in.";
		};
		reference = lib.mkOption {
			type = lib.types.package;
			description = "The pinned CppNix reference package the store and daemon tiers compare with.";
		};
	};
	config = lib.mkIf cfg.enable {
		environment.systemPackages = [ harness ];
		systemd.tmpfiles.rules = [ "d /var/lib/noxi-harness 0755 root root -" ];
	};
}
