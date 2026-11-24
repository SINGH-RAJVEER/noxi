mod app;
mod askpass;
mod delta;
mod keyboard;
mod model;
mod native;
mod nixos;
mod options;
mod repository;
mod theme;
mod ui;

fn main() -> std::process::ExitCode {
	native::main()
}
