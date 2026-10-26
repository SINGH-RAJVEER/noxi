fn main() -> std::process::ExitCode {
	nix_rs_commands::cli::main("nix-hash", nix_rs_commands::nix::hash::legacy)
}
