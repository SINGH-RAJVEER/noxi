//! `nix-store --serve`: the serve protocol on standard input and output.

use std::io;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::nixcpp::store_reference::StoreReference;
use noxi_daemon::client::DaemonStore;
use noxi_daemon::serve_protocol::serve;
use noxi_store::build_derivation::OutputFeatures;

use crate::cli::Failure;
use crate::machine;

pub fn run(
	flags: &[String],
	args: &[String],
	uri: &str,
	settings: &Settings,
) -> Result<(), Failure> {
	let mut write = false;
	for flag in flags {
		if flag != "--write" {
			return Err(Failure::Usage(format!("unknown flag '{flag}'")));
		}
		write = true;
	}
	if !args.is_empty() {
		return Err(Failure::Usage("no arguments expected".into()));
	}
	let features = settings.experimental_features();
	let features = OutputFeatures {
		ca_derivations: features.contains("ca-derivations"),
		impure_derivations: features.contains("impure-derivations"),
		dynamic_derivations: features.contains("dynamic-derivations"),
		git_hashing: features.contains("git-hashing"),
	};
	let cwd = std::env::current_dir()?;
	let reference = StoreReference::parse(uri, &Default::default(), &cwd, &mut Vec::new())
		.map_err(|e| Failure::Error(e.to_string()))?;
	let input = io::stdin().lock();
	let output = io::stdout().lock();
	let result = match machine::local(&reference, settings)? {
		Some(mut store) => serve(&mut store, features, input, output, 0, write),
		None => {
			let mut store: DaemonStore = machine::daemon(&reference, settings)?;
			serve(&mut store, features, input, output, 0, write)
		}
	};
	result.map_err(|e| Failure::Error(e.to_string()))
}
