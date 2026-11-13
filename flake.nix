{
	description = "Preview noxi command and compatibility workspace";

	nixConfig = {
		extra-substituters = [ "https://noxios.cachix.org" ];
		extra-trusted-public-keys = [ "noxios.cachix.org-1:sDINeeBObCrgzbHv5/VcgY/hkoNHRli9j44F+fbU9LE=" ];
	};

	inputs = {
		nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
		rust-overlay = {
			url = "github:oxalica/rust-overlay";
			inputs.nixpkgs.follows = "nixpkgs";
		};
		nix-reference.url = "github:NixOS/nix/2.35.2";
		# The pinned Nixpkgs, Home Manager and CppNix of the qualification
		# matrix row. A relative input shares this repository's lock, and its
		# own inputs keep the revisions tests/qualification/flake.lock records.
		qualification.url = "path:./tests/qualification";
		# The engine revision before this one (origin/main), for the lifecycle
		# mode's release-to-release upgrade. Its lock entry was computed from
		# the local git objects of that pushed commit.
		noxi-previous = {
			url = "github:SINGH-RAJVEER/nix-rs/72262bcef840e937781ca0dd4d347fb8273a4985";
			flake = false;
		};
	};

	outputs = { self, nixpkgs, rust-overlay, nix-reference, qualification, noxi-previous }:
		let
			systems = [ "x86_64-linux" ];
			forAllSystems = f: nixpkgs.lib.genAttrs systems f;
			preview = system: import ./nix/package.nix {
				pkgs = nixpkgs.legacyPackages.${system};
				inherit rust-overlay;
			};
			replacement = import ./tests/qualification/replacement/test.nix {
				inherit self rust-overlay noxi-previous;
				inherit (qualification.inputs) nixpkgs home-manager nix-reference;
			};
		in
		{
			# The generations the qualification VMs switch to, for flake-mode
			# probes and inspection. Never deploy them to a host.
			nixosConfigurations = replacement.configurations;
			homeConfigurations.qualification = qualification.inputs.home-manager.lib.homeManagerConfiguration {
				pkgs = qualification.inputs.nixpkgs.legacyPackages.x86_64-linux;
				modules = [ ./tests/qualification/replacement/home.nix ];
			};
			overlays.default = final: prev: {
				noxi = import ./nix/package.nix {
					pkgs = final;
					inherit rust-overlay;
				};
			};
			devShells = forAllSystems (system: {
				default = import ./nix/shell.nix {
					pkgs = nixpkgs.legacyPackages.${system};
				};
			});
			formatter = forAllSystems (system: nixpkgs.legacyPackages.${system}.nixfmt);
			# The provisional replacement for disposable VM overlays only.
			overlays.qualification = final: prev: {
				noxi-replacement = import ./nix/package.nix {
					pkgs = final;
					inherit rust-overlay;
					replacement = true;
				};
			};
			nixosModules.qualification = import ./nix/qualification-module.nix;
			packages = forAllSystems (system: {
				default = preview system;
				noxi = preview system;
				# Built against the qualification row's Nixpkgs.
				noxi-replacement = replacement.candidate;
				qualification-nixos-report = replacement.reports.nixos;
				qualification-home-manager-report = replacement.reports.home-manager;
				qualification-lifecycle-report = replacement.reports.lifecycle;
				qualification-daemon-report = replacement.reports.daemon;
				nix-reference = nix-reference.packages.${system}.nix;
				# A statically linked busybox with no references, used as the
				# builder in the build comparison tests.
				test-busybox = nixpkgs.legacyPackages.${system}.pkgsStatic.busybox;
			});
			apps = forAllSystems (system:
				let
					previewApp = {
						type = "app";
						program = "${self.packages.${system}.noxi}/bin/noxi";
						meta.description = "Run the noxi preview command";
					};
				in
				{
					default = previewApp;
					noxi = previewApp;
				} // nixpkgs.lib.genAttrs
					(builtins.filter (name: name != "noxi") self.packages.${system}.noxi-replacement.implemented)
					(name: {
						type = "app";
						program = "${self.packages.${system}.noxi-replacement}/bin/${name}";
						meta.description = "Run the unqualified noxi ${name} implementation";
					}));
			checks = forAllSystems (system:
				let
					pkgs = nixpkgs.legacyPackages.${system};
					package = self.packages.${system}.noxi-replacement;
				in
				{
					noxi = self.packages.${system}.default;
					replacement = package;
					# The reference's executable names, the daemon units, and the
					# evaluator answering from the packaged binaries.
					replacement-layout = pkgs.runCommand "noxi-replacement-layout" { } ''
						package=${package}
						ls ${self.packages.${system}.nix-reference}/bin | sort > reference-commands
						ls "$package/bin" | grep -vx noxi | sort > replacement-commands
						diff -u reference-commands replacement-commands
						printf '%s\n' ${pkgs.lib.escapeShellArgs package.implemented} | sort > engine-commands
						ls ${package.binaryPackage}/bin | sort > built-commands
						diff -u engine-commands built-commands
						test -s "$package/share/doc/noxi/THIRD_PARTY.md"
						test -s "$package/share/doc/noxi/crates/noxi/PROVENANCE.md"
						test -s "$package/share/doc/noxi/crates/noxi/eval/COPYING"
						for command in ${pkgs.lib.escapeShellArgs (package.implemented ++ package.unimplemented)}; do
							test -x "$package/bin/$command"
						done
						for command in ${pkgs.lib.escapeShellArgs package.unimplemented}; do
							status=0
							"$package/bin/$command" --version 2> err || status=$?
							test "$status" -eq 127
							grep -q 'is not implemented' err
						done
						grep -qx "ExecStart=@$package/bin/nix-daemon nix-daemon --daemon" "$package/lib/systemd/system/nix-daemon.service"
						grep -qx 'ListenStream=/nix/var/nix/daemon-socket/socket' "$package/lib/systemd/system/nix-daemon.socket"
						test -f "$package/lib/tmpfiles.d/nix-daemon.conf"
						"$package/bin/noxi" eval --expr '1 + 2' | grep -x 3
						"$package/bin/nix" --extra-experimental-features 'nix-command flakes' --store dummy:// eval --expr '1 + 2' | grep -x 3
						"$package/bin/nix-instantiate" --store dummy:// --eval --strict --expr '1 + 2' | grep -x 3
						# The newly packaged commands run from the package. These smoke
						# probes do not qualify channel updates or generations.
						for command in nix-channel nix-prefetch-url; do
							"$package/bin/$command" --version | grep -x "$command (Nix) 2.35.2"
						done
						export HOME="$PWD/home" NIX_CONF_DIR="$PWD/etc"
						mkdir -p "$HOME" "$NIX_CONF_DIR"
						echo hello > fetched
						"$package/bin/nix-prefetch-url" --store "local?root=$PWD/root" "file://$PWD/fetched" 2> prefetch-err \
							| grep -x 00xyyr3fi8l6hb839bv3f7yb86yjv7xi1cgh1xnhipym4asvb4aq
						grep -x "path is '/nix/store/pi0hrnj8zhpb7hbxf7iyp184v2hc9zra-fetched'" prefetch-err
						test -z "$("$package/bin/nix-channel" --list)"
						touch "$out"
					'';
					# Strict replacement gates: each fails unless every step of its
					# VM scenario passed. `just qualification-report MODE` keeps
					# the report of a failing run.
					qualification-nixos = replacement.checks.nixos;
					qualification-home-manager = replacement.checks.home-manager;
					qualification-lifecycle = replacement.checks.lifecycle;
					qualification-daemon = replacement.checks.daemon;
					# Boots a NixOS machine running the pinned reference; see tests/vm/.
					vm = import ./tests/vm/test.nix {
						noxi = self.packages.${system}.default;
						nixReference = self.packages.${system}.nix-reference;
						inherit (pkgs) testers;
					};
				});
		};
}
