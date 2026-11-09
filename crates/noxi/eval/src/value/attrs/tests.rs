use bstr::B;

use super::*;

#[test]
fn test_empty_attrs() {
	let attrs = NixAttrs::construct(0, vec![], &[])
		.expect("empty attr construction should succeed")
		.unwrap();

	assert!(
		matches!(attrs.0.as_ref(), AttrsRep::Empty),
		"empty attribute set should use optimised representation"
	);
}

#[test]
fn test_simple_attrs() {
	let attrs = NixAttrs::construct(1, vec![Value::from("key"), Value::from("value")], &[])
		.expect("simple attr construction should succeed")
		.unwrap();

	assert!(
		matches!(attrs.0.as_ref(), AttrsRep::Map { .. }),
		"simple attribute set should use map representation",
	)
}

#[test]
fn test_kv_attrs() {
	let name_val = Value::from("name");
	let value_val = Value::from("value");
	let meaning_val = Value::from("meaning");
	let forty_two_val = Value::Integer(42);

	let kv_attrs = NixAttrs::construct(
		2,
		vec![
			value_val,
			forty_two_val.clone(),
			name_val,
			meaning_val.clone(),
		],
		&[],
	)
	.expect("constructing K/V pair attrs should succeed")
	.unwrap();

	match kv_attrs.0.as_ref() {
		AttrsRep::KV { name, value }
			if name.to_str().unwrap() == meaning_val.to_str().unwrap()
				|| value.to_str().unwrap() == forty_two_val.to_str().unwrap() => {}

		_ => panic!("K/V attribute set should use optimised representation, but got {kv_attrs:?}"),
	}
}

#[test]
fn test_empty_attrs_iter() {
	let attrs = NixAttrs::construct(0, vec![], &[]).unwrap().unwrap();
	assert!(attrs.iter().next().is_none());
}

#[test]
fn test_kv_attrs_iter() {
	let name_val = Value::from("name");
	let value_val = Value::from("value");
	let meaning_val = Value::from("meaning");
	let forty_two_val = Value::Integer(42);

	let kv_attrs = NixAttrs::construct(
		2,
		vec![
			value_val,
			forty_two_val.clone(),
			name_val,
			meaning_val.clone(),
		],
		&[],
	)
	.expect("constructing K/V pair attrs should succeed")
	.unwrap();

	let mut iter = kv_attrs.iter().collect::<Vec<_>>().into_iter();
	let (k, v) = iter.next().unwrap();
	assert!(*k == *NAME);
	assert!(v.to_str().unwrap() == meaning_val.to_str().unwrap());
	let (k, v) = iter.next().unwrap();
	assert!(*k == *VALUE);
	assert!(v.as_int().unwrap() == forty_two_val.as_int().unwrap());
	assert!(iter.next().is_none());
}

#[test]
fn test_map_attrs_iter() {
	let attrs = NixAttrs::construct(1, vec![Value::from("key"), Value::from("value")], &[])
		.expect("simple attr construction should succeed")
		.unwrap();

	let mut iter = attrs.iter().collect::<Vec<_>>().into_iter();
	let (k, v) = iter.next().unwrap();
	assert!(k == &NixString::from("key"));
	assert_eq!(v.to_str().unwrap(), B("value"));
	assert!(iter.next().is_none());
}

#[test]
fn dropping_deep_attribute_chains_uses_bounded_stack() {
	std::thread::Builder::new()
		.stack_size(64 * 1024)
		.spawn(|| {
			let mut attrs = NixAttrs::empty();
			for _ in 0..10_000 {
				attrs = NixAttrs::from_iter([("tail", Value::Attrs(attrs))]);
			}
			drop(attrs);
		})
		.unwrap()
		.join()
		.unwrap();
}

#[test]
fn cached_literals_validate_names_and_positions_before_reusing_layout() {
	let mut source = codemap::CodeMap::new();
	let span = source.add_file("attrs.nix".into(), "z a".into()).span;
	let spans = [span.subspan(0, 1), span.subspan(2, 3)];
	let cache = OnceCell::new();
	let make = |keys: [&str; 2], values: [i64; 2], spans: &[Span]| {
		NixAttrs::construct_with_positions(
			2,
			vec![
				keys[0].into(),
				values[0].into(),
				keys[1].into(),
				values[1].into(),
			],
			spans,
			Some(&cache),
		)
	};
	make(["z", "a"], [1, 2], &spans).unwrap().unwrap();
	let second = make(["z", "a"], [3, 4], &spans).unwrap().unwrap();
	assert_eq!(second.select("z").unwrap().as_int().unwrap(), 3);
	assert_eq!(second.select("a").unwrap().as_int().unwrap(), 4);
	assert_eq!(second.get_attr_pos("z"), Some(spans[0]));
	assert!(matches!(
		make(["z", "z"], [5, 6], &spans),
		Err(ErrorKind::DuplicateDefinition { .. })
	));
	let swapped = [spans[1], spans[0]];
	let third = make(["z", "a"], [7, 8], &swapped).unwrap().unwrap();
	assert_eq!(third.get_attr_pos("z"), Some(swapped[0]));
	let omitted = NixAttrs::construct_with_positions(
		2,
		vec!["z".into(), 9.into(), Value::Null, 10.into()],
		&spans,
		Some(&cache),
	)
	.unwrap()
	.unwrap();
	assert_eq!(omitted.len(), 1);
	assert_eq!(omitted.select("z").unwrap().as_int().unwrap(), 9);
	let key = NixString::new_context_from(
		crate::value::NixContextElement::Plain("/nix/store/source".into()).into(),
		"z",
	);
	let contextual = NixAttrs::construct_with_positions(
		2,
		vec![key.into(), 11.into(), "a".into(), 12.into()],
		&spans,
		Some(&cache),
	)
	.unwrap()
	.unwrap();
	assert!(
		contextual
			.keys()
			.find(|key| key.as_bytes() == b"z")
			.unwrap()
			.has_context()
	);
}

#[test]
fn wide_binding_lookups_survive_shared_owned_iteration() {
	let attrs = NixAttrs::from_iter((0..128).map(|index| (format!("{index:03}"), index as i64)));
	assert_eq!(attrs.select("063").unwrap().as_int().unwrap(), 63);
	assert_eq!(
		attrs
			.clone()
			.into_iter()
			.map(|(_, value)| value.as_int().unwrap())
			.sum::<i64>(),
		8128
	);
	assert_eq!(attrs.select("127").unwrap().as_int().unwrap(), 127);
	assert!(attrs.select("absent").is_none());
}

#[test]
fn cached_indexes_follow_names_when_literal_input_order_changes() {
	let mut source = codemap::CodeMap::new();
	let span = source.add_file("attrs.nix".into(), "b d a c".into()).span;
	let spans = [
		span.subspan(0, 1),
		span.subspan(2, 3),
		span.subspan(4, 5),
		span.subspan(6, 7),
	];
	let cache = OnceCell::new();
	let make = |keys: [&str; 4], start: i64, spans: &[Span]| {
		let values = keys
			.into_iter()
			.enumerate()
			.flat_map(|(index, key)| [key.into(), (start + index as i64).into()])
			.collect();
		NixAttrs::construct_with_positions(4, values, spans, Some(&cache))
			.unwrap()
			.unwrap()
	};
	let first = make(["b", "d", "a", "c"], 1, &spans);
	assert_eq!(first.select("a").unwrap().as_int().unwrap(), 3);
	let second = make(["b", "d", "a", "c"], 5, &spans);
	assert_eq!(second.select("a").unwrap().as_int().unwrap(), 7);
	let reordered = [spans[2], spans[3], spans[0], spans[1]];
	let third = make(["a", "c", "b", "d"], 9, &reordered);
	assert_eq!(third.select("a").unwrap().as_int().unwrap(), 9);
	assert_eq!(third.select("b").unwrap().as_int().unwrap(), 11);
	let changed = make(["b", "d", "a", "x"], 13, &spans);
	assert_eq!(changed.select("x").unwrap().as_int().unwrap(), 16);
	assert!(changed.select("c").is_none());
}

#[test]
fn uneven_updates_match_right_biased_map_collection() {
	let wide = NixAttrs::from_iter(
		(0..256)
			.map(|index| (format!("{index:03}"), index as i64))
			.chain([("name".into(), 500), ("value".into(), 600)]),
	);
	let small = NixAttrs::from_iter([("000", -1), ("127", -2), ("999", -3)]);
	let kv = NixAttrs::from_kv(700.into(), 800.into());
	for (left, right) in [
		(wide.clone(), small.clone()),
		(small, wide.clone()),
		(wide.clone(), kv.clone()),
		(kv, wide),
	] {
		let expected: BTreeMap<_, _> = left
			.iter()
			.chain(right.iter())
			.map(|(key, value)| (key.to_str_lossy().into_owned(), value.as_int().unwrap()))
			.collect();
		let actual: BTreeMap<_, _> = left
			.update(right)
			.into_iter()
			.map(|(key, value)| (key.to_str_lossy().into_owned(), value.as_int().unwrap()))
			.collect();
		assert_eq!(actual, expected);
	}
}

#[test]
fn positioned_duplicate_bindings_keep_last_value_and_last_present_span() {
	let mut source = codemap::CodeMap::new();
	let span = source.add_file("attrs.nix".into(), "a b".into()).span;
	let attrs = NixAttrs::from_iter_with_pos([
		("a".into(), 1.into(), Some(span.subspan(0, 1))),
		("a".into(), 2.into(), Some(span.subspan(2, 3))),
		("a".into(), 3.into(), None),
	]);
	assert_eq!(attrs.select("a").unwrap().as_int().unwrap(), 3);
	assert_eq!(attrs.get_attr_pos("a"), Some(span.subspan(2, 3)));
}

#[test]
fn owned_bindings_keep_the_last_duplicate() {
	let attrs = NixAttrs::from_iter([("z", 1), ("a", 2), ("z", 3), ("z", 4)]);
	assert_eq!(attrs.len(), 2);
	assert_eq!(attrs.select("z").unwrap().as_int().unwrap(), 4);
	assert_eq!(
		attrs
			.keys_sorted()
			.map(|key| key.to_str_lossy().into_owned())
			.collect::<Vec<_>>(),
		["a", "z"]
	);
}

#[test]
fn updates_preserve_right_values_with_shared_inputs() {
	let left = NixAttrs::from_iter([("a", 1), ("c", 3), ("e", 5)]);
	let right = NixAttrs::from_iter([("b", 2), ("c", 4), ("f", 6)]);
	let merged = left.clone().update(right.clone());
	assert_eq!(left.select("c").unwrap().as_int().unwrap(), 3);
	assert_eq!(merged.select("c").unwrap().as_int().unwrap(), 4);
	assert_eq!(merged.len(), 5);
	let intersection = left.intersect(&right);
	assert_eq!(intersection.len(), 1);
	assert_eq!(intersection.select("c").unwrap().as_int().unwrap(), 4);
}

#[test]
fn attribute_iterators_track_remaining_length() {
	for attrs in [
		NixAttrs::from_kv(1.into(), 2.into()),
		NixAttrs::from_iter([("a", 1), ("b", 2)]),
	] {
		let mut keys = attrs.keys();
		let mut values = attrs.iter();
		for expected in (0..=2).rev() {
			assert_eq!(keys.len(), expected);
			assert_eq!(values.len(), expected);
			assert_eq!(keys.size_hint(), (expected, Some(expected)));
			assert_eq!(values.size_hint(), (expected, Some(expected)));
			keys.next();
			values.next();
		}
	}
}
