fn main() -> std::process::ExitCode {
	noxi_commands::cli::main("nix-instantiate", noxi_commands::instantiate::main)
}
