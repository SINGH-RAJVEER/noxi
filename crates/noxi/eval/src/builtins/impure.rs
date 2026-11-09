use builtin_macros::builtins;
use genawaiter::rc::Gen;

use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
	self as noxi_eval, NixString, Value,
	errors::ErrorKind,
	value::NixAttrs,
	vm::generators::{self, GenCo},
};

#[builtins]
mod impure_builtins {
	use std::ffi::OsStr;
	use std::os::unix::ffi::OsStrExt;

	use super::*;
	use crate::{
		NixSearchPathEntry,
		builtins::{hash::hash_nix_string, realise_value_to_path},
		try_cek_to_value,
		value::CoercionKind,
	};

	/// `builtins.findFile`, which `<...>` calls when `__findFile` or
	/// `__nixPath` is rebound.
	#[builtin("findFile")]
	async fn builtin_find_file(
		co: GenCo,
		search_path: Value,
		path: Value,
	) -> Result<Value, ErrorKind> {
		let mut entries = Vec::new();
		for element in search_path.to_list()?.into_iter() {
			let element = generators::request_force(&co, element).await.to_attrs()?;
			let prefix = match element.select("prefix") {
				Some(prefix) => {
					let prefix = generators::request_force(&co, prefix.clone()).await;
					String::from_utf8_lossy(&prefix.to_str()?).into_owned()
				}
				None => String::new(),
			};
			let entry_path = element.select_required("path")?.clone();
			let kind = CoercionKind {
				strong: false,
				import_paths: false,
			};
			let entry_path =
				try_cek_to_value!(generators::request_string_coerce(&co, entry_path, kind).await);
			entries.push(NixSearchPathEntry {
				prefix,
				path: String::from_utf8_lossy(entry_path.as_bytes()).into_owned(),
			});
		}
		let path = String::from_utf8_lossy(&path.to_str()?).into_owned();
		Ok(generators::request_find_file(&co, entries, path).await)
	}

	#[builtin("getEnv")]
	async fn builtin_get_env(co: GenCo, var: Value) -> Result<Value, ErrorKind> {
		let key = OsStr::from_bytes(&var.to_str()?).to_os_string();

		let env = generators::request_get_env(&co, key).await;
		Ok(Value::String(NixString::from(env.as_bytes())))
	}

	#[builtin("hashFile")]
	async fn builtin_hash_file(co: GenCo, algo: Value, path: Value) -> Result<Value, ErrorKind> {
		let path = try_cek_to_value!(realise_value_to_path(&co, path).await?);
		let r = generators::request_open_file(&co, path).await;
		hash_nix_string(algo.to_str()?, r).map(Value::from)
	}

	#[builtin("pathExists")]
	async fn builtin_path_exists(co: GenCo, path: Value) -> Result<Value, ErrorKind> {
		generators::request_builtin_context(
			&co,
			vec![noxi_eval::Trace::new(
				None,
				"while realising the context of a path",
			)],
		)
		.await;
		let path = try_cek_to_value!(realise_value_to_path(&co, path).await?);
		Ok(generators::request_path_exists(&co, path).await)
	}

	#[builtin("readDir")]
	async fn builtin_read_dir(co: GenCo, path: Value) -> Result<Value, ErrorKind> {
		let path = try_cek_to_value!(realise_value_to_path(&co, path).await?);
		let dir = generators::request_read_dir(&co, path).await;
		let res = dir.into_iter().map(|(name, ftype)| {
			(
				NixString::from(name.as_ref()),
				Value::from(ftype.to_string()),
			)
		});

		Ok(Value::attrs(NixAttrs::from_iter(res)))
	}

	#[builtin("readFile")]
	async fn builtin_read_file(co: GenCo, path: Value) -> Result<Value, ErrorKind> {
		let path = try_cek_to_value!(realise_value_to_path(&co, path).await?);
		Ok(generators::request_read_file(&co, path).await)
	}

	#[builtin("readFileType")]
	async fn builtin_read_file_type(co: GenCo, path: Value) -> Result<Value, ErrorKind> {
		let path = try_cek_to_value!(realise_value_to_path(&co, path).await?);
		Ok(Value::from(
			generators::request_read_file_type(&co, path)
				.await
				.to_string(),
		))
	}
}

/// Return all impure builtins, that is all builtins which may perform I/O
/// outside of the VM and so cannot be used in all contexts (e.g. WASM).
pub fn impure_builtins() -> Vec<(&'static str, Value)> {
	let mut result = impure_builtins::builtins();

	// currentTime pins the time at which evaluation was started
	{
		let seconds = match SystemTime::now().duration_since(UNIX_EPOCH) {
			Ok(dur) => dur.as_secs() as i64,

			// This case is hit if the system time is *before* epoch.
			Err(err) => -(err.duration().as_secs() as i64),
		};

		result.push(("currentTime", Value::Integer(seconds)));
	}

	result
}

#[cfg(all(test, unix))]
mod tests {
	use std::ffi::OsStr;
	use std::os::unix::ffi::OsStrExt;

	use crate::{Evaluation, NixString};

	#[test]
	fn read_dir_preserves_non_utf8_filename() {
		let dir = tempfile::tempdir().unwrap();
		let filename = b"file-\xff";
		std::fs::write(dir.path().join(OsStr::from_bytes(filename)), b"").unwrap();

		let result = Evaluation::builder_impure()
			.build()
			.evaluate(&format!("builtins.readDir {}", dir.path().display()), None);
		assert!(
			result.errors.is_empty(),
			"evaluation failed: {:?}",
			result.errors
		);

		let value = result.value.unwrap();
		let attrs = value.to_attrs().unwrap();
		assert_eq!(attrs.len(), 1);
		let (key, file_type) = attrs.iter().next().unwrap();
		assert_eq!(&key[..], filename);
		assert_eq!(file_type.to_str().unwrap(), NixString::from("regular"));
	}
}
