//! Terminal settings for noxios.

use std::process::ExitCode;

use clap::Parser as _;
use noxios_settings_config::{Config, Target};
use tracing_subscriber::EnvFilter;

#[derive(clap::Parser)]
#[command(
	version,
	about = "noxios Settings terminal UI",
	after_help = "Run noxi settings --help for command-line operations."
)]
struct Args {
	#[arg(long, short = 't', value_parser = ["home-manager", "hm", "home", "nixos", "system", "nixos-system"])]
	target: Option<String>,
	#[arg(long, short = 'c')]
	channel: Option<String>,
}

fn main() -> ExitCode {
	let args = Args::parse();
	tracing_subscriber::fmt()
		.with_env_filter(
			EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
		)
		.with_writer(std::io::stderr)
		.init();
	let result = (|| -> anyhow::Result<()> {
		let mut config = Config::load_or_default()?;
		if let Some(target) = args.target {
			config.target = if matches!(target.as_str(), "home-manager" | "hm" | "home") {
				Target::HomeManager
			} else {
				Target::NixosSystem
			};
		}
		if let Some(channel) = args.channel {
			config.channel = channel;
		}
		tokio::runtime::Builder::new_multi_thread()
			.enable_all()
			.build()?
			.block_on(noxios_settings_tui::run_with_config(config))
	})();
	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			eprintln!("noxios-settings-tui: {error:#}");
			ExitCode::FAILURE
		}
	}
}
