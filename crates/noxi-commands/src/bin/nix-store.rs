use std::process::ExitCode;

fn main() -> ExitCode {
	noxi_commands::cli::main("nix-store", noxi_commands::nix_store::main)
}
