//! `nix-store --serve`: the serve protocol on standard input and output.

use std::io;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::nixcpp::store_reference::StoreReference;
use noxi_daemon::client::DaemonStore;
use noxi_daemon::serve_protocol::{AnyStore, serve};
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
	// A local store builds supplied derivations itself, a daemon through
	// its worker. Other stores are served as they are.
	let local = match crate::store::resolves_to_local(uri) {
		// Every parameter a command's local store takes.
		Ok(true) => Some(machine::backend(uri, &reference, settings)?),
		// A machine's `system-features` too.
		_ => machine::local(&reference, settings).ok().flatten(),
	};
	let result = if let Some(mut store) = local {
		serve(&mut store, features, input, output, 0, write)
	} else if let Ok(mut store) = machine::daemon(&reference, settings) {
		serve::<DaemonStore>(&mut store, features, input, output, 0, write)
	} else {
		let opened = crate::store::open(uri, settings)?
			.ok_or_else(|| Failure::Error(format!("store '{uri}' cannot hold store paths")))?;
		serve(
			&mut AnyStore(opened.store),
			features,
			input,
			output,
			0,
			write,
		)
	};
	result.map_err(|e| Failure::Error(e.to_string()))
}
