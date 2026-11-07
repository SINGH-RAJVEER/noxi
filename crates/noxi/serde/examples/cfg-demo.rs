//! This program demonstrates how to use noxi_serde to deserialise
//! program configuration (or other data) from Nix code.
//!
//! This makes it possible to use Nix as an embedded config language.
//! For greater control over evaluation, and for features like adding
//! additional builtins, depending directly on noxi_eval would be
//! required.
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Deserialize, Serialize)]
enum Flavour {
	Tasty,
	Okay,
	Eww,
}

#[allow(dead_code)]
#[derive(Debug, Deserialize, Serialize)]
struct Data {
	name: String,
	foods: HashMap<String, Flavour>,
}

fn main() {
	// Get the content from wherever, read it from a file, receive it
	// over the network - whatever floats your boat! We'll include it
	// as a string.
	let code_loaded = include_str!("foods.nix");

	// Now you can use noxi_serde to deserialise the struct:
	let foods: Data = noxi_serde::from_str(code_loaded).expect("deserialisation should succeed");

	println!("These are the foods:\n{foods:#?}\n");

	let code_serialized: String =
		noxi_serde::to_string(&foods).expect("serialisation should succeed");

	println!("And these are the foods as Nix:\n{code_serialized}");
}
