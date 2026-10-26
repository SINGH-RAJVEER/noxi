fn main() -> std::process::ExitCode {
	nix_rs_commands::cli::main(
		"nix-copy-closure",
		nix_rs_commands::legacy_environment::copy_closure,
	)
}
