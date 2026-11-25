//! Configuration inspection, following CppNix 2.35.2's config.cc. The values
//! are `noxi_build::config_values`.
use std::process::ExitCode;

use noxi_build::config_values::{text, values};

use super::Parsed;
use crate::cli::Failure;

pub(super) fn show(parsed: Parsed) -> Result<ExitCode, Failure> {
	if parsed.has("json") && !parsed.operands.is_empty() {
		return Err(Failure::Usage(
			"'--json' is not supported when specifying a setting name".into(),
		));
	}
	let settings = values(&parsed.settings, true).map_err(Failure::Usage)?;
	if let Some(name) = parsed.operands.first() {
		let entry = settings
			.get(name)
			.ok_or_else(|| Failure::Error(format!("could not find setting '{name}'")))?;
		println!("{}", text(&entry["value"], name));
	} else if parsed.has("json") {
		println!(
			"{}",
			serde_json::to_string(&settings).expect("settings JSON")
		);
	} else {
		for (name, entry) in settings.as_object().unwrap() {
			println!("{name} = {}", text(&entry["value"], name));
		}
		println!();
	}
	Ok(ExitCode::SUCCESS)
}
