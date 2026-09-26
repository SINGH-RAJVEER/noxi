use std::{
	collections::{BTreeMap, BTreeSet},
	future::Future,
};

use super::{NixDeserialize, NixRead};

/// Upper bound on the number of items preallocated from a length read off the wire.
const MAX_PREALLOCATED_ITEMS: usize = 1024;

#[allow(clippy::manual_async_fn)]
impl<T> NixDeserialize for Vec<T>
where
	T: NixDeserialize + Send,
{
	fn try_deserialize<R>(
		reader: &mut R,
	) -> impl Future<Output = Result<Option<Self>, R::Error>> + Send + '_
	where
		R: ?Sized + NixRead + Send,
	{
		async move {
			if let Some(len) = reader.try_read_value::<usize>().await? {
				// `len` comes from the peer: preallocate only a bounded amount,
				// so a bogus length fails on missing data instead of aborting
				// on an impossible allocation.
				let mut ret = Vec::with_capacity(len.min(MAX_PREALLOCATED_ITEMS));
				for _ in 0..len {
					ret.push(reader.read_value().await?);
				}
				Ok(Some(ret))
			} else {
				Ok(None)
			}
		}
	}
}

#[allow(clippy::manual_async_fn)]
impl<K, V> NixDeserialize for BTreeMap<K, V>
where
	K: NixDeserialize + Ord + Send,
	V: NixDeserialize + Send,
{
	fn try_deserialize<R>(
		reader: &mut R,
	) -> impl Future<Output = Result<Option<Self>, R::Error>> + Send + '_
	where
		R: ?Sized + NixRead + Send,
	{
		async move {
			if let Some(len) = reader.try_read_value::<usize>().await? {
				let mut ret = BTreeMap::new();
				for _ in 0..len {
					let key = reader.read_value().await?;
					let value = reader.read_value().await?;
					ret.insert(key, value);
				}
				Ok(Some(ret))
			} else {
				Ok(None)
			}
		}
	}
}

#[allow(clippy::manual_async_fn)]
impl<T> NixDeserialize for BTreeSet<T>
where
	T: NixDeserialize + Ord + Send,
{
	fn try_deserialize<R>(
		reader: &mut R,
	) -> impl Future<Output = Result<Option<Self>, R::Error>> + Send + '_
	where
		R: ?Sized + NixRead + Send,
	{
		async move {
			if let Some(len) = reader.try_read_value::<usize>().await? {
				let mut ret = BTreeSet::new();
				for _ in 0..len {
					ret.insert(reader.read_value().await?);
				}
				Ok(Some(ret))
			} else {
				Ok(None)
			}
		}
	}
}

#[cfg(test)]
mod test {
	use std::collections::BTreeMap;
	use std::fmt;

	use hex_literal::hex;
	use rstest::rstest;
	use tokio_test::io::Builder;

	use crate::wire::de::{NixDeserialize, NixRead, NixReader};

	#[rstest]
	#[case::empty(vec![], &hex!("0000 0000 0000 0000"))]
	#[case::one(vec![0x29], &hex!("0100 0000 0000 0000 2900 0000 0000 0000"))]
	#[case::two(vec![0x7469, 10], &hex!("0200 0000 0000 0000 6974 0000 0000 0000 0A00 0000 0000 0000"))]
	#[tokio::test]
	async fn test_read_small_vec(#[case] expected: Vec<usize>, #[case] data: &[u8]) {
		let mock = Builder::new().read(data).build();
		let mut reader = NixReader::new(mock);
		let actual: Vec<usize> = reader.read_value().await.unwrap();
		assert_eq!(actual, expected);
	}

	fn empty_map() -> BTreeMap<usize, u64> {
		BTreeMap::new()
	}
	macro_rules! map {
        ($($key:expr => $value:expr),*) => {{
            let mut ret = BTreeMap::new();
            $(ret.insert($key, $value);)*
            ret
        }};
    }

	#[rstest]
	#[case::empty(empty_map(), &hex!("0000 0000 0000 0000"))]
	#[case::one(map![0x7469usize => 10u64], &hex!("0100 0000 0000 0000 6974 0000 0000 0000 0A00 0000 0000 0000"))]
	#[tokio::test]
	async fn test_read_small_btree_map<E>(#[case] expected: E, #[case] data: &[u8])
	where
		E: NixDeserialize + PartialEq + fmt::Debug,
	{
		let mock = Builder::new().read(data).build();
		let mut reader = NixReader::new(mock);
		let actual: E = reader.read_value().await.unwrap();
		assert_eq!(actual, expected);
	}

	/// A length read off the wire must not decide how much is allocated up
	/// front: an impossible length fails on missing data instead of aborting.
	#[tokio::test]
	async fn huge_length_is_an_error() {
		let mut bytes = u64::MAX.to_le_bytes().to_vec();
		bytes.extend(0u64.to_le_bytes());
		let mut reader = NixReader::new(std::io::Cursor::new(bytes));
		assert!(reader.read_value::<Vec<u64>>().await.is_err());
	}
}
