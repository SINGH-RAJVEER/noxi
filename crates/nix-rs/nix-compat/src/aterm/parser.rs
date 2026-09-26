//! This module implements parsing code for some basic building blocks
//! of the [ATerm][] format, which is used by C++ Nix to serialize Derivations.
//!
//! [ATerm]: http://program-transformation.org/Tools/ATermFormat.html
use bstr::BString;
use nom::branch::alt;
use nom::bytes::streaming::{escaped_transform, is_not};
use nom::character::streaming::char as nomchar;
use nom::combinator::{map_res, opt, value};
use nom::multi::separated_list0;
use nom::sequence::delimited;
use nom::{IResult, Parser};

/// Parse a bstr and undo any escaping (which is why this needs to allocate).
// FUTUREWORK: have a version for fields that are known to not need escaping
// (like store paths), and use &str.
///
/// Like CppNix, bytes that have an escape sequence (newline, carriage return,
/// tab, backslash, double quote) must be written escaped, so a raw one ends the
/// field and fails the parse.
fn parse_escaped_bytes(i: &[u8]) -> IResult<&[u8], BString> {
	escaped_transform(
		is_not("\"\\\n\r\t"),
		'\\',
		alt((
			value("\\".as_bytes(), nomchar('\\')),
			value("\n".as_bytes(), nomchar('n')),
			value("\t".as_bytes(), nomchar('t')),
			value("\r".as_bytes(), nomchar('r')),
			value("\"".as_bytes(), nomchar('\"')),
		)),
	)(i)
	.map(|(i, v)| (i, BString::new(v)))
}

/// Parse a field in double quotes, undo any escaping, and return the unquoted
/// and decoded `Vec<u8>`.
pub(crate) fn parse_bytes_field(i: &[u8]) -> IResult<&[u8], BString> {
	delimited(
		nomchar('\"'),
		opt(parse_escaped_bytes).map(|opt_bstr| opt_bstr.unwrap_or_default()),
		nomchar('\"'),
	)
	.parse(i)
}

/// Parse a field in double quotes, undo any escaping, and return the unquoted
/// and decoded [String], if it's valid UTF-8.
/// Or fail parsing if the bytes are no valid UTF-8.
pub(crate) fn parse_string_field(i: &[u8]) -> IResult<&[u8], String> {
	delimited(
		nomchar('\"'),
		map_res(
			opt(parse_escaped_bytes).map(|opt_bstr| opt_bstr.unwrap_or_default()),
			|bstr| String::from_utf8(bstr.to_vec()),
		),
		nomchar('\"'),
	)
	.parse(i)
}

/// Parse a field in double quotes that must not contain any escape sequence,
/// and return it as a [String].
///
/// CppNix reads store paths, output names and the platform this way: they come
/// from restricted alphabets, so an escape would be a second encoding of the same
/// value and is rejected.
pub(crate) fn parse_unquoted_string_field(i: &[u8]) -> IResult<&[u8], String> {
	delimited(
		nomchar('\"'),
		map_res(opt(is_not("\"\\")), |b: Option<&[u8]>| {
			String::from_utf8(b.unwrap_or_default().to_vec())
		}),
		nomchar('\"'),
	)
	.parse(i)
}

/// Parse a list of unquoted string fields (enclosed in brackets).
pub(crate) fn parse_unquoted_string_list(i: &[u8]) -> IResult<&[u8], Vec<String>> {
	delimited(
		nomchar('['),
		separated_list0(nomchar(','), parse_unquoted_string_field),
		nomchar(']'),
	)
	.parse(i)
}

/// Parse a list of string fields (enclosed in brackets)
pub(crate) fn parse_string_list(i: &[u8]) -> IResult<&[u8], Vec<String>> {
	delimited(
		nomchar('['),
		separated_list0(nomchar(','), parse_string_field),
		nomchar(']'),
	)
	.parse(i)
}

#[cfg(test)]
mod tests {
	use rstest::rstest;

	#[rstest]
	#[case::empty(br#""""#, b"", b"")]
	#[case::hello_world(br#""Hello World""#, b"Hello World", b"")]
	#[case::doublequote(br#""\"""#, br#"""#, b"")]
	#[case::colon(br#"":""#, b":", b"")]
	#[case::doublequote_rest(br#""\""Rest"#, br#"""#, b"Rest")]
	fn test_parse_bstr_field(
		#[case] input: &[u8],
		#[case] expected: &[u8],
		#[case] exp_rest: &[u8],
	) {
		let (rest, parsed) = super::parse_bytes_field(input).expect("must parse");
		assert_eq!(exp_rest, rest, "expected remainder");
		assert_eq!(expected, parsed);
	}

	#[rstest]
	#[case::empty(br#""""#, "", b"")]
	#[case::hello_world(br#""Hello World""#, "Hello World", b"")]
	#[case::doublequote(br#""\"""#, r#"""#, b"")]
	#[case::colon(br#"":""#, ":", b"")]
	#[case::doublequote_rest(br#""\""Rest"#, r#"""#, b"Rest")]
	fn parse_string_field(#[case] input: &[u8], #[case] expected: &str, #[case] exp_rest: &[u8]) {
		let (rest, parsed) = super::parse_string_field(input).expect("must parse");
		assert_eq!(exp_rest, rest, "expected remainder");
		assert_eq!(expected, &parsed);
	}

	#[test]
	fn parse_string_field_invalid_encoding_fail() {
		let input: Vec<_> = vec![b'"', 0xc5, 0xc4, 0xd6, b'"'];

		super::parse_string_field(&input).expect_err("must fail");
	}

	#[rstest]
	#[case::single_foo(br#"["foo"]"#, vec!["foo".to_string()], b"")]
	#[case::empty_list(b"[]", vec![], b"")]
	#[case::empty_list_with_rest(b"[]blub", vec![], b"blub")]
	fn parse_list(#[case] input: &[u8], #[case] expected: Vec<String>, #[case] exp_rest: &[u8]) {
		let (rest, parsed) = super::parse_string_list(input).expect("must parse");
		assert_eq!(exp_rest, rest, "expected remainder");
		assert_eq!(expected, parsed);
	}
}
