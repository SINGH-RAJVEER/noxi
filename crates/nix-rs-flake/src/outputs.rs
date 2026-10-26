use super::*;
use nix_compat::store_path::StorePath;
use nix_rs_eval::NixAttrs;
use nix_rs_store::DerivedPath;
use std::future::Future;
use std::pin::Pin;

type LocalFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ErrorKind>> + 'a>>;
fn local_system() -> String {
	format!(
		"{}-{}",
		std::env::consts::ARCH,
		if cfg!(target_os = "macos") {
			"darwin"
		} else {
			std::env::consts::OS
		}
	)
}
#[derive(Clone, Debug)]
pub struct ShowOptions {
	pub system: String,
	pub all_systems: bool,
	pub legacy: bool,
}
impl Default for ShowOptions {
	fn default() -> Self {
		Self {
			system: local_system(),
			all_systems: false,
			legacy: false,
		}
	}
}
#[derive(Debug)]
pub struct ShowResult {
	pub json: Json,
	pub warnings: Vec<String>,
}
#[derive(Clone, Debug)]
pub struct CheckOptions {
	pub system: String,
	pub all_systems: bool,
	pub build: bool,
}
impl Default for CheckOptions {
	fn default() -> Self {
		Self {
			system: local_system(),
			all_systems: false,
			build: true,
		}
	}
}
#[derive(Debug, Default)]
pub struct CheckReport {
	pub derivations: BTreeMap<String, String>,
	pub check_builds: BTreeSet<String>,
	pub omitted_systems: BTreeSet<String>,
	pub warnings: Vec<String>,
}
async fn attrs(co: &GenCo, value: Value) -> Result<NixAttrs, ErrorKind> {
	generators::request_force(co, value).await.to_attrs()
}
async fn text(co: &GenCo, value: Value) -> Result<String, ErrorKind> {
	Ok(generators::request_force(co, value)
		.await
		.to_contextful_str()?
		.to_str()?
		.to_owned())
}
async fn derivation(co: &GenCo, value: &Value) -> Result<bool, ErrorKind> {
	let value = generators::request_force(co, value.clone()).await;
	let Value::Attrs(attrs) = value else {
		return Ok(false);
	};
	match attrs.select("type") {
		Some(value) => Ok(text(co, value.clone()).await? == "derivation"),
		None => Ok(false),
	}
}
async fn description(co: &GenCo, value: &Value) -> Result<Option<String>, ErrorKind> {
	let fields = attrs(co, value.clone()).await?;
	let Some(meta) = fields.select("meta") else {
		return Ok(None);
	};
	let meta = attrs(co, meta.clone()).await?;
	match meta.select("description") {
		Some(value) => Ok(Some(text(co, value.clone()).await?)),
		None => Ok(None),
	}
}
fn content_folder(path: &[String]) -> bool {
	let Some(first) = path.first().map(String::as_str) else {
		return false;
	};
	matches!(
		first,
		"apps" | "checks" | "devShells" | "legacyPackages" | "packages"
	) && path.len() <= 2
		|| matches!(
			first,
			"formatter" | "nixosConfigurations" | "nixosModules" | "overlays"
		) && path.len() == 1
}
fn has_content<'a>(co: &'a GenCo, value: Value, path: Vec<String>) -> LocalFuture<'a, bool> {
	Box::pin(async move {
		if !content_folder(&path) {
			return Ok(true);
		}
		for (name, child) in attrs(co, value).await?.iter_sorted() {
			let mut path = path.clone();
			path.push(name.to_str()?.to_owned());
			if has_content(co, child.clone(), path).await? {
				return Ok(true);
			}
		}
		Ok(false)
	})
}
fn show_node<'a>(
	co: &'a GenCo,
	value: Value,
	path: Vec<String>,
	options: &'a ShowOptions,
	warnings: &'a mut Vec<String>,
) -> LocalFuture<'a, Json> {
	Box::pin(async move {
		let depth = path.len();
		let first = path.first().map(String::as_str).unwrap_or("");
		let system_derivation = depth == 2
			&& matches!(first, "defaultPackage" | "devShell" | "formatter")
			|| depth == 3 && matches!(first, "checks" | "packages" | "devShells");
		let legacy = first == "legacyPackages";
		if system_derivation || legacy && depth > 1 {
			if legacy && !options.legacy {
				warnings.push(format!(
					"{} omitted (use '--legacy' to show)",
					path.join(".")
				));
				return Ok(json!({}));
			}
			if !options.all_systems && path[1] != options.system {
				warnings.push(format!(
					"{} omitted (use '--all-systems' to show)",
					path.join(".")
				));
				return Ok(json!({}));
			}
		}
		if system_derivation || first == "hydraJobs" || legacy && depth > 1 {
			if derivation(co, &value).await? {
				let name = text(
					co,
					attrs(co, value.clone())
						.await?
						.select_required("name")?
						.clone(),
				)
				.await?;
				return Ok(
					json!({"type":"derivation","name":name,"description":description(co,&value).await?.unwrap_or_default()}),
				);
			}
			if system_derivation {
				warnings.push(format!("{} is not a derivation", path.join(".")));
				return Ok(json!({}));
			}
			if legacy && depth > 2 {
				return Ok(json!({}));
			}
		}
		let recurse = depth == 0
			|| depth == 1
				&& matches!(
					first,
					"defaultPackage"
						| "devShell" | "formatter"
						| "nixosConfigurations"
						| "nixosModules" | "defaultApp"
						| "templates" | "overlays"
				) || (depth == 1 || depth == 2)
			&& matches!(first, "checks" | "packages" | "devShells" | "apps")
			|| first == "hydraJobs"
			|| legacy && depth <= 2;
		if recurse {
			let mut result = serde_json::Map::new();
			for (name, child) in attrs(co, value).await?.iter_sorted() {
				let mut child_path = path.clone();
				let name = name.to_str()?.to_owned();
				child_path.push(name.clone());
				if has_content(co, child.clone(), child_path.clone()).await? {
					result.insert(
						name,
						show_node(co, child.clone(), child_path, options, warnings).await?,
					);
				}
			}
			return Ok(Json::Object(result));
		}
		if depth == 2 && first == "defaultApp" || depth == 3 && first == "apps" {
			let app = attrs(co, value.clone()).await?;
			let Some(kind) = app.select("type") else {
				return Err(error("not an app definition"));
			};
			if text(co, kind.clone()).await? != "app" {
				return Err(error("not an app definition"));
			}
			let mut app = json!({"type":"app"});
			if let Some(description) = description(co, &value).await? {
				app["description"] = description.into();
			}
			return Ok(app);
		}
		if depth == 1 && first == "defaultTemplate" || depth == 2 && first == "templates" {
			return Ok(
				json!({"type":"template","description":text(co,attrs(co,value).await?.select_required("description")?.clone()).await?}),
			);
		}
		let kind = if depth == 1 && first == "overlay" || depth == 2 && first == "overlays" {
			"nixpkgs-overlay"
		} else if depth == 2 && first == "nixosConfigurations" {
			"nixos-configuration"
		} else if depth == 1 && first == "nixosModule" || depth == 2 && first == "nixosModules" {
			"nixos-module"
		} else {
			"unknown"
		};
		Ok(json!({"type":kind}))
	})
}

/// Project conventional output namespaces without serializing arbitrary outputs.
/// Omissions and non-derivation warnings are returned for the caller to render.
pub async fn show(
	store: &Rc<EvaluatorStore>,
	co: &GenCo,
	flake: &LockedFlake,
	options: &ShowOptions,
) -> Result<ShowResult, ErrorKind> {
	let value = flake_outputs(store, co, flake).await?;
	let mut warnings = Vec::new();
	let json = show_node(co, value, Vec::new(), options, &mut warnings).await?;
	Ok(ShowResult { json, warnings })
}

async fn check_derivation(
	co: &GenCo,
	value: Value,
	path: &str,
	report: &mut CheckReport,
	build: bool,
) -> Result<(), ErrorKind> {
	if !derivation(co, &value).await? {
		return Err(error(format!(
			"flake attribute '{path}' is not a derivation"
		)));
	}
	let drv = text(
		co,
		attrs(co, value).await?.select_required("drvPath")?.clone(),
	)
	.await?;
	report.derivations.insert(path.into(), drv.clone());
	if build {
		report.check_builds.insert(drv);
	}
	Ok(())
}
async fn check_app(
	co: &GenCo,
	value: Value,
	path: &str,
	report: &mut CheckReport,
) -> Result<(), ErrorKind> {
	let app = attrs(co, value).await?;
	for key in ["type", "program"] {
		let value = app
			.select(key)
			.ok_or_else(|| error(format!("app '{path}' lacks attribute '{key}'")))?;
		text(co, value.clone()).await?;
	}
	if let Some(meta) = app.select("meta") {
		let meta = attrs(co, meta.clone()).await?;
		if let Some(value) = meta.select("description") {
			text(co, value.clone()).await?;
		} else {
			report
				.warnings
				.push(format!("app '{path}' lacks attribute 'meta.description'"));
		}
	} else {
		report
			.warnings
			.push(format!("app '{path}' lacks attribute 'meta'"));
	}
	for (key, _) in app.iter_sorted() {
		let key = key.to_str()?;
		if !matches!(key, "type" | "program" | "meta") {
			return Err(error(format!(
				"app '{path}' has unsupported attribute '{key}'"
			)));
		}
	}
	Ok(())
}
async fn check_template(co: &GenCo, value: Value, path: &str) -> Result<(), ErrorKind> {
	let template = attrs(co, value).await?;
	let target = template
		.select("path")
		.ok_or_else(|| error(format!("template '{path}' lacks attribute 'path'")))?
		.clone();
	let exists = generators::request_builtin(co, "pathExists").await;
	let exists = generators::request_call_with(co, exists, [target.clone()]).await;
	if !generators::request_force(co, exists).await.as_bool()? {
		let stringify = generators::request_builtin(co, "toString").await;
		let target = generators::request_call_with(co, stringify, [target]).await;
		return Err(error(format!(
			"template '{path}' refers to a non-existent path '{}'",
			text(co, target).await?
		)));
	}
	text(
		co,
		template
			.select("description")
			.ok_or_else(|| error(format!("template '{path}' lacks attribute 'description'")))?
			.clone(),
	)
	.await?;
	for (key, _) in template.iter_sorted() {
		let key = key.to_str()?;
		if !matches!(key, "path" | "description" | "welcomeText") {
			return Err(error(format!(
				"template '{path}' has unsupported attribute '{key}'"
			)));
		}
	}
	Ok(())
}
async fn check_overlay(co: &GenCo, value: Value) -> Result<(), ErrorKind> {
	let value = generators::request_force(co, value).await;
	let Value::Closure(closure) = value else {
		return Err(error(format!(
			"overlay is not a function, but {} instead",
			value.type_of()
		)));
	};
	let lambda = closure.lambda();
	let name = lambda.parameter_name().unwrap_or("");
	if !(name == "final" || name == "_" || name == "_final") {
		return Err(error("overlay does not take an argument named 'final'"));
	}
	Ok(())
}
fn check_hydra<'a>(
	co: &'a GenCo,
	value: Value,
	path: String,
	report: &'a mut CheckReport,
) -> LocalFuture<'a, ()> {
	Box::pin(async move {
		if derivation(co, &value).await? {
			return Err(error("jobset should not be a derivation at top-level"));
		}
		for (name, value) in attrs(co, value).await?.iter_sorted() {
			let path = format!("{path}.{}", name.to_str()?);
			if derivation(co, value).await? {
				check_derivation(co, value.clone(), &path, report, false).await?;
			} else {
				check_hydra(co, value.clone(), path, report).await?;
			}
		}
		Ok(())
	})
}

/// Validate conventional output schemas and optionally realize local-system
/// `checks` through the caller's store owner. Return warnings and build targets.
pub async fn check(
	store: &Rc<EvaluatorStore>,
	co: &GenCo,
	flake: &LockedFlake,
	options: &CheckOptions,
) -> Result<CheckReport, ErrorKind> {
	let value = flake_outputs(store, co, flake).await?;
	let mut report = CheckReport::default();
	for (name, value) in attrs(co, value).await?.iter_sorted() {
		let name = name.to_str()?;
		let value = generators::request_force(co, value.clone()).await;
		let replacement = match name {
			"defaultPackage" => "packages.<system>.default",
			"defaultApp" => "apps.<system>.default",
			"defaultTemplate" => "templates.default",
			"defaultBundler" => "bundlers.<system>.default",
			"overlay" => "overlays.default",
			"devShell" => "devShells.<system>.default",
			"nixosModule" => "nixosModules.default",
			_ => "",
		};
		if !replacement.is_empty() {
			report.warnings.push(format!(
				"flake output attribute '{name}' is deprecated; use '{replacement}' instead"
			));
		}
		match name {
			"checks" | "packages" | "devShells" | "formatter" | "apps" | "defaultApp"
			| "defaultPackage" | "devShell" | "legacyPackages" | "defaultBundler" | "bundlers" => {
				for (system, output) in attrs(co, value).await?.iter_sorted() {
					let system = system.to_str()?;
					if !system.contains('-') {
						return Err(error(format!("'{system}' is not a valid system type")));
					}
					if !options.all_systems && system != options.system {
						report.omitted_systems.insert(system.into());
						continue;
					}
					if name == "legacyPackages" {
						continue;
					}
					let prefix = format!("{name}.{system}");
					let leaves = if matches!(
						name,
						"defaultPackage"
							| "devShell" | "formatter"
							| "defaultApp" | "defaultBundler"
					) {
						vec![(prefix, output.clone())]
					} else {
						attrs(co, output.clone())
							.await?
							.iter_sorted()
							.map(|(name, value)| {
								Ok((format!("{prefix}.{}", name.to_str()?), value.clone()))
							})
							.collect::<Result<Vec<_>, ErrorKind>>()?
					};
					for (path, value) in leaves {
						if matches!(name, "apps" | "defaultApp") {
							check_app(co, value, &path, &mut report).await?;
						} else if matches!(name, "bundlers" | "defaultBundler") {
							if !matches!(
								generators::request_force(co, value).await,
								Value::Closure(_)
							) {
								return Err(error("bundler must be a function"));
							}
						} else {
							check_derivation(
								co,
								value,
								&path,
								&mut report,
								name == "checks" && system == options.system,
							)
							.await?;
						}
					}
				}
			}
			"overlay" => check_overlay(co, value).await?,
			"overlays" => {
				for (_, value) in attrs(co, value).await?.iter_sorted() {
					check_overlay(co, value.clone()).await?;
				}
			}
			"nixosModule" => {}
			"nixosModules" => {
				for (_, value) in attrs(co, value).await?.iter_sorted() {
					generators::request_force(co, value.clone()).await;
				}
			}
			"nixosConfigurations" => {
				for (_, value) in attrs(co, value).await?.iter_sorted() {
					let mut value = value.clone();
					for key in ["config", "system", "build", "toplevel"] {
						value = attrs(co, value).await?.select_required(key)?.clone();
					}
					if !derivation(co, &value).await? {
						return Err(error(
							"attribute 'config.system.build.toplevel' is not a derivation",
						));
					}
				}
			}
			"hydraJobs" => check_hydra(co, value, name.into(), &mut report).await?,
			"defaultTemplate" => check_template(co, value, name).await?,
			"templates" => {
				for (key, value) in attrs(co, value).await?.iter_sorted() {
					check_template(co, value.clone(), &format!("{name}.{}", key.to_str()?)).await?;
				}
			}
			"lib"
			| "darwinConfigurations"
			| "darwinModules"
			| "flakeModule"
			| "flakeModules"
			| "herculesCI"
			| "homeConfigurations"
			| "homeModule"
			| "homeModules"
			| "nixopsConfigurations" => {}
			_ => report
				.warnings
				.push(format!("unknown flake output '{name}'")),
		}
	}
	if options.build && !report.check_builds.is_empty() {
		let targets = report
			.check_builds
			.iter()
			.map(|path| {
				Ok(DerivedPath::Built {
					drv: StorePath::from_absolute_path(path.as_bytes()).map_err(error)?,
					outputs: BTreeSet::new(),
				})
			})
			.collect::<Result<Vec<_>, ErrorKind>>()?;
		store
			.store()
			.ok_or_else(|| error("flake check builds require a writable store"))?
			.build_paths(&targets)
			.map_err(error)?;
	}
	Ok(report)
}
