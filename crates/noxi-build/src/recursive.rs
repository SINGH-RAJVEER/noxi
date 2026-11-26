//! Recursive Nix, following CppNix 2.35.2's `DerivationBuilderImpl::startDaemon`
//! (`unix/build/derivation-builder.cc`) and `RestrictionContext`
//! (`restricted-store.hh`).
//!
//! A derivation that requires the `recursive-nix` system feature gets a
//! worker-protocol socket, `.nix-socket`, in its build directory, which
//! only the builder can open (mode 0600, owned by the build user, in the
//! builder's private build directory), and `NIX_REMOTE` naming it. Each
//! connection is served as an untrusted client of a restricted view of the
//! store: only the input closure of the build and the paths added or built
//! through the socket can be queried, read or built. The server lives in
//! `noxi-daemon`, which depends on this crate, so the program registers it
//! with [`set_server`].

use std::collections::BTreeSet;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, OnceLock};

use nix_compat::store_path::StorePath;
use noxi_store::LocalStore;

use crate::settings::BuildSettings;

/// The paths a recursive build may see, `RestrictionContext`.
pub struct Restriction {
	/// `originalPaths`: the input closure of the build.
	inputs: BTreeSet<StorePath>,
	state: Mutex<State>,
	/// `addDependencyImpl`: makes an added path visible to the builder.
	/// Unsandboxed builders see the whole store, so this does nothing then.
	add_to_sandbox: Option<AddToSandbox>,
}

/// Makes a store path visible inside the builder's sandbox.
pub type AddToSandbox = Box<dyn Fn(&StorePath) -> Result<(), String> + Send + Sync>;

#[derive(Default)]
struct State {
	added_paths: BTreeSet<StorePath>,
	added_drv_outputs: BTreeSet<(StorePath, String)>,
}

impl Restriction {
	pub fn new(inputs: BTreeSet<StorePath>, add_to_sandbox: Option<AddToSandbox>) -> Self {
		Self {
			inputs,
			state: Mutex::default(),
			add_to_sandbox,
		}
	}

	fn state(&self) -> std::sync::MutexGuard<'_, State> {
		self.state.lock().unwrap_or_else(|e| e.into_inner())
	}

	/// `originalPaths`.
	pub fn original_paths(&self) -> &BTreeSet<StorePath> {
		&self.inputs
	}

	/// The paths added through the socket so far.
	pub fn added_paths(&self) -> BTreeSet<StorePath> {
		self.state().added_paths.clone()
	}

	/// `isAllowed(StorePath)`: an input or an added path.
	pub fn is_allowed(&self, path: &StorePath) -> bool {
		self.inputs.contains(path) || self.state().added_paths.contains(path)
	}

	/// `isAllowed(DrvOutput)`: an output built through the socket.
	pub fn is_allowed_output(&self, drv: &StorePath, output: &str) -> bool {
		self.state()
			.added_drv_outputs
			.contains(&(drv.clone(), output.to_owned()))
	}

	/// `addDependency`: allow `path` and make it visible to the builder.
	pub fn add_dependency(&self, path: &StorePath) -> Result<(), String> {
		if self.is_allowed(path) {
			return Ok(());
		}
		if let Some(add) = &self.add_to_sandbox {
			add(path)?;
		}
		self.state().added_paths.insert(path.clone());
		Ok(())
	}

	/// Record an output built through the socket.
	pub fn add_drv_output(&self, drv: &StorePath, output: &str) {
		self.state()
			.added_drv_outputs
			.insert((drv.clone(), output.to_owned()));
	}
}

/// What a connection to a recursive build's socket is served with.
pub struct Context<'a> {
	pub store: &'a LocalStore,
	pub settings: &'a BuildSettings,
	/// The substituters' URLs.
	pub substituters: Vec<String>,
	pub restriction: Arc<Restriction>,
}

/// Serves one connection: `daemon::processConnection` with `NotTrusted`
/// and `Recursive`.
pub type Server = fn(UnixStream, &Context<'_>);

static SERVER: OnceLock<Server> = OnceLock::new();

/// Register the worker-protocol server for recursive builds.
pub fn set_server(server: Server) {
	let _ = SERVER.set(server);
}

pub(crate) fn server() -> Option<Server> {
	SERVER.get().copied()
}

/// The socket's name in the build directory.
pub(crate) const SOCKET_NAME: &str = ".nix-socket";
