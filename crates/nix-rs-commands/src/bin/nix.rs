fn main() -> std::process::ExitCode {
	nix_rs_commands::cli::main("nix", nix_rs_commands::nix::main)
}
