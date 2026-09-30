use crate::*;
use pretty_assertions::assert_matches;

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
	assert_matches!(result.errors[0].kind, ErrorKind::UnknownStaticVariable);
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

	assert_matches!(
		&result.errors[0].kind,
		ErrorKind::CatchableError(CatchableErrorKind::Throw(s)) if s == &NixString::from("aaa")
	);
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
