use std::process::ExitCode;

fn main() -> ExitCode {
	noxi_commands::cli::main("nix-collect-garbage", noxi_commands::collect_garbage::main)
}
