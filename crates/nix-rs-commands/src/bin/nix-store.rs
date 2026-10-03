use std::process::ExitCode;

fn main() -> ExitCode {
	nix_rs_commands::cli::main("nix-store", nix_rs_commands::nix_store::main)
}
