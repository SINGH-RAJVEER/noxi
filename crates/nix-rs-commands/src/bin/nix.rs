fn main() -> std::process::ExitCode {
	if std::env::args().nth(1).as_deref() == Some("__nixrs_exec_in_store") {
		return nix_rs_commands::nix::execution::helper(std::env::args().skip(2).collect());
	}
	nix_rs_commands::cli::main("nix", nix_rs_commands::nix::main)
}
