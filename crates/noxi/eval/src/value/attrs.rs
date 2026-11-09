//! This module implements Nix attribute sets. They have flexible
//! backing implementations, as they are used in very versatile
//! use-cases that are all exposed the same way in the language
//! surface.
//!
//! Due to this, construction and management of attribute sets has
//! some peculiarities that are encapsulated within this module.
use std::borrow::Borrow;
use std::cell::OnceCell;
use std::collections::BTreeMap;
use std::hash::BuildHasher as _;
use std::iter::FromIterator;
use std::rc::Rc;
use std::sync::LazyLock;

use bstr::{BStr, ByteSlice};
use codemap::Span;
use rustc_hash::{FxHashMap, FxHashSet};
use serde::Deserialize;
use serde::de::{Deserializer, Error, Visitor};

use super::TotalDisplay;
use super::Value;
use super::string::NixString;
use super::thunk::ThunkSet;
use crate::CatchableErrorKind;
use crate::errors::ErrorKind;

/// Positions and validated key layouts shared by sets from one literal.
pub(crate) type AttrPositions = Rc<LiteralAttrs>;

#[derive(Debug)]
pub(crate) struct LiteralAttrs {
	positions: FxHashMap<NixString, Span>,
	layout: Option<Box<LiteralLayout>>,
}

#[derive(Debug)]
struct LiteralLayout {
	bindings: Box<[(NixString, usize)]>,
	spans: Box<[Span]>,
	index: OnceCell<Rc<hashbrown::HashTable<u32>>>,
}

fn binding_index<T>(bindings: &[(NixString, T)]) -> Rc<hashbrown::HashTable<u32>> {
	let mut index = hashbrown::HashTable::with_capacity(bindings.len());
	for (offset, (name, _)) in bindings.iter().enumerate() {
		index.insert_unique(
			rustc_hash::FxBuildHasher.hash_one(name.as_bstr()),
			offset as u32,
			|offset| rustc_hash::FxBuildHasher.hash_one(bindings[*offset as usize].0.as_bstr()),
		);
	}
	Rc::new(index)
}

fn sort_bindings(attrs: &mut Vec<(NixString, Value)>) {
	// Stable ordering retains the first key and the last value for duplicates,
	// matching collection into a hash map, including string contexts on keys.
	attrs.sort_by(|(a, _), (b, _)| a.cmp(b));
	attrs.dedup_by(|(a, value), (b, previous)| {
		if a == b {
			std::mem::swap(value, previous);
			true
		} else {
			false
		}
	});
}

fn merge_bindings(
	out: &mut Vec<(NixString, Value)>,
	mut left: &[(NixString, Value)],
	mut right: &[(NixString, Value)],
) {
	if left.len() > right.len().saturating_mul(8) {
		// A few overrides must not compare against every key of a large set.
		for entry in right {
			let split = left.partition_point(|(key, _)| key < &entry.0);
			out.extend_from_slice(&left[..split]);
			left = &left[split..];
			if left.first().is_some_and(|(key, _)| key == &entry.0) {
				left = &left[1..];
			}
			out.push(entry.clone());
		}
		out.extend_from_slice(left);
		return;
	}
	if right.len() > left.len().saturating_mul(8) {
		for entry in left {
			let split = right.partition_point(|(key, _)| key < &entry.0);
			out.extend_from_slice(&right[..split]);
			right = &right[split..];
			if right.first().is_some_and(|(key, _)| key == &entry.0) {
				out.push(right[0].clone());
				right = &right[1..];
			} else {
				out.push(entry.clone());
			}
		}
		out.extend_from_slice(right);
		return;
	}
	while let (Some(a), Some(b)) = (left.first(), right.first()) {
		match a.0.cmp(&b.0) {
			std::cmp::Ordering::Less => {
				out.push(a.clone());
				left = &left[1..];
			}
			std::cmp::Ordering::Greater => {
				out.push(b.clone());
				right = &right[1..];
			}
			std::cmp::Ordering::Equal => {
				out.push(b.clone());
				left = &left[1..];
				right = &right[1..];
			}
		}
	}
	out.extend_from_slice(left);
	out.extend_from_slice(right);
}

static NAME: LazyLock<NixString> = LazyLock::new(|| "name".into());
static VALUE: LazyLock<NixString> = LazyLock::new(|| "value".into());

#[cfg(test)]
mod tests;

#[derive(Clone, Debug, Deserialize, Default)]
pub(super) enum AttrsRep {
	#[default]
	Empty,

	Map {
		/// Unique bindings in lexical order; lookup tables store only 32-bit offsets.
		attrs: Box<[(NixString, Value)]>,

		#[serde(skip)]
		index: OnceCell<Rc<hashbrown::HashTable<u32>>>,

		#[serde(skip)]
		pos: Option<AttrPositions>,
	},

	/// Warning: this represents a **two**-attribute attrset, with
	/// attribute names "name" and "value", like `{name="foo";
	/// value="bar";}`, *not* `{foo="bar";}`!
	KV { name: Value, value: Value },
}

impl AttrsRep {
	fn select(&self, key: &BStr) -> Option<&Value> {
		match self {
			AttrsRep::Empty => None,

			AttrsRep::KV { name, value } => match &**key {
				b"name" => Some(name),
				b"value" => Some(value),
				_ => None,
			},

			AttrsRep::Map { attrs, index, pos } => {
				if attrs.len() < 4 {
					return attrs
						.iter()
						.find(|(name, _)| name.as_bstr() == key)
						.map(|(_, value)| value);
				}
				if u32::try_from(attrs.len()).is_err() {
					return attrs
						.binary_search_by(|(name, _)| name.as_bstr().cmp(key))
						.ok()
						.map(|index| &attrs[index].1);
				}
				let index = index.get_or_init(|| {
					// Cached positions cover every name in a literal layout. Equal
					// lengths therefore identify the same sorted binding offsets.
					if let Some(layout) = pos.as_ref().and_then(|pos| pos.layout.as_ref())
						&& layout.bindings.len() == attrs.len()
					{
						return layout
							.index
							.get_or_init(|| binding_index(&layout.bindings))
							.clone();
					}
					binding_index(attrs)
				});
				index
					.find(rustc_hash::FxBuildHasher.hash_one(key), |offset| {
						attrs[*offset as usize].0.as_bstr() == key
					})
					.map(|offset| &attrs[*offset as usize].1)
			}
		}
	}

	fn contains(&self, key: &BStr) -> bool {
		match self {
			AttrsRep::Empty => false,
			AttrsRep::KV { .. } => key == "name" || key == "value",
			AttrsRep::Map { .. } => self.select(key).is_some(),
		}
	}
}

#[repr(transparent)]
#[derive(Clone, Debug, Default)]
pub struct NixAttrs(pub(super) Rc<AttrsRep>);

thread_local! {
	static ATTR_DROP_QUEUE: std::cell::RefCell<(bool, Vec<Rc<AttrsRep>>)> = const { std::cell::RefCell::new((false, Vec::new())) };
}

impl Drop for NixAttrs {
	fn drop(&mut self) {
		if Rc::strong_count(&self.0) != 1 || matches!(*self.0, AttrsRep::Empty) {
			return;
		}
		// Releasing a forced linked list must not recurse through every tail.
		// Nested attribute drops enqueue their backing store for this loop.
		let rep = std::mem::take(&mut self.0);
		let mut rep = Some(rep);
		let _ = ATTR_DROP_QUEUE.try_with(|queue| {
			{
				let mut queue = queue.borrow_mut();
				queue.1.push(rep.take().expect("attribute backing store"));
				if queue.0 {
					return;
				}
				queue.0 = true;
			}
			loop {
				let item = queue.borrow_mut().1.pop();
				match item {
					Some(item) => drop(item),
					None => break,
				}
			}
			queue.borrow_mut().0 = false;
		});
	}
}

impl From<AttrsRep> for NixAttrs {
	fn from(rep: AttrsRep) -> Self {
		NixAttrs(Rc::new(rep))
	}
}

impl<K, V> FromIterator<(K, V)> for NixAttrs
where
	NixString: From<K>,
	Value: From<V>,
{
	fn from_iter<T>(iter: T) -> NixAttrs
	where
		T: IntoIterator<Item = (K, V)>,
	{
		let mut attrs: Vec<(NixString, Value)> = iter
			.into_iter()
			.map(|(k, v)| (k.into(), v.into()))
			.collect();
		sort_bindings(&mut attrs);
		NixAttrs::from_sorted(attrs, None)
	}
}

impl From<BTreeMap<NixString, Value>> for NixAttrs {
	fn from(map: BTreeMap<NixString, Value>) -> Self {
		Self::from_sorted(map.into_iter().collect(), None)
	}
}

impl From<FxHashMap<NixString, Value>> for NixAttrs {
	fn from(map: FxHashMap<NixString, Value>) -> Self {
		let mut attrs: Vec<_> = map.into_iter().collect();
		attrs.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
		Self::from_sorted(attrs, None)
	}
}

impl TotalDisplay for NixAttrs {
	fn total_fmt(&self, f: &mut std::fmt::Formatter<'_>, set: &mut ThunkSet) -> std::fmt::Result {
		if self.is_derivation() {
			write!(f, "«derivation")?;
			// We cant use the default TotalDisplay implementation, because nix doesn't put quotes
			// around the path here.
			if let Some(Value::Thunk(p)) = self.select("drvPath")
				&& p.is_forced()
				&& let Ok(drv) = p.value().to_contextful_str()
			{
				write!(f, " {}", drv.to_str_lossy())?;
			};
			return write!(f, "»");
		}

		f.write_str("{ ")?;

		for (name, value) in self.iter_sorted() {
			write!(f, "{} = ", name.ident_str())?;
			value.total_fmt(f, set)?;
			f.write_str("; ")?;
		}

		f.write_str("}")
	}
}

impl<'de> Deserialize<'de> for NixAttrs {
	fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
	where
		D: Deserializer<'de>,
	{
		struct MapVisitor;

		impl<'de> Visitor<'de> for MapVisitor {
			type Value = NixAttrs;

			fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
				formatter.write_str("a valid Nix attribute set")
			}

			fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
			where
				A: serde::de::MapAccess<'de>,
			{
				let mut stack_array = Vec::with_capacity(map.size_hint().unwrap_or(0) * 2);

				while let Some((key, value)) = map.next_entry()? {
					stack_array.push(key);
					stack_array.push(value);
				}

				Ok(NixAttrs::construct(stack_array.len() / 2, stack_array, &[])
					.map_err(A::Error::custom)?
					.expect("Catchable values are unreachable here"))
			}
		}

		deserializer.deserialize_map(MapVisitor)
	}
}

impl NixAttrs {
	fn into_rep(mut self) -> Rc<AttrsRep> {
		std::mem::take(&mut self.0)
	}

	pub fn empty() -> Self {
		AttrsRep::Empty.into()
	}

	/// Compare two attribute sets by pointer equality. Only makes
	/// sense for some attribute set representations, i.e. returning
	/// `false` does not mean that the two attribute sets do not have
	/// equal *content*.
	pub fn ptr_eq(&self, other: &Self) -> bool {
		Rc::ptr_eq(&self.0, &other.0)
	}

	/// The identity of the shared set, for detecting repeated values.
	pub(crate) fn as_ptr(&self) -> *const () {
		Rc::as_ptr(&self.0).cast()
	}

	/// Return an attribute set containing the merge of the two
	/// provided sets. Keys from the `other` set have precedence.
	pub fn update(self, other: Self) -> Self {
		if matches!(self.0.as_ref(), AttrsRep::Empty) {
			return other;
		}
		if matches!(other.0.as_ref(), AttrsRep::Empty) {
			return self;
		}
		if matches!(
			(self.0.as_ref(), other.0.as_ref()),
			(AttrsRep::KV { .. }, AttrsRep::KV { .. })
		) {
			return other;
		}
		let mut attrs = Vec::with_capacity(self.len() + other.len());
		match (self.0.as_ref(), other.0.as_ref()) {
			(AttrsRep::Map { attrs: left, .. }, AttrsRep::Map { attrs: right, .. }) => {
				merge_bindings(&mut attrs, left, right);
			}
			(AttrsRep::Map { attrs: left, .. }, AttrsRep::KV { name, value }) => {
				let right = [(NAME.clone(), name.clone()), (VALUE.clone(), value.clone())];
				merge_bindings(&mut attrs, left, &right);
			}
			(AttrsRep::KV { name, value }, AttrsRep::Map { attrs: right, .. }) => {
				let left = [(NAME.clone(), name.clone()), (VALUE.clone(), value.clone())];
				merge_bindings(&mut attrs, &left, right);
			}
			_ => unreachable!("empty and KV updates handled above"),
		}
		let positions = attrs
			.iter()
			.filter_map(|(key, _)| {
				let span = if other.select(key).is_some() {
					other.get_attr_pos(key)
				} else {
					self.get_attr_pos(key)
				};
				span.map(|span| (key.clone(), span))
			})
			.collect::<Vec<_>>();
		Self::from_sorted(
			attrs,
			(!positions.is_empty()).then(|| {
				Rc::new(LiteralAttrs {
					positions: positions.into_iter().collect(),
					layout: None,
				})
			}),
		)
	}

	/// Return the number of key-value entries in an attrset.
	pub fn len(&self) -> usize {
		match self.0.as_ref() {
			AttrsRep::Map { attrs, .. } => attrs.len(),
			AttrsRep::Empty => 0,
			AttrsRep::KV { .. } => 2,
		}
	}

	pub fn is_empty(&self) -> bool {
		match self.0.as_ref() {
			AttrsRep::Map { attrs, .. } => attrs.is_empty(),
			AttrsRep::Empty => true,
			AttrsRep::KV { .. } => false,
		}
	}

	/// Select a value from an attribute set by key.
	pub fn select<K>(&self, key: &K) -> Option<&Value>
	where
		K: Borrow<BStr> + ?Sized,
	{
		self.0.select(key.borrow())
	}

	/// Select a required value from an attribute set by key, return
	/// an `AttributeNotFound` error if it is missing.
	pub fn select_required<K>(&self, key: &K) -> Result<&Value, ErrorKind>
	where
		K: Borrow<BStr> + ?Sized,
	{
		self.select(key)
			.ok_or_else(|| ErrorKind::AttributeNotFound {
				name: key.borrow().to_string(),
				suggestions: vec![],
			})
	}

	pub fn contains<'a, K: 'a>(&self, key: K) -> bool
	where
		&'a BStr: From<K>,
	{
		self.0.contains(key.into())
	}

	/// Construct an iterator over all the key-value pairs in the attribute set.
	#[allow(clippy::needless_lifetimes)]
	pub fn iter<'a>(&'a self) -> Iter<KeyValue<'a>> {
		Iter(match &self.0.as_ref() {
			AttrsRep::Map { attrs, .. } => KeyValue::Map(attrs.iter()),
			AttrsRep::Empty => KeyValue::Empty,

			AttrsRep::KV { name, value } => KeyValue::KV {
				name,
				value,
				at: IterKV::default(),
			},
		})
	}

	/// Same as iter(), but marks call sites which rely on the
	/// iteration being lexicographic.
	pub fn iter_sorted(&self) -> Iter<KeyValue<'_>> {
		self.iter()
	}

	/// Same as [IntoIterator::into_iter], marking lexicographic iteration.
	pub fn into_iter_sorted(self) -> OwnedAttrsIterator {
		self.into_iter()
	}

	/// Construct an iterator over all keys in the attribute set.
	pub fn keys(&self) -> Keys<'_> {
		Keys(match self.0.as_ref() {
			AttrsRep::Empty => KeysInner::Empty,
			AttrsRep::KV { .. } => KeysInner::KV(IterKV::default()),
			AttrsRep::Map { attrs, .. } => KeysInner::Map(attrs.iter()),
		})
	}

	/// Same as [Self::keys], marking lexicographic iteration.
	pub fn keys_sorted(&self) -> Keys<'_> {
		self.keys()
	}

	/// Implement construction logic of an attribute set, to encapsulate
	/// logic about attribute set optimisations inside of this module.
	pub fn construct(
		count: usize,
		stack_slice: Vec<Value>,
		spans: &[Span],
	) -> Result<Result<Self, CatchableErrorKind>, ErrorKind> {
		Self::construct_with_positions(count, stack_slice, spans, None)
	}

	/// [`construct`](Self::construct) a set from a literal, sharing the
	/// positions in `cache` with earlier sets from it that had the same.
	pub(crate) fn construct_with_positions(
		count: usize,
		mut stack_slice: Vec<Value>,
		spans: &[Span],
		cache: Option<&OnceCell<AttrPositions>>,
	) -> Result<Result<Self, CatchableErrorKind>, ErrorKind> {
		debug_assert!(
			stack_slice.len() == count * 2,
			"construct_attrs called with count == {}, but slice.len() == {}",
			count,
			stack_slice.len(),
		);
		// FUTUREWORK: this invariant will be true once we fully
		// implement span collection from everywhere e.g. inherit (from),
		// formals, etc.
		//
		// debug_assert!(
		//     spans.len() == count || spans.is_empty(),
		//     "construct_attrs called with count == {}, but spans.len() == {}",
		//     count,
		//     spans.len(),
		// );

		// Optimisation: Empty attribute set
		if count == 0 {
			return Ok(Ok(AttrsRep::Empty.into()));
		}

		// Optimisation: KV pattern
		if count == 2
			&& spans.is_empty()
			&& let Some(kv) = attempt_optimise_kv(&mut stack_slice)
		{
			return Ok(Ok(kv));
		}

		if let Some(cached) = cache.and_then(OnceCell::get)
			&& let Some(layout) = &cached.layout
			&& layout.bindings.len() == count
			&& layout.spans.as_ref() == spans
			&& layout.bindings.iter().all(
				|(key, offset)| matches!(&stack_slice[offset * 2], Value::String(name) if name == key),
			) {
			// This exact key layout already passed duplicate/name validation.
			// Its cached permutation constructs the sorted bindings directly.
			let attrs = layout
				.bindings
				.iter()
				.map(|(_, offset)| {
					let Value::String(key) =
						std::mem::replace(&mut stack_slice[offset * 2], Value::Null)
					else {
						unreachable!("validated literal key");
					};
					let value = std::mem::replace(&mut stack_slice[offset * 2 + 1], Value::Null);
					(key, value)
				})
				.collect();
			return Ok(Ok(Self::from_sorted(attrs, Some(cached.clone()))));
		}

		let cache_layout = spans.len() == count && cache.is_some_and(|cache| cache.get().is_none());
		let mut layout_keys = cache_layout.then(|| Vec::with_capacity(count));
		let layout_spans = cache_layout.then(|| spans.to_vec().into_boxed_slice());
		let mut attrs_map = Vec::with_capacity(count);
		let mut seen = (count > 8)
			.then(|| FxHashSet::with_capacity_and_hasher(count, rustc_hash::FxBuildHasher));
		let mut attrs_pos = Vec::with_capacity(spans.len());
		let mut spans = spans.iter().rev().copied();

		for offset in (0..count).rev() {
			let value = stack_slice.pop().unwrap();
			let key = stack_slice.pop().unwrap();
			let span = spans.next();

			match key {
				Value::String(ks) => {
					if let Some(keys) = &mut layout_keys {
						keys.push((ks.clone(), offset));
					}
					set_attr(&mut attrs_map, &mut seen, ks, value, &mut attrs_pos, span)?
				}

				Value::Null => {
					// This is in fact valid, but leads to the value
					// being ignored and nothing being set, i.e. `{
					// ${null} = 1; } => { }`.
					continue;
				}

				Value::Catchable(err) => return Ok(Err(*err)),

				other => return Err(ErrorKind::InvalidAttributeName(other)),
			}
		}

		let pos = if attrs_pos.is_empty() {
			None
		} else if let Some(cached) = cache.and_then(OnceCell::get)
			&& cached.positions.len() == attrs_pos.len()
			&& attrs_pos
				.iter()
				.all(|(key, span)| cached.positions.get(key) == Some(span))
		{
			Some(cached.clone())
		} else {
			let layout = layout_keys
				.filter(|keys| keys.len() == count)
				.map(|mut keys| {
					keys.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
					Box::new(LiteralLayout {
						bindings: keys.into_boxed_slice(),
						spans: layout_spans.expect("cached literal spans"),
						index: OnceCell::new(),
					})
				});
			let pos: AttrPositions = Rc::new(LiteralAttrs {
				positions: attrs_pos.into_iter().collect(),
				layout,
			});
			if let Some(cache) = cache {
				let _ = cache.set(pos.clone());
			}
			Some(pos)
		};

		attrs_map.sort_unstable_by(|(a, _), (b, _)| a.cmp(b));
		Ok(Ok(Self::from_sorted(attrs_map, pos)))
	}

	/// Construct an optimized "KV"-style attribute set given the value for the
	/// `"name"` key, and the value for the `"value"` key
	pub(crate) fn from_kv(name: Value, value: Value) -> Self {
		AttrsRep::KV { name, value }.into()
	}

	/// Calculate the intersection of the attribute sets.
	/// The right side value is used when the keys match.
	pub(crate) fn intersect(&self, other: &Self) -> NixAttrs {
		if matches!(self.0.as_ref(), AttrsRep::Empty) || matches!(other.0.as_ref(), AttrsRep::Empty)
		{
			return Self::empty();
		}
		let mut attrs = Vec::with_capacity(self.len().min(other.len()));
		if self.len() < other.len() {
			for key in self.keys() {
				if let Some(value) = other.select(key) {
					attrs.push((key.clone(), value.clone()));
				}
			}
		} else {
			for (key, value) in other {
				if self.contains(key.as_bstr()) {
					attrs.push((key.clone(), value.clone()));
				}
			}
		}
		Self::from_sorted(attrs, None)
	}

	/// Returns whether this attr set is a derivation.
	pub fn is_derivation(&self) -> bool {
		let Some(Value::String(kind)) = self.select("type") else {
			return false;
		};
		*kind == "derivation"
	}

	/// An attribute set whose attributes have the given positions.
	pub(crate) fn from_iter_with_pos(
		iter: impl IntoIterator<Item = (NixString, Value, Option<Span>)>,
	) -> NixAttrs {
		let iter = iter.into_iter();
		let mut attrs = Vec::with_capacity(iter.size_hint().0);
		let mut pos = FxHashMap::default();
		for (key, value, span) in iter {
			if let Some(span) = span {
				pos.insert(key.clone(), span);
			}
			attrs.push((key, value));
		}
		if attrs.is_empty() {
			return AttrsRep::Empty.into();
		}
		sort_bindings(&mut attrs);
		Self::from_sorted(
			attrs,
			(!pos.is_empty()).then(|| {
				Rc::new(LiteralAttrs {
					positions: pos,
					layout: None,
				})
			}),
		)
	}

	/// Build a compact set from unique bindings already in lexical order.
	fn from_sorted(attrs: Vec<(NixString, Value)>, pos: Option<AttrPositions>) -> Self {
		let index = pos
			.as_ref()
			.and_then(|pos| pos.layout.as_ref())
			.filter(|layout| layout.bindings.len() == attrs.len())
			.and_then(|layout| layout.index.get())
			.cloned()
			.map(OnceCell::from)
			.unwrap_or_default();
		AttrsRep::Map {
			attrs: attrs.into_boxed_slice(),
			index,
			pos,
		}
		.into()
	}

	pub fn get_attr_pos<K>(&self, key: &K) -> Option<Span>
	where
		K: Borrow<BStr> + ?Sized,
	{
		match self.0.as_ref() {
			AttrsRep::Map { pos, .. } => pos.as_ref()?.positions.get(key.borrow()).copied(),
			_ => None,
		}
	}
}

impl IntoIterator for NixAttrs {
	type Item = (NixString, Value);
	type IntoIter = OwnedAttrsIterator;

	fn into_iter(self) -> Self::IntoIter {
		match Rc::unwrap_or_clone(self.into_rep()) {
			AttrsRep::Empty => OwnedAttrsIterator(IntoIterRepr::Empty),
			AttrsRep::KV { name, value } => OwnedAttrsIterator(IntoIterRepr::Finite(
				vec![(NAME.clone(), name), (VALUE.clone(), value)].into_iter(),
			)),
			AttrsRep::Map { attrs, .. } => {
				OwnedAttrsIterator(IntoIterRepr::Finite(attrs.into_vec().into_iter()))
			}
		}
	}
}

/// In Nix, name/value attribute pairs are frequently constructed from
/// literals. This particular case should avoid allocation of a map,
/// additional heap values etc. and use the optimised `KV` variant
/// instead.
///
/// ```norust
/// `slice` is the top of the stack from which the attrset is being
/// constructed, e.g.
///
///   slice: [ "value" 5 "name" "foo" ]
///   index:   0       1 2      3
///   stack:   3       2 1      0
/// ```
fn attempt_optimise_kv(slice: &mut [Value]) -> Option<NixAttrs> {
	let (name_idx, value_idx) = {
		match (&slice[2], &slice[0]) {
			(Value::String(s1), Value::String(s2)) if (*s1 == *NAME && *s2 == *VALUE) => (3, 1),
			(Value::String(s1), Value::String(s2)) if (*s1 == *VALUE && *s2 == *NAME) => (1, 3),

			// Technically this branch lets type errors pass,
			// but they will be caught during normal attribute
			// set construction instead.
			_ => return None,
		}
	};

	Some(NixAttrs::from_kv(
		slice[name_idx].clone(),
		slice[value_idx].clone(),
	))
}

/// Set an attribute on an in-construction attribute set, while
/// checking against duplicate keys.
fn set_attr(
	attrs: &mut Vec<(NixString, Value)>,
	seen: &mut Option<FxHashSet<NixString>>,
	key: NixString,
	value: Value,
	pos: &mut Vec<(NixString, Span)>,
	span: Option<Span>,
) -> Result<(), ErrorKind> {
	let duplicate = match seen {
		Some(seen) => !seen.insert(key.clone()),
		None => attrs.iter().any(|(name, _)| *name == key),
	};
	if duplicate {
		if let (Some(original), Some(duplicate)) = (
			pos.iter()
				.find(|(name, _)| *name == key)
				.map(|(_, span)| *span),
			span,
		) {
			return Err(ErrorKind::DuplicateDefinition {
				key: key.to_str_lossy().into_owned(),
				original: if original.low() < duplicate.low() {
					original
				} else {
					duplicate
				},
				duplicate: if original.low() < duplicate.low() {
					duplicate
				} else {
					original
				},
				dynamic: true,
			});
		}
		return Err(ErrorKind::DuplicateAttrsKey {
			key: key.to_string(),
		});
	}
	if let Some(span) = span {
		pos.push((key.clone(), span));
	}
	attrs.push((key, value));
	Ok(())
}

/// Internal helper type to track the iteration status of an iterator
/// over the name/value representation.
#[derive(Debug, Default)]
pub enum IterKV {
	#[default]
	Name,
	Value,
	Done,
}

impl IterKV {
	fn next(&mut self) {
		match *self {
			Self::Name => *self = Self::Value,
			Self::Value => *self = Self::Done,
			Self::Done => {}
		}
	}
}

/// Iterator representation over the keys *and* values of an attribute
/// set.
pub enum KeyValue<'a> {
	Empty,

	KV {
		name: &'a Value,
		value: &'a Value,
		at: IterKV,
	},

	Map(std::slice::Iter<'a, (NixString, Value)>),
}

/// Iterator over a Nix attribute set.
// This wrapper type exists to make the inner "raw" iterator
// inaccessible.
#[repr(transparent)]
pub struct Iter<T>(T);

impl<'a> Iterator for Iter<KeyValue<'a>> {
	type Item = (&'a NixString, &'a Value);

	fn size_hint(&self) -> (usize, Option<usize>) {
		let len = self.len();
		(len, Some(len))
	}

	fn next(&mut self) -> Option<Self::Item> {
		match &mut self.0 {
			KeyValue::Map(inner) => inner.next().map(|(key, value)| (key, value)),
			KeyValue::Empty => None,
			KeyValue::KV { name, value, at } => match at {
				IterKV::Name => {
					at.next();
					Some((&NAME, name))
				}

				IterKV::Value => {
					at.next();
					Some((&VALUE, value))
				}

				IterKV::Done => None,
			},
		}
	}
}

impl ExactSizeIterator for Iter<KeyValue<'_>> {
	fn len(&self) -> usize {
		match &self.0 {
			KeyValue::Empty => 0,
			KeyValue::KV { at, .. } => match at {
				IterKV::Name => 2,
				IterKV::Value => 1,
				IterKV::Done => 0,
			},
			KeyValue::Map(inner) => inner.len(),
		}
	}
}

enum KeysInner<'a> {
	Empty,
	KV(IterKV),
	Map(std::slice::Iter<'a, (NixString, Value)>),
}

pub struct Keys<'a>(KeysInner<'a>);

impl<'a> Iterator for Keys<'a> {
	type Item = &'a NixString;

	fn size_hint(&self) -> (usize, Option<usize>) {
		let len = self.len();
		(len, Some(len))
	}

	fn next(&mut self) -> Option<Self::Item> {
		match &mut self.0 {
			KeysInner::Empty => None,
			KeysInner::KV(at @ IterKV::Name) => {
				at.next();
				Some(&NAME)
			}
			KeysInner::KV(at @ IterKV::Value) => {
				at.next();
				Some(&VALUE)
			}
			KeysInner::KV(IterKV::Done) => None,
			KeysInner::Map(m) => m.next().map(|(key, _)| key),
		}
	}
}

impl<'a> IntoIterator for &'a NixAttrs {
	type Item = (&'a NixString, &'a Value);

	type IntoIter = Iter<KeyValue<'a>>;

	fn into_iter(self) -> Self::IntoIter {
		self.iter()
	}
}

impl ExactSizeIterator for Keys<'_> {
	fn len(&self) -> usize {
		match &self.0 {
			KeysInner::Empty => 0,
			KeysInner::KV(at) => match at {
				IterKV::Name => 2,
				IterKV::Value => 1,
				IterKV::Done => 0,
			},
			KeysInner::Map(m) => m.len(),
		}
	}
}

/// Internal representation of an owning attrset iterator
pub enum IntoIterRepr {
	Empty,
	Finite(std::vec::IntoIter<(NixString, Value)>),
}

/// Wrapper type which hides the internal implementation details from
/// users.
#[repr(transparent)]
pub struct OwnedAttrsIterator(IntoIterRepr);

impl Iterator for OwnedAttrsIterator {
	type Item = (NixString, Value);

	fn size_hint(&self) -> (usize, Option<usize>) {
		let len = self.len();
		(len, Some(len))
	}

	fn next(&mut self) -> Option<Self::Item> {
		match &mut self.0 {
			IntoIterRepr::Empty => None,
			IntoIterRepr::Finite(inner) => inner.next(),
		}
	}
}

impl ExactSizeIterator for OwnedAttrsIterator {
	fn len(&self) -> usize {
		match &self.0 {
			IntoIterRepr::Empty => 0,
			IntoIterRepr::Finite(inner) => inner.len(),
		}
	}
}

impl DoubleEndedIterator for OwnedAttrsIterator {
	fn next_back(&mut self) -> Option<Self::Item> {
		match &mut self.0 {
			IntoIterRepr::Empty => None,
			IntoIterRepr::Finite(inner) => inner.next_back(),
		}
	}
}

/// The names CppNix interns when it starts, in order. Its attribute sets
/// iterate in symbol order, so these come first, before other names.
const CPPNIX_STATIC_SYMBOLS: &[&[u8]] = &[
	b"outPath",
	b"drvPath",
	b"type",
	b"meta",
	b"name",
	b"value",
	b"system",
	b"__overrides",
	b"outputs",
	b"outputName",
	b"__ignoreNulls",
	b"file",
	b"line",
	b"column",
	b"__functor",
	b"__toString",
	b"right",
	b"wrong",
	b"__structuredAttrs",
	b"__json",
	b"allowedReferences",
	b"allowedRequisites",
	b"disallowedReferences",
	b"disallowedRequisites",
	b"maxSize",
	b"maxClosureSize",
	b"builder",
	b"args",
	b"__contentAddressed",
	b"__impure",
	b"outputHash",
	b"outputHashAlgo",
	b"outputHashMode",
	b"recurseForDerivations",
	b"description",
	b"self",
	b"startSet",
	b"operator",
	b"key",
	b"path",
	b"prefix",
	b"outputSpecified",
	b"__sub",
	b"__lessThan",
	b"__mul",
	b"__div",
	b"or",
	b"__findFile",
	b"__nixPath",
	b"body",
];

/// An approximation of the order CppNix iterates an attribute set in,
/// which decides which failing attribute it reports: names it interns at
/// startup first, in that order, then the others by name. CppNix orders
/// the others by when it first parsed them.
pub(crate) fn cppnix_attribute_order(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
	let rank = |name: &[u8]| {
		CPPNIX_STATIC_SYMBOLS
			.iter()
			.position(|symbol| *symbol == name)
			.unwrap_or(CPPNIX_STATIC_SYMBOLS.len())
	};
	rank(a).cmp(&rank(b)).then_with(|| a.cmp(b))
}
