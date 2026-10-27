//! Serialisation from Rust values to Nix.

use noxi_eval::{NixAttrs, NixList, NixString, Value};
use serde::ser;

use crate::error::Error;

/// Serialise a Rust value into a [`noxi_eval::Value`].
///
/// This is the inverse of [`crate::from_value`].
pub fn to_value<T: serde::Serialize>(value: &T) -> Result<Value, Error> {
	value.serialize(NixSerializer)
}

/// Serialise a Rust value into a Nix code string.
///
/// This is a convenience wrapper around [`to_value`] which converts the resulting
/// [`noxi_eval::Value`] into a string.
pub fn to_string<T: serde::Serialize>(value: &T) -> Result<String, Error> {
	let v = to_value(value)?;
	Ok(v.to_string())
}

struct NixSerializer;

impl ser::Serializer for NixSerializer {
	type Ok = Value;
	type Error = Error;

	type SerializeSeq = SeqSerializer;
	type SerializeTuple = SeqSerializer;
	type SerializeTupleStruct = SeqSerializer;
	type SerializeTupleVariant = TupleVariantSerializer;
	type SerializeMap = MapSerializer;
	type SerializeStruct = MapSerializer;
	type SerializeStructVariant = StructVariantSerializer;

	fn serialize_bool(self, v: bool) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Bool(v))
	}

	fn serialize_i8(self, v: i8) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Integer(v as i64))
	}

	fn serialize_i16(self, v: i16) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Integer(v as i64))
	}

	fn serialize_i32(self, v: i32) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Integer(v as i64))
	}

	fn serialize_i64(self, v: i64) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Integer(v))
	}

	fn serialize_u8(self, v: u8) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Integer(v as i64))
	}

	fn serialize_u16(self, v: u16) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Integer(v as i64))
	}

	fn serialize_u32(self, v: u32) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Integer(v as i64))
	}

	fn serialize_u64(self, v: u64) -> Result<Self::Ok, Self::Error> {
		i64::try_from(v)
			.map(Value::Integer)
			.map_err(|_| Error::IntegerOverflow { got: v })
	}

	fn serialize_f32(self, v: f32) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Float(v as f64))
	}

	fn serialize_f64(self, v: f64) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Float(v))
	}

	fn serialize_char(self, v: char) -> Result<Self::Ok, Self::Error> {
		let mut buf = [0u8; 4];
		Ok(Value::String(NixString::from(
			v.encode_utf8(&mut buf) as &str
		)))
	}

	fn serialize_str(self, v: &str) -> Result<Self::Ok, Self::Error> {
		Ok(Value::String(NixString::from(v)))
	}

	fn serialize_bytes(self, _v: &[u8]) -> Result<Self::Ok, Self::Error> {
		Err(Error::Unsupported { wanted: "bytes" })
	}

	fn serialize_none(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Null)
	}

	fn serialize_some<T>(self, value: &T) -> Result<Self::Ok, Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		value.serialize(self)
	}

	fn serialize_unit(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Null)
	}

	fn serialize_unit_struct(self, _name: &'static str) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Null)
	}

	// Unit variants are serialised as plain strings, mirroring
	// `deserialize_enum` in de.rs which maps a Nix string to a unit variant.
	fn serialize_unit_variant(
		self,
		_name: &'static str,
		_variant_index: u32,
		variant: &'static str,
	) -> Result<Self::Ok, Self::Error> {
		Ok(Value::String(NixString::from(variant)))
	}

	fn serialize_newtype_struct<T>(
		self,
		_name: &'static str,
		value: &T,
	) -> Result<Self::Ok, Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		value.serialize(self)
	}

	// Newtype variants are serialised as `{ VariantName = value; }`.
	fn serialize_newtype_variant<T>(
		self,
		_name: &'static str,
		_variant_index: u32,
		variant: &'static str,
		value: &T,
	) -> Result<Self::Ok, Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		let inner = value.serialize(NixSerializer)?;
		Ok(Value::Attrs(NixAttrs::from_iter([(variant, inner)])))
	}

	fn serialize_seq(self, len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
		Ok(SeqSerializer {
			items: Vec::with_capacity(len.unwrap_or(0)),
		})
	}

	fn serialize_tuple(self, len: usize) -> Result<Self::SerializeTuple, Self::Error> {
		self.serialize_seq(Some(len))
	}

	fn serialize_tuple_struct(
		self,
		_name: &'static str,
		len: usize,
	) -> Result<Self::SerializeTupleStruct, Self::Error> {
		self.serialize_seq(Some(len))
	}

	// Tuple variants are serialised as `{ VariantName = [a b ...]; }`.
	fn serialize_tuple_variant(
		self,
		_name: &'static str,
		_variant_index: u32,
		variant: &'static str,
		len: usize,
	) -> Result<Self::SerializeTupleVariant, Self::Error> {
		Ok(TupleVariantSerializer {
			variant,
			seq: SeqSerializer {
				items: Vec::with_capacity(len),
			},
		})
	}

	fn serialize_map(self, len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
		Ok(MapSerializer {
			entries: Vec::with_capacity(len.unwrap_or(0)),
			pending_key: None,
		})
	}

	fn serialize_struct(
		self,
		_name: &'static str,
		len: usize,
	) -> Result<Self::SerializeStruct, Self::Error> {
		self.serialize_map(Some(len))
	}

	// Struct variants are serialised as `{ VariantName = { fields... }; }`.
	fn serialize_struct_variant(
		self,
		_name: &'static str,
		_variant_index: u32,
		variant: &'static str,
		len: usize,
	) -> Result<Self::SerializeStructVariant, Self::Error> {
		Ok(StructVariantSerializer {
			variant,
			map: MapSerializer {
				entries: Vec::with_capacity(len),
				pending_key: None,
			},
		})
	}
}

struct SeqSerializer {
	items: Vec<Value>,
}

impl ser::SerializeSeq for SeqSerializer {
	type Ok = Value;
	type Error = Error;

	fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		self.items.push(value.serialize(NixSerializer)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::List(NixList::from(self.items)))
	}
}

impl ser::SerializeTuple for SeqSerializer {
	type Ok = Value;
	type Error = Error;

	fn serialize_element<T>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		self.items.push(value.serialize(NixSerializer)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::List(NixList::from(self.items)))
	}
}

impl ser::SerializeTupleStruct for SeqSerializer {
	type Ok = Value;
	type Error = Error;

	fn serialize_field<T>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		self.items.push(value.serialize(NixSerializer)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::List(NixList::from(self.items)))
	}
}

struct TupleVariantSerializer {
	variant: &'static str,
	seq: SeqSerializer,
}

impl ser::SerializeTupleVariant for TupleVariantSerializer {
	type Ok = Value;
	type Error = Error;

	fn serialize_field<T>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		self.seq.items.push(value.serialize(NixSerializer)?);
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		let list = Value::List(NixList::from(self.seq.items));
		Ok(Value::Attrs(NixAttrs::from_iter([(self.variant, list)])))
	}
}

struct MapSerializer {
	entries: Vec<(NixString, Value)>,
	pending_key: Option<NixString>,
}

/// Extract the NixString from a serialized key value; only string keys are
/// valid in Nix attribute sets.
fn value_to_nixstring(v: Value) -> Result<NixString, Error> {
	match v {
		Value::String(s) => Ok(s),
		_ => Err(Error::NonStringKey),
	}
}

impl ser::SerializeMap for MapSerializer {
	type Ok = Value;
	type Error = Error;

	fn serialize_key<T>(&mut self, key: &T) -> Result<(), Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		let v = key.serialize(NixSerializer)?;
		self.pending_key = Some(value_to_nixstring(v)?);
		Ok(())
	}

	fn serialize_value<T>(&mut self, value: &T) -> Result<(), Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		let key = self
			.pending_key
			.take()
			.expect("serialize_value called before serialize_key");
		let val = value.serialize(NixSerializer)?;
		self.entries.push((key, val));
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		Ok(Value::Attrs(NixAttrs::from_iter(self.entries)))
	}
}

impl ser::SerializeStruct for MapSerializer {
	type Ok = Value;
	type Error = Error;

	fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		let val = value.serialize(NixSerializer)?;
		self.entries.push((NixString::from(key), val));
		Ok(())
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		ser::SerializeMap::end(self)
	}
}

struct StructVariantSerializer {
	variant: &'static str,
	map: MapSerializer,
}

impl ser::SerializeStructVariant for StructVariantSerializer {
	type Ok = Value;
	type Error = Error;

	fn serialize_field<T>(&mut self, key: &'static str, value: &T) -> Result<(), Self::Error>
	where
		T: ?Sized + serde::Serialize,
	{
		ser::SerializeStruct::serialize_field(&mut self.map, key, value)
	}

	fn end(self) -> Result<Self::Ok, Self::Error> {
		let inner = ser::SerializeStruct::end(self.map)?;
		Ok(Value::Attrs(NixAttrs::from_iter([(self.variant, inner)])))
	}
}
