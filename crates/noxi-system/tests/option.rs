//! `noxi system option` evaluating real Nixpkgs modules in-process. Set
//! `NOXI_NIXPKGS` to a Nixpkgs source, as `just nixpkgs-paths` does.

use std::fs;
use std::process::ExitCode;

use noxi_system::option;

fn nixpkgs() -> String {
	std::env::var("NOXI_NIXPKGS").expect("NOXI_NIXPKGS")
}

fn invoke(args: &[&str]) -> (ExitCode, String, String) {
	let (mut out, mut err) = (Vec::new(), Vec::new());
	let code = option::main(
		args.iter().map(|arg| (*arg).to_owned()).collect(),
		&mut out,
		&mut err,
	);
	(
		code,
		String::from_utf8(out).unwrap(),
		String::from_utf8(err).unwrap(),
	)
}

/// A file evaluating one small module set the way a NixOS evaluation
/// returns it: `config`, `options` and `pkgs.lib`.
fn module_file(dir: &std::path::Path) -> String {
	let file = dir.join("system.nix");
	fs::write(
		&file,
		format!(
			r#"let
	lib = import {nixpkgs}/lib;
	eval = lib.evalModules {{
		modules = [
			{{
				options.services.web = {{
					enable = lib.mkEnableOption "the web server";
					port = lib.mkOption {{
						type = lib.types.port;
						default = 80;
						description = "The port to listen on.";
					}};
				}};
				config.services.web.enable = true;
			}}
		];
	}};
in
{{
	inherit (eval) config options;
	pkgs.lib = lib;
}}
"#,
			nixpkgs = nixpkgs()
		),
	)
	.unwrap();
	file.to_string_lossy().into_owned()
}

#[test]
#[ignore = "needs NOXI_NIXPKGS; run with `just nixpkgs-paths`"]
fn renders_an_option_and_a_set_like_nixos_option() {
	let dir = tempfile::tempdir().unwrap();
	let file = module_file(dir.path());

	let (code, out, err) = invoke(&["--file", &file, "services.web.port"]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	assert!(out.starts_with("Value:\n  80\n\nDefault:\n  80\n\nType:\n  16 bit unsigned integer; between 0 and 65535 (both inclusive)\n\nDescription:\n  The port to listen on.\n\nDeclared by:\n"), "{out}");

	let (code, out, _) = invoke(&["--file", &file, "services.web."]);
	assert_eq!(code, ExitCode::SUCCESS);
	assert_eq!(out, "This attribute set contains:\nenable\nport\n");

	let (code, out, _) = invoke(&["--file", &file, "-r", "services.web"]);
	assert_eq!(code, ExitCode::SUCCESS);
	assert_eq!(
		out,
		"services.web.enable = true;\nservices.web.port = 80;\n"
	);

	// The evaluator reports the throw on the process standard error.
	let (code, out, _) = invoke(&["--file", &file, "services.missing"]);
	assert_eq!(code, ExitCode::FAILURE);
	assert!(out.is_empty());
}

#[test]
#[ignore = "needs NOXI_NIXPKGS; run with `just nixpkgs-paths`"]
fn evaluates_nixos_config_from_the_lookup_path() {
	let dir = tempfile::tempdir().unwrap();
	let config = dir.path().join("configuration.nix");
	fs::write(
		&config,
		"{ ... }: { networking.hostName = \"probe\"; boot.loader.grub.enable = false; fileSystems.\"/\".device = \"/dev/vda\"; system.stateVersion = \"26.11\"; }\n",
	)
	.unwrap();
	let (code, out, err) = invoke(&[
		"-I",
		&format!("nixpkgs={}", nixpkgs()),
		"-I",
		&format!("nixos-config={}", config.display()),
		"networking.hostName",
	]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	assert!(
		out.starts_with("Value:\n  \"probe\"\n\nDefault:\n  \"nixos\"\n"),
		"{out}"
	);
	assert!(
		out.contains(&format!("Defined by:\n  {}", config.display())),
		"{out}"
	);
}

#[test]
#[ignore = "needs NOXI_NIXPKGS; run with `just nixpkgs-paths`"]
fn evaluates_a_flake_configuration() {
	let dir = tempfile::tempdir().unwrap();
	// Not a Git work tree, so the flake is read as a `path:` flake.
	fs::write(
		dir.path().join("flake.nix"),
		format!(
			r#"{{
	inputs.nixpkgs.url = "path:{nixpkgs}";
	outputs = {{ nixpkgs, ... }}: {{
		nixosConfigurations.box = nixpkgs.lib.nixosSystem {{
			modules = [
				{{
					networking.hostName = "flaky";
					boot.loader.grub.enable = false;
					fileSystems."/".device = "/dev/vda";
					nixpkgs.hostPlatform = "x86_64-linux";
					system.stateVersion = "26.11";
				}}
			];
		}};
	}};
}}
"#,
			nixpkgs = nixpkgs()
		),
	)
	.unwrap();
	let reference = format!("{}#box", dir.path().display());
	let (code, out, err) = invoke(&["--flake", &reference, "-r", "networking.hostName"]);
	assert_eq!(code, ExitCode::SUCCESS, "{err}");
	assert_eq!(out, "networking.hostName = \"flaky\";\n");
}
