//! `nix fmt`, `nix formatter run` and `nix formatter build`, following
//! CppNix 2.35.2 `formatter.cc` and `InstallableValue::toApp`.
use std::collections::BTreeSet;
use std::process::ExitCode;

use nix_compat::store_path::StorePath;
use noxi_eval::{NixContextElement, Value};
use noxi_store::DerivedPath;

use super::{Parsed, unsupported};
use crate::cli::Failure;
use crate::instantiate::{self, nix_string_literal};

/// The current directory's flake as `parseInstallable(store, ".")` reads it:
/// its source directory and the evaluated application program.
struct App {
	program: String,
	context: Vec<DerivedPath>,
	store: std::rc::Rc<dyn noxi_store::Store>,
}

fn source_dir(parsed: &Parsed) -> Result<String, Failure> {
	let store = noxi_glue::EvaluatorStore::new();
	let mut config = noxi_glue::fetch::FetchConfig {
		cwd: std::env::current_dir()?,
		..Default::default()
	};
	config.settings.flakes = true;
	store.configure_fetching(config);
	let _ = parsed;
	let reference = noxi_glue::fetch::flake::parse_reference(&store, ".")
		.map_err(|e| Failure::Error(e.to_string()))?;
	Ok(reference
		.input
		.get_source_path()
		.map(|path| path.to_string_lossy().into_owned())
		.unwrap_or_default())
}

fn resolve(parsed: &mut Parsed) -> Result<Result<App, ExitCode>, Failure> {
	if parsed.has("expr") || parsed.has("file") {
		return Err(unsupported(&format!(
			"{} with --expr or --file",
			parsed.command
		)));
	}
	let operands = std::mem::take(&mut parsed.operands);
	let mut options = super::expression::options(parsed, true)?;
	parsed.operands = operands;
	options.strict = true;
	options.read_write_mode = true;
	options.json = false;
	let system = parsed.settings.get("system").unwrap_or("x86_64-linux");
	let attr = format!("formatter.{system}");
	let label = "__noxi_flake_label \".\"";
	options.inputs = vec![format!(
		"let v = __noxi_flake_select (__noxi_flake \".\") [ {} ] ({label}); in if v.type != \"derivation\" then null else let n = v.meta.mainProgram or (v.pname or (builtins.parseDrvName v.name).name); in v.outPath + \"/bin/\" + n",
		nix_string_literal(&attr)
	)];
	let evaluated = match instantiate::evaluate(&options, &std::env::current_dir()?) {
		Ok(value) => value,
		Err(code) => return Ok(Err(code)),
	};
	if let Some(Value::Null) = evaluated.results.first() {
		return Err(Failure::Error(format!(
			"attribute '{attr}' should have type 'derivation'"
		)));
	}
	let Some(Value::String(program)) = evaluated.results.first() else {
		return Err(Failure::Error("formatter program is not a string".into()));
	};
	let store = evaluated
		.store
		.clone()
		.ok_or_else(|| unsupported("formatter without a writable store"))?;
	let mut context = Vec::new();
	for element in program.iter_context().flat_map(|context| context.iter()) {
		let parse =
			|path: &str| StorePath::from_absolute_path(path.as_bytes()).map_err(|e| e.to_string());
		context.push(match element {
			NixContextElement::Plain(path) => DerivedPath::Opaque(parse(path)?),
			NixContextElement::Derivation(path) => DerivedPath::Built {
				drv: parse(path)?,
				outputs: BTreeSet::new(),
			},
			NixContextElement::Single { derivation, name } => DerivedPath::Built {
				drv: parse(derivation)?,
				outputs: BTreeSet::from([name.clone()]),
			},
		});
	}
	let program = std::str::from_utf8(program.as_bytes())
		.map_err(|e| e.to_string())?
		.to_owned();
	Ok(Ok(App {
		program,
		context,
		store,
	}))
}

/// `UnresolvedApp::resolve`: build the context and require a store program.
fn build(parsed: &Parsed, app: &App) -> Result<(), Failure> {
	if parsed.settings.get_bool("print-missing", true) {
		crate::paths::print_missing(app.store.as_ref(), &app.store.query_missing(&app.context)?)?;
	}
	app.store.build_paths(&app.context)?;
	if !app
		.program
		.starts_with(nix_compat::store_path::store_dir_with_slash())
	{
		return Err(Failure::Error(format!(
			"app program '{}' is not in the Nix store",
			app.program
		)));
	}
	Ok(())
}

pub(super) fn run(mut parsed: Parsed) -> Result<ExitCode, Failure> {
	let app = match resolve(&mut parsed)? {
		Ok(app) => app,
		Err(code) => return Ok(code),
	};
	build(&parsed, &app)?;
	if parsed.command == "formatter build" {
		if !parsed.has("no-link") {
			let link = parsed.value("out-link").unwrap_or("result");
			for (index, path) in app.context.iter().enumerate() {
				let outputs = super::expression::outputs(app.store.as_ref(), path)?;
				for (name, output) in outputs {
					let mut symlink = link.to_owned();
					if index > 0 {
						symlink.push_str(&format!("-{index}"));
					}
					if !name.is_empty() && name != "out" {
						symlink.push_str(&format!("-{name}"));
					}
					app.store
						.add_perm_root(&output, &std::env::current_dir()?.join(symlink))?;
				}
			}
		}
		println!("{}", app.program);
		return Ok(ExitCode::SUCCESS);
	}
	let root = source_dir(&parsed)?;
	let mut child = std::process::Command::new(&app.program);
	child.args(&parsed.operands).env("PRJ_ROOT", root);
	super::execution::exec_store(&parsed, child)
}
