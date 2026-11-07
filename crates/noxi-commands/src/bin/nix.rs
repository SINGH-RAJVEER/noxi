use std::process::ExitCode;

use noxi_commands::{
	cli, collect_garbage, instantiate, legacy_environment, nix, nix_build, nix_env, nix_shell,
	nix_store,
};

fn main() -> ExitCode {
	if std::env::args().nth(1).as_deref() == Some("__noxi_exec_in_store") {
		return nix::execution::helper(std::env::args().skip(2).collect());
	}
	// Like CppNix, the executable is also each legacy command it is
	// installed as, selected by its name without extension.
	let program = std::env::args().next().unwrap_or_default();
	let name = std::path::Path::new(&program)
		.file_name()
		.map(|name| name.to_string_lossy().into_owned())
		.unwrap_or_default();
	let name = name
		.rsplit_once('.')
		.map_or(name.as_str(), |(stem, _)| stem);
	// `nix __build-remote` runs the build hook, `build-remote`.
	let name = if std::env::args().nth(1).as_deref() == Some("__build-remote") {
		"build-remote"
	} else {
		name
	};
	match name {
		"nix-hash" => cli::main("nix-hash", nix::hash::legacy),
		"nix-build" => cli::main("nix-build", nix_build::main),
		"nix-store" => cli::main("nix-store", nix_store::main),
		"nix-env" => cli::main("nix-env", nix_env::main),
		"nix-collect-garbage" => cli::main("nix-collect-garbage", collect_garbage::main),
		"nix-shell" => cli::main("nix-shell", nix_shell::main),
		"nix-instantiate" => cli::main("nix-instantiate", instantiate::main),
		"nix-copy-closure" => cli::main("nix-copy-closure", legacy_environment::copy_closure),
		// The other legacy commands the reference registers.
		"nix-channel" | "nix-prefetch-url" | "nix-daemon" | "build-remote" => {
			cli::print_error(&format!("{name} is not supported by noxi yet"));
			ExitCode::FAILURE
		}
		_ => cli::main("nix", nix::main),
	}
}
