use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_rs_daemon::{DaemonSettings, server};
use nix_rs_store::LocalStoreConfig;

fn main() -> ExitCode {
	match run() {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			eprintln!("error: {error}");
			ExitCode::FAILURE
		}
	}
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
	let mut settings = Settings::load(|name| std::env::var(name).ok())?;
	let mut args = std::env::args().skip(1);
	let mut root = None;
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--store" => {
				let uri = args.next().ok_or("--store requires an argument")?;
				let path = uri
					.strip_prefix("local?root=")
					.filter(|p| {
						!p.is_empty() && !p.contains(['&', '?', '%']) && Path::new(p).is_absolute()
					})
					.ok_or("nix-daemon requires --store local?root=ABSOLUTE_ROOT")?;
				root = Some(PathBuf::from(path));
			}
			"--version" => {
				println!("nix-daemon (Nix) 2.35.2");
				return Ok(());
			}
			"--help" => {
				println!(
					"usage: nix-daemon --store local?root=ABSOLUTE_ROOT [settings flags]\nNIX_DAEMON_SOCKET_PATH selects the listening Unix socket."
				);
				return Ok(());
			}
			flag => settings
				.apply_flag(flag, || args.next())
				.ok_or_else(|| format!("unrecognised flag '{flag}'"))??,
		}
	}
	for warning in settings.finish() {
		eprintln!("warning: {warning}");
	}
	let root = root.ok_or("nix-daemon requires --store local?root=ABSOLUTE_ROOT")?;
	let config = LocalStoreConfig::diverted(&root)?;
	let socket = std::env::var_os("NIX_DAEMON_SOCKET_PATH")
		.map(PathBuf::from)
		.unwrap_or_else(|| config.state_dir.join("daemon-socket/socket"));
	if !socket.is_absolute() {
		return Err("daemon socket must be an absolute path".into());
	}
	if let Some(parent) = socket.parent() {
		std::fs::create_dir_all(parent)?;
	}
	let settings = DaemonSettings::from_config(config, &settings)?;
	let listener = UnixListener::bind(&socket)?;
	server::serve(listener, &settings)?;
	Ok(())
}
