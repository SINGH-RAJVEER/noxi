use std::process::ExitCode;

fn main() -> ExitCode {
	nix_rs_commands::cli::main(
		"nix-collect-garbage",
		nix_rs_commands::collect_garbage::main,
	)
}
