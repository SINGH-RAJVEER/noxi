# The nixrs preview, or with `replacement`, the provisional engine package
# laid out like CppNix's: commands in bin, daemon units and tmpfiles rules in
# lib. It is unqualified and used only in disposable qualification VMs.
#
# `rust-overlay` may be a flake input or a plain store path string: the
# qualification guest evaluates this file from a copied source tree and must
# reach the same derivation as the host.
{ pkgs, rust-overlay, replacement ? false }:
let
	lib = pkgs.lib;
	rustPkgs = pkgs.extend (import rust-overlay);
	toolchain = rustPkgs.rust-bin.selectLatestNightlyWith (t: t.minimal);
	rustPlatform = pkgs.makeRustPlatform {
		cargo = toolchain;
		rustc = toolchain;
	};
	members = [ "nix-rs-cli" ] ++ lib.optionals replacement [ "nix-rs-commands" "nix-rs-daemon" ];
	flags = lib.concatMap (member: [ "-p" member ]) members;
	# The executables CppNix 2.35.2 installs that nix-rs does not implement.
	unimplemented = [ "nix-channel" "nix-prefetch-url" ];
	implemented = [ "nixrs" "nix" "nix-build" "nix-instantiate" "nix-store" "nix-env" "nix-collect-garbage" "nix-daemon" "nix-shell" "nix-copy-closure" "nix-hash" ];
	notices = lib.fileset.toSource {
		root = ../.;
		fileset = lib.fileset.unions (
			[ (lib.fileset.fileFilter (file: builtins.elem file.name [ "LICENSE" "COPYING" "NOTICE" "PROVENANCE.md" ]) ../crates) ]
			++ lib.filter builtins.pathExists [ ../THIRD_PARTY.md ../LICENSE ../NOTICE ]
		);
	};
	binaryPackage = rustPlatform.buildRustPackage {
		pname = if replacement then "nixrs-replacement-provisional" else "nixrs-preview";
		version = "0.1.1";
		src = lib.fileset.toSource {
			root = ../.;
			fileset = lib.fileset.unions [ ../.cargo ../Cargo.lock ../Cargo.toml ../crates ];
		};
		cargoLock.lockFile = ../Cargo.lock;
		cargoBuildFlags = flags;
		cargoTestFlags = flags;
		cargoInstallFlags = flags;
		postInstall = ''
			mkdir -p "$out/share/doc/nix-rs"
			cp -r ${notices}/. "$out/share/doc/nix-rs/"
		'';
		# CppNix's package compiles in a static busybox as the sandbox's
		# /bin/sh; nix-rs reads the same setting at build time.
		env = lib.optionalAttrs replacement {
			NIXRS_SANDBOX_SHELL = "${pkgs.busybox-sandbox-shell}/bin/busybox";
		};
		# The engine crates' tests need the reference executables and fixture
		# stores; they run in the development shell, not the package build.
		doCheck = !replacement;
		meta = {
			description = if replacement then "Unqualified nix-rs replacement layout for disposable VMs" else "nix-rs preview evaluator";
			platforms = [ "x86_64-linux" ];
			license = lib.licenses.gpl3Only;
			mainProgram = "nixrs";
		};
	};
in
if !replacement then binaryPackage else pkgs.runCommand "nixrs-replacement-provisional-${binaryPackage.version}" {
	inherit (binaryPackage) pname version meta;
	passthru = { inherit binaryPackage implemented unimplemented; };
} ''
	mkdir -p "$out/bin" "$out/libexec/nix-rs" "$out/share/nix-rs" "$out/lib/systemd/system" "$out/lib/tmpfiles.d"
	cp -r ${binaryPackage}/share/doc "$out/share/"
	for command in ${lib.escapeShellArgs implemented}; do
		cp ${binaryPackage}/bin/"$command" "$out/libexec/nix-rs/$command"
		ln -s "../libexec/nix-rs/$command" "$out/bin/$command"
	done
	# Reserve the reference layout without delegating to CppNix. These entry
	# points fail until their engine implementations exist.
	cat > "$out/libexec/nix-rs/unimplemented" <<-'EOF'
	#!${pkgs.runtimeShell}
	printf 'nix-rs qualification gap: %s is not implemented\n' "''${0##*/}" >&2
	exit 127
	EOF
	chmod +x "$out/libexec/nix-rs/unimplemented"
	for command in ${lib.escapeShellArgs unimplemented}; do
		ln -s ../libexec/nix-rs/unimplemented "$out/bin/$command"
	done
	echo ${lib.escapeShellArgs unimplemented} > "$out/share/nix-rs/unimplemented-commands"

	# The units and tmpfiles rules CppNix 2.35.2 installs, pointing at this
	# package. The NixOS daemon module takes them through systemd.packages.
	cat > "$out/lib/systemd/system/nix-daemon.service" <<-EOF
	[Unit]
	Description=Nix Daemon
	Documentation=man:nix-daemon https://nixos.org/manual
	RequiresMountsFor=/nix/store
	RequiresMountsFor=/nix/var
	RequiresMountsFor=/nix/var/nix/db
	ConditionPathIsReadWrite=/nix/var/nix/daemon-socket

	[Service]
	ExecStart=@$out/bin/nix-daemon nix-daemon --daemon
	KillMode=process
	LimitNOFILE=1048576
	TasksMax=1048576
	Delegate=

	[Install]
	WantedBy=multi-user.target
	EOF
	cat > "$out/lib/systemd/system/nix-daemon.socket" <<-EOF
	[Unit]
	Description=Nix Daemon Socket
	Before=multi-user.target
	RequiresMountsFor=/nix/store
	ConditionPathIsReadWrite=/nix/var/nix/daemon-socket

	[Socket]
	ListenStream=/nix/var/nix/daemon-socket/socket

	[Install]
	WantedBy=sockets.target
	EOF
	cat > "$out/lib/tmpfiles.d/nix-daemon.conf" <<-EOF
	d /nix/var/nix/daemon-socket 0755 root root -  -
	d /nix/var/nix/builds        0755 root root 7d -
	EOF
''
