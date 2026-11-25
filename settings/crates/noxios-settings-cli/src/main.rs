use std::process::ExitCode;

fn main() -> ExitCode {
	let mut args: Vec<_> = std::env::args_os().collect();
	if args.len() == 1 {
		args.push("--help".into());
	}
	noxios_settings_cmd::run_from("noxios-settings", args)
}
