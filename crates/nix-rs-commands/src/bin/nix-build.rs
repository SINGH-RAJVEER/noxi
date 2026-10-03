use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_rs_commands::build;

fn main() -> ExitCode {
	std::thread::Builder::new()
		.stack_size(64 * 1024 * 1024)
		.spawn(run)
		.expect("start evaluation thread")
		.join()
		.unwrap_or(ExitCode::FAILURE)
}

fn run() -> ExitCode {
	let result = (|| {
		let settings =
			Settings::load(|name| std::env::var(name).ok()).map_err(|error| error.to_string())?;
		let mut options = build::parse_args(std::env::args().skip(1), settings)?;
		for warning in options.instantiate.settings.finish() {
			eprintln!("warning: {warning}");
		}
		let cwd = std::env::current_dir().map_err(|error| error.to_string())?;
		Ok::<_, String>(build::run(
			&options,
			&cwd,
			&mut std::io::stdout().lock(),
			&mut std::io::stderr().lock(),
		))
	})();
	match result {
		Ok(code) => code,
		Err(error) => {
			eprintln!("error: {error}");
			ExitCode::FAILURE
		}
	}
}
