fn main() -> std::process::ExitCode {
	nix_rs_commands::cli::main("nix-instantiate", nix_rs_commands::instantiate::main)
}
