fn main() -> std::process::ExitCode {
	nix_rs_commands::cli::main("nix-shell", nix_rs_commands::nix_shell::main)
}
