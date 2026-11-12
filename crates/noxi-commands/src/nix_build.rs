//! `nix-build`, following CppNix 2.35.2's `src/nix/nix-build/nix-build.cc`
//! without its `nix-shell` mode.
//!
//! It accepts `nix-instantiate`'s evaluation arguments plus `--out-link`/`-o`,
//! `--no-out-link`, `--dry-run` and the obsolete `--indirect`,
//! `--add-drv-link` and `--drv-link`. Each derivation found is built for its
//! `outputName`, made a GC root named after the out link, and its output path
//! printed. `--check`, `--repair` and store derivation arguments fail
//! explicitly.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::path::Path;
use std::process::ExitCode;

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use noxi_store::DerivedPath;

use crate::cli::{Failure, print_setting_warnings};
use crate::instantiate::{self, absolute_path};
use crate::paths::print_missing;

/// Run `nix-build` with the arguments after the program name.
pub fn main(settings: Settings, args: Vec<String>) -> Result<ExitCode, Failure> {
	let mut out_link = Some("./result".to_owned());
	let mut dry_run = false;
	let mut unsupported = None;
	let mut options = instantiate::parse_args_with(args, settings, |flag, rest| {
		let value = |rest: &mut dyn Iterator<Item = String>| {
			rest.next()
				.ok_or_else(|| format!("flag '{flag}' requires 1 argument"))
		};
		Some(match flag {
			"--version" => {
				println!("nix-build (Nix) 2.35.2");
				std::process::exit(0);
			}
			"--add-drv-link" | "--indirect" => Ok(()),
			"--drv-link" => value(rest).map(drop),
			"--no-out-link" | "--no-link" => {
				out_link = None;
				Ok(())
			}
			"--out-link" | "-o" => value(rest).map(|link| out_link = Some(link)),
			"--dry-run" => {
				dry_run = true;
				Ok(())
			}
			"--check" | "--repair" | "--help" | "--run-env" | "--pure" | "--impure" => {
				unsupported = Some(flag.to_owned());
				Ok(())
			}
			"--eval" | "--eval-only" | "--strict" | "--json" | "--xml" | "--add-root"
			| "--find-file" => Err(format!("unrecognised flag '{flag}'")),
			_ => return None,
		})
	})
	.map_err(Failure::Usage)?;
	if let Some(flag) = unsupported {
		return Err(Failure::Error(format!(
			"nix-build {flag} is not supported by noxi yet"
		)));
	}
	print_setting_warnings(&mut options.settings);
	if !options.from_args && options.inputs.is_empty() {
		options.inputs.push(".".to_owned());
	}
	for input in &options.inputs {
		let path = input.split('!').next().unwrap_or(input);
		if !options.from_args
			&& path.starts_with(nix_compat::store_path::store_dir_with_slash())
			&& path.ends_with(".drv")
		{
			return Err(Failure::Error(
				"nix-build with store derivation arguments is not supported by noxi yet".to_owned(),
			));
		}
	}
	let cwd = std::env::current_dir()?;
	let evaluated = match instantiate::evaluate(&options, &cwd) {
		Ok(evaluated) => evaluated,
		Err(code) => return Ok(code),
	};
	let derivations = instantiate::derivations(&evaluated.results)?;
	let store = evaluated
		.store
		.ok_or_else(|| Failure::Error("the selected store cannot build derivations".to_owned()))?;

	let paths: Vec<DerivedPath> = derivations
		.iter()
		.map(|(drv, output)| DerivedPath::Built {
			drv: drv.clone(),
			outputs: BTreeSet::from([output.clone()]),
		})
		.collect();
	print_missing(store.as_ref(), &store.query_missing(&paths)?)?;
	if dry_run {
		return Ok(ExitCode::SUCCESS);
	}
	store.build_paths(&paths)?;

	// `--no-out-link` makes the roots in a temporary directory that is
	// removed afterwards, as CppNix does.
	let temp = match &out_link {
		Some(_) => None,
		None => Some(tempfile::Builder::new().prefix("nix-build-").tempdir()?),
	};
	let out_link = match (&out_link, &temp) {
		(Some(link), _) => absolute_path(&cwd, Path::new(link)),
		(None, Some(temp)) => temp.path().join("result"),
		(None, None) => unreachable!(),
	};
	let mut counters: BTreeMap<StorePath, usize> = BTreeMap::new();
	let mut out_paths = Vec::new();
	for (drv, output) in &derivations {
		let next = counters.len();
		let counter = *counters.entry(drv.clone()).or_insert(next);
		let mut link = out_link.clone().into_os_string();
		if counter > 0 {
			link.push(format!("-{}", counter + 1));
		}
		if output != "out" {
			link.push(format!("-{output}"));
		}
		let out_path = store
			.query_derivation_outputs(drv)?
			.remove(output)
			.ok_or_else(|| {
				Failure::Error(format!(
					"derivation '{}' does not have an output named '{output}'",
					drv.to_absolute_path()
				))
			})?;
		store.add_perm_root(&out_path, Path::new(&link))?;
		out_paths.push(out_path);
	}
	let mut stdout = io::stdout().lock();
	for path in out_paths {
		writeln!(stdout, "{}", path.to_absolute_path())?;
	}
	Ok(ExitCode::SUCCESS)
}
