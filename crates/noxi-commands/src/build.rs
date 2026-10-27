//! `nix-build`: instantiate and realize selections through the Rust engine.

use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use noxi_store::DerivedPath;

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
	_stderr: &mut dyn Write,
) -> Result<(), Box<dyn std::error::Error>> {
	let inst = &options.instantiate;
	let _root = inst
		.store
		.as_deref()
		.and_then(|uri| uri.strip_prefix("local?root="))
		.filter(|root| {
			!root.is_empty() && !root.contains(['&', '?', '%']) && Path::new(root).is_absolute()
		})
		.ok_or("nix-build requires --store local?root=ABSOLUTE_ROOT")?;
	let evaluated = instantiate::evaluate(inst, cwd).map_err(|_| "evaluation failed")?;
	let derivations = instantiate::derivations(&evaluated.results)?;
	let store = evaluated
		.store
		.as_ref()
		.ok_or("selected store cannot build derivations")?;
	let targets: Vec<_> = derivations
		.iter()
		.map(|(drv, output)| DerivedPath::Built {
			drv: drv.clone(),
			outputs: BTreeSet::from([output.clone()]),
		})
		.collect();
	store.build_paths(&targets)?;
	let mut outputs = BTreeSet::new();
	for (drv, output) in derivations {
		let paths = store.query_derivation_outputs(&drv)?;
		outputs.insert(
			paths
				.get(&output)
				.ok_or("selected derivation output is missing")?
				.clone(),
		);
	}
	for (index, output) in outputs.iter().enumerate() {
		if let Some(link) = &options.out_link {
			let link = if index == 0 {
				link.clone()
			} else {
				PathBuf::from(format!("{}-{}", link.display(), index + 1))
			};
			store.add_perm_root(output, &cwd.join(link))?;
		}
		writeln!(stdout, "{}", output.to_absolute_path())?;
	}
	Ok(())
}
