use crate::*;
use pretty_assertions::assert_matches;

#[rstest::rstest]
#[case("{ f = x: x; }")]
#[case("let f = x: x; in { inherit f; }")]
#[case("{ f = (x: x); }")]
fn lazy_function_literals_keep_unforced_previews(#[case] expression: &str) {
	let evaluation = Evaluation::builder_pure().build();
	let source = evaluation.source_map();
	let result = evaluation.evaluate(expression, None);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_eq!(
		ValuePrinter::print(&source, PrintOptions::ERROR, &result.value.unwrap()),
		"{ f = «thunk»; }".as_bytes()
	);
}

#[test]
fn returned_function_literals_keep_recursive_captures() {
	let result = Evaluation::builder_pure().build().evaluate(
		"let f = x: y: if y == 0 then x else f x (y - 1); in f 42 3",
		None,
	);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_eq!(result.value.unwrap().as_int().unwrap(), 42);
}

#[rstest::rstest]
#[case(0)]
#[case(1)]
#[case(2)]
fn shared_native_thunk_aliases_memoize_once(#[case] depth: usize) {
	use crate::value::Thunk;
	use std::{cell::Cell, rc::Rc};
	let calls = Rc::new((0..=depth).map(|_| Cell::new(0)).collect::<Vec<_>>());
	let leaf_calls = calls.clone();
	let mut thunk = Thunk::new_suspended_native(Box::new(move || {
		leaf_calls[0].set(leaf_calls[0].get() + 1);
		Ok(Value::Integer(21))
	}));
	let mut aliases = vec![thunk.clone()];
	for index in 1..=depth {
		let inner = thunk;
		let calls = calls.clone();
		thunk = Thunk::new_suspended_native(Box::new(move || {
			calls[index].set(calls[index].get() + 1);
			Ok(Value::Thunk(inner.clone()))
		}));
		aliases.push(thunk.clone());
	}
	let result = Evaluation::builder_pure()
		.add_builtins([("sharedNative", Value::Thunk(thunk))])
		.build()
		.evaluate(
			"builtins.add builtins.sharedNative builtins.sharedNative",
			None,
		);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_eq!(result.value.unwrap().as_int().unwrap(), 42);
	for (thunk, calls) in aliases.iter().zip(calls.iter()) {
		assert_eq!(calls.get(), 1);
		assert!(thunk.is_forced());
		assert_eq!(thunk.value().as_int().unwrap(), 21);
	}
}

#[test]
fn evaluator_message_settings_remain_independent() {
	// Build both evaluations before running either. Settings attached to
	// one builtin set must not change a second set on the same thread.
	let fatal = Evaluation::builder_pure()
		.abort_on_warn(true)
		.warning_output(false)
		.build();
	let ordinary = Evaluation::builder_pure().warning_output(false).build();
	let expr = r#"builtins.tryEval (builtins.warn "warning" 1)"#;
	let fatal = fatal.evaluate(expr, None);
	assert_eq!(fatal.errors.len(), 1);
	assert_matches!(underlying_error_kind(&fatal.errors[0].kind), ErrorKind::EvalError(message) if message.contains("abort-on-warn"));
	let ordinary = ordinary.evaluate(expr, None);
	assert!(ordinary.errors.is_empty(), "{:?}", ordinary.errors);
	let attrs = ordinary.value.unwrap().to_attrs().unwrap();
	assert_eq!(attrs.select("success").unwrap().as_bool().unwrap(), true);

	let verbose = Evaluation::builder_pure().trace_verbose(true).build();
	let quiet = Evaluation::builder_pure().build();
	let expr = r#"builtins.traceVerbose (throw "forced message") 1"#;
	assert_eq!(verbose.evaluate(expr, None).errors.len(), 1);
	let quiet = quiet.evaluate(expr, None);
	assert!(quiet.errors.is_empty(), "{:?}", quiet.errors);
	assert_matches!(quiet.value, Some(Value::Integer(1)));
}

#[test]
fn evaluator_message_sink_preserves_evaluation_order() {
	let messages = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
	let sink = messages.clone();
	let result = Evaluation::builder_pure()
		.trace_verbose(true)
		.message_output(move |bytes| sink.borrow_mut().push(bytes.to_vec()))
		.build()
		.evaluate(
			r#"builtins.warn "first" (builtins.traceVerbose "second" 123)"#,
			None,
		);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_matches!(result.value, Some(Value::Integer(123)));
	assert_eq!(
		*messages.borrow(),
		vec![
			b"evaluation warning: first\n".to_vec(),
			b"trace: second\n".to_vec()
		]
	);
}

fn underlying_error_kind(mut kind: &ErrorKind) -> &ErrorKind {
	while let ErrorKind::BytecodeError(error) | ErrorKind::NativeError { err: error, .. } = kind {
		kind = &error.kind;
	}
	kind
}

#[rstest::rstest]
#[case("abort")]
#[case("throw")]
fn error_builtin_coerces_message(#[case] builtin: &str) {
	for argument in [
		r#"{ __toString = _: "coerced"; }"#,
		r#"{ outPath = "coerced"; }"#,
		r#"builtins.appendContext "coerced" { "/nix/store/00000000000000000000000000000000-source" = { path = true; }; }"#,
	] {
		let expression = format!("{builtin} ({argument})");
		let result = Evaluation::builder_pure()
			.build()
			.evaluate(&expression, None);
		assert_eq!(result.errors.len(), 1, "{expression}: {:?}", result.errors);
		match underlying_error_kind(&result.errors[0].kind) {
			ErrorKind::Abort(message) if builtin == "abort" => assert_eq!(message, "coerced"),
			ErrorKind::CatchableError(cek) if builtin == "throw" => {
				assert_matches!(cek.untraced(), CatchableErrorKind::Throw(message) if message.as_bytes() == b"coerced");
			}
			error => panic!("{expression}: unexpected error {error:?}"),
		}
	}
}

#[rstest::rstest]
#[case("abort")]
#[case("throw")]
fn error_builtin_uses_weak_coercion(#[case] builtin: &str) {
	for argument in ["42", "true", "null", r#"[ "message" ]"#] {
		let expression = format!("builtins.tryEval ({builtin} ({argument}))");
		let result = Evaluation::builder_pure()
			.build()
			.evaluate(&expression, None);
		assert_eq!(result.errors.len(), 1, "{expression}: {:?}", result.errors);
		assert_matches!(
			underlying_error_kind(&result.errors[0].kind),
			ErrorKind::NotCoercibleToString { .. }
		);
	}
}

#[rstest::rstest]
#[case("abort")]
#[case("throw")]
fn error_builtin_propagates_coercion_throw(#[case] builtin: &str) {
	let expression = format!(r#"{builtin} {{ __toString = _: throw "inner"; }}"#);
	let result = Evaluation::builder_pure()
		.build()
		.evaluate(&expression, None);
	assert_eq!(result.errors.len(), 1);
	let ErrorKind::CatchableError(cek) = &result.errors[0].kind else {
		panic!("unexpected error {:?}", result.errors[0]);
	};
	assert_matches!(cek.untraced(), CatchableErrorKind::Throw(message) if message.as_bytes() == b"inner");
}

#[test]
fn append_context_adds_and_preserves_dependencies() {
	let result = Evaluation::builder_pure().build().evaluate(
		r#"
		let
			path = "/nix/store/00000000000000000000000000000000-source";
			drv = "/nix/store/00000000000000000000000000000000-test.drv";
			plain = { ${path} = { path = true; }; };
			outputs = { ${drv} = { allOutputs = true; outputs = [ "out" ]; }; };
			initial = builtins.appendContext "text" plain;
			grown = builtins.appendContext initial outputs;
		in
			builtins.getContext initial == plain
			&& builtins.getContext grown == plain // outputs
			&& builtins.getContext (builtins.appendContext grown plain) == plain // outputs
			&& builtins.getContext (builtins.appendContext initial {}) == plain
			&& !(builtins.hasContext (builtins.appendContext "text" {}))
			&& builtins.unsafeDiscardStringContext grown == "text"
		"#,
		None,
	);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_matches!(result.value, Some(Value::Bool(true)));
}

// Results checked with the pinned CppNix 2.35.2 reference. Its split uses
// std::cregex_iterator, including an empty match after a nonempty match.
#[rstest::rstest]
#[case("", "abc", r#"[ "" [] "a" [] "b" [] "c" [] "" ]"#)]
#[case("()", "abc", r#"[ "" [ "" ] "a" [ "" ] "b" [ "" ] "c" [ "" ] "" ]"#)]
#[case("^", "abc", r#"[ "" [] "abc" ]"#)]
#[case("$", "abc", r#"[ "abc" [] "" ]"#)]
#[case("a*", "abc", r#"[ "" [] "" [] "b" [] "c" [] "" ]"#)]
#[case("b*", "abc", r#"[ "" [] "a" [] "" [] "c" [] "" ]"#)]
#[case("", "", r#"[ "" [] "" ]"#)]
#[case("a*", "aa", r#"[ "" [] "" [] "" ]"#)]
#[case(
	"",
	"é",
	r#"[ "" [] (builtins.substring 0 1 "é") [] (builtins.substring 1 1 "é") [] "" ]"#
)]
#[case(
	"(.)",
	"é",
	r#"[ "" [ (builtins.substring 0 1 "é") ] "" [ (builtins.substring 1 1 "é") ] "" ]"#
)]
fn split_zero_width_matches(#[case] regex: &str, #[case] text: &str, #[case] expected: &str) {
	let expression = format!(r#"builtins.split "{regex}" "{text}" == {expected}"#);
	let result = Evaluation::builder_pure()
		.build()
		.evaluate(&expression, None);
	assert!(
		result.errors.is_empty(),
		"{expression}: {:?}",
		result.errors
	);
	assert_matches!(result.value, Some(Value::Bool(true)));
}

#[rstest::rstest]
#[case(r#"lengths (builtins.split "" "é") == [ 0 null 1 null 1 null 0 ]"#)]
#[case(r#"lengths (builtins.split "." "é") == [ 0 null 0 null 0 ]"#)]
#[case(r#"builtins.split "[[:alpha:]]" "é" == [ "é" ]"#)]
#[case(r#"lengths (builtins.split "" byte) == [ 0 null 1 null 0 ]"#)]
#[case(r#"builtins.split "(.)" byte == [ "" [ byte ] "" ]"#)]
#[case(r#"builtins.split "x" byte == [ byte ]"#)]
fn split_matches_and_preserves_bytes(#[case] assertion: &str) {
	// Checked with CppNix 2.35.2, including the invalid UTF-8 substring.
	let expression = format!(
		r#"let
			byte = builtins.substring 0 1 "é";
			lengths = xs: map (x: if builtins.isString x then builtins.stringLength x else null) xs;
		in {assertion}"#
	);
	let result = Evaluation::builder_pure()
		.build()
		.evaluate(&expression, None);
	assert!(
		result.errors.is_empty(),
		"{expression}: {:?}",
		result.errors
	);
	assert_matches!(result.value, Some(Value::Bool(true)));
}

#[test]
fn test_source_builtin() {
	// Test an evaluation with a source-only builtin. The test ensures
	// that the artificially constructed thunking is correct.

	let eval = Evaluation::builder_pure()
		.add_src_builtin("testSourceBuiltin", "42")
		.build();

	let result = eval.evaluate("builtins.testSourceBuiltin", None);
	assert!(
		result.errors.is_empty(),
		"evaluation failed: {:?}",
		result.errors
	);

	let value = result.value.unwrap();
	assert_matches!(value, Value::Integer(i) if i == 42);
}

#[test]
fn skip_broken_bytecode() {
	let result = Evaluation::builder_pure()
		.build()
		.evaluate(/* code = */ "x", None);

	assert_eq!(result.errors.len(), 1);
	assert_matches!(result.errors[0].kind, ErrorKind::UnknownStaticVariable(_));
}

/// Checks that deep forcing happens in lexicographic key order
#[test]
fn key_order_deep_force() {
	let result = Evaluation::builder_pure().build().evaluate(
		/* code = */
		r#"builtins.toXML {
          c = throw "ccc";
          a = throw "aaa";
          b = throw "bbb";
          d = throw "dd";
        }"#,
		None,
	);
	assert_eq!(result.errors.len(), 1);

	let ErrorKind::CatchableError(cek) = &result.errors[0].kind else {
		panic!("unexpected error {:?}", result.errors[0]);
	};
	assert_matches!(cek.untraced(), CatchableErrorKind::Throw(s) if s == &NixString::from("aaa"));
}

#[test]
fn integer_arithmetic_add_overflow_is_an_evaluation_error() {
	let result = Evaluation::builder_pure()
		.build()
		.evaluate("9223372036854775807 + 1", None);
	assert!(result.value.is_none());
	assert_eq!(result.errors.len(), 1);
	assert!(
		result.errors[0]
			.fancy_format_str()
			.contains("integer overflow in adding 9223372036854775807 + 1")
	);
}

#[test]
fn integer_arithmetic_negation_overflow_is_an_evaluation_error() {
	let result = Evaluation::builder_pure()
		.build()
		.evaluate("let min = -9223372036854775807 - 1; in -min", None);
	assert!(result.value.is_none());
	assert_eq!(result.errors.len(), 1);
	assert!(
		result.errors[0]
			.fancy_format_str()
			.contains("integer overflow in subtracting 0 - -9223372036854775808")
	);
}

#[rstest::rstest]
#[case("min - 1", "integer overflow in subtracting -9223372036854775808 - 1")]
#[case(
	"min * (-1)",
	"integer overflow in multiplying -9223372036854775808 * -1"
)]
#[case("min / (-1)", "integer overflow in dividing -9223372036854775808 / -1")]
#[case(
	"builtins.add max 1",
	"integer overflow in adding 9223372036854775807 + 1"
)]
#[case(
	"builtins.sub min 1",
	"integer overflow in subtracting -9223372036854775808 - 1"
)]
#[case(
	"builtins.mul min (-1)",
	"integer overflow in multiplying -9223372036854775808 * -1"
)]
#[case(
	"builtins.div min (-1)",
	"integer overflow in dividing -9223372036854775808 / -1"
)]
#[case(
	"builtins.tryEval (max + 1)",
	"integer overflow in adding 9223372036854775807 + 1"
)]
#[case("1 / 0", "division by zero")]
#[case("1 / 0.0", "division by zero")]
#[case("1.0 / 0", "division by zero")]
#[case("1.0 / (-0.0)", "division by zero")]
#[case("builtins.div 1 0", "division by zero")]
#[case("builtins.div 1 0.0", "division by zero")]
#[case("builtins.div 1.0 0", "division by zero")]
#[case("builtins.div 1.0 (-0.0)", "division by zero")]
#[case("builtins.div \"invalid\" 0", "division by zero")]
fn integer_arithmetic_errors(#[case] expression: &str, #[case] message: &str) {
	let expression = format!("let max = 9223372036854775807; min = -max - 1; in {expression}");
	let result = Evaluation::builder_pure()
		.build()
		.evaluate(&expression, None);
	assert!(result.value.is_none(), "{expression}: {:?}", result.value);
	assert_eq!(result.errors.len(), 1, "{expression}");
	let diagnostic = result.errors[0].fancy_format_str();
	assert!(diagnostic.contains(message), "{expression}: {diagnostic}");
}

#[rstest::rstest]
#[case("max + 0", Value::Integer(i64::MAX))]
#[case("min - 0", Value::Integer(i64::MIN))]
#[case("min * 1", Value::Integer(i64::MIN))]
#[case("min / 1", Value::Integer(i64::MIN))]
#[case("-max", Value::Integer(-i64::MAX))]
#[case("-0", Value::Integer(0))]
#[case("(-5) / 2", Value::Integer(-2))]
#[case("builtins.add max 0", Value::Integer(i64::MAX))]
#[case("builtins.sub min 0", Value::Integer(i64::MIN))]
#[case("builtins.mul min 1", Value::Integer(i64::MIN))]
#[case("builtins.div min 1", Value::Integer(i64::MIN))]
#[case("builtins.div (-5) 2", Value::Integer(-2))]
#[case("1 + 2.5", Value::Float(3.5))]
#[case("2.5 + 1", Value::Float(3.5))]
#[case("5 - 2.5", Value::Float(2.5))]
#[case("2.5 - 1", Value::Float(1.5))]
#[case("2 * 2.5", Value::Float(5.0))]
#[case("2.5 * 2", Value::Float(5.0))]
#[case("5 / 2.0", Value::Float(2.5))]
#[case("5.0 / 2", Value::Float(2.5))]
#[case("builtins.add 1.5 2.0", Value::Float(3.5))]
#[case("builtins.sub 5.0 2.5", Value::Float(2.5))]
#[case("builtins.mul 2.0 2.5", Value::Float(5.0))]
#[case("builtins.div 5.0 2.0", Value::Float(2.5))]
fn integer_arithmetic_preserves_valid_results(#[case] expression: &str, #[case] expected: Value) {
	let expression = format!("let max = 9223372036854775807; min = -max - 1; in {expression}");
	let result = Evaluation::builder_pure()
		.build()
		.evaluate(&expression, None);
	assert!(
		result.errors.is_empty(),
		"{expression}: {:?}",
		result.errors
	);
	match (result.value.unwrap(), expected) {
		(Value::Integer(actual), Value::Integer(expected)) => assert_eq!(actual, expected),
		(Value::Float(actual), Value::Float(expected)) => assert_eq!(actual, expected),
		(actual, expected) => panic!("{expression}: expected {expected:?}, got {actual:?}"),
	}
}

/// Edge cases of builtins and literals the copied language fixtures do not
/// check, with results and messages recorded from CppNix 2.35.2.
#[rstest::rstest]
#[case("builtins.ceil (-9223372036854775807 - 1)", Ok("-9223372036854775808"))]
#[case(
	"builtins.floor 1.0e30",
	Err("NixFloat argument 1e+30 is not in the range of NixInt")
)]
#[case(
	"builtins.replaceStrings [ \"a\" ] [ ] \"x\"",
	Err("'from' and 'to' arguments passed to builtins.replaceStrings have different lengths")
)]
#[case("\"a\r\nb\\\r\nc\rd\"", Ok("\"a\\nb\\r\\nc\\nd\""))]
fn builtin_and_literal_edge_cases(#[case] expression: &str, #[case] expected: Result<&str, &str>) {
	let result = Evaluation::builder_pure()
		.build()
		.evaluate(expression, None);
	match expected {
		Ok(value) => {
			assert!(
				result.errors.is_empty(),
				"{expression}: {:?}",
				result.errors
			);
			assert_eq!(result.value.unwrap().to_string(), value, "{expression}");
		}
		Err(message) => {
			assert_eq!(result.errors.len(), 1, "{expression}");
			let diagnostic = result.errors[0].fancy_format_str();
			assert!(diagnostic.contains(message), "{expression}: {diagnostic}");
		}
	}
}

#[test]
fn named_sources_resolve_relative_paths_without_host_files() {
	let location = std::path::PathBuf::from("/nonexistent/noxi-virtual-source/source.nix");
	let expected = std::path::Path::new("/nonexistent/noxi-virtual-source/child.nix");
	let result = Evaluation::builder_pure()
		.build()
		.evaluate("./child.nix", Some(location));
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_eq!(result.value.unwrap().to_path().unwrap().as_path(), expected);
}

#[test]
#[cfg(feature = "impure")]
fn ordinary_imports_reuse_resolution_after_the_file_is_removed() {
	let dir = tempfile::tempdir().unwrap();
	let file = dir.path().join("value.nix");
	std::fs::write(&file, r#"builtins.trace "remove source" 17"#).unwrap();
	let remove = file.clone();
	let result = Evaluation::builder_impure()
		.message_output(move |_| std::fs::remove_file(&remove).unwrap())
		.build()
		.evaluate_in_dir(
			"if import ./value.nix == 17 then import ./value.nix else 0",
			dir.path().to_owned(),
		);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_matches!(result.value, Some(Value::Integer(17)));
	assert!(!file.exists());
}

#[test]
#[cfg(all(feature = "impure", unix))]
fn scoped_imports_resolve_symlinks_each_time() {
	let dir = tempfile::tempdir().unwrap();
	let link = dir.path().join("link.nix");
	std::fs::write(
		dir.path().join("first.nix"),
		r#"builtins.trace "retarget" 17"#,
	)
	.unwrap();
	std::fs::write(dir.path().join("second.nix"), "23").unwrap();
	std::os::unix::fs::symlink("first.nix", &link).unwrap();
	let result = Evaluation::builder_impure()
		.message_output(move |_| {
			std::fs::remove_file(&link).unwrap();
			std::os::unix::fs::symlink("second.nix", &link).unwrap();
		})
		.build()
		.evaluate_in_dir(
			"if scopedImport {} ./link.nix == 17 then scopedImport {} ./link.nix else 0",
			dir.path().to_owned(),
		);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	assert_matches!(result.value, Some(Value::Integer(23)));
}
