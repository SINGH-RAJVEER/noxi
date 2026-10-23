{ pkgs, rust-overlay, replacement ? false }:
let
	rustPkgs = pkgs.extend (import rust-overlay);
	toolchain = rustPkgs.rust-bin.nightly."2026-09-20".minimal;
	rustPlatform = pkgs.makeRustPlatform {
		cargo = toolchain;
		rustc = toolchain;
	};
	members = [ "nix-rs-cli" ] ++ pkgs.lib.optionals replacement [ "nix-rs-commands" "nix-rs-daemon" ];
	flags = pkgs.lib.concatMap (member: [ "-p" member ]) members;
	binaryPackage = rustPlatform.buildRustPackage {
	pname = if replacement then "nixrs-replacement-provisional" else "nixrs-preview";
	version = "0.1.1";
	src = pkgs.lib.fileset.toSource {
		root = ../.;
		fileset = pkgs.lib.fileset.unions [ ../.cargo ../Cargo.lock ../Cargo.toml ../crates ];
	};
	cargoLock.lockFile = ../Cargo.lock;
	cargoBuildFlags = flags;
	cargoTestFlags = flags;
	cargoInstallFlags = flags;
	nativeBuildInputs = pkgs.lib.optionals replacement [ pkgs.pkg-config ];
	buildInputs = pkgs.lib.optionals replacement [ pkgs.openssl ];
	# Engine integration tests require the qualification VM, reference and
	# fixtures. Never exercise store-writing commands in the package build.
	doCheck = !replacement;
	postInstall = pkgs.lib.optionalString replacement ''
		mkdir -p "$out/libexec/nix-rs" "$out/share/nix-rs"
		for command in nixrs nix-build nix-instantiate nix-store nix-env nix-collect-garbage nix-daemon; do
			mv "$out/bin/$command" "$out/libexec/nix-rs/$command"
			ln -s "../libexec/nix-rs/$command" "$out/bin/$command"
		done
		# Reserve the full reference layout without silently invoking CppNix.
		# These entry points fail until their engine implementations exist.
		printf '%s\n' \
			'#!${pkgs.runtimeShell}' \
			'command="''${0##*/}"' \
			'printf "nix-rs qualification gap: %s is not implemented\n" "$command" >&2' \
			'exit 127' > "$out/libexec/nix-rs/unimplemented"
		chmod +x "$out/libexec/nix-rs/unimplemented"
		for command in nix nix-shell nix-copy-closure nix-channel nix-prefetch-url nix-hash; do
			ln -s ../libexec/nix-rs/unimplemented "$out/bin/$command"
		done
		printf '%s\n' 'nix nix-shell nix-copy-closure nix-channel nix-prefetch-url nix-hash' > "$out/share/nix-rs/unimplemented-commands"
	'';
	meta = {
		description = if replacement then "Unqualified nix-rs replacement layout for disposable VMs" else "nix-rs preview evaluator";
		platforms = [ "x86_64-linux" ];
		license = pkgs.lib.licenses.gpl3Only;
		mainProgram = "nixrs";
	};
};
in
if !replacement then binaryPackage else pkgs.symlinkJoin {
	name = "nixrs-replacement-provisional-${binaryPackage.version}";
	inherit (binaryPackage) pname version meta;
	paths = [ binaryPackage ];
	postBuild = ''
		mkdir -p "$out/lib/tmpfiles.d" "$out/lib/systemd/system"
		printf '%s\n' \
			'd /nix/var/nix/daemon-socket 0755 root root -' \
			'd /nix/var/nix/db 0755 root root -' \
			'd /nix/var/nix/profiles 0755 root root -' \
			'd /nix/var/nix/gcroots 0755 root root -' > "$out/lib/tmpfiles.d/nix-daemon.conf"
		printf '%s\n' '[Unit]' 'Description=nix-rs qualification daemon' \
			'[Service]' "ExecStart=$out/bin/nix-daemon --daemon" > "$out/lib/systemd/system/nix-daemon.service"
		printf '%s\n' '[Unit]' 'Description=nix-rs qualification daemon socket' \
			'[Socket]' 'ListenStream=/nix/var/nix/daemon-socket/socket' \
			'SocketMode=0666' 'DirectoryMode=0755' > "$out/lib/systemd/system/nix-daemon.socket"
	'';
}
