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
				terminated(aterm::parse_unquoted_string_field, nomchar(',')),
				terminated(aterm::parse_unquoted_string_field, nomchar(',')),
				terminated(aterm::parse_string_field, nomchar(',')),
				aterm::parse_bytes_field,
			)
				.parse(i)
				.map_err(into_nomerror)
		},
		nomchar(')'),
	)
	.parse(i)?;

	// Validate outside a combinator, so the specific error reaches the caller.
	let output_name: OutputName = output_name_str.parse().map_err(|err| {
		nom::Err::Failure(NomError {
			input: i,
			code: ErrorKind::InvalidOutputName(err),
		})
	})?;

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
			let mut previous: Option<OutputName> = None;
			for (output_name, store_path, output_hash) in outputs_lst {
				if let Some(previous) = &previous
					&& previous > &output_name
				{
					return Err(nom::Err::Failure(NomError {
						input: i,
						code: ErrorKind::Unsorted(format!("output name '{output_name}'")),
					}));
				}
				previous = Some(output_name.clone());
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

fn parse_input_derivations(
	i: &[u8],
) -> NomResult<&[u8], BTreeMap<StorePath, BTreeSet<OutputName>>> {
	let (i, input_derivations_list) = parse_kv(
		aterm::parse_unquoted_string_field,
		aterm::parse_unquoted_string_list,
	)(i)?;

	// This is a HashMap of drv paths to a list of output names.
	let mut input_derivations: BTreeMap<StorePath, BTreeSet<_>> = BTreeMap::new();

	for (input_derivation, output_names_strings) in input_derivations_list {
		if output_names_strings.is_empty() {
			return Err(nom::Err::Failure(NomError {
				input: i,
				code: ErrorKind::InputDerivationWithoutOutputs(input_derivation),
			}));
		}

		let mut output_names = BTreeSet::<OutputName>::new();
		for output_name_string in output_names_strings.into_iter() {
			let output_name = OutputName::try_from(output_name_string).map_err(|err| {
				nom::Err::Failure(NomError {
					input: i,
					code: ErrorKind::InvalidOutputName(err),
				})
			})?;

			if output_names.contains(&output_name) {
				return Err(nom::Err::Failure(NomError {
					input: i,
					code: ErrorKind::DuplicateInputDerivationOutputName(
						output_name,
						input_derivation,
					),
				}));
			}
			if output_names.last().is_some_and(|last| last > &output_name) {
				return Err(nom::Err::Failure(NomError {
					input: i,
					code: ErrorKind::Unsorted(format!(
						"output name '{output_name}' of input derivation {input_derivation}"
					)),
				}));
			}
			output_names.insert(output_name);
		}

		let input_derivation = string_to_store_path(i, input_derivation.as_str())?;

		input_derivations.insert(input_derivation, output_names);
	}

	Ok((i, input_derivations))
}

fn parse_input_sources(i: &[u8]) -> NomResult<&[u8], BTreeSet<StorePath>> {
	let (i, input_sources_lst) = aterm::parse_unquoted_string_list(i).map_err(into_nomerror)?;

	let mut input_sources: BTreeSet<StorePath> = BTreeSet::new();
	for input_source in input_sources_lst.into_iter() {
		let input_source = string_to_store_path(i, input_source.as_str())?;
		match input_sources.last() {
			Some(last) if last == &input_source => {
				return Err(nom::Err::Failure(NomError {
					input: i,
					code: ErrorKind::DuplicateInputSource(input_source),
				}));
			}
			Some(last) if last > &input_source => {
				return Err(nom::Err::Failure(NomError {
					input: i,
					code: ErrorKind::Unsorted(format!("input source {input_source}")),
				}));
			}
			_ => {}
		}
		input_sources.insert(input_source);
	}

	Ok((i, input_sources))
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
					terminated(aterm::parse_unquoted_string_field, nomchar(','))
						.parse(i)
						.map_err(into_nomerror)
				},
				// // parse builder
				|i| {
					terminated(aterm::parse_string_field, nomchar(','))
						.parse(i)
						.map_err(into_nomerror)
				},
				// // parse arguments
				|i| {
					terminated(aterm::parse_string_list, nomchar(','))
						.parse(i)
						.map_err(into_nomerror)
				},
				// parse environment
				parse_kv(aterm::parse_string_field, aterm::parse_bytes_field),
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

/// Parse a list of key/value pairs into a BTreeMap.
/// The parser for the values can be passed in.
/// In terms of ATerm, this is just a 2-tuple,
/// but we have the additional restriction that the first element needs to be
/// unique across all tuples.
///
/// Like CppNix, the keys must appear in strictly ascending order.
pub(crate) fn parse_kv<'a, V, KF, VF>(
	kf: KF,
	vf: VF,
) -> impl FnMut(&'a [u8]) -> NomResult<&'a [u8], Vec<(String, V)>> + 'static
where
	KF: FnMut(&'a [u8]) -> nom::IResult<&'a [u8], String, nom::error::Error<&'a [u8]>>
		+ Clone
		+ 'static,
	VF: FnMut(&'a [u8]) -> nom::IResult<&'a [u8], V, nom::error::Error<&'a [u8]>> + Clone + 'static,
{
	move |i|
    // inside brackets
    delimited(
        nomchar('['),
        |ii| {
            let res = separated_list0(
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
            ).parse(ii).map_err(into_nomerror);

            match res {
                Ok((rest, pairs)) => {
                    // fail if a key was already seen, or is out of order.
                    for window in pairs.windows(2) {
                        let (previous, key) = (&window[0].0, &window[1].0);
                        if previous == key {
                            return Err(nom::Err::Failure(NomError {
                                input: i,
                                code: ErrorKind::DuplicateMapKey(key.clone()),
                            }));
                        }
                        if previous > key {
                            return Err(nom::Err::Failure(NomError {
                                input: i,
                                code: ErrorKind::Unsorted(format!("map key '{key}'")),
                            }));
                        }
                    }
                    Ok((rest, pairs))
                }
                Err(e) => Err(e),
            }
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

	/// Ensures the kv parser complains about duplicate map keys
	#[test]
	fn parse_kv_fail_dup_keys() {
		let input: &'static [u8] = b"[(\"a\",\"1\"),(\"a\",\"2\")]";
		let e = super::parse_kv(
			crate::aterm::parse_string_field,
			crate::aterm::parse_bytes_field,
		)(input)
		.expect_err("must fail");

		match e {
			nom::Err::Failure(e) => {
				assert_eq!(ErrorKind::DuplicateMapKey("a".to_string()), e.code);
			}
			_ => panic!("unexpected error"),
		}
	}

	/// Keys must be in ascending order, as CppNix's `parseMap` requires.
	#[test]
	fn parse_kv_fail_unsorted_keys() {
		let input: &'static [u8] = b"[(\"b\",\"1\"),(\"a\",\"2\")]";
		let e = super::parse_kv(
			crate::aterm::parse_string_field,
			crate::aterm::parse_bytes_field,
		)(input)
		.expect_err("must fail");

		match e {
			nom::Err::Failure(e) => {
				assert_eq!(ErrorKind::Unsorted("map key 'a'".to_string()), e.code);
			}
			_ => panic!("unexpected error"),
		}
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

	/// Ensures the input derivation parser complains about duplicate output names
	#[test]
	fn parse_input_derivations_fail_dup_output_names() {
		let input_str = format!(
			"[(\"{0}\",[\"out\"]),(\"{1}\",[\"out\",\"out\"])]",
			"/nix/store/8bjm87p310sb7r2r0sg4xrynlvg86j8k-hello-2.12.1.tar.gz.drv",
			"/nix/store/p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv"
		);
		let e = super::parse_input_derivations(input_str.as_bytes()).expect_err("must fail");

		match e {
			nom::Err::Failure(e) => {
				assert_eq!(
					ErrorKind::DuplicateInputDerivationOutputName(
						"out".parse().expect("Valid OutputName"),
						"/nix/store/p3jc8aw45dza6h52v81j7lk69khckmcj-bash-5.2-p15.drv".to_string(),
					),
					e.code
				);
			}
			_ => panic!("unexpected error"),
		}
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

	/// A repeated input source is rejected, as CppNix's `checkMonotonic` does.
	#[test]
	fn parse_input_sources_fail_dup() {
		let input: &'static [u8] = b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo\",\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo\"]";
		let e = super::parse_input_sources(input).expect_err("must fail");

		match e {
			nom::Err::Failure(e) => assert_eq!(
				ErrorKind::DuplicateInputSource(
					StorePathRef::from_absolute_path(
						"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo".as_bytes()
					)
					.unwrap()
					.to_owned()
				),
				e.code
			),
			_ => panic!("unexpected error"),
		}
	}

	/// Two paths that share a digest but differ in name are distinct, and are
	/// ordered by name. The anonymised CppNix `hello.drv` fixture relies on this.
	#[test]
	fn parse_input_sources_same_digest_different_names() {
		let input: &'static [u8] = b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-bar\",\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo\"]";
		let (rest, parsed) = super::parse_input_sources(input).expect("must parse");
		assert!(rest.is_empty());
		assert_eq!(parsed.len(), 2);

		let reversed: &'static [u8] = b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-foo\",\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-bar\"]";
		super::parse_input_sources(reversed).expect_err("unsorted input sources must fail");
	}

	/// Store paths are unquoted fields: an escape sequence in one is rejected.
	#[test]
	fn parse_input_sources_fail_escape() {
		let input: &'static [u8] = b"[\"/nix/store/55lwldka5nyxa08wnvlizyqw02ihy8ic-f\\\"oo\"]";
		super::parse_input_sources(input).expect_err("must fail");
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

	/// Output kinds cannot be mixed, and outputs must be sorted.
	#[rstest]
	#[case::mixed(
		br#"[("dev","","",""),("out","/nix/store/5vyvcwah9l9kf07d52rcgdk70g2f4y13-foo","","")]"#
	)]
	#[case::unsorted(br#"[("out","","",""),("dev","","","")]"#)]
	fn parse_outputs_fail(#[case] input: &[u8]) {
		super::parse_outputs(input).expect_err("must fail");
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
