//! Configuration inspection, following CppNix 2.35.2's config.cc.
//!
//! config-settings.json contains the setting metadata from that release's
//! GlobalConfig::toJSON (GPL-3.0-only). Machine-dependent defaults and
//! constructor assignments are supplied here, before configuration overrides.
use std::collections::BTreeSet;
use std::path::Path;
use std::process::ExitCode;

use serde_json::{Value, json};

use super::Parsed;
use crate::cli::Failure;

fn text(value: &Value, name: &str) -> String {
	match value {
		Value::Null => String::new(),
		Value::String(s) => s.clone(),
		Value::Array(a) => a
			.iter()
			.map(|v| text(v, name))
			.collect::<Vec<_>>()
			.join(" "),
		Value::Object(m) if name == "sandbox-paths" => m
			.iter()
			.map(|(target, v)| {
				let source = v["source"].as_str().unwrap();
				format!(
					"{target}{}{}",
					if source == target {
						String::new()
					} else {
						format!("={source}")
					},
					if v["optional"] == true { "?" } else { "" }
				)
			})
			.collect::<Vec<_>>()
			.join(" "),
		Value::Object(m) => m
			.iter()
			.map(|(k, v)| format!(" {k}={}", v.as_str().unwrap()))
			.collect(),
		_ => value.to_string(),
	}
}

fn parse(name: &str, input: &str, default: &Value) -> Result<Value, Failure> {
	Ok(match default {
		Value::Bool(_) => json!(input == "true"),
		Value::Number(_) => {
			if name == "max-jobs" && input == "auto" {
				json!(std::thread::available_parallelism().map_or(1, |n| n.get()))
			} else {
				serde_json::from_str(input).map_err(|_| {
					Failure::Usage(format!("setting '{name}' has invalid value '{input}'"))
				})?
			}
		}
		Value::Array(_) => {
			let mut words: Vec<_> = input.split_whitespace().map(str::to_owned).collect();
			if matches!(
				name,
				"experimental-features"
					| "extra-platforms"
					| "system-features"
					| "ignored-acls"
					| "allowed-impure-host-deps"
					| "trusted-substituters"
			) {
				words.sort();
				words.dedup();
			}
			json!(words)
		}
		Value::Object(_) => {
			let mut map = serde_json::Map::new();
			for word in input.split_whitespace() {
				if name == "sandbox-paths" {
					let optional = word.ends_with('?');
					let word = word.strip_suffix('?').unwrap_or(word);
					let (target, source) = word.split_once('=').unwrap_or((word, word));
					map.entry(target.to_owned())
						.or_insert_with(|| json!({"optional": optional, "source": source}));
				} else if let Some((k, v)) = word.split_once('=') {
					map.entry(k.to_owned()).or_insert_with(|| json!(v));
				} else {
					return Err(Failure::Usage(format!(
						"setting '{name}' has invalid value '{input}'"
					)));
				}
			}
			Value::Object(map)
		}
		Value::Null if input.is_empty() => Value::Null,
		_ => json!(input),
	})
}

fn extra_platforms() -> Value {
	let mut platforms = BTreeSet::new();
	#[cfg(all(target_arch = "x86_64", target_os = "linux"))]
	{
		if !std::fs::read_to_string("/proc/sys/kernel/osrelease")
			.unwrap_or_default()
			.trim()
			.ends_with("-Microsoft")
		{
			platforms.insert("i686-linux".to_owned());
		}
		let v2 = std::is_x86_feature_detected!("sse3")
			&& std::is_x86_feature_detected!("ssse3")
			&& std::is_x86_feature_detected!("sse4.1")
			&& std::is_x86_feature_detected!("sse4.2")
			&& std::is_x86_feature_detected!("popcnt")
			&& std::is_x86_feature_detected!("cmpxchg16b");
		let v3 = v2
			&& std::is_x86_feature_detected!("avx2")
			&& std::is_x86_feature_detected!("bmi1")
			&& std::is_x86_feature_detected!("bmi2")
			&& std::is_x86_feature_detected!("f16c")
			&& std::is_x86_feature_detected!("fma")
			&& std::is_x86_feature_detected!("lzcnt")
			&& std::is_x86_feature_detected!("movbe");
		let v4 = v3
			&& std::is_x86_feature_detected!("avx512f")
			&& std::is_x86_feature_detected!("avx512bw")
			&& std::is_x86_feature_detected!("avx512cd")
			&& std::is_x86_feature_detected!("avx512dq")
			&& std::is_x86_feature_detected!("avx512vl");
		for level in 1..=if v4 {
			4
		} else if v3 {
			3
		} else if v2 {
			2
		} else {
			1
		} {
			platforms.insert(format!("x86_64-v{level}-linux"));
		}
	}
	json!(platforms)
}

pub(super) fn show(parsed: Parsed) -> Result<ExitCode, Failure> {
	if parsed.has("json") && !parsed.operands.is_empty() {
		return Err(Failure::Usage(
			"'--json' is not supported when specifying a setting name".into(),
		));
	}
	let mut settings: Value = serde_json::from_str(include_str!("config-settings.json"))
		.expect("pinned settings metadata");
	let conf = std::env::var("NIX_CONF_DIR").unwrap_or_else(|_| "/etc/nix".into());
	for (name, default) in [
		("builders", json!(format!("@{conf}/machines"))),
		("netrc-file", json!(format!("{conf}/netrc"))),
		("extra-platforms", extra_platforms()),
		(
			"store",
			json!(
				std::env::var("NIX_REMOTE")
					.ok()
					.filter(|s| !s.is_empty())
					.unwrap_or_else(|| "auto".into())
			),
		),
	] {
		settings[name]["defaultValue"] = default;
	}
	let mut features = vec!["benchmark", "big-parallel", "nixos-test"];
	if cfg!(target_os = "linux") {
		features.push("uid-range");
		// SAFETY: the literal is a valid, terminated C string.
		if unsafe { libc::access(c"/dev/kvm".as_ptr(), libc::R_OK | libc::W_OK) } == 0 {
			features.push("kvm");
		}
	}
	features.sort();
	settings["system-features"]["defaultValue"] = json!(features);
	let ssl = std::env::var("NIX_SSL_CERT_FILE")
		.or_else(|_| std::env::var("SSL_CERT_FILE"))
		.ok()
		.or_else(|| {
			[
				"/etc/ssl/certs/ca-certificates.crt",
				"/nix/var/nix/profiles/default/etc/ssl/certs/ca-bundle.crt",
			]
			.into_iter()
			.find(|p| Path::new(p).exists())
			.map(str::to_owned)
		});
	settings["ssl-cert-file"]["defaultValue"] = json!(ssl);
	for entry in settings.as_object_mut().unwrap().values_mut() {
		entry["value"] = entry["defaultValue"].clone();
	}
	settings["pure-eval"]["value"] = json!(true);
	settings["build-users-group"]["value"] = json!(if unsafe { libc::geteuid() } == 0 {
		"nixbld"
	} else {
		""
	});
	settings["allow-symlinked-store"]["value"] =
		json!(std::env::var("NIX_IGNORE_SYMLINK_STORE").as_deref() == Ok("1"));
	settings["abort-on-warn"]["value"] = json!(matches!(
		std::env::var("NIX_ABORT_ON_WARN").as_deref(),
		Ok("1" | "yes" | "true")
	));
	if let Ok(systems) = std::env::var("NIX_REMOTE_SYSTEMS") {
		settings["builders"]["value"] = json!(
			systems
				.split(':')
				.map(|p| format!("@{p}"))
				.collect::<Vec<_>>()
				.join("\n")
		);
	}
	if let Some(shell) = noxi_build::settings::SANDBOX_SHELL {
		settings["sandbox-paths"]["value"] =
			json!({"/bin/sh": {"source": shell, "optional": false}});
	}
	// The hook is package-relative, just as CppNix's compiled nixBinDir is.
	let bin = std::env::var_os("NIX_BIN_DIR")
		.map(std::path::PathBuf::from)
		.or_else(|| {
			std::env::current_exe()
				.ok()
				.and_then(|p| p.parent().map(Path::to_path_buf))
		});
	if let Some(bin) = bin {
		settings["build-hook"]["value"] =
			json!([bin.join("nix").to_string_lossy(), "__build-remote"]);
	}
	for (name, value, replaces) in parsed.settings.overridden() {
		let entry = &mut settings[name];
		let mut input = String::new();
		if !replaces {
			input = text(&entry["value"], name);
			input.push(' ');
		}
		input.push_str(value);
		entry["value"] = parse(name, &input, &entry["defaultValue"])?;
	}
	settings["experimental-features"]["value"] = json!(parsed.settings.experimental_features());
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
