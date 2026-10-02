use std::{
	mem::MaybeUninit,
	pin::Pin,
	sync::{
		Arc,
		atomic::{AtomicBool, Ordering},
	},
	task::{self, Poll},
};

use tokio::io::{self, AsyncBufRead, AsyncRead, ErrorKind::InvalidData};

// Required reading for understanding this module.
use crate::{
	nar::{self, wire::PadPar},
	wire::{self, BytesReader},
};

mod read;
#[cfg(test)]
mod test;

pub type Reader<'a> = dyn AsyncBufRead + Unpin + Send + 'a;

/// Start reading a NAR file from `reader`.
pub async fn open<'a, 'r>(reader: &'a mut Reader<'r>) -> io::Result<Node<'a, 'r>> {
	read::token(reader, &nar::wire::TOK_NAR).await?;
	Node::new(reader, Status::top()).await
}

pub enum Node<'a, 'r: 'a> {
	Symlink {
		target: Vec<u8>,
	},
	File {
		executable: bool,
		reader: FileReader<'a, 'r>,
	},
	Directory(DirReader<'a, 'r>),
}

impl<'a, 'r: 'a> Node<'a, 'r> {
	/// Start reading a [Node], matching the next [nar::wire::Node].
	///
	/// Reading the terminating [nar::wire::TOK_PAR] is done immediately for [Node::Symlink],
	/// but is otherwise left to [DirReader] or [FileReader].
	async fn new(reader: &'a mut Reader<'r>, status: Status) -> io::Result<Self> {
		let mut guard = status.guard();
		let node = match read::tag(reader).await? {
			nar::wire::Node::Sym => {
				let target = wire::read_bytes(reader, 1..=nar::wire::MAX_TARGET_LEN).await?;

				if target.contains(&0) {
					return Err(InvalidData.into());
				}

				read::token(reader, &nar::wire::TOK_PAR).await?;
				status.finish();

				Node::Symlink { target }
			}
			tag @ (nar::wire::Node::Reg | nar::wire::Node::Exe) => {
				let inner = BytesReader::new_internal(reader, ..).await?;
				if inner.is_empty() {
					status.finish();
				}
				Node::File {
					executable: tag == nar::wire::Node::Exe,
					reader: FileReader { inner, status },
				}
			}
			nar::wire::Node::Dir => Node::Directory(DirReader::new(reader, status)),
		};
		guard.disarm();
		Ok(node)
	}
}

/// File contents, readable through the [AsyncRead] trait.
///
/// It comes with some caveats:
///  * You must always read the entire file, unless you intend to abandon the entire archive reader.
///  * You must abandon the entire archive reader upon the first error.
///
/// It's fine to read exactly `reader.len()` bytes without ever seeing an explicit EOF.
pub struct FileReader<'a, 'r> {
	inner: BytesReader<&'a mut Reader<'r>, PadPar>,
	status: Status,
}

impl FileReader<'_, '_> {
	pub fn is_empty(&self) -> bool {
		self.len() == 0
	}

	pub fn len(&self) -> u64 {
		self.inner.len()
	}
}

impl AsyncRead for FileReader<'_, '_> {
	fn poll_read(
		self: Pin<&mut Self>,
		cx: &mut task::Context,
		buf: &mut io::ReadBuf,
	) -> Poll<io::Result<()>> {
		let this = self.get_mut();
		this.status.check_poisoned();
		let result = Pin::new(&mut this.inner).poll_read(cx, buf);
		match &result {
			Poll::Ready(Err(_)) => this.status.poison(),
			Poll::Ready(Ok(())) if this.inner.is_empty() => this.status.finish(),
			_ => {}
		}
		result
	}
}

impl AsyncBufRead for FileReader<'_, '_> {
	fn poll_fill_buf(self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<io::Result<&[u8]>> {
		let this = self.get_mut();
		this.status.check_poisoned();
		let result = Pin::new(&mut this.inner).poll_fill_buf(cx);
		if let Poll::Ready(Err(_)) = &result {
			this.status.poison();
		}
		result
	}

	fn consume(self: Pin<&mut Self>, amt: usize) {
		let this = self.get_mut();
		this.status.check_poisoned();
		Pin::new(&mut this.inner).consume(amt);
		if this.inner.is_empty() {
			this.status.finish();
		}
	}
}

/// A directory iterator, yielding a sequence of [Node]s.
/// It must be fully consumed before reading further from the [DirReader] that produced it, if any.
pub struct DirReader<'a, 'r> {
	reader: &'a mut Reader<'r>,
	status: Status,
	/// Previous directory entry name.
	/// We have to hang onto this to enforce name monotonicity.
	prev_name: Vec<u8>,
}

pub struct Entry<'a, 'r> {
	pub name: &'a [u8],
	pub node: Node<'a, 'r>,
}

impl<'a, 'r> DirReader<'a, 'r> {
	fn new(reader: &'a mut Reader<'r>, status: Status) -> Self {
		Self {
			reader,
			status,
			prev_name: vec![],
		}
	}

	/// Read the next [Entry] from the directory.
	///
	/// We explicitly don't implement [Iterator], since treating this as
	/// a regular Rust iterator will surely lead you astray.
	///
	///  * You must always consume the entire iterator, unless you abandon the entire archive reader.
	///  * You must abandon the entire archive reader on the first error.
	///  * You must abandon the directory reader upon the first [None].
	///  * Even if you know the amount of elements up front, you must keep reading until you encounter [None].
	pub async fn next(&mut self) -> io::Result<Option<Entry<'_, 'r>>> {
		self.status.check();
		let mut guard = self.status.guard();
		// COME FROM the previous iteration: if we've already read an entry,
		// read its terminating TOK_PAR here.
		if !self.prev_name.is_empty() {
			read::token(self.reader, &nar::wire::TOK_PAR).await?;
		}

		if let nar::wire::Entry::None = read::tag(self.reader).await? {
			self.status.finish();
			guard.disarm();
			return Ok(None);
		}

		let mut name = [MaybeUninit::uninit(); nar::wire::MAX_NAME_LEN + 1];
		let name =
			wire::read_bytes_buf(self.reader, &mut name, 1..=nar::wire::MAX_NAME_LEN).await?;

		if name.contains(&0) || name.contains(&b'/') || name == b"." || name == b".." {
			return Err(InvalidData.into());
		}

		// Enforce strict monotonicity of directory entry names.
		if &self.prev_name[..] >= name {
			return Err(InvalidData.into());
		}

		self.prev_name.clear();
		self.prev_name.extend_from_slice(name);

		read::token(self.reader, &nar::wire::TOK_NOD).await?;

		let node = Node::new(self.reader, self.status.child()).await?;
		guard.disarm();
		Ok(Some(Entry {
			name: &self.prev_name,
			node,
		}))
	}
}

/// Shared error state and per-node readiness. These checks also apply in release builds.
/// A dropped child leaves its parent blocked until the archive is abandoned.
struct Status {
	poisoned: Arc<AtomicBool>,
	ready: Arc<AtomicBool>,
	parent_ready: Option<Arc<AtomicBool>>,
}

impl Status {
	fn top() -> Self {
		Self {
			poisoned: Arc::new(AtomicBool::new(false)),
			ready: Arc::new(AtomicBool::new(true)),
			parent_ready: None,
		}
	}

	fn child(&self) -> Self {
		self.ready.store(false, Ordering::Relaxed);
		Self {
			poisoned: self.poisoned.clone(),
			ready: Arc::new(AtomicBool::new(true)),
			parent_ready: Some(self.ready.clone()),
		}
	}

	fn finish(&self) {
		self.ready.store(false, Ordering::Relaxed);
		if let Some(parent) = &self.parent_ready {
			parent.store(true, Ordering::Relaxed);
		}
	}

	fn poison(&self) {
		self.poisoned.store(true, Ordering::Relaxed);
	}

	fn check_poisoned(&self) {
		assert!(
			!self.poisoned.load(Ordering::Relaxed),
			"Archive reader used after it was meant to be abandoned!"
		);
	}

	fn check(&self) {
		self.check_poisoned();
		assert!(
			self.ready.load(Ordering::Relaxed),
			"Non-ready archive reader used! (Should've been reading from something else)"
		);
	}

	fn guard(&self) -> PoisonGuard {
		PoisonGuard(Some(self.poisoned.clone()))
	}
}

/// Errors and cancellation both leave a partially parsed node unusable.
struct PoisonGuard(Option<Arc<AtomicBool>>);

impl PoisonGuard {
	fn disarm(&mut self) {
		self.0 = None;
	}
}

impl Drop for PoisonGuard {
	fn drop(&mut self) {
		if let Some(poisoned) = &self.0 {
			poisoned.store(true, Ordering::Relaxed);
		}
	}
}
