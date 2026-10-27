use noxi_flakeref::FlakeRef;

fn roundtrip(input: &str) -> FlakeRef {
	let parsed = input.parse::<FlakeRef>().unwrap();
	let uri = parsed.to_uri();
	let reparsed = uri.as_str().parse::<FlakeRef>().unwrap();
	assert_eq!(
		format!("{parsed:?}"),
		format!("{reparsed:?}"),
		"{input} -> {uri}"
	);
	assert_eq!(uri, reparsed.to_uri(), "formatting must be stable: {input}");
	parsed
}

#[test]
fn repository_reference_fixtures() {
	// CppNix 2.35.2, 2c73b59da29606068c0c98db015dd3a66955525d:
	// tests/nix/src/libflake-tests/flakeref.cc, GitArchiveInput and InputFromURL.
	for (input, expected) in [
		("github:foo/bar/branch%23", "github:foo/bar/branch%23"),
		("github:foo/bar?ref=branch%23", "github:foo/bar/branch%23"),
		(
			"github://////owner%42/////repo%41///branch%43////",
			"github:ownerB/repoA/branchC",
		),
		(
			"gitlab:/owner%252Fsubgroup/////repo%41///branch%43////",
			"gitlab:owner%252Fsubgroup/repoA/branchC",
		),
	] {
		assert_eq!(roundtrip(input).to_uri().as_str(), expected);
	}
}

#[test]
fn empty_repository_refs_remain_in_the_query() {
	for scheme in ["github", "gitlab", "sourcehut"] {
		let input = format!("{scheme}:owner/repo?ref=");
		let parsed = roundtrip(&input);
		assert_eq!(parsed.to_uri().as_str(), input);
		let r#ref = match parsed {
			FlakeRef::GitHub { r#ref, .. }
			| FlakeRef::GitLab { r#ref, .. }
			| FlakeRef::SourceHut { r#ref, .. } => r#ref,
			_ => unreachable!(),
		};
		assert_eq!(r#ref.as_deref(), Some(""));
	}
}

#[test]
fn repository_hosts_refs_and_revisions() {
	for scheme in ["github", "gitlab", "sourcehut"] {
		for suffix in [
			"",
			"/topic/branch",
			"/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
		] {
			let input = format!("{scheme}:owner/repo{suffix}?host=forge.example");
			let parsed = roundtrip(&input);
			let uri = parsed.to_uri();
			assert_eq!(uri.host_str(), None);
			assert!(uri.as_str().starts_with(&format!("{scheme}:owner/repo")));
			assert_eq!(uri.query(), Some("host=forge.example"));
		}
	}
	assert!(matches!(
		roundtrip("github:owner/repo/AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
		FlakeRef::GitHub { rev: Some(rev), r#ref: None, .. }
		if rev == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	));
	assert!(matches!(
		roundtrip("github:owner/repo?rev=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"),
		FlakeRef::GitHub { rev: Some(rev), r#ref: None, .. }
		if rev == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
	));
	// Preserve selectors even where the imported permissive parser accepts
	// values that cannot be placed unambiguously in CppNix's path syntax.
	for input in [
		"github:owner/repo?ref=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
		"github:owner/repo?rev=v1.2.3",
	] {
		assert_eq!(roundtrip(input).to_uri().as_str(), input);
	}
	for input in [
		"github:owner/repo/branch?ref=other",
		"github:owner/repo/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?rev=bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
		"github:owner/repo/branch?rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
		"github:owner/repo/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa?ref=branch",
		"github:owner/",
	] {
		assert!(input.parse::<FlakeRef>().is_err(), "{input}");
	}
}

#[test]
fn transport_queries_do_not_accumulate_fetcher_attributes() {
	for input in [
		"https://example.org/file?token=a%2Bb&token=second&narHash=hash&rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&revCount=2&lastModified=3",
		"https://example.org/archive.tar.gz?token=a%2Bb&token=second&narHash=hash&rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&revCount=2&lastModified=3",
		"git+https://example.org/repo?token=a%2Bb&token=second&ref=main&rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa&shallow=1&submodules=1&exportIgnore=1&allRefs=1&verifyCommit=1&keytype=ssh-ed25519&publicKey=key&publicKeys=a,b",
	] {
		let parsed = roundtrip(input);
		let url = match &parsed {
			FlakeRef::File { url, .. }
			| FlakeRef::Tarball { url, .. }
			| FlakeRef::Git { url, .. } => url,
			_ => panic!("expected transport-backed reference"),
		};
		assert_eq!(url.query(), Some("token=a%2Bb&token=second"));
		let uri = parsed.to_uri();
		for key in [
			"narHash",
			"rev",
			"revCount",
			"lastModified",
			"ref",
			"shallow",
			"submodules",
			"exportIgnore",
			"allRefs",
			"verifyCommit",
			"keytype",
			"publicKey",
			"publicKeys",
		] {
			assert!(
				uri.query_pairs().filter(|(name, _)| name == key).count() <= 1,
				"{uri}"
			);
		}
	}
}

#[test]
fn explicit_fetch_types_survive_extension_inference() {
	assert!(matches!(
		roundtrip("file+https://example.org/archive.tar.gz?revCount=2"),
		FlakeRef::File { .. }
	));
	assert!(matches!(
		roundtrip("tarball+https://example.org/download?lastModified=3"),
		FlakeRef::Tarball { .. }
	));
}

#[test]
fn plain_git_fixtures_keep_their_scheme() {
	// InputFromURL in CppNix's libflake-tests/flakeref.cc.
	for input in [
		"git://somewhere/repo?ref=branch",
		"git+https://somewhere.aaaaaaa/repo?ref=branch",
	] {
		assert_eq!(roundtrip(input).to_uri().as_str(), input);
	}
}

#[test]
fn first_query_value_wins_and_git_booleans_require_one() {
	let parsed = roundtrip(
		"git+https://example.org/repo?ref=first&ref=second&shallow=true&submodules=1&submodules=0",
	);
	assert!(
		matches!(parsed, FlakeRef::Git { r#ref: Some(r#ref), shallow: false, submodules: true, .. } if r#ref == "first")
	);
	let parsed = roundtrip("https://example.org/file?revCount=2&revCount=3");
	assert!(matches!(
		parsed,
		FlakeRef::File {
			rev_count: Some(2),
			..
		}
	));
}

#[test]
fn mercurial_retains_transport_and_selectors() {
	// src/libfetchers/mercurial.cc inputFromURL/toURL at the pinned revision.
	for transport in [
		"http://example.org/repo",
		"https://example.org/repo",
		"ssh://user@example.org/repo",
		"file:///tmp/repo",
	] {
		assert!(matches!(
			roundtrip(&format!("hg+{transport}")),
			FlakeRef::Mercurial {
				r#ref: None,
				rev: None,
				..
			}
		));
		let input = format!(
			"hg+{transport}?token=a%2Bb&ref=main&rev=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
		);
		let parsed = roundtrip(&input);
		assert!(
			matches!(parsed, FlakeRef::Mercurial { url, r#ref: Some(r#ref), rev: Some(rev) }
			if url.as_str() == format!("{transport}?token=a%2Bb") && r#ref == "main" && rev == "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
		);
	}
	assert!("hg+git://example.org/repo".parse::<FlakeRef>().is_err());
}

#[test]
fn relative_path_roundtrips_without_becoming_an_authority() {
	assert_eq!(roundtrip("path:./go").to_uri().as_str(), "path:./go");
}

#[test]
fn formatter_replaces_stale_url_attributes_with_current_fields() {
	for input in [
		"https://example.org/file?rev=old",
		"https://example.org/archive.tar.gz?rev=old",
		"git+https://example.org/repo?rev=old&shallow=1",
	] {
		let mut parsed = input.parse::<FlakeRef>().unwrap();
		match &mut parsed {
			FlakeRef::File { url, rev, .. } | FlakeRef::Tarball { url, rev, .. } => {
				url.set_query(Some("token=a%2Bb&%72ev=stale&rev=duplicate"));
				*rev = Some("new".into());
			}
			FlakeRef::Git {
				url, rev, shallow, ..
			} => {
				url.set_query(Some("token=a%2Bb&%72ev=stale&rev=duplicate&shallow=1"));
				*rev = Some("new".into());
				*shallow = false;
			}
			_ => unreachable!(),
		}
		let uri = parsed.to_uri();
		assert_eq!(uri.query(), Some("token=a%2Bb&rev=new"));
		let reparsed = roundtrip(uri.as_str());
		assert_eq!(uri, reparsed.to_uri());
	}
}
