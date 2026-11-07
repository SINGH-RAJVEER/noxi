fn main() -> std::process::ExitCode {
	noxi_commands::cli::main("nix-hash", noxi_commands::nix::hash::legacy)
}
