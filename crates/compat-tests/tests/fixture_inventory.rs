//! Checks that every file copied from CppNix into `tests/nix` has a
//! classification in `expectations/fixtures.txt`.
//!
//! A new fixture, a pattern that no longer matches anything, or a `pass` or
//! `table` entry naming a test that does not read the fixture fails this test.
//! Whether each fixture actually passes is checked by the test it names.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const MANIFEST: &str = include_str!("../expectations/fixtures.txt");

/// Directories whose files the manifest must classify, relative to `tests/nix`.
/// The C++ test sources next to the `data` directories are specifications, not
/// fixtures, and are not listed.
const ROOTS: &[&str] = &["src", "tests"];

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Class {
	Table(String),
	Pass(String),
	Gap,
	Anomaly,
	Unrun,
	Ignore,
}

struct Rule {
	line: usize,
	class: Class,
	pattern: String,
}

fn snapshot_root() -> PathBuf {
	Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/nix")
}

fn parse_manifest() -> Vec<Rule> {
	let mut rules = Vec::new();
	for (index, raw) in MANIFEST.lines().enumerate() {
		let line = raw.split(" #").next().unwrap().trim();
		if line.is_empty() || line.starts_with('#') {
			continue;
		}
		let mut fields = line.split_whitespace();
		let class = fields.next().unwrap();
		let pattern = fields
			.next()
			.unwrap_or_else(|| panic!("line {}: missing pattern", index + 1));
		assert!(
			fields.next().is_none(),
			"line {}: unexpected field after the pattern",
			index + 1
		);
		let class = match class.split_once(':') {
			Some(("table", test)) => Class::Table(test.to_owned()),
			Some(("pass", test)) => Class::Pass(test.to_owned()),
			None if class == "gap" => Class::Gap,
			None if class == "anomaly" => Class::Anomaly,
			None if class == "unrun" => Class::Unrun,
			None if class == "ignore" => Class::Ignore,
			_ => panic!("line {}: unknown class {class:?}", index + 1),
		};
		rules.push(Rule {
			line: index + 1,
			class,
			pattern: pattern.to_owned(),
		});
	}
	rules
}

/// Matches one path segment against a pattern segment where `*` matches any
/// run of characters.
fn segment_matches(pattern: &str, text: &str) -> bool {
	match pattern.split_once('*') {
		None => pattern == text,
		Some((prefix, rest)) => {
			let Some(text) = text.strip_prefix(prefix) else {
				return false;
			};
			(0..=text.len())
				.filter(|&i| text.is_char_boundary(i))
				.any(|i| segment_matches(rest, &text[i..]))
		}
	}
}

/// Matches a `/`-separated path where a `**` segment matches zero or more
/// whole segments.
fn glob_matches(pattern: &[&str], path: &[&str]) -> bool {
	match pattern.split_first() {
		None => path.is_empty(),
		Some((&"**", rest)) => (0..=path.len()).any(|skip| glob_matches(rest, &path[skip..])),
		Some((segment, rest)) => path.split_first().is_some_and(|(first, tail)| {
			segment_matches(segment, first) && glob_matches(rest, tail)
		}),
	}
}

fn collect_files(dir: &Path, root: &Path, only_data: bool, out: &mut Vec<String>) {
	for entry in std::fs::read_dir(dir).expect("snapshot directory must be readable") {
		let entry = entry.expect("directory entry");
		let path = entry.path();
		let relative = path
			.strip_prefix(root)
			.unwrap()
			.to_string_lossy()
			.into_owned();
		// Do not follow directory symlinks: src/nix-functional-tests links to
		// tests/functional, which is walked directly.
		if entry.file_type().expect("file type").is_dir() {
			collect_files(&path, root, only_data, out);
		} else if !only_data || relative.split('/').any(|segment| segment == "data") {
			out.push(relative);
		}
	}
}

fn fixture_files() -> Vec<String> {
	let root = snapshot_root();
	let mut files = Vec::new();
	for top in ROOTS {
		// Under src/, only the `*-tests/data` trees hold fixtures.
		collect_files(&root.join(top), &root, *top == "src", &mut files);
	}
	files.sort();
	files
}

/// The last literal directory of a pattern, before its first wildcard.
fn fixture_dir(pattern: &str) -> &str {
	let pattern = if pattern.contains('*') {
		pattern
	} else {
		pattern
			.rsplit_once('/')
			.map_or(pattern, |(parent, _)| parent)
	};
	pattern
		.split('/')
		.take_while(|segment| !segment.contains('*'))
		.last()
		.expect("pattern starts with a literal directory")
}

fn file_stem(path: &str) -> &str {
	let name = path.rsplit('/').next().unwrap();
	name.split_once('.').map_or(name, |(stem, _)| stem)
}

#[test]
fn every_fixture_is_classified() {
	let rules = parse_manifest();
	let files = fixture_files();
	assert!(
		files.len() > 1000,
		"snapshot looks incomplete: {} files",
		files.len()
	);

	let mut unclassified = Vec::new();
	let mut hits = vec![0usize; rules.len()];
	let mut by_class: BTreeMap<Class, usize> = BTreeMap::new();
	for file in &files {
		let segments: Vec<&str> = file.split('/').collect();
		let rule = rules.iter().position(|rule| {
			let pattern: Vec<&str> = rule.pattern.split('/').collect();
			glob_matches(&pattern, &segments)
		});
		match rule {
			Some(index) => {
				hits[index] += 1;
				*by_class.entry(rules[index].class.clone()).or_default() += 1;
			}
			None => unclassified.push(file.clone()),
		}
	}

	assert!(
		unclassified.is_empty(),
		"add these fixtures to expectations/fixtures.txt:\n{}",
		unclassified.join("\n")
	);
	let stale: Vec<String> = rules
		.iter()
		.zip(&hits)
		.filter(|(_, hits)| **hits == 0)
		.map(|(rule, _)| format!("line {}: {}", rule.line, rule.pattern))
		.collect();
	assert!(
		stale.is_empty(),
		"patterns that match no file:\n{}",
		stale.join("\n")
	);

	eprintln!("fixture classification: {by_class:?}");
}

/// A `pass` or `table` entry must name a test in this crate that refers to the
/// fixture's directory, and a `pass` entry's test must also name each file.
#[test]
fn claimed_tests_read_their_fixtures() {
	let rules = parse_manifest();
	let files = fixture_files();
	let tests_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests");

	let mut problems = Vec::new();
	for (index, rule) in rules.iter().enumerate() {
		let (test, per_file) = match &rule.class {
			Class::Table(test) => (test, false),
			Class::Pass(test) => (test, true),
			_ => continue,
		};
		let Ok(source) = std::fs::read_to_string(tests_dir.join(format!("{test}.rs"))) else {
			problems.push(format!("line {}: no test file tests/{test}.rs", rule.line));
			continue;
		};
		let dir = fixture_dir(&rule.pattern);
		if !source.contains(dir) && !table_lists(test, &rule.pattern) {
			problems.push(format!("line {}: {test} never mentions {dir:?}", rule.line));
		}
		if !per_file {
			continue;
		}
		let pattern: Vec<&str> = rule.pattern.split('/').collect();
		for file in &files {
			let segments: Vec<&str> = file.split('/').collect();
			// Only files this rule actually classifies, i.e. no earlier rule matched.
			let first = rules.iter().position(|rule| {
				let pattern: Vec<&str> = rule.pattern.split('/').collect();
				glob_matches(&pattern, &segments)
			});
			if first == Some(index) && glob_matches(&pattern, &segments) {
				let stem = file_stem(file);
				if !source.contains(&format!("\"{stem}")) && !table_lists(test, file) {
					problems.push(format!(
						"line {}: {test} never names {stem:?} ({file})",
						rule.line
					));
				}
			}
		}
	}
	assert!(problems.is_empty(), "{}", problems.join("\n"));
}

/// Whether the test's expectation table, `expectations/<test>.txt` with
/// dashes for underscores, has a line for the file.
fn table_lists(test: &str, file: &str) -> bool {
	let table = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("expectations")
		.join(format!("{}.txt", test.replace('_', "-")));
	std::fs::read_to_string(table).is_ok_and(|text| {
		text.lines()
			.any(|line| line.split_whitespace().next() == Some(file))
	})
}

/// Each functional test script's class here matches the class
/// `upstream_functional` recorded from its noxi and reference outcomes, and
/// every support file falls to that test's table entry.
#[test]
fn functional_scripts_match_their_recorded_class() {
	let rules = parse_manifest();
	let table = include_str!("../expectations/upstream-functional.txt");
	let mut problems = Vec::new();
	let mut scripts = 0;
	for line in table.lines() {
		let fields: Vec<&str> = line
			.split(" # ")
			.next()
			.unwrap()
			.split_whitespace()
			.collect();
		let (path, recorded) = match fields.as_slice() {
			[path, "support"] => (*path, None),
			[path, class, _, _] if !path.starts_with('#') => (*path, Some(*class)),
			_ => continue,
		};
		let segments: Vec<&str> = path.split('/').collect();
		let class = rules
			.iter()
			.find(|rule| {
				let pattern: Vec<&str> = rule.pattern.split('/').collect();
				glob_matches(&pattern, &segments)
			})
			.map(|rule| rule.class.clone());
		let expected = match recorded {
			None => Class::Table("upstream_functional".to_owned()),
			Some("pass") => Class::Pass("upstream_functional".to_owned()),
			Some("gap") => Class::Gap,
			Some("anomaly") => Class::Anomaly,
			Some("unrun") => Class::Unrun,
			Some(other) => {
				problems.push(format!("{path}: unknown class {other}"));
				continue;
			}
		};
		scripts += usize::from(recorded.is_some());
		if class.as_ref() != Some(&expected) {
			problems.push(format!(
				"{path}: fixtures.txt says {class:?}, upstream-functional.txt says {expected:?}"
			));
		}
	}
	assert!(scripts > 200, "too few functional scripts: {scripts}");
	assert!(problems.is_empty(), "{}", problems.join("\n"));
}

#[test]
fn glob_semantics() {
	assert_eq!(fixture_dir("tests/functional/hash-path.sh"), "functional");
	assert_eq!(fixture_dir("tests/functional/flakes/*.sh"), "flakes");
	let matches = |pattern: &str, path: &str| {
		glob_matches(
			&pattern.split('/').collect::<Vec<_>>(),
			&path.split('/').collect::<Vec<_>>(),
		)
	};
	assert!(matches("a/**/*.drv", "a/x.drv"));
	assert!(matches("a/**/*.drv", "a/b/c/x.drv"));
	assert!(!matches("a/**/*.drv", "a/b/x.json"));
	assert!(matches("a/*", "a/x"));
	assert!(!matches("a/*", "a/b/x"));
	assert!(matches(
		"src/*/data/.gitkeep",
		"src/libexpr-tests/data/.gitkeep"
	));
	assert!(matches("a/b-*-c", "a/b-x-c"));
	assert!(!matches("a/b-*-c", "a/b-x-d"));
}
