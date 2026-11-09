fn main() -> std::process::ExitCode {
	noxi_commands::cli::main("nix-channel", noxi_commands::channel::run)
}
