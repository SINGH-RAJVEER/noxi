use nix_compat::nixcpp::settings::Settings;
use nix_rs_commands::store;
use std::process::ExitCode;

fn main() -> ExitCode {
	let result = (|| {
		let settings =
			Settings::load(|name| std::env::var(name).ok()).map_err(|error| error.to_string())?;
		let mut options = store::parse_args(std::env::args().skip(1), settings)?;
		for warning in options.settings.finish() {
			eprintln!("warning: {warning}");
		}
		Ok::<_, String>(store::run(
			&options,
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
