//! Git object framing and tree entries used by CppNix's `git-hashing` mode.

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObjectKind {
	Blob,
	Tree,
}

impl ObjectKind {
	fn bytes(self) -> &'static [u8] {
		match self {
			Self::Blob => b"blob",
			Self::Tree => b"tree",
		}
	}
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Object<'a> {
	pub kind: ObjectKind,
	pub body: &'a [u8],
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TreeEntry {
	pub mode: u32,
	pub name: Vec<u8>,
	pub hash: Vec<u8>,
}

#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum Error {
	#[error("invalid Git object header")]
	Header,
	#[error("Git object body length does not match its header")]
	Length,
	#[error("invalid Git tree entry")]
	TreeEntry,
}

pub fn parse_object(bytes: &[u8]) -> Result<Object<'_>, Error> {
	let (header, body) = split_once(bytes, 0).ok_or(Error::Header)?;
	let (kind, length) = split_once(header, b' ').ok_or(Error::Header)?;
	let kind = match kind {
		b"blob" => ObjectKind::Blob,
		b"tree" => ObjectKind::Tree,
		_ => return Err(Error::Header),
	};
	if length.is_empty() || (length.len() > 1 && length[0] == b'0') {
		return Err(Error::Header);
	}
	let mut expected = 0usize;
	for byte in length {
		if !byte.is_ascii_digit() {
			return Err(Error::Header);
		}
		expected = expected
			.checked_mul(10)
			.and_then(|n| n.checked_add((byte - b'0') as usize))
			.ok_or(Error::Header)?;
	}
	if body.len() != expected {
		return Err(Error::Length);
	}
	Ok(Object { kind, body })
}

pub fn encode_object(kind: ObjectKind, body: &[u8]) -> Vec<u8> {
	let mut bytes = Vec::with_capacity(kind.bytes().len() + 1 + 20 + 1 + body.len());
	bytes.extend_from_slice(kind.bytes());
	bytes.push(b' ');
	bytes.extend_from_slice(body.len().to_string().as_bytes());
	bytes.push(0);
	bytes.extend_from_slice(body);
	bytes
}

pub fn parse_tree(body: &[u8], hash_len: usize) -> Result<Vec<TreeEntry>, Error> {
	if hash_len == 0 {
		return Err(Error::TreeEntry);
	}
	let mut remaining = body;
	let mut entries = Vec::new();
	while !remaining.is_empty() {
		let (mode, rest) = split_once(remaining, b' ').ok_or(Error::TreeEntry)?;
		let (name, rest) = split_once(rest, 0).ok_or(Error::TreeEntry)?;
		if name.is_empty() || name.contains(&b'/') || mode.is_empty() {
			return Err(Error::TreeEntry);
		}
		let mode = std::str::from_utf8(mode)
			.ok()
			.and_then(|mode| u32::from_str_radix(mode, 8).ok())
			.ok_or(Error::TreeEntry)?;
		if !matches!(mode, 0o100644 | 0o100755 | 0o40000 | 0o120000) {
			return Err(Error::TreeEntry);
		}
		let (hash, tail) = rest.split_at_checked(hash_len).ok_or(Error::TreeEntry)?;
		entries.push(TreeEntry {
			mode,
			name: name.to_vec(),
			hash: hash.to_vec(),
		});
		remaining = tail;
	}
	Ok(entries)
}

pub fn encode_tree(entries: &[TreeEntry], hash_len: usize) -> Result<Vec<u8>, Error> {
	let mut bytes = Vec::new();
	for entry in entries {
		if !matches!(entry.mode, 0o100644 | 0o100755 | 0o40000 | 0o120000)
			|| entry.name.is_empty()
			|| entry.name.contains(&0)
			|| entry.name.contains(&b'/')
			|| entry.hash.len() != hash_len
		{
			return Err(Error::TreeEntry);
		}
		bytes.extend_from_slice(format!("{:o}", entry.mode).as_bytes());
		bytes.push(b' ');
		bytes.extend_from_slice(&entry.name);
		bytes.push(0);
		bytes.extend_from_slice(&entry.hash);
	}
	Ok(bytes)
}

fn split_once(bytes: &[u8], delimiter: u8) -> Option<(&[u8], &[u8])> {
	let position = bytes.iter().position(|byte| *byte == delimiter)?;
	Some((&bytes[..position], &bytes[position + 1..]))
}
