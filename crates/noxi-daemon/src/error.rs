use std::io;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// An error reported to the client as CppNix reports `nix::Error`.
	#[error("{0}")]
	Nix(String),

	/// The client sent something that is not the protocol.
	#[error("{0}")]
	Protocol(String),

	/// The client closed the connection.
	#[error("unexpected end-of-file")]
	EndOfFile,

	#[error(transparent)]
	Io(#[from] io::Error),

	#[error(transparent)]
	Store(#[from] noxi_store::Error),

	#[error(transparent)]
	Build(#[from] noxi_build::BuildError),
}
