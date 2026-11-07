use std::process::ExitCode;

fn main() -> ExitCode {
	noxi_commands::cli::main("nix-build", noxi_commands::nix_build::main)
}
