# Import only in disposable qualification test nodes. This is deliberately
# not a public host migration module, and makes no compatibility claim.
{ config, lib, pkgs, ... }:
{
	options.nixrs.qualification.enable = lib.mkEnableOption "the unqualified nix-rs engine in a disposable VM";
	config = lib.mkIf config.nixrs.qualification.enable {
		# Nixpkgs requires a Nix version >= 2.2 even with checkConfig off.
		# Supply the target format version as VM-only metadata. Executables
		# retain their Rust crate versions; this is not a compatibility claim.
		nix.package = lib.mkForce (pkgs.nixrs-replacement // {
			version = "2.35.2";
			out = pkgs.nixrs-replacement.out // { version = "2.35.2"; };
		});
		# The upstream package normally ships these units. Define them here
		# against the same package, retaining exactly one daemon service.
		systemd.services.nix-daemon.serviceConfig = {
			ExecStart = lib.mkForce "${config.nix.package}/bin/nix-daemon --daemon";
			KillMode = lib.mkForce "control-group";
			Restart = lib.mkForce "no";
		};
		systemd.sockets.nix-daemon = {
			wantedBy = [ "sockets.target" ];
			listenStreams = [ "/nix/var/nix/daemon-socket/socket" ];
			socketConfig = { SocketMode = "0666"; DirectoryMode = "0755"; };
		};
		# Nixpkgs validates with the modern `nix` CLI, currently absent.
		# Its absence is tested in the guest instead of blocking VM creation.
		nix.checkConfig = false;
	};
}
