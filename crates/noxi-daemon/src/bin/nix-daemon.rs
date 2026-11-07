use noxi_daemon::connection::{Trust, process_connection};
use noxi_store::LocalStore;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use noxi_daemon::{DaemonSettings, server};
use noxi_store::LocalStoreConfig;

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
	let mut store_uri = None;
	let mut stdio = false;
	let mut force_trust = None;
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--store" => {
				store_uri = Some(args.next().ok_or("--store requires an argument")?);
			}
			"--daemon" => {}
			"--stdio" => stdio = true,
			"--force-trusted" | "--force-untrusted" | "--default-trust" => {
				require_feature(&settings, "daemon-trust-override")?;
				force_trust = match arg.as_str() {
					"--force-trusted" => Some(Trust::Trusted),
					"--force-untrusted" => Some(Trust::NotTrusted),
					_ => None,
				};
			}
			"--process-ops" => {
				require_feature(&settings, "mounted-ssh-store")?;
			}

			"--version" => {
				println!("nix-daemon (Nix) 2.35.2");
				return Ok(());
			}
			"--help" => {
				println!(
					"usage: nix-daemon [--daemon | --stdio] [--store STORE] [settings flags]\nNIX_DAEMON_SOCKET_PATH selects the listening Unix socket; systemd may supply listening descriptors."
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
	let uri = store_uri
		.or_else(|| settings.get("store").map(str::to_owned))
		.or_else(|| std::env::var("NIX_REMOTE").ok())
		.unwrap_or_default();
	let config = local_config(&uri)?;
	let socket = std::env::var_os("NIX_DAEMON_SOCKET_PATH")
		.map(PathBuf::from)
		.unwrap_or_else(|| config.state_dir.join("daemon-socket/socket"));
	let mut settings = DaemonSettings::from_config(config, &settings)?;
	settings.force_trust = force_trust;
	if stdio {
		let store = LocalStore::open(settings.store.clone())?;
		process_connection(
			std::io::stdin(),
			std::io::stdout(),
			&store,
			&settings,
			force_trust.unwrap_or(Trust::Trusted),
		)?;
	} else {
		server::serve_listeners(server::listeners(&socket)?, &settings)?;
	}
	Ok(())
}

fn require_feature(settings: &Settings, feature: &str) -> Result<(), Box<dyn std::error::Error>> {
	if !settings.experimental_features().contains(feature) {
		return Err(format!("experimental Nix feature '{feature}' is disabled; add '--extra-experimental-features {feature}' to enable it").into());
	}
	Ok(())
}

fn local_config(uri: &str) -> Result<LocalStoreConfig, Box<dyn std::error::Error>> {
	use nix_compat::nixcpp::store_reference::{StoreReference, Variant};
	let reference = StoreReference::parse(
		uri,
		&Default::default(),
		&std::env::current_dir()?,
		&mut Vec::new(),
	)?;
	match &reference.variant {
		Variant::Auto | Variant::Local => {}
		Variant::Specified { scheme, authority } if scheme == "local" && authority.is_empty() => {}
		_ => return Err(format!("daemon backing store '{uri}' is not supported").into()),
	}
	if let Some(root) = reference
		.params
		.get("root")
		.filter(|root| !root.is_empty() && *root != "/")
	{
		Ok(LocalStoreConfig::diverted(Path::new(root))?)
	} else {
		Ok(LocalStoreConfig::system())
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn default_store_uses_system_paths_without_opening_it() {
		for uri in ["", "auto", "local", "local?root=/"] {
			let config = local_config(uri).unwrap();
			assert_eq!(config.real_store_dir, Path::new("/nix/store"));
			assert_eq!(
				config.state_dir,
				std::env::var_os("NIX_STATE_DIR")
					.map(PathBuf::from)
					.unwrap_or_else(|| PathBuf::from("/nix/var/nix"))
			);
		}
	}
}
