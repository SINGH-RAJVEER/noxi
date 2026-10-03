use std::process::ExitCode;

fn main() -> ExitCode {
	nix_rs_commands::cli::main("nix-build", nix_rs_commands::nix_build::main)
}
