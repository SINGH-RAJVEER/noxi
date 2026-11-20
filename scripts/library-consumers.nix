# Lists the Nix library inputs of known C and C++ consumers in the pinned
# Nixpkgs, for the consumer inventory in docs/reference-inventory.md.
#
#   nix eval --impure --json --file scripts/library-consumers.nix
#
# The Nixpkgs revision is the qualification flake's `nixpkgs_2` input, fetched
# by its locked hash. The repository itself is never copied to the store.
let
	lock = (builtins.fromJSON (builtins.readFile ../tests/qualification/flake.lock)).nodes.nixpkgs_2.locked;
	src = builtins.fetchTree { inherit (lock) type owner repo rev narHash; };
	pkgs = import src { system = "x86_64-linux"; };
	lib = pkgs.lib;
	# A Nix or Lix library or package derivation: nix-store-2.35.2, nix-2.34.8, lix-2.95.2.
	isNix = drv: lib.isDerivation drv && builtins.match "(nix|lix)(-[a-z-]+)?-[0-9].*" (drv.name or "") != null;
	inputsOf = drv: lib.concatMap (attr: lib.filter lib.isDerivation (lib.flatten (drv.${attr} or [ ]))) [ "buildInputs" "propagatedBuildInputs" "nativeBuildInputs" ];
	report = path: let
		result = builtins.tryEval ((value: builtins.deepSeq value value) (let
			drv = lib.attrByPath (lib.splitString "." path) null pkgs;
			inputs = if drv == null then [ ] else inputsOf drv;
		in {
			name = if drv == null then null else drv.name;
			# Nix inputs of the package itself.
			nix = lib.unique (map (input: input.name) (lib.filter isNix inputs));
			# Nix inputs reached through one non-Nix input, such as a Haskell binding.
			via = lib.filterAttrs (_: names: names != [ ]) (lib.listToAttrs (map (input: {
				name = input.name;
				value = lib.unique (map (inner: inner.name) (lib.filter isNix (inputsOf input)));
			}) (lib.filter (input: !isNix input) inputs)));
		}));
	in if result.success then result.value else { error = "evaluation failed"; };
	consumers = [
		"nix-eval-jobs" "nix-unit" "nixd" "nixf" "nixt" "hydra" "nix-plugins" "nix-inspect"
		"nix-scheduler-hook" "nix-heuristic-gc" "nix-du" "attic-client" "attic-server" "devenv"
		"cachix" "hercules-ci-agent" "haskellPackages.hercules-ci-cnix-store" "haskellPackages.hercules-ci-cnix-expr"
		"nix-serve-ng" "nixVersions.nixComponents_2_35.nix-perl-bindings" "nixVersions.nixComponents_2_34.nix-perl-bindings"
		"colmena" "nil" "harmonia" "nix-serve" "nix-fast-build" "nix-index" "nixpkgs-review" "nix-output-monitor"
		"nix-tree" "nix-diff" "nvd" "nh" "comma" "nurl" "npins" "lon" "nix-direnv" "nix-update" "nix-init"
		"nixos-rebuild-ng" "nixos-option" "system-manager" "vulnix" "nix-bundle" "dydisnix" "libnixxml" "nix-web"
	];
in {
	nixpkgs = lock.rev;
	defaultNix = pkgs.nix.version;
	consumers = lib.genAttrs consumers report;
}
