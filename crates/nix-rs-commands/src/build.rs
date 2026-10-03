//! `nix-build`: instantiate and realize selections through the Rust engine.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use nix_rs_build::{local::LocalBuilder, settings::BuildSettings};
use nix_rs_glue::EvaluatorStore;
use nix_rs_store::binary_cache::BinaryCache;

use crate::instantiate;

#[derive(Debug)]
pub struct Options {
	pub instantiate: instantiate::Options,
	pub out_link: Option<PathBuf>,
}

pub fn parse_args(
	args: impl IntoIterator<Item = String>,
	settings: Settings,
) -> Result<Options, String> {
	let mut args = args.into_iter();
	let mut forwarded = Vec::new();
	let mut out_link = Some(PathBuf::from("result"));
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"--no-out-link" => out_link = None,
			"--out-link" | "-o" => {
				out_link =
					Some(PathBuf::from(args.next().ok_or_else(|| {
						format!("flag '{arg}' requires 1 argument")
					})?))
			}
			"--" => {
				forwarded.push(arg);
				forwarded.extend(args.by_ref());
			}
			_ => forwarded.push(arg),
		}
	}
	let mut instantiate = instantiate::parse_args(forwarded, settings)?;
	if instantiate.eval || instantiate.json || instantiate.xml {
		return Err("evaluation output flags are not supported by nix-build".into());
	}
	instantiate.strict = true;
	Ok(Options {
		instantiate,
		out_link,
	})
}

pub fn run(
	options: &Options,
	cwd: &Path,
	stdout: &mut dyn Write,
	stderr: &mut dyn Write,
) -> ExitCode {
	match build(options, cwd, stdout, stderr) {
		Ok(()) => ExitCode::SUCCESS,
		Err(error) => {
			let _ = writeln!(stderr, "error: {error}");
			ExitCode::FAILURE
		}
	}
}

fn build(
	options: &Options,
	cwd: &Path,
	stdout: &mut dyn Write,
	stderr: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
	let inst = &options.instantiate;
	let root = inst
		.store
		.as_deref()
		.and_then(|uri| uri.strip_prefix("local?root="))
		.filter(|root| {
			!root.is_empty() && !root.contains(['&', '?', '%']) && Path::new(root).is_absolute()
		})
		.ok_or("nix-build requires --store local?root=ABSOLUTE_ROOT")?;
	let store = EvaluatorStore::writable_with_settings(Path::new(root), &inst.settings)?;
	let mut drv_bytes = Vec::new();
	if instantiate::run_for_build(inst, cwd, &mut drv_bytes, Rc::clone(&store)) != ExitCode::SUCCESS
	{
		return Err("evaluation failed".into());
	}
	let local = store.local_store().expect("writable evaluator store");
	let mut settings = BuildSettings::default();
	settings.apply_config(&inst.settings);
	let caches = if settings.use_substitutes {
		inst.settings
			.get_list("substituters", &["https://cache.nixos.org/"])
			.into_iter()
			.map(BinaryCache::open)
			.collect::<nix_rs_store::Result<Vec<_>>>()?
	} else {
		Vec::new()
	};
	let builder = LocalBuilder::new(local, &settings).with_substituters(&caches);
	let mut outputs = BTreeSet::new();
	let mut failures = Vec::new();
	for drv in std::str::from_utf8(&drv_bytes)?.lines() {
		let selection: serde_json::Value = serde_json::from_str(drv)?;
		let drv = selection["drvPath"]
			.as_str()
			.ok_or("missing derivation path")?;
		let output = selection["outputName"]
			.as_str()
			.ok_or("missing output name")?;
		let drv = StorePath::from_absolute_path(drv.as_bytes())?.to_owned();
		let wanted = BTreeSet::from([output.to_owned()]);
		let mut log_error = None;
		let result = builder.realise_outputs(&drv, &wanted, &mut |bytes| {
			if let Err(error) = stderr.write_all(bytes) {
				log_error = Some(error);
			}
		});
		if let Some(error) = log_error {
			return Err(error.into());
		}
		match result {
			Ok(paths) => {
				outputs.insert(
					paths
						.get(output)
						.ok_or("selected derivation output is missing")?
						.clone(),
				);
			}
			Err(error) if settings.keep_going => failures.push(error.to_string()),
			Err(error) => return Err(error.into()),
		}
	}
	for (index, output) in outputs.iter().enumerate() {
		if let Some(link) = &options.out_link {
			let link = if index == 0 {
				link.clone()
			} else {
				PathBuf::from(format!("{}-{}", link.display(), index + 1))
			};
			local.add_perm_root(output, &cwd.join(link))?;
		}
		writeln!(stdout, "{}", output.to_absolute_path())?;
	}
	if !failures.is_empty() {
		return Err(failures.join("\n").into());
	}
	Ok(())
}
