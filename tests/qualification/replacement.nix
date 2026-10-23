# All writes and daemon handoffs happen inside disposable disks. Reports
# retain failed probes; the strict derivations below reject any gap.
{ self, nixpkgs, home-manager, nix-reference }:
let
	system = "x86_64-linux";
	pkgs = import nixpkgs { inherit system; overlays = [ self.overlays.qualification ]; };
	lib = pkgs.lib;
	candidate = self.packages.${system}.nixrs-replacement;
	reference = nix-reference.packages.${system}.nix;
	hm = home-manager.packages.${system}.home-manager;
	# A distinct packaging upgrade fixture, with the same engine binaries.
	upgrade = pkgs.symlinkJoin {
		name = "nixrs-replacement-upgrade-fixture";
		paths = [ candidate ];
		postBuild = ''echo upgrade-fixture > "$out/qualification-upgrade"'';
	};
	common = { lib, ... }: {
		disabledModules = [ "virtualisation/qemu-vm.nix" ];
		imports = [ (import ../../nix/qualification-qemu-vm.nix { inherit nixpkgs; }) self.nixosModules.qualification home-manager.nixosModules.home-manager ];
		system.stateVersion = "26.05";
		system.switch.enable = true;
		virtualisation = {
			memorySize = 4096;
			cores = 2;
			diskSize = 16384;
			useBootLoader = true;
			mountHostNixStore = false;
			writableStore = false;
		};
		boot.loader.grub.enable = true;
		nix.package = lib.mkDefault reference;
		nix.settings = {
			experimental-features = [ "nix-command" "flakes" ];
			substituters = lib.mkForce [ ];
			flake-registry = "";
		};
		users.users.qualification = { isNormalUser = true; password = ""; };
		home-manager.useGlobalPkgs = true;
		home-manager.users.qualification.home.stateVersion = "26.05";
		environment.systemPackages = [ hm pkgs.jq ];
		environment.etc."qualification/home.nix".text = ''
			{ pkgs, ... }: {
				home.username = "qualification";
				home.homeDirectory = "/home/qualification";
				home.stateVersion = "26.05";
				home.file."qualification-marker".text = "standalone";
			}
		'';
	};
	mkTest = extra: script: pkgs.testers.runNixOSTest {
		name = "nixrs-qualification-${script}";
		nodes.machine = { imports = [ common extra ]; };
		testScript = lib.concatStringsSep "\n" [
			"CANDIDATE = \"${candidate}\""
			"REFERENCE = \"${reference}\""
			"UPGRADE = \"${upgrade}\""
			"NIXPKGS = \"${nixpkgs}\""
			"HM = \"${home-manager}\""
			"MODE = \"${script}\""
			(builtins.readFile ./replacement-script.py)
		];
	};
	# These are real bootable NixOS generations. The guest is required to
	# rebuild them itself before activation counts as replacement evidence.
	generation = marker: engine: (mkTest {
		nixrs.qualification.enable = engine;
		environment.etc."qualification-generation".text = marker;
	} "nixos").config.nodes.machine;
	bootstrap = generation "bootstrap" true;
	first = generation "one" true;
	second = generation "two" true;
	recovery = generation "cppnix" false;
	inputSources = [ self nixpkgs home-manager nix-reference ] ++ builtins.attrValues nix-reference.inputs;
	reports = lib.genAttrs [ "nixos" "home-manager" "distribution" ] (mode: mkTest {
		environment.etc."qualification/source".source = self;
		environment.etc."qualification/paths.json".text = builtins.toJSON {
			bootstrap = bootstrap.system.build.toplevel;
			first = first.system.build.toplevel;
			second = second.system.build.toplevel;
			recovery = recovery.system.build.toplevel;
		};
		system.extraDependencies = [
			candidate upgrade
			bootstrap.system.build.toplevel
			first.system.build.toplevel second.system.build.toplevel recovery.system.build.toplevel
		] ++ inputSources;
	} mode);
	strict = mode: pkgs.runCommand "nixrs-${mode}-qualification-gate" { nativeBuildInputs = [ pkgs.jq ]; } ''
		report=${reports.${mode}}/report.json
		cat "$report"
		jq -e 'all(.steps[]; .status == "pass")' "$report"
		mkdir -p "$out"
		cp "$report" "$out/report.json"
	'';
in
{
	inherit reports;
	checks = lib.genAttrs [ "nixos" "home-manager" "distribution" ] strict;
	configurations = {
		qualification-one.config = first;
		qualification-two.config = second;
		qualification-cppnix.config = recovery;
	};
}
