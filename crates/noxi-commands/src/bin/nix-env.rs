use std::process::ExitCode;

fn main() -> ExitCode {
	noxi_commands::cli::main("nix-env", noxi_commands::nix_env::main)
}
