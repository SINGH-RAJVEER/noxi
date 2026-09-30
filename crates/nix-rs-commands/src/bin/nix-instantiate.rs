use std::process::ExitCode;

use nix_rs_commands::instantiate;

fn main() -> ExitCode {
	let options = match instantiate::parse_args(std::env::args().skip(1)) {
		Ok(options) => options,
		Err(message) => {
			eprintln!("error: {message}\nTry 'nix-instantiate --help' for more information.");
			return ExitCode::FAILURE;
		}
	};
	let cwd = match std::env::current_dir() {
		Ok(cwd) => cwd,
		Err(error) => {
			eprintln!("error: cannot determine the current directory: {error}");
			return ExitCode::FAILURE;
		}
	};
	instantiate::run(&options, &cwd, &mut std::io::stdout().lock())
}
