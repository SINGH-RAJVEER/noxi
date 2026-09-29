//! This module constructs a [UnverifiedDerivation] by parsing its [ATerm][]
//! serialization.
//!
//! [ATerm]: http://program-transformation.org/Tools/ATermFormat.html

use nom::Parser;
use nom::bytes::streaming::tag;
use nom::character::streaming::char as nomchar;
use nom::combinator::{all_consuming, consumed};
use nom::multi::separated_list0;
use nom::sequence::{delimited, preceded, separated_pair, terminated};
use std::collections::{BTreeMap, BTreeSet};
use thiserror;

use crate::derivation::parse_error::{ErrorKind, NomError, NomResult, into_nomerror};
use crate::derivation::{
	DerivationBuilder, OutputHash, OutputName, Outputs, UnverifiedDerivation,
	UnverifiedOutputsBuilder, write,
};
use crate::store_path::{self, StorePath};
use crate::{aterm, nixhash};

#[derive(Debug, thiserror::Error)]
/// The error type for derivation parsing errors
#[allow(missing_docs)]
pub enum Error<I> {
	#[error("parsing error: {0}")]
	Parser(#[from] NomError<I>),
	#[error("premature EOF")]
	Incomplete,
	#[error("validation error: {0}")]
	Validation(super::DerivationError),
}

/// Convenience conversion of borrowed Error to an owned counterpart.
impl From<Error<&[u8]>> for Error<Vec<u8>> {
	fn from(value: Error<&[u8]>) -> Self {
		match value {
			Error::Parser(nom_error) => Error::Parser(NomError {
				input: nom_error.input.to_vec(),
				code: nom_error.code,
			}),
			Error::Incomplete => Error::Incomplete,
			Error::Validation(e) => Error::Validation(e),
		}
	}
}

pub fn parse(i: &[u8]) -> Result<UnverifiedDerivation, Error<&[u8]>> {
	match all_consuming(parse_derivation).parse(i) {
		Ok((rest, (builder, outputs))) => {
			// this shouldn't happen, as all_consuming shouldn't return.
			debug_assert!(rest.is_empty());

			// invoke validate
			builder
				.build_unverified_as_parsed(outputs)
				.map_err(Error::Validation)
		}
		Err(nom::Err::Incomplete(_)) => Err(Error::Incomplete),
		Err(nom::Err::Error(e) | nom::Err::Failure(e)) => Err(e.into()),
	}
}

/// This parses a derivation in streaming fashion.
/// If the parse is successful, it returns the leftover bytes which were not used for the parsing.
/// If the parse is unsuccessful, either it returns incomplete or an error with the input as
/// leftover.
#[allow(dead_code)]
pub fn parse_streaming(i: &[u8]) -> (Result<UnverifiedDerivation, Error<&[u8]>>, &[u8]) {
	match consumed(parse_derivation).parse(i) {
		Ok((_, (rest, (builder, outputs)))) => {
			// invoke validate
			match builder.build_unverified_as_parsed(outputs) {
				Ok(derivation) => (Ok(derivation), rest),
				Err(e) => (Err(Error::Validation(e)), i),
			}
		}
		Err(nom::Err::Incomplete(_)) => (Err(Error::Incomplete), i),
		Err(nom::Err::Error(e) | nom::Err::Failure(e)) => (Err(e.into()), i),
	}
}

/// Parse one output in ATerm. This is 4 string fields inside parans:
/// output name, output path, algo (and mode), digest.
/// Returns the [`OutputName`], the output [`StorePath`] (absent for outputs
/// whose path is not known yet) and possible FOD [`OutputHash`].
fn parse_output(i: &[u8]) -> NomResult<&[u8], (OutputName, Option<StorePath>, Option<OutputHash>)> {
	let (rest, (output_name_str, output_path_str, algo_and_mode, encoded_digest)) = delimited(
		nomchar('('),
		|i| {
			(
				terminated(aterm::parse_bytes_field, nomchar(',')),
				terminated(aterm::parse_string_field, nomchar(',')),
				terminated(aterm::parse_string_field, nomchar(',')),
				aterm::parse_bytes_field,
			)
				.parse(i)
				.map_err(into_nomerror)
		},
		nomchar(')'),
	)
	.parse(i)?;

	// CppNix's ATerm parser stores output names without validating them.
	let output_name = OutputName::from_aterm_unchecked(output_name_str);

	// An empty path is a deferred output, or a floating content-addressed one.
	let path = if output_path_str.is_empty() {
		None
	} else {
		Some(string_to_store_path(i, &output_path_str)?)
	};

	// A hash algorithm without a path is a floating content-addressed or
	// impure output, which CppNix only accepts with experimental features.
	if path.is_none() && !algo_and_mode.is_empty() {
		return Err(nom::Err::Failure(NomError {
			input: i,
			code: ErrorKind::FloatingContentAddressedOutput(output_name),
		}));
	}

	let output_hash = if algo_and_mode.is_empty() && encoded_digest.is_empty() {
		None
	} else {
		let digest = data_encoding::HEXLOWER
			.decode(&encoded_digest)
			.map_err(|err| {
				nom::Err::Failure(NomError {
					input: i,
					code: ErrorKind::NixHashError(
						// TODO: do we still need the outer error?
						nixhash::Error::InvalidBase16Encoding(err),
					),
				})
			})?;

		Some(
			OutputHash::from_mode_algo_and_digest(&algo_and_mode, digest).map_err(|err| {
				nom::Err::Failure(NomError {
					input: i,
					code: ErrorKind::NixHashError(err),
				})
			})?,
		)
	};
	Ok((rest, (output_name, path, output_hash)))
}

/// Parse multiple outputs in ATerm. This is a list of things acccepted by
/// parse_output, and takes care of turning the (String, Output) returned from
/// it to a BTreeMap.
/// We don't use parse_kv here, as it's dealing with 2-tuples, and these are
/// 4-tuples.
///
/// Like CppNix 2.35.2, outputs may appear in any order, and when a name repeats
/// the first output with that name is kept (`emplace`).
fn parse_outputs(i: &[u8]) -> NomResult<&[u8], Outputs> {
	let res = delimited(
		nomchar('['),
		separated_list0(tag(","), parse_output),
		nomchar(']'),
	)
	.parse(i);

	match res {
		Ok((rst, outputs_lst)) => {
			let mut builder = UnverifiedOutputsBuilder::default();
			let mut seen = BTreeSet::<OutputName>::new();
			for (output_name, store_path, output_hash) in outputs_lst {
				if !seen.insert(output_name.clone()) {
					continue;
				}
				let inserted = match (store_path, output_hash) {
					(Some(store_path), output_hash) => {
						builder.try_insert(output_name, store_path, output_hash)
					}
					(None, None) => builder.try_insert_deferred(output_name),
					(None, Some(_)) => {
						return Err(nom::Err::Failure(NomError {
							input: i,
							code: ErrorKind::FloatingContentAddressedOutput(output_name),
						}));
					}
				};
				inserted.map_err(|err| {
					nom::Err::Failure(NomError {
						input: i,
						code: ErrorKind::InvalidOutputs(err),
					})
				})?;
			}
			// CppNix's parser accepts a derivation without outputs; it is
			// rejected later, when its type is determined.
			let outputs = builder.build_allow_empty();
			Ok((rst, outputs))
		}
		// pass regular parse errors along
		Err(e) => Err(e),
	}
}

/// Parse the input derivations.
///
/// Like CppNix 2.35.2, entries may appear in any order, and a repeated path
/// replaces the earlier entry (`insert_or_assign`). Each entry's output names
/// form a set: they may appear in any order, repeats collapse, and the set may
/// be empty.
fn parse_input_derivations(
	i: &[u8],
) -> NomResult<&[u8], BTreeMap<StorePath, BTreeSet<OutputName>>> {
	let (i, input_derivations_list) =
		parse_kv(aterm::parse_string_field, aterm::parse_bytes_list)(i)?;

	let mut input_derivations: BTreeMap<StorePath, BTreeSet<_>> = BTreeMap::new();

	for (input_derivation, output_names_strings) in input_derivations_list {
		let output_names = output_names_strings
			.into_iter()
			.map(OutputName::from_aterm_unchecked)
			.collect();

		let input_derivation = string_to_store_path(i, input_derivation.as_str())?;

		input_derivations.insert(input_derivation, output_names);
	}

	Ok((i, input_derivations))
}

/// Parse the input sources. Like CppNix 2.35.2, they form a set: any order is
/// accepted, and repeats collapse.
fn parse_input_sources(i: &[u8]) -> NomResult<&[u8], BTreeSet<StorePath>> {
	let (i, input_sources_lst) = aterm::parse_string_list(i).map_err(into_nomerror)?;

	input_sources_lst
		.iter()
		.map(|input_source| string_to_store_path(i, input_source))
		.collect::<Result<_, _>>()
		.map(|input_sources| (i, input_sources))
}

fn string_to_store_path<'i>(
	i: &'i [u8],
	path_str: &str,
) -> Result<StorePath, nom::Err<NomError<&'i [u8]>>> {
	let path = StorePath::from_absolute_path(path_str.as_bytes()).map_err(
		|e: store_path::ParseStorePathError| {
			nom::Err::Failure(NomError {
				input: i,
				code: e.into(),
			})
		},
	)?;

	#[cfg(debug_assertions)]
	assert_eq!(path_str, path.to_absolute_path());

	Ok(path)
}

pub fn parse_derivation(i: &[u8]) -> NomResult<&[u8], (DerivationBuilder, Outputs)> {
	use nom::Parser;
	preceded(
		tag(write::DERIVATION_PREFIX),
		delimited(
			// inside parens
			nomchar('('),
			// tuple requires all errors to be of the same type, so we need to be a
			// bit verbose here wrapping generic IResult into [NomATermResult].
			(
				// parse outputs
				terminated(parse_outputs, nomchar(',')),
				// // parse input derivations
				terminated(parse_input_derivations, nomchar(',')),
				// // parse input sources
				terminated(parse_input_sources, nomchar(',')),
				// // parse system
				|i| {
					terminated(aterm::parse_bytes_field, nomchar(','))
						.parse(i)
						.map_err(into_nomerror)
				},
				// // parse builder
				|i| {
					terminated(aterm::parse_bytes_field, nomchar(','))
						.parse(i)
						.map_err(into_nomerror)
				},
				// // parse arguments
				|i| {
					terminated(aterm::parse_bytes_list, nomchar(','))
						.parse(i)
						.map_err(into_nomerror)
				},
				// parse environment
				parse_kv(aterm::parse_bytes_field, aterm::parse_bytes_field),
			),
			nomchar(')'),
		)
		.map(
			|(
				outputs,
				input_derivations,
				input_sources,
				system,
				command,
				arguments,
				environment,
			)| {
				let outputs_builder = outputs.clone().into_builder();
				let builder = DerivationBuilder {
					arguments,
					command,
					environment: environment.into_iter().collect(),
					input_derivations,
					input_sources,
					outputs: outputs_builder,
					system,
				};
				(builder, outputs)
			},
		),
	)
	.parse(i)
}

/// Parse a list of key/value pairs.
/// The parser for the values can be passed in.
/// In terms of ATerm, this is just a list of 2-tuples. Pairs are returned in
/// input order, so collecting them into a map keeps the last value of a
/// repeated key, as CppNix 2.35.2's `insert_or_assign` does.
pub(crate) fn parse_kv<'a, K, V, KF, VF>(
	kf: KF,
	vf: VF,
) -> impl FnMut(&'a [u8]) -> NomResult<&'a [u8], Vec<(K, V)>> + 'static
where
	KF: FnMut(&'a [u8]) -> nom::IResult<&'a [u8], K, nom::error::Error<&'a [u8]>> + Clone + 'static,
	VF: FnMut(&'a [u8]) -> nom::IResult<&'a [u8], V, nom::error::Error<&'a [u8]>> + Clone + 'static,
{
	move |i|
    // inside brackets
    delimited(
        nomchar('['),
        |ii| {
            separated_list0(
                nomchar(','),
                // inside parens
                delimited(
                    nomchar('('),
                    separated_pair(
                        kf.clone(),
                        nomchar(','),
                        vf.clone(),
                    ),
                    nomchar(')'),
                ),
            ).parse(ii).map_err(into_nomerror)
        },
        nomchar(']'),
    ).parse(i)
}

#[cfg(test)]
mod tests {
	use super::OutputHash;
	use crate::derivation::{OutputHashMode, OutputName, Outputs};
	use crate::store_path::StorePathRef;
	use crate::{
		derivation::{NixHash, parse_error::ErrorKind},
		store_path::StorePath,
	};
	use std::collections::{BTreeMap, BTreeSet};
	use std::sync::LazyLock;

	use bstr::{BString, ByteSlice};
	use hex_literal::hex;
	use rstest::rstest;

	static EXP_MULTI_OUTPUTS: LazyLock<Outputs> = LazyLock::new(|| {
		Outputs::input_addressed_from_iter([
			(
				OutputName::from_static("lib").unwrap(),
				StorePath::from_bytes(b"2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib")
					.unwrap(),
			),
			(
				OutputName::out(),
				StorePath::from_bytes(b"55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out".as_bytes())
					.unwrap(),
			),
		])
		.expect("valid outputs")
	});

	static EXP_AB_MAP: LazyLock<BTreeMap<String, BString>> = LazyLock::new(|| {
		let mut b = BTreeMap::new();
		b.insert("a".to_string(), b"1".into());
		b.insert("b".to_string(), b"2".into());
		b
	});

	static EXP_INPUT_DERIVATIONS_SIMPLE: LazyLock<BTreeMap<StorePath, BTreeSet<OutputName>>> =
		LazyLock::new(|| {
			let mut b = BTreeMap::new();
			b.insert(
				StorePath::from_bytes(b"8bjm87p310sb7r2r0sg4xrynlvg86j8k-hello-2.12.1.tar.gz.drv")
					.unwrap(),
				BTreeSet::from([OutputName::out()]),
			);
			b.insert(
				StorePath::from_bytes(b"p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv")
					.unwrap(),
				BTreeSet::from([OutputName::out(), "lib".parse().expect("valid OutputName")]),
			);
			b
		});

	static EXP_INPUT_DERIVATIONS_SIMPLE_ATERM: LazyLock<String> = LazyLock::new(|| {
		format!(
			"[(\"{0}\",[\"out\"]),(\"{1}\",[\"lib\",\"out\"])]",
			"/nix/store/8bjm87p310sb7r2r0sg4xrynlvg86j8k-hello-2.12.1.tar.gz.drv",
			"/nix/store/p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv"
		)
	});

	static EXP_INPUT_SOURCES_SIMPLE: LazyLock<BTreeSet<String>> = LazyLock::new(|| {
		let mut b = BTreeSet::new();
		b.insert("/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out".to_string());
		b.insert("/nix/store/2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib".to_string());
		b
	});

	/// Ensure parsing KVs works
	#[rstest]
	#[case::empty(b"[]", &BTreeMap::new(), b"")]
	#[case::simple(b"[(\"a\",\"1\"),(\"b\",\"2\")]", &EXP_AB_MAP, b"")]
	fn parse_kv(
		#[case] input: &'static [u8],
		#[case] expected: &BTreeMap<String, BString>,
		#[case] exp_rest: &[u8],
	) {
		let (rest, parsed) = super::parse_kv(
			crate::aterm::parse_string_field,
			crate::aterm::parse_bytes_field,
		)(input)
		.expect("must parse");
		assert_eq!(exp_rest, rest, "expected remainder");
		assert_eq!(*expected, parsed.into_iter().collect());
	}

	#[rstest]
	#[case::incomplete_empty(b"[")]
	#[case::incomplete_simple(b"[(\"a\",\"1\")")]
	#[case::incomplete_complicated_escape(b"[(\"a")]
	#[case::incomplete_complicated_sep(b"[(\"a\",")]
	#[case::incomplete_complicated_multi_escape(b"[(\"a\",\"")]
	#[case::incomplete_complicated_multi_outer_sep(b"[(\"a\",\"b\"),")]
	fn parse_kv_incomplete(#[case] input: &'static [u8]) {
		assert!(matches!(
			super::parse_kv(
				crate::aterm::parse_string_field,
				crate::aterm::parse_bytes_field
			)(input),
			Err(nom::Err::Incomplete(_))
		));
	}

	/// Pairs come back in input order, whatever it is, so collecting them into
	/// a map keeps the last value of a repeated key, as CppNix 2.35.2 does.
	#[test]
	fn parse_kv_any_order_last_wins() {
		let input: &'static [u8] = b"[(\"b\",\"1\"),(\"a\",\"2\"),(\"b\",\"3\")]";
		let (rest, parsed) = super::parse_kv(
			crate::aterm::parse_string_field,
			crate::aterm::parse_bytes_field,
		)(input)
		.expect("must parse");
		assert!(rest.is_empty());

		let map: BTreeMap<String, BString> = parsed.into_iter().collect();
		assert_eq!(
			map,
			BTreeMap::from([
				("a".to_string(), BString::from("2")),
				("b".to_string(), BString::from("3")),
			])
		);
	}

	/// Ensure parsing input derivations works.
	#[rstest]
	#[case::empty(b"[]", &BTreeMap::new())]
	#[case::simple(EXP_INPUT_DERIVATIONS_SIMPLE_ATERM.as_bytes(), &EXP_INPUT_DERIVATIONS_SIMPLE)]
	fn parse_input_derivations(
		#[case] input: &'static [u8],
		#[case] expected: &BTreeMap<StorePath, BTreeSet<OutputName>>,
	) {
		let (rest, parsed) = super::parse_input_derivations(input).expect("must parse");

		assert_eq!(expected, &parsed, "parsed mismatch");
		assert!(rest.is_empty(), "rest must be empty");
	}

	/// Input derivations may be in any order and a repeated path replaces the
	/// earlier entry. Output names may be in any order, repeats collapse, and an
	/// empty list is accepted.
	#[test]
	fn parse_input_derivations_any_order_last_wins() {
		let bash = "/nix/store/p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv";
		let hello = "/nix/store/8bjm87p310sb7r2r0sg4xrynlvg86j8k-hello-2.12.1.tar.gz.drv";
		let input_str = format!(
			"[(\"{bash}\",[\"out\"]),(\"{hello}\",[]),(\"{bash}\",[\"out\",\"lib\",\"out\"])]"
		);
		let (rest, parsed) =
			super::parse_input_derivations(input_str.as_bytes()).expect("must parse");
		assert!(rest.is_empty());

		let expected = BTreeMap::from([
			(
				StorePath::from_absolute_path(hello.as_bytes()).unwrap(),
				BTreeSet::new(),
			),
			(
				StorePath::from_absolute_path(bash.as_bytes()).unwrap(),
				BTreeSet::from([OutputName::out(), "lib".parse().unwrap()]),
			),
		]);
		assert_eq!(expected, parsed);
	}

	/// Ensure parsing input sources works
	#[rstest]
	#[case::empty(b"[]", &BTreeSet::new())]
	#[case::simple(b"[\"/nix/store/2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib\",\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out\"]", &EXP_INPUT_SOURCES_SIMPLE)]
	fn parse_input_sources(#[case] input: &'static [u8], #[case] expected: &BTreeSet<String>) {
		let (rest, parsed) = super::parse_input_sources(input).expect("must parse");

		assert_eq!(
			expected,
			&parsed
				.iter()
				.map(StorePath::to_absolute_path)
				.collect::<BTreeSet<_>>(),
			"parsed mismatch"
		);
		assert!(rest.is_empty(), "rest must be empty");
	}

	/// Input sources form a set: repeats collapse and any order is accepted.
	/// Two paths that share a digest but differ in name are distinct; the
	/// anonymised CppNix `hello.drv` fixture relies on this.
	#[test]
	fn parse_input_sources_set() {
		let input: &'static [u8] = b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo\",\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-bar\",\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo\"]";
		let (rest, parsed) = super::parse_input_sources(input).expect("must parse");
		assert!(rest.is_empty());
		assert_eq!(
			parsed
				.iter()
				.map(StorePath::to_absolute_path)
				.collect::<Vec<_>>(),
			[
				"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-bar",
				"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo",
			]
		);
	}

	/// Store paths are decoded like any other string, so an escaped character
	/// is accepted if the decoded path is valid.
	#[test]
	fn parse_input_sources_escape() {
		let input: &'static [u8] = b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-f\\oo\"]";
		let (_, parsed) = super::parse_input_sources(input).expect("must parse");
		assert_eq!(
			parsed.first().map(StorePath::to_absolute_path).as_deref(),
			Some("/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo")
		);

		let invalid: &'static [u8] = b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-f\\\"oo\"]";
		super::parse_input_sources(invalid).expect_err("a quote is not valid in a store path");
	}

	#[rstest]
	#[case::simple(
        br#"("out","/nix/store/5vyvcwah9l9kf07d52rcgdk70g2f4y13-foo","","")"#,
        (
            OutputName::out(),
            StorePathRef::from_absolute_path("/nix/store/5vyvcwah9l9kf07d52rcgdk70g2f4y13-foo".as_bytes()).unwrap().to_owned(),
            None
        )
    )]
	#[case::fod(
        br#"("out","/nix/store/4q0pg5zpfmznxscq3avycvf9xdvx50n3-bar","r:sha256","08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba")"#,
        (
            OutputName::out(),
            StorePathRef::from_absolute_path(
                "/nix/store/4q0pg5zpfmznxscq3avycvf9xdvx50n3-bar".as_bytes()).unwrap().to_owned(),
            Some(OutputHash{
                mode: OutputHashMode::Recursive,
                hash: NixHash::Sha256(hex!("08813cbee9903c62be4c5027726a418a300da4500b2d369d3af9286f4815ceba")),
            }),
        )
    )]
	#[case::deferred(
        br#"("out","","","")"#,
        (OutputName::out(), None, None)
    )]
	fn parse_output(
		#[case] input: &[u8],
		#[case] expected: (OutputName, impl Into<Option<StorePath>>, Option<OutputHash>),
	) {
		let (rest, parsed) = super::parse_output(input).expect("must parse");
		assert!(rest.is_empty());
		assert_eq!((expected.0, expected.1.into(), expected.2), parsed);
	}

	/// A hash algorithm without a path is a floating content-addressed output,
	/// which CppNix only accepts with the ca-derivations experimental feature.
	#[test]
	fn parse_output_fail_floating() {
		let e = super::parse_output(br#"("out","","r:sha256","")"#).expect_err("must fail");
		match e {
			nom::Err::Failure(e) => assert_eq!(
				ErrorKind::FloatingContentAddressedOutput(OutputName::out()),
				e.code
			),
			_ => panic!("unexpected error"),
		}
	}

	/// CppNix's parser accepts deferred outputs and derivations without outputs.
	#[rstest]
	#[case::deferred(br#"[("dev","","",""),("out","","","")]"#, 2)]
	#[case::none(b"[]", 0)]
	fn parse_outputs_unverified_forms(#[case] input: &[u8], #[case] len: usize) {
		let (rest, parsed) = super::parse_outputs(input).expect("must parse");
		assert!(rest.is_empty());
		assert_eq!(parsed.len(), len);
		assert_eq!(parsed.iter().count(), 0, "no output has a known path");
	}

	/// Output kinds cannot be mixed.
	#[test]
	fn parse_outputs_fail_mixed() {
		super::parse_outputs(
			br#"[("dev","","",""),("out","/nix/store/5vyvcwah9l9kf07d52rcgdk70g2f4y13-foo","","")]"#,
		)
		.expect_err("must fail");
	}

	/// Outputs may be in any order, and the first of two outputs with the same
	/// name is kept, as CppNix 2.35.2's `emplace` does. The discarded duplicate
	/// would otherwise be an invalid mix of kinds.
	#[test]
	fn parse_outputs_any_order_first_wins() {
		let (rest, parsed) = super::parse_outputs(
			br#"[("out","/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out","",""),("lib","/nix/store/2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib","",""),("out","","","")]"#,
		)
		.expect("must parse");
		assert!(rest.is_empty());
		assert_eq!(*EXP_MULTI_OUTPUTS, parsed);
	}

	#[rstest]
	#[case::multi_out(
        br#"[("lib","/nix/store/2vixb94v0hy2xc6p7mbnxxcyc095yyia-has-multi-out-lib","",""),("out","/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-has-multi-out","","")]"#,
        &EXP_MULTI_OUTPUTS
    )]
	fn parse_outputs(#[case] input: &[u8], #[case] expected: &Outputs) {
		let (rest, parsed) = super::parse_outputs(input).expect("must parse");
		assert!(rest.is_empty());
		assert_eq!(*expected, parsed);
	}
}
