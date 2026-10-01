//! Metadata of a valid store path, CppNix's `ValidPathInfo`.

use std::collections::BTreeSet;

use nix_compat::narinfo::{Signature, VerifyingKey};
use nix_compat::nixbase32;
use nix_compat::nixhash::{CAHash, HashAlgo, NixHash};
use nix_compat::store_path::{self, StorePath, StorePathRef};

/// What the database records about a valid store path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ValidPathInfo {
	pub path: StorePath,
	pub deriver: Option<StorePath>,
	/// SHA-256 of the path's NAR serialisation.
	pub nar_hash: [u8; 32],
	pub references: BTreeSet<StorePath>,
	/// Seconds since the epoch; 0 means "now" when registering.
	pub registration_time: i64,
	/// NAR size in bytes; 0 means unknown.
	pub nar_size: u64,
	/// Whether this store built the path itself.
	pub ultimate: bool,
	/// Signatures as `name:base64` strings.
	pub sigs: BTreeSet<String>,
	pub ca: Option<CAHash>,
}

impl ValidPathInfo {
	pub fn new(path: StorePath, nar_hash: [u8; 32]) -> Self {
		Self {
			path,
			deriver: None,
			nar_hash,
			references: BTreeSet::new(),
			registration_time: 0,
			nar_size: 0,
			ultimate: false,
			sigs: BTreeSet::new(),
			ca: None,
		}
	}

	/// The NAR hash as the database stores it: `sha256:` and base16.
	pub fn nar_hash_base16(&self) -> String {
		NixHash::Sha256(self.nar_hash)
			.as_nix_lowerhex_string_fmt()
			.to_string()
	}

	/// The NAR hash in CppNix's default `sha256:` and Nix base32 form.
	pub fn nar_hash_nix32(&self) -> String {
		format!("sha256:{}", nixbase32::encode(&self.nar_hash))
	}

	/// The string signatures cover: `1;PATH;NARHASH;NARSIZE;REFS`.
	pub fn fingerprint(&self) -> Option<String> {
		if self.nar_size == 0 {
			return None;
		}
		let refs: Vec<StorePathRef<'_>> = self.references.iter().map(StorePath::as_ref).collect();
		Some(nix_compat::narinfo::fingerprint(
			&self.path.as_ref(),
			&self.nar_hash,
			self.nar_size,
			refs.iter(),
		))
	}

	/// The path its `ca` field and references determine, if it has one.
	pub fn ca_path(&self) -> Option<StorePath> {
		let ca = self.ca.as_ref()?;
		let name = self.path.name();
		let path = match ca {
			CAHash::Text(digest) => {
				let refs = self.references.iter().map(StorePath::as_ref);
				store_path::build_text_path_from_content_digest(name, *digest, refs)
			}
			CAHash::Flat(hash) | CAHash::Nar(hash) => {
				let recursive = matches!(ca, CAHash::Nar(_));
				let self_ref = self.references.contains(&self.path);
				if !(recursive && hash.algo() == HashAlgo::Sha256)
					&& (self_ref || self.references.iter().any(|r| r != &self.path))
				{
					return None;
				}
				let refs = self
					.references
					.iter()
					.filter(|r| *r != &self.path)
					.map(StorePath::as_ref);
				store_path::build_ca_path(name, recursive, hash, refs, self_ref)
			}
		};
		path.ok().map(|p| p.to_owned())
	}

	/// Whether the path matches its content-address claim.
	pub fn is_content_addressed(&self) -> bool {
		match self.ca_path() {
			Some(path) => {
				let ok = path == self.path;
				if !ok {
					tracing::error!(
						"warning: path '{}' claims to be content-addressed but isn't",
						self.path.to_absolute_path()
					);
				}
				ok
			}
			None => false,
		}
	}

	/// The number of valid signatures by `keys`, or `usize::MAX` for a
	/// content-addressed path, which needs none.
	pub fn check_signatures(&self, keys: &[VerifyingKey]) -> usize {
		if self.is_content_addressed() {
			return usize::MAX;
		}
		let Some(fingerprint) = self.fingerprint() else {
			return 0;
		};
		self.sigs
			.iter()
			.filter(|sig| {
				let Ok(sig) = Signature::<&str>::parse(sig) else {
					return false;
				};
				keys.iter()
					.any(|key| key.name() == *sig.name() && key.verify(&fingerprint, &sig))
			})
			.count()
	}
}
