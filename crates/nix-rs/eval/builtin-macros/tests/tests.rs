pub use nix_rs_eval::{Builtin, Value};
use nix_rs_eval_builtin_macros::builtins;

#[builtins]
mod builtins {
	use nix_rs_eval::generators::{Gen, GenCo};
	use nix_rs_eval::{ErrorKind, Value};

	/// Test docstring.
	///
	/// It has multiple lines!
	#[builtin("identity")]
	pub async fn builtin_identity(co: GenCo, x: Value) -> Result<Value, ErrorKind> {
		Ok(x)
	}

	#[builtin("tryEval")]
	pub async fn builtin_try_eval(_co: GenCo, #[lazy] _x: Value) -> Result<Value, ErrorKind> {
		unimplemented!("builtin is never called")
	}
}

#[test]
fn builtins() {
	let builtins = builtins::builtins();
	assert_eq!(builtins.len(), 2);

	let (_, identity) = builtins
		.iter()
		.find(|(name, _)| *name == "identity")
		.unwrap();

	match identity {
		Value::Builtin(identity) => assert_eq!(
			identity.documentation(),
			Some(
				r#" Test docstring.

 It has multiple lines!"#
			)
		),

		_ => panic!("builtin was not a builtin"),
	}
}

#[builtins(state = "std::rc::Rc<i64>")]
mod synchronous {
	use nix_rs_eval::generators::Gen;
	use nix_rs_eval::{ErrorKind, Value};

	#[builtin("atSpan")]
	fn at_span(
		state: std::rc::Rc<i64>,
		span: impl std::fmt::Debug,
		value: Value,
	) -> Result<Value, ErrorKind> {
		Ok(Value::from(format!(
			"{span:?}:{}:{}",
			state,
			value.as_int()?
		)))
	}

	#[builtin("addState")]
	fn add_state(state: std::rc::Rc<i64>, value: Value) -> Result<Value, ErrorKind> {
		Ok(Value::Integer(*state + value.as_int()?))
	}

	#[builtin("pick")]
	fn pick(#[lazy] _ignored: Value, #[catch] value: Value) -> Result<Value, ErrorKind> {
		if value.is_catchable() {
			Ok(Value::Integer(77))
		} else {
			Ok(value)
		}
	}

	#[builtin("pair")]
	fn pair(left: Value, right: Value) -> Result<Value, ErrorKind> {
		Ok(Value::Integer(left.as_int()? + right.as_int()?))
	}
}

#[test]
fn synchronous_arguments_and_generator_fallback() {
	for (expression, expected) in [
		("builtins.addState 2", 12),
		("builtins.addState (1 + 2)", 13),
		("builtins.pair 1 (1 + 2)", 4),
		("builtins.pair (1 + 2) 4", 7),
		("builtins.pair (1 + 2) (3 + 4)", 10),
		(r#"builtins.pick (abort "lazy") 5"#, 5),
		(r#"builtins.pick (abort "lazy") (1 + 2)"#, 3),
		(r#"builtins.pick (abort "lazy") (throw "caught")"#, 77),
		(
			r#"let x = throw "caught"; in builtins.seq (builtins.tryEval x) (builtins.pick (abort "lazy") x)"#,
			77,
		),
		(
			r#"let x = 1 + 2; in builtins.seq x (builtins.pick (abort "lazy") x)"#,
			3,
		),
		(
			r#"if (builtins.tryEval (builtins.pair (throw "left") (abort "right"))).success then 0 else 1"#,
			1,
		),
	] {
		let result = nix_rs_eval::Evaluation::builder_pure()
			.add_builtins(synchronous::builtins(std::rc::Rc::new(10)))
			.build()
			.evaluate(expression, None);
		assert!(
			result.errors.is_empty(),
			"{expression}: {:?}",
			result.errors
		);
		assert_eq!(
			result.value.unwrap().as_int().unwrap(),
			expected,
			"{expression}"
		);
	}
}
#[builtins(state = "std::rc::Rc<i64>")]
mod legacy_span {
	use nix_rs_eval::generators::{self, Gen, GenCo};
	use nix_rs_eval::{ErrorKind, Value};

	#[builtin("atSpan")]
	pub async fn at_span(
		state: std::rc::Rc<i64>,
		co: GenCo,
		value: Value,
	) -> Result<Value, ErrorKind> {
		let span = generators::request_span(&co).await;
		Ok(Value::from(format!(
			"{span:?}:{}:{}",
			state,
			value.as_int()?
		)))
	}
}

#[test]
fn synchronous_span_matches_generator() {
	for expression in [
		"builtins.atSpan 2",
		"builtins.atSpan (1 + 2)",
		"let x = 1 + 2; in builtins.seq x (builtins.atSpan x)",
	] {
		let mut outputs = vec![];
		for builtins in [
			synchronous::builtins(std::rc::Rc::new(10)),
			legacy_span::builtins(std::rc::Rc::new(10)),
		] {
			let result = nix_rs_eval::Evaluation::builder_pure()
				.add_builtins(builtins)
				.build()
				.evaluate(expression, None);
			assert!(
				result.errors.is_empty(),
				"{expression}: {:?}",
				result.errors
			);
			outputs.push(result.value.unwrap().to_str().unwrap().as_bytes().to_vec());
		}
		assert_eq!(outputs[0], outputs[1], "{expression}");
	}
}
