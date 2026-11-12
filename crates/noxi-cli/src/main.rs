use std::{env, fs, path::PathBuf, process::ExitCode};

use noxi_eval::{EvalMode, Evaluation};

// The mimalloc global allocator is declared once, in `noxi-commands`, which
// this binary links through `noxi-system`.

fn main() -> ExitCode {
	// The native system operations take their own argument vectors.
	let mut native = env::args().skip(1);
	match native.next().as_deref() {
		Some("flake") => return noxi_system::flake::main(native.collect()),
		Some("system") => return noxi_system::rebuild::main(native.collect()),
		Some("snapshot") => return noxi_system::snapshot::main(native.collect()),
		_ => {}
	}
	let mut args = env::args().skip(1);
	match (args.next().as_deref(), args.next().as_deref(), args.next()) {
		(Some("--version"), None, None) => {
			println!("noxi {} (preview)", env!("CARGO_PKG_VERSION"));
			ExitCode::SUCCESS
		}
		(Some("eval"), Some("--expr"), Some(expression)) if args.next().is_none() => {
			evaluate(&expression, None)
		}
		(Some("eval"), Some("--file"), Some(path)) if args.next().is_none() => {
			let path = PathBuf::from(path);
			match fs::read_to_string(&path) {
				Ok(expression) => evaluate(&expression, Some(path)),
				Err(error) => {
					eprintln!("error: cannot read {}: {error}", path.display());
					ExitCode::FAILURE
				}
			}
		}
		_ => {
			eprintln!(
				"usage: noxi eval --expr EXPRESSION\n       noxi eval --file PATH\n       noxi flake update|lock ...\n       noxi system build|test|boot|switch|dry-activate|rollback ...\n       noxi snapshot list|show|rollback ...\n       noxi --version"
			);
			ExitCode::from(2)
		}
	}
}

fn evaluate(expression: &str, location: Option<PathBuf>) -> ExitCode {
	let result = Evaluation::builder_pure()
		.mode(EvalMode::Strict)
		.build()
		.evaluate(expression, location);
	if !result.errors.is_empty() {
		for error in result.errors {
			error.fancy_format_stderr();
		}
		return ExitCode::FAILURE;
	}

	match result.value {
		Some(value) => {
			println!("{value}");
			ExitCode::SUCCESS
		}
		None => {
			eprintln!("error: evaluation produced no value");
			ExitCode::FAILURE
		}
	}
}
