fn main() -> std::process::ExitCode {
	noxi_commands::cli::main(
		"nix-copy-closure",
		noxi_commands::legacy_environment::copy_closure,
	)
}
