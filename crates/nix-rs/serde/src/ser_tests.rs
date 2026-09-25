use nix_rs_eval::Value;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::{de::from_value, ser::to_value};

/// Assert that serialising `value` produces a `Value` matching `pat`.
macro_rules! assert_value {
	($value:expr, $pat:pat) => {{
		let v = to_value(&$value).expect("should serialise");
		assert!(matches!(v, $pat), "got: {:?}", v);
	}};
}

#[test]
fn serialize_none() {
	assert_value!(None::<u8>, Value::Null);
}

#[test]
fn serialize_some() {
	assert_value!(Some(42u32), Value::Integer(42));
}

#[test]
fn serialize_bool_true() {
	assert_value!(true, Value::Bool(true));
}

#[test]
fn serialize_bool_false() {
	assert_value!(false, Value::Bool(false));
}

#[test]
fn serialize_i64() {
	assert_value!(42i64, Value::Integer(42));
}

#[test]
fn serialize_negative_i64() {
	assert_value!(-1i64, Value::Integer(-1));
}

#[test]
fn serialize_u32() {
	assert_value!(100u32, Value::Integer(100));
}

#[test]
fn serialize_u64_in_range() {
	assert_value!(42u64, Value::Integer(42));
}

#[test]
fn serialize_u64_overflow() {
	let result = to_value(&u64::MAX);
	assert!(
		matches!(result, Err(crate::Error::IntegerOverflow { .. })),
		"expected IntegerOverflow, got: {result:?}"
	);
}

#[test]
fn serialize_f64() {
	let v = to_value(&1.5f64).expect("should serialise");
	assert!(matches!(v, Value::Float(f) if f == 1.5));
}

#[test]
fn serialize_f32() {
	let v = to_value(&1.5f32).expect("should serialise");
	assert!(matches!(v, Value::Float(_)));
}

#[test]
fn serialize_string() {
	let v = to_value(&"hello").expect("should serialise");
	assert!(matches!(v, Value::String(ref s) if *s == "hello"));
}

#[test]
fn serialize_char() {
	let v = to_value(&'x').expect("should serialise");
	assert!(matches!(v, Value::String(ref s) if *s == "x"));
}

#[test]
fn serialize_unit() {
	assert_value!((), Value::Null);
}

#[test]
fn serialize_empty_list() {
	let v: Vec<u8> = vec![];
	let result = to_value(&v).expect("should serialise");
	assert!(matches!(result, Value::List(ref l) if l.is_empty()));
}

#[test]
fn serialize_integer_list() {
	let v = vec![1i64, 2, 3];
	let result = to_value(&v).expect("should serialise");
	if let Value::List(list) = result {
		assert_eq!(list.len(), 3);
		assert!(matches!(list[0], Value::Integer(1)));
		assert!(matches!(list[1], Value::Integer(2)));
		assert!(matches!(list[2], Value::Integer(3)));
	} else {
		panic!("expected list");
	}
}

#[test]
fn serialize_empty_map() {
	let v: HashMap<String, u8> = HashMap::new();
	let result = to_value(&v).expect("should serialise");
	assert!(matches!(result, Value::Attrs(ref a) if a.is_empty()));
}

#[test]
fn serialize_string_map() {
	let mut map = HashMap::new();
	map.insert("age".to_string(), 42u32);
	let result = to_value(&map).expect("should serialise");
	if let Value::Attrs(attrs) = result {
		let mut found = false;
		for (k, v) in attrs.into_iter() {
			if k == "age" {
				assert!(matches!(v, Value::Integer(42)));
				found = true;
			}
		}
		assert!(found, "key 'age' not found");
	} else {
		panic!("expected attrs");
	}
}

#[test]
fn serialize_non_string_key_fails() {
	let mut map: HashMap<u32, u32> = HashMap::new();
	map.insert(1, 42);
	let result = to_value(&map);
	assert!(
		matches!(result, Err(crate::Error::NonStringKey)),
		"expected NonStringKey, got: {result:?}"
	);
}

#[test]
fn serialize_struct() {
	#[derive(Serialize)]
	struct Person {
		name: String,
		age: u32,
	}

	let p = Person {
		name: "Slartibartfast".into(),
		age: 42,
	};
	let result = to_value(&p).expect("should serialise");
	if let Value::Attrs(attrs) = result {
		let mut name_ok = false;
		let mut age_ok = false;
		for (k, v) in attrs.into_iter() {
			if k == "name" {
				assert!(matches!(v, Value::String(ref s) if *s == "Slartibartfast"));
				name_ok = true;
			} else if k == "age" {
				assert!(matches!(v, Value::Integer(42)));
				age_ok = true;
			}
		}
		assert!(name_ok && age_ok);
	} else {
		panic!("expected attrs");
	}
}

#[test]
fn serialize_newtype_struct() {
	#[derive(Serialize)]
	struct Number(u32);

	assert_value!(Number(42), Value::Integer(42));
}

#[test]
fn serialize_tuple() {
	let v = ("foo", 42u32);
	let result = to_value(&v).expect("should serialise");
	if let Value::List(list) = result {
		assert_eq!(list.len(), 2);
		assert!(matches!(list[0], Value::String(ref s) if *s == "foo"));
		assert!(matches!(list[1], Value::Integer(42)));
	} else {
		panic!("expected list");
	}
}

#[test]
fn serialize_unit_variant() {
	#[derive(Serialize)]
	#[allow(dead_code)]
	enum Color {
		Red,
		Green,
	}

	let v = to_value(&Color::Red).expect("should serialise");
	assert!(matches!(v, Value::String(ref s) if *s == "Red"));
}

#[test]
fn serialize_newtype_variant() {
	#[derive(Serialize)]
	enum Wrapper {
		Num(u32),
	}

	let v = to_value(&Wrapper::Num(7)).expect("should serialise");
	if let Value::Attrs(attrs) = v {
		assert_eq!(attrs.len(), 1);
		let (k, val) = attrs.into_iter().next().unwrap();
		assert_eq!(k, "Num");
		assert!(matches!(val, Value::Integer(7)));
	} else {
		panic!("expected attrs");
	}
}

#[test]
fn serialize_tuple_variant() {
	#[derive(Serialize)]
	enum Foo {
		Bar(String, u32),
	}

	let v = to_value(&Foo::Bar("hello".into(), 42)).expect("should serialise");
	if let Value::Attrs(attrs) = v {
		assert_eq!(attrs.len(), 1);
		let (k, val) = attrs.into_iter().next().unwrap();
		assert_eq!(k, "Bar");
		if let Value::List(list) = val {
			assert_eq!(list.len(), 2);
			assert!(matches!(list[0], Value::String(ref s) if *s == "hello"));
			assert!(matches!(list[1], Value::Integer(42)));
		} else {
			panic!("expected list inside attrs");
		}
	} else {
		panic!("expected attrs");
	}
}

#[test]
fn serialize_struct_variant() {
	#[derive(Serialize)]
	enum Foo {
		Baz { name: String, age: u32 },
	}

	let v = to_value(&Foo::Baz {
		name: "Slartibartfast".into(),
		age: 42,
	})
	.expect("should serialise");

	if let Value::Attrs(outer) = v {
		assert_eq!(outer.len(), 1);
		let (k, inner_val) = outer.into_iter().next().unwrap();
		assert_eq!(k, "Baz");
		if let Value::Attrs(inner) = inner_val {
			let mut name_ok = false;
			let mut age_ok = false;
			for (k, v) in inner.into_iter() {
				if k == "name" {
					assert!(matches!(v, Value::String(ref s) if *s == "Slartibartfast"));
					name_ok = true;
				} else if k == "age" {
					assert!(matches!(v, Value::Integer(42)));
					age_ok = true;
				}
			}
			assert!(name_ok && age_ok);
		} else {
			panic!("expected inner attrs");
		}
	} else {
		panic!("expected attrs");
	}
}

#[test]
fn roundtrip_struct() {
	#[derive(Debug, Serialize, Deserialize, PartialEq)]
	struct Person {
		name: String,
		age: u32,
	}

	let original = Person {
		name: "Slartibartfast".into(),
		age: 42,
	};

	let nix_value = to_value(&original).expect("should serialise");
	let back: Person = from_value(nix_value).expect("should deserialise");
	assert_eq!(original, back);
}

#[test]
fn roundtrip_option() {
	let some: Option<u32> = Some(42);
	let v = to_value(&some).expect("should serialise");
	let back: Option<u32> = from_value(v).expect("should deserialise");
	assert_eq!(some, back);

	let none: Option<u32> = None;
	let v = to_value(&none).expect("should serialise");
	let back: Option<u32> = from_value(v).expect("should deserialise");
	assert_eq!(none, back);
}

#[test]
fn roundtrip_vec() {
	let original = vec![1u32, 2, 3, 42];
	let v = to_value(&original).expect("should serialise");
	let back: Vec<u32> = from_value(v).expect("should deserialise");
	assert_eq!(original, back);
}

#[test]
fn roundtrip_map() {
	let mut original: HashMap<String, u32> = HashMap::new();
	original.insert("age".into(), 42);
	original.insert("count".into(), 7);

	let v = to_value(&original).expect("should serialise");
	let back: HashMap<String, u32> = from_value(v).expect("should deserialise");
	assert_eq!(original, back);
}

#[test]
fn roundtrip_unit_enum() {
	#[derive(Debug, Serialize, Deserialize, PartialEq)]
	enum Color {
		Red,
		Green,
		Blue,
	}

	for color in [Color::Red, Color::Green, Color::Blue] {
		let v = to_value(&color).expect("should serialise");
		let back: Color = from_value(v).expect("should deserialise");
		assert_eq!(color, back);
	}
}

#[test]
fn roundtrip_enum_all() {
	#[derive(Debug, Serialize, Deserialize, PartialEq)]
	#[serde(rename_all = "snake_case")]
	enum TestEnum {
		Unit,
		Tuple(String, String),
		Struct { name: String, age: u32 },
	}

	let cases = vec![
		TestEnum::Tuple("UK".into(), "cask ale".into()),
		TestEnum::Unit,
		TestEnum::Struct {
			name: "Slartibartfast".into(),
			age: 42,
		},
		TestEnum::Tuple("Russia".into(), "квас".into()),
	];

	for case in cases {
		let v = to_value(&case).expect("should serialise");
		let back: TestEnum = from_value(v).expect("should deserialise");
		assert_eq!(case, back);
	}
}

#[test]
fn nix_expression_roundtrip() {
	// A moderately complex configuration-style struct.
	#[derive(Debug, Serialize, Deserialize, PartialEq)]
	struct Server {
		host: String,
		port: u32,
		tls: bool,
		tags: Vec<String>,
		limits: Limits,
	}

	#[derive(Debug, Serialize, Deserialize, PartialEq)]
	struct Limits {
		max_connections: u32,
		timeout_secs: u32,
	}

	let original = Server {
		host: "example.com".into(),
		port: 8080,
		tls: true,
		tags: vec!["web".into(), "prod".into()],
		limits: Limits {
			max_connections: 100,
			timeout_secs: 30,
		},
	};

	// Step 1: serialize to a nix_rs_eval::Value
	let nix_value = to_value(&original).expect("should serialise");

	// Step 2: render the Value as a Nix expression string using Display
	let nix_expr = nix_value.to_string();

	// The expression is valid Nix, e.g.:
	//   { host = "example.com"; limits = { max_connections = 100; timeout_secs = 30; }; port = 8080; tags = [ "web" "prod" ]; tls = true; }
	assert!(nix_expr.contains("example.com"));
	assert!(nix_expr.contains("8080"));
	assert!(nix_expr.contains("true"));
	assert!(nix_expr.contains("web"));

	// Step 3: deserialize back from the Nix expression string
	let back: Server = crate::de::from_str(&nix_expr).expect("should deserialise");

	assert_eq!(original, back);
}
