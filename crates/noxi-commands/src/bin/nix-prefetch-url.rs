fn main() -> std::process::ExitCode {
	noxi_commands::cli::main("nix-prefetch-url", noxi_commands::prefetch_url::run)
}
