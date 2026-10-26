//! This module implements the builtins exposed in the Nix language.
//!
//! These are the builtins available to the current evaluator.

use bstr::{ByteSlice, ByteVec};
use builtin_macros::builtins;
use genawaiter::rc::Gen;
use regex::bytes::Regex;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::cmp::{self, Ordering};
use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::collections::hash_map::Entry;
use std::io::Write as _;
use std::path::PathBuf;
use std::rc::Rc;

use crate::vm::generators::{self, GenCo};
use crate::warnings::WarningKind;
use crate::{
	self as nix_rs_eval,
	builtins::hash::hash_nix_string,
	errors::{AddContext, CatchableErrorKind, ErrorKind},
	value::{CoercionKind, NixAttrs, NixList, NixString, PrintOptions, Thunk, Value},
};
use crate::{arithmetic_op, try_cek};

use self::versions::{VersionPart, VersionPartsIter};

mod extended_regex;
mod from_data;
pub(crate) use from_data::nul_message;
mod hash;
mod sort;
mod to_xml;
mod versions;

// technically #[cfg(test)], but right now we need it for tests in other crates…
pub use to_xml::value_to_xml;

#[cfg(feature = "impure")]
mod impure;

#[cfg(feature = "impure")]
pub use impure::impure_builtins;

// we set NIX_RS_CURRENT_SYSTEM in build.rs
pub const CURRENT_PLATFORM: &str = env!("NIX_RS_CURRENT_SYSTEM");

/// The type a CppNix primop forces an argument to, such as `forceList`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentType {
	/// `forceFunction`: a function, builtin or set with `__functor`.
	Function,
	List,
	Attrs,
	Int,
	Bool,
	/// `forceFloat`: a float or an integer.
	Float,
	/// `forceString`: a string, not a path.
	String,
}

/// CppNix's checks of the operands of `builtins.add` and the other arithmetic
/// builtins: floats if either operand is a float, otherwise integers.
/// `first_float` is the first operand's trace frame in the float case.
fn check_arithmetic(
	x: &Value,
	y: &Value,
	first: &'static str,
	first_float: &'static str,
	second: &'static str,
) -> Result<(), ErrorKind> {
	if matches!(x, Value::Float(_)) || matches!(y, Value::Float(_)) {
		check_argument(x, ArgumentType::Float, first_float)?;
		check_argument(y, ArgumentType::Float, second)
	} else {
		check_argument(x, ArgumentType::Int, first)?;
		check_argument(y, ArgumentType::Int, second)
	}
}

/// Check a forced builtin argument as the CppNix primop does, failing with
/// its type error and `context` trace frame. The builtin macro emits these
/// checks for the arguments listed in its table of CppNix's checks.
pub fn check_argument(
	value: &Value,
	expected: ArgumentType,
	context: &'static str,
) -> Result<(), ErrorKind> {
	if let Value::Thunk(thunk) = value {
		return check_argument(&thunk.value(), expected, context);
	}
	let ok = match (expected, value) {
		(ArgumentType::Function, Value::Closure(_) | Value::Builtin(_)) => true,
		(ArgumentType::Function, Value::Attrs(attrs)) => attrs.select("__functor").is_some(),
		(ArgumentType::List, Value::List(_))
		| (ArgumentType::Attrs, Value::Attrs(_))
		| (ArgumentType::Int, Value::Integer(_))
		| (ArgumentType::Bool, Value::Bool(_))
		| (ArgumentType::Float, Value::Integer(_) | Value::Float(_))
		| (ArgumentType::String, Value::String(_)) => true,
		_ => false,
	};
	if ok {
		return Ok(());
	}
	let expected = match expected {
		ArgumentType::Function => "lambda",
		ArgumentType::List => "list",
		ArgumentType::Attrs => "set",
		ArgumentType::Int => "int",
		ArgumentType::Bool => "bool",
		ArgumentType::Float => "float",
		ArgumentType::String => "string",
	};
	Err(ErrorKind::TypeError {
		expected,
		actual: value.type_of(),
		value: Some(Box::new(value.clone())),
	}
	.trace(None, context))
}

/// Coerce a Nix Value to a plain path, e.g. in order to access the
/// file it points to via either `builtins.toPath` or an impure
/// builtin. This coercion can _never_ be performed in a Nix program
/// without using builtins (i.e. the trick `path: /. + path` to
/// convert from a string to a path wouldn't hit this code).
///
/// This operation doesn't import a Nix path value into the store.
pub async fn coerce_value_to_path(
	co: &GenCo,
	v: Value,
) -> Result<Result<PathBuf, CatchableErrorKind>, ErrorKind> {
	let value = generators::request_force(co, v).await;
	if let Value::Path(p) = value {
		return Ok(Ok(*p));
	}

	let vs = try_cek!(
		generators::request_string_coerce(
			co,
			value,
			CoercionKind {
				strong: false,
				import_paths: false,
			},
		)
		.await
	);

	let path = vs.to_path()?.to_owned();
	if path.is_absolute() {
		Ok(Ok(path))
	} else {
		Err(ErrorKind::NotAnAbsolutePath(path))
	}
}

/// Coerce a filesystem path and realize its string-context dependencies.
/// Path literals are passed through without copying them into the store.
pub async fn realise_value_to_path(
	co: &GenCo,
	value: Value,
) -> Result<Result<PathBuf, CatchableErrorKind>, ErrorKind> {
	let value = generators::request_force(co, value).await;
	if let Value::Path(path) = value {
		return Ok(Ok(*path));
	}
	let string = try_cek!(
		generators::request_string_coerce(
			co,
			value,
			CoercionKind {
				strong: false,
				import_paths: false
			}
		)
		.await
	);
	let path = string.to_path()?.to_owned();
	if !path.is_absolute() {
		return Err(ErrorKind::NotAnAbsolutePath(path));
	}
	Ok(Ok(generators::request_realise_path(co, string).await))
}

#[derive(Default)]
struct BuiltinState {
	regex_cache: RefCell<FxHashMap<String, Regex>>,
	split_regex_cache: RefCell<FxHashMap<String, regex::bytes::Regex>>,
	/// The enabled CppNix experimental features, shared with the builder.
	experimental_features: ExperimentalFeatures,
	messages: EvaluationMessages,
}

/// Message settings shared by a builder and its own builtin closures.
/// Separate evaluations on one thread retain independent settings.
#[derive(Clone)]
pub(crate) struct EvaluationMessages {
	pub(crate) trace_verbose: Rc<std::cell::Cell<bool>>,
	pub(crate) abort_on_warn: Rc<std::cell::Cell<bool>>,
	pub(crate) warning_output: Rc<std::cell::Cell<bool>>,
	pub(crate) output: Rc<RefCell<Rc<dyn Fn(&[u8])>>>,
}

impl Default for EvaluationMessages {
	fn default() -> Self {
		Self {
			trace_verbose: Rc::new(std::cell::Cell::new(false)),
			abort_on_warn: Rc::new(std::cell::Cell::new(false)),
			warning_output: Rc::new(std::cell::Cell::new(true)),
			output: Rc::new(RefCell::new(Rc::new(|bytes| {
				let _ = std::io::stderr().write_all(bytes);
			}))),
		}
	}
}

/// The enabled CppNix experimental features, by name. An
/// [`crate::EvaluationBuilder`] and the builtins it creates share one set.
#[derive(Clone, Debug, Default)]
pub struct ExperimentalFeatures(Rc<RefCell<std::collections::BTreeSet<String>>>);

impl ExperimentalFeatures {
	pub fn set(&self, features: impl IntoIterator<Item = String>) {
		*self.0.borrow_mut() = features.into_iter().collect();
	}

	pub fn enabled(&self, feature: &str) -> bool {
		self.0.borrow().contains(feature)
	}
}

impl BuiltinState {
	fn get_split_regex(&self, pattern: &str) -> Result<regex::bytes::Regex, regex::Error> {
		let mut map = self.split_regex_cache.borrow_mut();
		Ok(match map.entry(pattern.to_string()) {
			Entry::Occupied(occupied) => occupied.get().clone(),
			Entry::Vacant(vacant) => {
				// CppNix uses std::regex over char iterators, matching individual bytes.
				let re = regex::bytes::RegexBuilder::new(pattern)
					.unicode(false)
					.build()?;
				vacant.insert(re).clone()
			}
		})
	}

	fn get_regex(&self, pattern: &str) -> Result<Regex, ()> {
		let mut map = self.regex_cache.borrow_mut();
		Ok(match map.entry(pattern.to_string()) {
			Entry::Occupied(occupied) => occupied.get().clone(),
			Entry::Vacant(vacant) => {
				let re = extended_regex::compile(pattern)?;
				vacant.insert(re).clone()
			}
		})
	}
}

/// Print a `builtins.trace` message: strings as they are and other values
/// with CppNix's `ValuePrinter`.
async fn print_trace(co: &GenCo, message: Value, messages: &EvaluationMessages) {
	let message = match message {
		Value::String(s) => s,
		message => generators::request_print(co, message, PrintOptions::UNLIMITED).await,
	};
	let mut line = b"trace: ".to_vec();
	line.extend_from_slice(message.as_bytes());
	line.push(b'\n');
	(messages.output.borrow())(&line);
}

/// A message as CppNix's `showErrorInfo` lays it out: trailing whitespace
/// removed from the message and from every line, the prefix on the first
/// line and continuation lines indented to its width. An empty message
/// prints only the newline.
fn warning_lines(prefix: &[u8], message: &[u8]) -> Vec<u8> {
	fn chomp(s: &[u8]) -> &[u8] {
		let end = s
			.iter()
			.rposition(|b| !b" \n\r\t".contains(b))
			.map_or(0, |i| i + 1);
		&s[..end]
	}
	let mut out = Vec::new();
	let message = chomp(message);
	if !message.is_empty() {
		let indent = vec![b' '; prefix.len()];
		for (i, line) in message.split(|&b| b == b'\n').enumerate() {
			let mut indented = if i == 0 {
				prefix.to_vec()
			} else {
				out.push(b'\n');
				indent.clone()
			};
			indented.extend_from_slice(line);
			out.extend_from_slice(chomp(&indented));
		}
	}
	out.push(b'\n');
	out
}

#[builtins(state = "Rc<BuiltinState>")]
mod pure_builtins {
	use std::ffi::OsString;

	use bstr::{B, BString, ByteSlice};
	use itertools::Itertools;
	use os_str_bytes::OsStringBytes;
	use rustc_hash::{FxHashMap, FxHashSet};

	use crate::{NixContext, NixContextElement, try_cek_to_value, value::PointerEquality};

	use super::*;

	macro_rules! try_value {
		($value:expr) => {{
			let val = $value;
			if val.is_catchable() {
				return Ok(val);
			}
			val
		}};
	}

	#[builtin("abort")]
	async fn builtin_abort(co: GenCo, message: Value) -> Result<Value, ErrorKind> {
		let message = try_cek_to_value!(
			generators::request_string_coerce(
				&co,
				message,
				CoercionKind {
					strong: false,
					import_paths: true,
				},
			)
			.await
		);
		Err(ErrorKind::Abort(message.as_bstr().to_string()))
	}

	#[builtin("add")]
	async fn builtin_add(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		check_arithmetic(
			&x,
			&y,
			"while evaluating the first argument of the addition",
			"while evaluating the first argument of the addition",
			"while evaluating the second argument of the addition",
		)?;
		arithmetic_op!(&x, &y, +)
	}

	#[builtin("all")]
	async fn builtin_all(co: GenCo, pred: Value, list: Value) -> Result<Value, ErrorKind> {
		for value in list.to_list()?.into_iter() {
			let pred_result = generators::request_call_with(&co, pred.clone(), [value]).await;
			let pred_result = try_value!(generators::request_force(&co, pred_result).await);

			if !pred_result.as_bool().trace(
				None,
				"while evaluating the return value of the function passed to builtins.all",
			)? {
				return Ok(Value::Bool(false));
			}
		}

		Ok(Value::Bool(true))
	}

	#[builtin("any")]
	async fn builtin_any(co: GenCo, pred: Value, list: Value) -> Result<Value, ErrorKind> {
		for value in list.to_list()?.into_iter() {
			let pred_result = generators::request_call_with(&co, pred.clone(), [value]).await;
			let pred_result = try_value!(generators::request_force(&co, pred_result).await);

			if pred_result.as_bool().trace(
				None,
				"while evaluating the return value of the function passed to builtins.any",
			)? {
				return Ok(Value::Bool(true));
			}
		}

		Ok(Value::Bool(false))
	}

	#[builtin("attrNames")]
	async fn builtin_attr_names(co: GenCo, set: Value) -> Result<Value, ErrorKind> {
		let xs = set.to_attrs()?;
		let mut output = Vec::with_capacity(xs.len());

		for (key, _val) in xs.iter_sorted() {
			output.push(Value::from(key.clone()));
		}

		Ok(Value::List(NixList::construct(output.len(), output)))
	}

	#[builtin("attrValues")]
	async fn builtin_attr_values(co: GenCo, set: Value) -> Result<Value, ErrorKind> {
		let xs = set.to_attrs()?;
		let mut output = Vec::with_capacity(xs.len());

		for (_key, val) in xs.iter_sorted() {
			output.push(val.clone());
		}

		Ok(Value::List(NixList::construct(output.len(), output)))
	}

	#[builtin("baseNameOf")]
	async fn builtin_base_name_of(co: GenCo, s: Value) -> Result<Value, ErrorKind> {
		let span = generators::request_span(&co).await;
		let s = s
			.coerce_to_string(
				co,
				CoercionKind {
					strong: false,
					import_paths: false,
				},
				span,
			)
			.await
			.trace(
				None,
				"while evaluating the first argument passed to builtins.baseNameOf",
			)?
			.to_contextful_str()?;

		// CppNix's `legacyBaseNameOf`: drop one trailing slash, then take
		// what follows the last slash.
		let bytes: &[u8] = &s;
		if bytes.is_empty() {
			return Ok(NixString::new_inherit_context_from(&s, "").into());
		}
		let mut last = bytes.len() - 1;
		if bytes[last] == b'/' && last > 0 {
			last -= 1;
		}
		let start = bytes[..=last]
			.iter()
			.rposition(|&b| b == b'/')
			.map_or(0, |pos| pos + 1);
		let bs: BString = bytes[start..=last].into();
		Ok(NixString::new_inherit_context_from(&s, bs).into())
	}

	#[builtin("bitAnd")]
	async fn builtin_bit_and(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		Ok(Value::Integer(x.as_int()? & y.as_int()?))
	}

	#[builtin("bitOr")]
	async fn builtin_bit_or(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		Ok(Value::Integer(x.as_int()? | y.as_int()?))
	}

	#[builtin("bitXor")]
	async fn builtin_bit_xor(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		Ok(Value::Integer(x.as_int()? ^ y.as_int()?))
	}

	/// Without a debugger, `break` returns its argument, as CppNix's does.
	#[builtin("break")]
	async fn builtin_break(co: GenCo, #[lazy] value: Value) -> Result<Value, ErrorKind> {
		let _ = co;
		Ok(value)
	}

	#[builtin("catAttrs")]
	async fn builtin_cat_attrs(co: GenCo, key: Value, list: Value) -> Result<Value, ErrorKind> {
		let key = key.to_str()?;
		let list = list.to_list()?;
		let mut output = vec![];

		for item in list.into_iter() {
			let set = generators::request_force(&co, item)
				.await
				.to_attrs()
				.trace(
					None,
					"while evaluating an element in the list passed as second argument to builtins.catAttrs",
				)?;

			if let Some(value) = set.select(&key) {
				output.push(value.clone());
			}
		}

		Ok(Value::List(NixList::construct(output.len(), output)))
	}

	#[builtin("ceil")]
	async fn builtin_ceil(co: GenCo, double: Value) -> Result<Value, ErrorKind> {
		round_to_int(&double, f64::ceil)
	}

	#[builtin("compareVersions")]
	async fn builtin_compare_versions(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		let s1 = x.to_str()?;
		let s1 = VersionPartsIter::new_for_cmp((&s1).into());
		let s2 = y.to_str()?;
		let s2 = VersionPartsIter::new_for_cmp((&s2).into());

		match s1.cmp(s2) {
			std::cmp::Ordering::Less => Ok(Value::Integer(-1)),
			std::cmp::Ordering::Equal => Ok(Value::Integer(0)),
			std::cmp::Ordering::Greater => Ok(Value::Integer(1)),
		}
	}

	#[builtin("concatLists")]
	async fn builtin_concat_lists(co: GenCo, lists: Value) -> Result<Value, ErrorKind> {
		let mut out = Vec::new();

		for value in lists.to_list()? {
			let list = try_value!(generators::request_force(&co, value).await)
				.to_list()
				.trace(
					None,
					"while evaluating a value of the list passed to builtins.concatLists",
				)?;
			out.extend(list);
		}

		Ok(Value::List(out.into()))
	}

	#[builtin("concatMap")]
	async fn builtin_concat_map(co: GenCo, f: Value, list: Value) -> Result<Value, ErrorKind> {
		let list = list.to_list()?;
		let mut res = Vec::new();
		for val in list {
			let out = generators::request_call_with(&co, f.clone(), [val]).await;
			let out = try_value!(generators::request_force(&co, out).await);
			res.extend(out.to_list().trace(
				None,
				"while evaluating the return value of the function passed to builtins.concatMap",
			)?);
		}
		Ok(Value::List(res.into()))
	}

	#[builtin("concatStringsSep")]
	async fn builtin_concat_strings_sep(
		co: GenCo,
		separator: Value,
		list: Value,
	) -> Result<Value, ErrorKind> {
		let mut separator = separator.to_contextful_str()?;

		let mut context = NixContext::new();
		if let Some(sep_context) = separator.take_context() {
			context.extend(*sep_context)
		}
		let list = list.to_list()?;
		let mut res = BString::default();
		for (i, val) in list.into_iter().enumerate() {
			if i != 0 {
				res.push_str(&separator);
			}
			let mut s = try_cek_to_value!(
				generators::request_string_coerce_in(
					&co,
					val,
					CoercionKind {
						strong: false,
						import_paths: true,
					},
					Some(
						"while evaluating one element of the list of strings to concat passed to builtins.concatStringsSep",
					),
				)
				.await
			);
			res.push_str(&s);
			if let Some(other_context) = s.take_context() {
				context.extend(*other_context);
			}
		}
		// FIXME: pass immediately the string res.
		Ok(NixString::new_context_from(context, res).into())
	}

	#[builtin("deepSeq")]
	async fn builtin_deep_seq(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		try_value!(generators::request_deep_force(&co, x).await);
		Ok(y)
	}

	#[builtin("div")]
	async fn builtin_div(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		// The divisor is checked first, and a zero divisor fails before the
		// dividend is checked.
		check_argument(
			&y,
			ArgumentType::Float,
			"while evaluating the second operand of the division",
		)?;
		if !matches!(y, Value::Integer(0)) && !matches!(y, Value::Float(f) if f == 0.0) {
			check_arithmetic(
				&x,
				&y,
				"while evaluating the first operand of the division",
				"while evaluating the first operand of the division",
				"while evaluating the second operand of the division",
			)?;
		}
		arithmetic_op!(&x, &y, /)
	}

	#[builtin("dirOf")]
	async fn builtin_dir_of(co: GenCo, s: Value) -> Result<Value, ErrorKind> {
		let is_path = s.is_path();
		let span = generators::request_span(&co).await;
		let str = s
			.coerce_to_string(
				co,
				CoercionKind {
					strong: false,
					import_paths: false,
				},
				span,
			)
			.await
			.trace(
				None,
				"while evaluating the first argument passed to 'builtins.dirOf'",
			)?
			.to_contextful_str()?;
		let result = str
			.rfind_char('/')
			.map(|last_slash| {
				let x = &str[..last_slash];
				if x.is_empty() { B("/") } else { x }
			})
			.unwrap_or(b".");
		if is_path {
			Ok(Value::Path(Box::new(PathBuf::from(
				OsString::assert_from_raw_vec(result.to_owned()),
			))))
		} else {
			Ok(Value::from(NixString::new_inherit_context_from(
				&str, result,
			)))
		}
	}

	#[builtin("elem")]
	async fn builtin_elem(co: GenCo, x: Value, xs: Value) -> Result<Value, ErrorKind> {
		for val in xs.to_list()? {
			match try_cek_to_value!(
				generators::check_equality(&co, x.clone(), val, PointerEquality::AllowAll).await?
			) {
				true => return Ok(true.into()),
				false => continue,
			}
		}
		Ok(false.into())
	}

	#[builtin("elemAt")]
	async fn builtin_elem_at(co: GenCo, xs: Value, i: Value) -> Result<Value, ErrorKind> {
		let xs = xs.to_list()?;
		let i = i.as_int()?;
		match usize::try_from(i).ok().and_then(|index| xs.get(index)) {
			Some(value) => Ok(value.clone()),
			None => Err(ErrorKind::EvalError(format!("'builtins.elemAt' called with index {i} on a list of size {}", xs.len()))),
		}
	}

	#[builtin("filter")]
	async fn builtin_filter(co: GenCo, pred: Value, list: Value) -> Result<Value, ErrorKind> {
		let list: NixList = list.to_list()?;
		let mut out = Vec::new();

		for value in list {
			let result = generators::request_call_with(&co, pred.clone(), [value.clone()]).await;
			let verdict = try_value!(generators::request_force(&co, result).await);
			if verdict.as_bool().trace(
				None,
				"while evaluating the return value of the filtering function passed to builtins.filter",
			)? {
				out.push(value);
			}
		}

		Ok(Value::List(out.into()))
	}

	#[builtin("floor")]
	async fn builtin_floor(co: GenCo, double: Value) -> Result<Value, ErrorKind> {
		round_to_int(&double, f64::floor)
	}

	#[builtin("foldl'")]
	async fn builtin_foldl(
		co: GenCo,
		op: Value,
		#[lazy] nul: Value,
		list: Value,
	) -> Result<Value, ErrorKind> {
		let mut nul = nul;
		let list = list.to_list()?;
		for val in list {
			// Every call of `op` is forced immediately, but `nul` is not, see
			// https://github.com/NixOS/nix/blob/940e9eb8/src/libexpr/primops.cc#L3069-L3070C36
			// and our tests for foldl'.
			nul = generators::request_call_with(&co, op.clone(), [nul, val]).await;
			nul = generators::request_force(&co, nul).await;
			if let c @ Value::Catchable(_) = nul {
				return Ok(c);
			}
		}

		Ok(nul)
	}

	#[builtin("functionArgs")]
	async fn builtin_function_args(co: GenCo, f: Value) -> Result<Value, ErrorKind> {
		if matches!(f, Value::Builtin(_)) {
			return Ok(Value::attrs(NixAttrs::empty()));
		}

		let lambda = &f.as_closure().map_err(|_| ErrorKind::EvalError("'functionArgs' requires a function".into()))?.lambda();
		let formals = if let Some(formals) = &lambda.formals {
			formals
		} else {
			return Ok(Value::attrs(NixAttrs::empty()));
		};
		Ok(Value::attrs(NixAttrs::from_iter_with_pos(
			formals.arguments.iter().map(|(k, v)| {
				(
					k.clone(),
					Value::Bool(*v),
					formals.argument_spans.get(k).copied(),
				)
			}),
		)))
	}

	#[builtin("fromJSON")]
	async fn builtin_from_json(co: GenCo, json: Value) -> Result<Value, ErrorKind> {
		let json_str = json.to_str()?;
		let json: serde_json::Value = serde_json::from_slice(&json_str)?;
		from_data::json_to_value(json)
	}

	#[builtin("toJSON")]
	async fn builtin_to_json(co: GenCo, val: Value) -> Result<Value, ErrorKind> {
		match val.into_contextful_json(&co).await {
			Err(ErrorKind::CatchableError(catchable)) => Ok(Value::Catchable(Box::new(catchable))),
			Err(err) => Err(err),
			Ok((json, context)) => {
				let json_str = crate::value::into_json_string(json)?;
				Ok(Value::String(NixString::new_context_from(
					context, json_str,
				)))
			}
		}
	}

	#[builtin("fromTOML")]
	async fn builtin_from_toml(
		state: Rc<BuiltinState>,
		co: GenCo,
		toml: Value,
	) -> Result<Value, ErrorKind> {
		let toml_str = toml.to_str()?;
		let toml: toml::Value = toml::from_str(toml_str.to_str()?)?;
		from_data::toml_to_value(
			toml,
			state.experimental_features.enabled("parse-toml-timestamps"),
		)
	}

	#[builtin("genericClosure")]
	async fn builtin_generic_closure(co: GenCo, input: Value) -> Result<Value, ErrorKind> {
		// After CppNix's `prim_genericClosure`, which keeps the keys in an
		// ordered set, so keys must be comparable.
		let attrs = input.to_attrs()?;
		let start_set =
			generators::request_force(&co, attrs.select_required("startSet")?.clone()).await;
		let start_set = try_value!(start_set).to_list()?;
		if start_set.is_empty() {
			return Ok(Value::List(start_set));
		}
		let operator = attrs.select_required("operator")?.clone();
		let operator = try_value!(generators::request_force(&co, operator).await);
		if !matches!(operator, Value::Closure(_) | Value::Builtin(_))
			&& !matches!(&operator, Value::Attrs(attrs) if attrs.select("__functor").is_some())
		{
			return Err(ErrorKind::TypeError {
				expected: "lambda",
				actual: operator.type_of(),
				value: Some(Box::new(operator.clone())),
			});
		}

		let span = generators::request_span(&co).await;
		let mut work_set: VecDeque<Value> = start_set.into_iter().collect();
		let mut res = Vec::new();
		let mut keys: Vec<Value> = Vec::new();

		while let Some(val) = work_set.pop_front() {
			let val = try_value!(generators::request_force(&co, val).await);
			let key = val.to_attrs()?.select_required("key")?.clone();
			let key = try_value!(generators::request_force(&co, key).await);

			// Binary search for the key among the sorted keys seen so far.
			let (mut low, mut high) = (0, keys.len());
			let mut found = false;
			while low < high {
				let mid = (low + high) / 2;
				match try_cek_to_value!(
					key.clone()
						.nix_cmp_ordering_ref(keys[mid].clone(), &co, span)
						.await?
				) {
					Ordering::Less => high = mid,
					Ordering::Greater => low = mid + 1,
					Ordering::Equal => {
						found = true;
						break;
					}
				}
			}
			if found {
				continue;
			}
			keys.insert(low, key);

			res.push(val.clone());
			let op_result = generators::request_force(
				&co,
				generators::request_call_with(&co, operator.clone(), [val]).await,
			)
			.await;
			work_set.extend(try_value!(op_result).to_list()?);
		}

		Ok(Value::List(NixList::from(res)))
	}

	#[builtin("genList")]
	async fn builtin_gen_list(
		co: GenCo,
		// Nix 2.3 doesn't propagate failures here
		#[lazy] generator: Value,
		length: Value,
	) -> Result<Value, ErrorKind> {
		let len = length.as_int()?;
		let mut out = Vec::with_capacity(
			len.try_into()
				.map_err(|_| ErrorKind::EvalError(format!("cannot create list of size {len}")))?,
		);
		if len == 0 {
			return Ok(Value::List(out.into()));
		}

		// the best span we can get…
		let span = generators::request_span(&co).await;

		let lambda = Thunk::suspended_call_lambda(span);
		for i in 0..len {
			let val = Value::Thunk(Thunk::new_suspended_call_with_lambda(
				generator.clone(),
				i.into(),
				lambda.clone(),
			));
			out.push(val);
		}

		Ok(Value::List(out.into()))
	}

	#[builtin("getAttr")]
	async fn builtin_get_attr(co: GenCo, key: Value, set: Value) -> Result<Value, ErrorKind> {
		let k = key.to_str()?;
		let xs = set.to_attrs()?;

		match xs.select(&k) {
			Some(x) => Ok(x.clone()),
			None => Err(ErrorKind::AttributeNotFound {
				name: k.to_str_lossy().into_owned(),
				suggestions: vec![],
			}
			.trace(None, "in the attribute set under consideration")),
		}
	}

	#[builtin("groupBy")]
	async fn builtin_group_by(co: GenCo, f: Value, list: Value) -> Result<Value, ErrorKind> {
		let mut res: BTreeMap<NixString, Vec<Value>> = BTreeMap::new();
		for val in list.to_list()? {
			let key = try_value!(
				generators::request_force(
					&co,
					generators::request_call_with(&co, f.clone(), [val.clone()]).await,
				)
				.await
			)
			.to_str()
			.trace(
				None,
				"while evaluating the return value of the grouping function passed to builtins.groupBy",
			)?;

			res.entry(key).or_default().push(val);
		}
		Ok(Value::attrs(NixAttrs::from_iter(
			res.into_iter()
				.map(|(k, v)| (k, Value::List(NixList::from(v)))),
		)))
	}

	#[builtin("hasAttr")]
	fn builtin_has_attr(key: Value, set: Value) -> Result<Value, ErrorKind> {
		let k = key.to_str()?;
		let xs = set.to_attrs()?;

		Ok(Value::Bool(xs.contains(&k)))
	}

	#[builtin("hasContext")]
	#[allow(non_snake_case)]
	async fn builtin_hasContext(co: GenCo, e: Value) -> Result<Value, ErrorKind> {
		if e.is_catchable() {
			return Ok(e);
		}

		let v = e.to_contextful_str()?;
		Ok(Value::Bool(v.has_context()))
	}

	#[builtin("getContext")]
	#[allow(non_snake_case)]
	async fn builtin_getContext(co: GenCo, e: Value) -> Result<Value, ErrorKind> {
		if e.is_catchable() {
			return Ok(e);
		}

		let s = e.to_contextful_str()?;

		let groups = s
			.iter_context()
			.flat_map(|context| context.iter())
			// Do not think `group_by` works here.
			// `group_by` works on consecutive elements of the iterator.
			// Due to how `HashSet` works (ordering is not guaranteed),
			// this can become a source of non-determinism if you `group_by` naively.
			// I know I did.
			.into_grouping_map_by(|ctx_element| match ctx_element {
				NixContextElement::Plain(spath) => spath,
				NixContextElement::Single { derivation, .. } => derivation,
				NixContextElement::Derivation(drv_path) => drv_path,
			})
			.collect::<Vec<_>>();

		let elements = groups.into_iter().map(|(key, group)| {
			let mut outputs: Vec<NixString> = Vec::new();
			let mut is_path = false;
			let mut all_outputs = false;

			for ctx_element in group {
				match ctx_element {
					NixContextElement::Plain(spath) => {
						debug_assert!(
							spath == key,
							"Unexpected group containing mixed keys, expected: {key:?}, encountered {spath:?}"
						);
						is_path = true;
					}

					NixContextElement::Single { name, derivation } => {
						debug_assert!(
							derivation == key,
							"Unexpected group containing mixed keys, expected: {key:?}, encountered {derivation:?}"
						);
						outputs.push(name.clone().into());
					}

					NixContextElement::Derivation(drv_path) => {
						debug_assert!(
							drv_path == key,
							"Unexpected group containing mixed keys, expected: {key:?}, encountered {drv_path:?}"
						);
						all_outputs = true;
					}
				}
			}

			// FIXME(raitobezarius): is there a better way to construct an attribute set
			// conditionally?
			let mut vec_attrs: Vec<(&str, Value)> = Vec::new();

			if is_path {
				vec_attrs.push(("path", true.into()));
			}

			if all_outputs {
				vec_attrs.push(("allOutputs", true.into()));
			}

			if !outputs.is_empty() {
				outputs.sort();
				vec_attrs.push((
					"outputs",
					Value::List(
						outputs
							.into_iter()
							.map(|s| s.into())
							.collect::<Vec<Value>>()
							.into(),
					),
				));
			}

			(key.clone(), Value::attrs(NixAttrs::from_iter(vec_attrs)))
		});

		Ok(Value::attrs(NixAttrs::from_iter(elements)))
	}

	#[builtin("appendContext")]
	#[allow(non_snake_case)]
	async fn builtin_appendContext(
		co: GenCo,
		origin: Value,
		added_context: Value,
	) -> Result<Value, ErrorKind> {
		// `appendContext` is a "grow" context function.
		// It cannot remove a context element, neither replace a piece of its contents.
		//
		// Growing context is always a safe operation, there's no loss of dependency tracking
		// information.
		//
		// This is why this operation is not prefixed by `unsafe` and is deemed *safe*.
		// Nonetheless, it is possible to craft nonsensical context elements referring
		// to inexistent derivations, output paths or output names.
		//
		// In Nix, those nonsensical context elements are partially mitigated by checking
		// that various parameters are indeed syntatically valid store paths in the context, i.e.
		// starting with the same prefix as `builtins.storeDir`, or ending with `.drv`.
		// In addition, if writing to the store is possible (evaluator not in read-only mode), Nix
		// will realize some paths and ensures they are present in the store.
		//
		// In this implementation, we do none of that, no syntax checks, no realization.
		// The next `TODO` are the checks that Nix implements.
		let mut ctx_elements: FxHashSet<NixContextElement> = FxHashSet::default();
		let mut origin = try_cek_to_value!(
			generators::request_string_coerce(
				&co,
				origin,
				CoercionKind {
					strong: true,
					import_paths: true,
				},
			)
			.await
		);

		let added_context = added_context.to_attrs()?;
		for (context_key, context_element) in added_context.into_iter() {
			// Invariant checks:
			// - TODO: context_key must be a syntactically valid store path.
			let context_element =
				try_value!(generators::request_force(&co, context_element).await).to_attrs()?;
			if let Some(path) = context_element.select("path") {
				if try_value!(generators::request_force(&co, path.clone()).await).as_bool()? {
					ctx_elements
						.insert(NixContextElement::Plain(context_key.as_bstr().to_string()));
				}
			}
			if let Some(all_outputs) = context_element.select("allOutputs") {
				if try_value!(generators::request_force(&co, all_outputs.clone()).await)
					.as_bool()?
				{
					// TODO: check if `context_key` is a derivation path.
					// This may require realization.
					ctx_elements.insert(NixContextElement::Derivation(
						context_key.as_bstr().to_string(),
					));
				}
			}
			if let Some(some_outputs) = context_element.select("outputs") {
				let some_outputs =
					try_value!(generators::request_force(&co, some_outputs.clone()).await)
						.to_list()?;
				// TODO: check if `context_key` is a derivation path.
				// This may require realization.
				for output in some_outputs.into_iter() {
					let output =
						try_value!(generators::request_force(&co, output).await).to_str()?;
					ctx_elements.insert(NixContextElement::Single {
						derivation: context_key.as_bstr().to_string(),
						name: output.as_bstr().to_string(),
					});
				}
			}
		}

		if let Some(origin_ctx) = origin.context_mut() {
			origin_ctx.extend(ctx_elements);
		} else {
			origin = NixString::new_context_from(ctx_elements.into(), origin);
		}

		Ok(origin.into())
	}

	#[builtin("convertHash")]
	async fn builtin_convert_hash(
		state: Rc<BuiltinState>,
		co: GenCo,
		args: Value,
	) -> Result<Value, ErrorKind> {
		let blake3 = state.experimental_features.enabled("blake3-hashes");
		let args = args.to_attrs()?;
		let hash = args.select_required("hash")?.clone();
		let hash = try_value!(generators::request_force(&co, hash).await).to_str()?;
		let algo = match args.select("hashAlgo") {
			Some(algo) => {
				let algo = try_value!(generators::request_force(&co, algo.clone()).await);
				Some(crate::builtins::hash::parse_hash_algo(
					&algo.to_str()?,
					blake3,
				)?)
			}
			None => None,
		};
		let format = args.select_required("toHashFormat")?.clone();
		let format = try_value!(generators::request_force(&co, format).await).to_str()?;
		let format = crate::builtins::hash::parse_hash_format(&format)?;
		let hash = crate::builtins::hash::parse_any(&hash, algo, blake3)?;
		Ok(Value::from(crate::builtins::hash::to_string(&hash, format)))
	}

	#[builtin("hashString")]
	async fn builtin_hash_string(co: GenCo, algo: Value, s: Value) -> Result<Value, ErrorKind> {
		hash_nix_string(algo.to_str()?, std::io::Cursor::new(s.to_str()?)).map(Value::from)
	}

	#[builtin("head")]
	fn builtin_head(list: Value) -> Result<Value, ErrorKind> {
		if list.is_catchable() {
			return Ok(list);
		}

		match list.to_list()?.get(0) {
			Some(x) => Ok(x.clone()),
			None => Err(ErrorKind::EvalError("'builtins.head' called on an empty list".into())),
		}
	}

	#[builtin("intersectAttrs")]
	async fn builtin_intersect_attrs(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		if x.is_catchable() {
			return Ok(x);
		}
		if y.is_catchable() {
			return Ok(y);
		}
		let left_set = x.to_attrs()?;
		let right_set = y.to_attrs()?;

		if left_set.is_empty() || right_set.is_empty() {
			return Ok(Value::attrs(NixAttrs::empty()));
		}

		Ok(Value::attrs(left_set.intersect(&right_set)))
	}

	#[builtin("isAttrs")]
	fn builtin_is_attrs(value: Value) -> Result<Value, ErrorKind> {
		// TODO(edef): make this beautiful
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(value, Value::Attrs(_))))
	}

	#[builtin("isBool")]
	fn builtin_is_bool(value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(value, Value::Bool(_))))
	}

	#[builtin("isFloat")]
	fn builtin_is_float(value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(value, Value::Float(_))))
	}

	#[builtin("isFunction")]
	fn builtin_is_function(value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(
			value,
			Value::Closure(_) | Value::Builtin(_)
		)))
	}

	#[builtin("isInt")]
	fn builtin_is_int(value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(value, Value::Integer(_))))
	}

	#[builtin("isList")]
	fn builtin_is_list(value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(value, Value::List(_))))
	}

	#[builtin("isNull")]
	fn builtin_is_null(value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(value, Value::Null)))
	}

	#[builtin("isPath")]
	fn builtin_is_path(value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(value, Value::Path(_))))
	}

	#[builtin("isString")]
	fn builtin_is_string(value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			return Ok(value);
		}

		Ok(Value::Bool(matches!(value, Value::String(_))))
	}

	#[builtin("length")]
	fn builtin_length(list: Value) -> Result<Value, ErrorKind> {
		if list.is_catchable() {
			return Ok(list);
		}
		Ok(Value::Integer(list.to_list()?.len() as i64))
	}

	#[builtin("lessThan")]
	async fn builtin_less_than(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		let span = generators::request_span(&co).await;
		match try_cek_to_value!(x.nix_cmp_ordering(y, co, span).await?) {
			Ordering::Less => Ok(Value::Bool(true)),
			_ => Ok(Value::Bool(false)),
		}
	}

	#[builtin("listToAttrs")]
	async fn builtin_list_to_attrs(co: GenCo, list: Value) -> Result<Value, ErrorKind> {
		let list = list.to_list()?;
		let mut map = FxHashMap::default();
		for val in list {
			let attrs = try_value!(generators::request_force(&co, val).await).to_attrs()?;
			let name = try_value!(
				generators::request_force(&co, attrs.select_required("name")?.clone()).await
			)
			.to_str()?;
			let value = attrs.select_required("value")?.clone();
			// Map entries earlier in the list take precedence over entries later in the list
			map.entry(name).or_insert(value);
		}
		Ok(Value::attrs(NixAttrs::from(map)))
	}

	#[builtin("map")]
	async fn builtin_map(co: GenCo, #[lazy] f: Value, list_val: Value) -> Result<Value, ErrorKind> {
		let list = list_val.to_list()?;
		let mut out = Vec::with_capacity(list.len());
		if list.is_empty() {
			return Ok(Value::List(out.into()));
		}

		// the best span we can get…
		let span = generators::request_span(&co).await;

		let lambda = Thunk::suspended_call_lambda(span);
		for val in list {
			let result = Value::Thunk(Thunk::new_suspended_call_with_lambda(
				f.clone(),
				val,
				lambda.clone(),
			));
			out.push(result)
		}

		Ok(Value::List(out.into()))
	}

	#[builtin("mapAttrs")]
	async fn builtin_map_attrs(
		co: GenCo,
		#[lazy] f: Value,
		attrs: Value,
	) -> Result<Value, ErrorKind> {
		let attrs = attrs.to_attrs()?;
		let mut out = FxHashMap::default();
		if attrs.is_empty() {
			return Ok(Value::attrs(out.into()));
		}

		// the best span we can get…
		let span = generators::request_span(&co).await;

		let lambda = Thunk::suspended_call_lambda(span);
		for (key, value) in attrs.into_iter() {
			let result = Value::Thunk(Thunk::new_suspended_call_with_lambda(
				f.clone(),
				key.clone().into(),
				lambda.clone(),
			));
			let result = Value::Thunk(Thunk::new_suspended_call_with_lambda(
				result,
				value,
				lambda.clone(),
			));

			out.insert(key, result);
		}

		Ok(Value::attrs(out.into()))
	}

	#[builtin("match")]
	async fn builtin_match(
		state: Rc<BuiltinState>,
		co: GenCo,
		regex: Value,
		str: Value,
	) -> Result<Value, ErrorKind> {
		let s = str;
		if s.is_catchable() {
			return Ok(s);
		}
		let s = s.to_contextful_str()?;
		let re = regex;
		if re.is_catchable() {
			return Ok(re);
		}
		let re = re.to_str()?;
		let re = re.to_str()?;
		let re = state
			.get_regex(re)
			.map_err(|_| ErrorKind::InvalidRegex(re.to_string()))?;

		match re.captures(s.as_bytes()) {
			Some(caps) => Ok(Value::List(
				caps.iter()
					.skip(1)
					.map(|grp| {
						// Surprisingly, Nix does not propagate
						// the original context here.
						// Though, it accepts contextful strings as an argument.
						// An example of such behaviors in nixpkgs
						// can be observed in make-initrd.nix when it comes
						// to compressors which are matched over their full command
						// and then a compressor name will be extracted from that.
						grp.map(|g| Value::from(g.as_bytes()))
							.unwrap_or(Value::Null)
					})
					.collect::<Vec<Value>>()
					.into(),
			)),
			None => Ok(Value::Null),
		}
	}

	#[builtin("mul")]
	async fn builtin_mul(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		// CppNix's message for a float product omits "argument".
		check_arithmetic(
			&x,
			&y,
			"while evaluating the first argument of the multiplication",
			"while evaluating the first of the multiplication",
			"while evaluating the second argument of the multiplication",
		)?;
		arithmetic_op!(&x, &y, *)
	}

	#[builtin("parseDrvName")]
	async fn builtin_parse_drv_name(co: GenCo, s: Value) -> Result<Value, ErrorKind> {
		if s.is_catchable() {
			return Ok(s);
		}

		// This replicates cppnix's (mis?)handling of codepoints
		// above U+007f following 0x2d ('-')
		let s = s.to_str()?;
		let slice: &[u8] = s.as_ref();
		let (name, dash_and_version) = slice.split_at(
			slice
				.windows(2)
				.enumerate()
				.find_map(|x| match x {
					(idx, [b'-', c1]) if !c1.is_ascii_alphabetic() => Some(idx),
					_ => None,
				})
				.unwrap_or(slice.len()),
		);
		let version = dash_and_version
			.split_first()
			.map(|x| core::str::from_utf8(x.1))
			.unwrap_or(Ok(""))?;
		Ok(Value::attrs(NixAttrs::from_iter([
			("name", core::str::from_utf8(name)?),
			("version", version),
		])))
	}

	#[builtin("partition")]
	async fn builtin_partition(co: GenCo, pred: Value, list: Value) -> Result<Value, ErrorKind> {
		let mut right: Vec<Value> = Default::default();
		let mut wrong: Vec<Value> = Default::default();

		let list: NixList = list.to_list()?;
		for elem in list {
			let result = generators::request_call_with(&co, pred.clone(), [elem.clone()]).await;

			if try_value!(generators::request_force(&co, result).await)
				.as_bool()
				.trace(
					None,
					"while evaluating the return value of the partition function passed to builtins.partition",
				)? {
				right.push(elem);
			} else {
				wrong.push(elem);
			};
		}

		let res = [
			("right", Value::List(NixList::from(right))),
			("wrong", Value::List(NixList::from(wrong))),
		];

		Ok(Value::attrs(NixAttrs::from_iter(res)))
	}

	#[builtin("removeAttrs")]
	async fn builtin_remove_attrs(
		co: GenCo,
		attrs: Value,
		keys: Value,
	) -> Result<Value, ErrorKind> {
		let attrs = attrs.to_attrs()?;
		let keys = keys
			.to_list()?
			.into_iter()
			.map(|v| v.to_str())
			.collect::<Result<FxHashSet<_>, _>>()?;
		let res = attrs.iter().filter_map(|(k, v)| {
			if !keys.contains(k) {
				Some((k.clone(), v.clone()))
			} else {
				None
			}
		});
		Ok(Value::attrs(NixAttrs::from_iter(res)))
	}

	#[builtin("replaceStrings")]
	async fn builtin_replace_strings(
		co: GenCo,
		from: Value,
		to: Value,
		s: Value,
	) -> Result<Value, ErrorKind> {
		// After CppNix's `prim_replaceStrings`: the `from` strings are forced
		// up front, a replacement only when it is first used.
		let from = from.to_list()?;
		let to = to.to_list()?;
		if from.len() != to.len() {
			return Err(ErrorKind::EvalError(
				"'from' and 'to' arguments passed to builtins.replaceStrings have different lengths"
					.into(),
			));
		}
		let mut patterns = Vec::with_capacity(from.len());
		for val in &from {
			let val = try_value!(generators::request_force(&co, val.clone()).await);
			patterns.push(val.to_contextful_str().trace(None, "while evaluating one of the strings to replace passed to builtins.replaceStrings")?);
		}

		let mut string = s.to_contextful_str().trace(None, "while evaluating the third argument passed to builtins.replaceStrings")?;
		let mut context = NixContext::new();
		if let Some(string_context) = string.take_context() {
			context.extend(*string_context);
		}
		let string: &[u8] = &string;

		let mut cache: Vec<Option<BString>> = vec![None; to.len()];
		let mut res = BString::default();
		// Loop one past the end so that an empty pattern also matches there.
		let mut p = 0;
		while p <= string.len() {
			let found = patterns
				.iter()
				.position(|pattern| string[p..].starts_with(pattern));
			match found {
				Some(index) => {
					if cache[index].is_none() {
						let replacement =
							try_value!(generators::request_force(&co, to[index].clone()).await);
						let mut replacement = replacement.to_contextful_str()?;
						if let Some(to_ctx) = replacement.take_context() {
							context.extend(*to_ctx);
						}
						cache[index] = Some(BString::from(&**replacement));
					}
					res.push_str(cache[index].as_ref().unwrap());
					if patterns[index].is_empty() {
						if let Some(&byte) = string.get(p) {
							res.push(byte);
						}
						p += 1;
					} else {
						p += patterns[index].len();
					}
				}
				None => {
					if let Some(&byte) = string.get(p) {
						res.push(byte);
					}
					p += 1;
				}
			}
		}

		Ok(Value::from(NixString::new_context_from(context, res)))
	}

	#[builtin("seq")]
	async fn builtin_seq(co: GenCo, _x: Value, #[lazy] y: Value) -> Result<Value, ErrorKind> {
		// Only the first argument is forced by the calling infrastructure.
		// The second must remain lazy until the first succeeds, including
		// when the first throws and the second would abort.
		Ok(generators::request_force(&co, y).await)
	}

	#[builtin("split")]
	async fn builtin_split(
		state: Rc<BuiltinState>,
		co: GenCo,
		regex: Value,
		str: Value,
	) -> Result<Value, ErrorKind> {
		if str.is_catchable() {
			return Ok(str);
		}

		if regex.is_catchable() {
			return Ok(regex);
		}

		let s = str.to_contextful_str()?;
		let text = s.as_bytes();
		let re = regex.to_str()?;
		let re = re.to_str()?;
		let re = state
			.get_split_regex(re)
			.map_err(|_| ErrorKind::InvalidRegex(re.to_string()))?;
		let mut capture_locations = re.capture_locations();
		let num_captures = capture_locations.len();
		let mut ret = Vec::new();
		let mut pos = 0;
		let mut search_pos = 0;

		while let Some(thematch) = re.captures_read_at(&mut capture_locations, text, search_pos) {
			// push the unmatched characters preceding the match
			ret.push(Value::from(NixString::new_inherit_context_from(
				&s,
				&text[pos..thematch.start()],
			)));

			// Push a list with one element for each capture
			// group in the regex, containing the characters
			// matched by that capture group, or null if no match.
			// We skip capture 0; it represents the whole match.
			let v: Vec<Value> = (1..num_captures)
				.map(|i| capture_locations.get(i))
				.map(|o| {
					o.map(|(start, end)| {
						// Here, a surprising thing happens: we silently discard the original
						// context. This is as intended, Nix does the same.
						Value::from(&text[start..end])
					})
					.unwrap_or(Value::Null)
				})
				.collect();
			ret.push(Value::List(NixList::from(v)));
			pos = thematch.end();
			search_pos = pos;
			if thematch.is_empty() {
				// CppNix's regex iterator advances after an empty match, while
				// retaining skipped characters in the next unmatched prefix.
				if pos == text.len() {
					break;
				}
				search_pos += 1;
			}
		}

		// push the unmatched characters following the last match
		// Here, a surprising thing happens: we silently discard the original
		// context. This is as intended, Nix does the same.
		ret.push(Value::from(&text[pos..]));

		Ok(Value::List(NixList::from(ret)))
	}

	#[builtin("sort")]
	async fn builtin_sort(co: GenCo, comparator: Value, list: Value) -> Result<Value, ErrorKind> {
		// After CppNix's `prim_sort`: the list is forced first, then the
		// comparator, which must be a function, then every element.
		let list = list.to_list()?;
		if list.is_empty() {
			return Ok(Value::List(list));
		}
		let comparator = try_value!(comparator);
		let is_function = match &comparator {
			Value::Closure(_) | Value::Builtin(_) => true,
			Value::Attrs(attrs) => attrs.select("__functor").is_some(),
			_ => false,
		};
		if !is_function {
			return Err(ErrorKind::TypeError {
				expected: "lambda",
				actual: comparator.type_of(),
				value: Some(Box::new(comparator.clone())),
			});
		}
		let mut data = Vec::with_capacity(list.len());
		for value in list.into_inner() {
			data.push(try_value!(generators::request_force(&co, value).await));
		}
		match sort::peeksort(&co, &comparator, data).await? {
			Ok(data) => Ok(Value::List(data.into())),
			Err(catchable) => Ok(catchable),
		}
	}

	#[builtin("splitVersion")]
	async fn builtin_split_version(co: GenCo, s: Value) -> Result<Value, ErrorKind> {
		if s.is_catchable() {
			return Ok(s);
		}
		let s = s.to_str()?;
		let s = VersionPartsIter::new((&s).into());

		let parts = s
			.map(|s| {
				Value::from(match s {
					VersionPart::Number(n) => n,
					VersionPart::Word(w) => w,
				})
			})
			.collect::<Vec<Value>>();
		Ok(Value::List(NixList::construct(parts.len(), parts)))
	}

	#[builtin("stringLength")]
	async fn builtin_string_length(co: GenCo, #[lazy] s: Value) -> Result<Value, ErrorKind> {
		// also forces the value
		let span = generators::request_span(&co).await;
		let s = s
			.coerce_to_string(
				co,
				CoercionKind {
					strong: false,
					import_paths: true,
				},
				span,
			)
			.await
			.trace(
				None,
				"while evaluating the argument passed to builtins.stringLength",
			)?;

		if s.is_catchable() {
			return Ok(s);
		}

		Ok(Value::Integer(s.to_contextful_str()?.len() as i64))
	}

	#[builtin("sub")]
	async fn builtin_sub(co: GenCo, x: Value, y: Value) -> Result<Value, ErrorKind> {
		check_arithmetic(
			&x,
			&y,
			"while evaluating the first argument of the subtraction",
			"while evaluating the first argument of the subtraction",
			"while evaluating the second argument of the subtraction",
		)?;
		arithmetic_op!(&x, &y, -)
	}

	#[builtin("substring")]
	async fn builtin_substring(
		co: GenCo,
		start: Value,
		len: Value,
		s: Value,
	) -> Result<Value, ErrorKind> {
		let beg = start.as_int()?;
		let len = len.as_int()?;
		let span = generators::request_span(&co).await;
		let x = s
			.coerce_to_string(
				co,
				CoercionKind {
					strong: false,
					import_paths: true,
				},
				span,
			)
			.await
			.trace(
				None,
				"while evaluating the third argument (the string) passed to builtins.substring",
			)?;
		if x.is_catchable() {
			return Ok(x);
		}
		let x = x.to_contextful_str()?;

		if beg < 0 {
			return Err(ErrorKind::EvalError("negative start position in 'substring'".into()));
		}
		let beg = beg as usize;

		// Nix doesn't assert that the length argument is
		// non-negative when the starting index is GTE the
		// string's length.
		if beg >= x.len() {
			return Ok(Value::from(NixString::new_inherit_context_from(
				&x,
				BString::default(),
			)));
		}

		let end = if len < 0 {
			x.len()
		} else {
			cmp::min(beg + (len as usize), x.len())
		};

		Ok(Value::from(NixString::new_inherit_context_from(
			&x,
			&x[beg..end],
		)))
	}

	#[builtin("tail")]
	fn builtin_tail(list: Value) -> Result<Value, ErrorKind> {
		if list.is_catchable() {
			return Ok(list);
		}

		let xs = list.to_list()?;

		if xs.is_empty() {
			Err(ErrorKind::TailEmptyList)
		} else {
			let output = xs.into_iter().skip(1).collect::<Vec<_>>();
			Ok(Value::List(NixList::construct(output.len(), output)))
		}
	}

	#[builtin("throw")]
	async fn builtin_throw(co: GenCo, message: Value) -> Result<Value, ErrorKind> {
		// If it's already some error, let's propagate it immediately.
		if message.is_catchable() {
			return Ok(message);
		}

		let message = try_cek_to_value!(
			generators::request_string_coerce(
				&co,
				message,
				CoercionKind {
					strong: false,
					import_paths: true,
				},
			)
			.await
		);
		Ok(Value::from(CatchableErrorKind::Throw(message)))
	}

	#[builtin("toString")]
	async fn builtin_to_string(co: GenCo, #[lazy] x: Value) -> Result<Value, ErrorKind> {
		// TODO(edef): please fix me w.r.t. to catchability.
		// coerce_to_string forces for us
		// FIXME: should `coerce_to_string` preserve context?
		// it does for now.
		let span = generators::request_span(&co).await;
		x.coerce_to_string(
			co,
			CoercionKind {
				strong: true,
				import_paths: false,
			},
			span,
		)
		.await
		.trace(
			None,
			"while evaluating the first argument passed to builtins.toString",
		)
	}

	#[builtin("toXML")]
	async fn builtin_to_xml(co: GenCo, value: Value) -> Result<Value, ErrorKind> {
		let value = generators::request_deep_force(&co, value).await;
		if value.is_catchable() {
			return Ok(value);
		}

		let mut buf: Vec<u8> = vec![];
		let context = to_xml::value_to_xml(&mut buf, &value)?;

		Ok(NixString::new_context_from(context, buf).into())
	}

	#[builtin("trace")]
	async fn builtin_trace(
		state: Rc<BuiltinState>,
		co: GenCo,
		message: Value,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		// TODO(grfn): `trace` should be pluggable and capturable, probably via a method on
		// the VM
		if message.is_catchable() {
			return Ok(message);
		}
		print_trace(&co, message, &state.messages).await;
		Ok(generators::request_force(&co, value).await)
	}

	/// `builtins.trace` when `trace-verbose` is set, and otherwise a function
	/// returning its second argument, as in CppNix.
	#[builtin("traceVerbose")]
	async fn builtin_trace_verbose(
		state: Rc<BuiltinState>,
		co: GenCo,
		#[lazy] message: Value,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		if state.messages.trace_verbose.get() {
			let message = generators::request_force(&co, message).await;
			if message.is_catchable() {
				return Ok(message);
			}
			print_trace(&co, message, &state.messages).await;
		}
		Ok(generators::request_force(&co, value).await)
	}

	#[builtin("warn")]
	async fn builtin_warn(
		state: Rc<BuiltinState>,
		co: GenCo,
		message: Value,
		#[lazy] value: Value,
	) -> Result<Value, ErrorKind> {
		if message.is_catchable() {
			return Ok(message);
		}
		// Only strings are accepted, so that later versions can give other
		// values a meaning.
		let message = message.to_contextful_str()?;
		if state.messages.warning_output.get() {
			(state.messages.output.borrow())(&warning_lines(
				b"evaluation warning: ",
				message.as_bytes(),
			));
		}
		if state.messages.abort_on_warn.get() {
			return Err(ErrorKind::EvalError(
				"aborting to reveal stack trace of warning, as abort-on-warn is set".to_owned(),
			));
		}
		Ok(generators::request_force(&co, value).await)
	}

	#[builtin("toPath")]
	async fn builtin_to_path(co: GenCo, s: Value) -> Result<Value, ErrorKind> {
		if s.is_catchable() {
			return Ok(s);
		}

		let path = try_cek_to_value!(coerce_value_to_path(&co, s).await?);
		let path: Value = crate::value::canon_path(path).into();
		let span = generators::request_span(&co).await;
		path.coerce_to_string(
			co,
			CoercionKind {
				strong: false,
				import_paths: false,
			},
			span,
		)
		.await
	}

	#[builtin("tryEval")]
	async fn builtin_try_eval(co: GenCo, #[lazy] e: Value) -> Result<Value, ErrorKind> {
		let res = match generators::request_try_force(&co, e).await {
			Value::Catchable(_) => [("value", false.into()), ("success", false.into())],
			value => [("value", value), ("success", true.into())],
		};

		Ok(Value::attrs(NixAttrs::from_iter(res)))
	}

	#[builtin("typeOf")]
	fn builtin_type_of(x: Value) -> Result<Value, ErrorKind> {
		if x.is_catchable() {
			return Ok(x);
		}

		Ok(Value::from(x.type_of()))
	}

	#[builtin("zipAttrsWith")]
	async fn builtin_zip_attrs_with(
		co: GenCo,
		#[lazy] f: Value,
		list: Value,
	) -> Result<Value, ErrorKind> {
		let mut merged: FxHashMap<NixString, Vec<Value>> = FxHashMap::default();

		// For each key, merge values of the same key into one single attrset
		for set in list.to_list()? {
			let set = generators::request_force(&co, set).await;
			let set = set.to_attrs()?;
			for (key, val) in set {
				match merged.entry(key) {
					Entry::Occupied(mut occupied) => occupied.get_mut().push(val),
					Entry::Vacant(vacant) => {
						vacant.insert(vec![val]);
					}
				}
			}
		}

		if merged.is_empty() {
			return Ok(Value::attrs(NixAttrs::empty()));
		}
		let span = generators::request_span(&co).await;

		// apply the function to each element
		let lambda = Thunk::suspended_call_lambda(span);
		let attrs = merged.into_iter().map(|(key, vals)| {
			let val = Value::Thunk(Thunk::new_suspended_call_with_lambda(
				Value::Thunk(Thunk::new_suspended_call_with_lambda(
					f.clone(),
					key.clone().into(),
					lambda.clone(),
				)),
				Value::List(NixList::construct(vals.len(), vals)),
				lambda.clone(),
			));
			(key, val)
		});

		Ok(Value::attrs(NixAttrs::from_iter(attrs)))
	}
}

/// `builtins.floor` and `builtins.ceil` after CppNix: an integer is returned
/// unchanged, and a rounded float must lie within the integer range.
fn round_to_int(value: &Value, round: fn(f64) -> f64) -> Result<Value, ErrorKind> {
	if let Value::Integer(i) = value {
		return Ok(Value::Integer(*i));
	}
	let float = value.as_float()?;
	let rounded = round(float);
	// A power of two, so the comparison is exact.
	let min = i64::MIN as f64;
	if rounded >= min && rounded < -min {
		Ok(Value::Integer(rounded as i64))
	} else {
		Err(ErrorKind::EvalError(format!(
			"NixFloat argument {} is not in the range of NixInt",
			Value::Float(float)
		)))
	}
}

/// The set of standard pure builtins in Nix, mostly concerned with
/// data structure manipulation (string, attrs, list, etc. functions).
/// `builtins.nixPath`: the lookup path as `{ prefix, path }` sets.
pub(crate) fn nix_path_value(search_path: &crate::NixSearchPath) -> Value {
	let entries: Vec<Value> = search_path
		.entries()
		.iter()
		.map(|entry| {
			Value::attrs(NixAttrs::from_iter([
				("path", Value::from(entry.path.as_str())),
				("prefix", Value::from(entry.prefix.as_str())),
			]))
		})
		.collect();
	Value::List(NixList::from(entries))
}

pub fn pure_builtins() -> Vec<(&'static str, Value)> {
	pure_builtins_with_features(ExperimentalFeatures::default())
}

/// [`pure_builtins`] whose behaviour depends on `experimental_features`.
pub fn pure_builtins_with_features(
	experimental_features: ExperimentalFeatures,
) -> Vec<(&'static str, Value)> {
	pure_builtins_with_messages(experimental_features, EvaluationMessages::default())
}

pub(crate) fn pure_builtins_with_messages(
	experimental_features: ExperimentalFeatures,
	messages: EvaluationMessages,
) -> Vec<(&'static str, Value)> {
	let mut result = pure_builtins::builtins(Rc::new(BuiltinState {
		experimental_features,
		messages,
		..Default::default()
	}));

	// Pure-value builtins
	result.push(("nixVersion", Value::from("2.18.3-compat-nix-rs-0.1")));
	result.push(("langVersion", Value::Integer(6)));
	result.push(("null", Value::Null));
	result.push(("true", Value::Bool(true)));
	result.push(("false", Value::Bool(false)));

	result.push((
		"currentSystem",
		crate::systems::llvm_triple_to_nix_double(CURRENT_PLATFORM).into(),
	));

	result
}

#[builtins]
mod placeholder_builtins {
	use crate::NixContext;

	use super::*;

	#[builtin("unsafeDiscardStringContext")]
	async fn builtin_unsafe_discard_string_context(
		co: GenCo,
		s: Value,
	) -> Result<Value, ErrorKind> {
		let span = generators::request_span(&co).await;
		let mut v = s
			.coerce_to_string(
				co,
				// It's weak because
				// lists, integers, floats and null are not
				// accepted as parameters.
				CoercionKind {
					strong: false,
					import_paths: true,
				},
				span,
			)
			.await?
			.to_contextful_str()?;
		v.clear_context();
		Ok(Value::from(v))
	}

	#[builtin("unsafeDiscardOutputDependency")]
	async fn builtin_unsafe_discard_output_dependency(
		co: GenCo,
		s: Value,
	) -> Result<Value, ErrorKind> {
		let span = generators::request_span(&co).await;
		let mut v = s
			.coerce_to_string(
				co,
				// It's weak because
				// lists, integers, floats and null are not
				// accepted as parameters.
				CoercionKind {
					strong: false,
					import_paths: true,
				},
				span,
			)
			.await?
			.to_contextful_str()?;

		// If there's any context, we will swap any ... by a path one.
		if let Some(c) = v.take_context() {
			let mut context = NixContext::new();
			context.extend(c.into_iter().map(|elem| match elem {
				crate::NixContextElement::Derivation(drv_path) => {
					crate::NixContextElement::Plain(drv_path.to_string())
				}
				elem => elem.clone(),
			}));

			return Ok(Value::String(NixString::new_context_from(context, v)));
		}
		Ok(Value::from(v))
	}

	#[builtin("addErrorContext")]
	async fn builtin_add_error_context(
		co: GenCo,
		#[lazy] _context: Value,
		#[lazy] val: Value,
	) -> Result<Value, ErrorKind> {
		generators::emit_warning_kind(&co, WarningKind::NotImplemented("builtins.addErrorContext"))
			.await;
		Ok(val)
	}

	#[builtin("unsafeGetAttrPos")]
	async fn builtin_unsafe_get_attr_pos(
		co: GenCo,
		name: Value,
		attrset: Value,
	) -> Result<Value, ErrorKind> {
		let attrs = attrset.to_attrs()?;
		let span = attrs.get_attr_pos(&name.to_str()?);

		let Some(span) = span else {
			return Ok(Value::Null);
		};

		Ok(generators::request_span_pos(&co, span).await)
	}
}

pub fn placeholders() -> Vec<(&'static str, Value)> {
	placeholder_builtins::builtins()
}
