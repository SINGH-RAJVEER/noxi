fn main() -> std::process::ExitCode {
	noxi_commands::cli::main("nix-shell", noxi_commands::nix_shell::main)
}
