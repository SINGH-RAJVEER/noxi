//! The `nix-daemon` executable.

fn main() -> std::process::ExitCode {
	noxi_daemon::nix_daemon::main()
}
