use std::process::ExitCode;

fn main() -> ExitCode {
	nix_rs_commands::cli::main("nix-env", nix_rs_commands::nix_env::main)
}
