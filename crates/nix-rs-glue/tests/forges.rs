//! Forge API resolution through an in-process HTTP fixture, without external access.
use bstr::ByteSlice;
use nix_rs_eval::{EvalMode, Evaluation};
use nix_rs_glue::fetch::{
	FetchConfig,
	http::{Client, Request, Response},
};
use nix_rs_glue::{EvaluatorStore, configure};
use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

const REV: &str = "0123456789abcdef0123456789abcdef01234567";

#[derive(Debug)]
struct Forge {
	tar: Vec<u8>,
	calls: RefCell<Vec<String>>,
}
impl Client for Forge {
	fn request(&self, request: &Request) -> nix_rs_flakeref::error::Result<Response> {
		assert_eq!(request.method, "GET");
		self.calls.borrow_mut().push(request.url.clone());
		let body = if request
			.url
			.contains("api.github.com/repos/owner/repo/commits/main")
		{
			format!("{{\"sha\":\"{REV}\"}}").into_bytes()
		} else if request
			.url
			.contains("gitlab.com/api/v4/projects/owner%2Frepo/repository/commits?ref_name=main")
		{
			format!("[{{\"id\":\"{REV}\"}}]").into_bytes()
		} else if request.url.ends_with("/HEAD") {
			b"ref: refs/heads/main\n".to_vec()
		} else if request.url.ends_with("/info/refs") {
			format!("{REV}\trefs/heads/main\n").into_bytes()
		} else if request.url.contains(REV)
			&& (request.url.contains("archive") || request.url.contains("tarball"))
		{
			self.tar.clone()
		} else {
			panic!("unexpected fixture request {}", request.url);
		};
		Ok(Response {
			status: 200,
			effective_url: request.url.clone(),
			body,
			..Default::default()
		})
	}
}

fn projection(tree: &str) -> String {
	format!(
		"let tree = ({tree}); in builtins.map (key: {{ inherit key; value = tree.${{key}}; }}) (builtins.attrNames tree)"
	)
}

fn evaluate(cache: &Path, fixture: Rc<Forge>, reference: &str, offline: bool) -> serde_json::Value {
	let store = EvaluatorStore::new();
	store.configure_fetching(FetchConfig {
		cache_dir: Some(cache.to_owned()),
		http: Some(fixture),
		offline,
		..Default::default()
	});
	let expr = format!(
		"builtins.toJSON ({})",
		projection(&format!(
			"builtins.fetchTree {}",
			serde_json::json!(reference)
		))
	);
	let result = configure(
		Evaluation::builder_pure().experimental_features(["flakes", "fetch-tree"]),
		store,
	)
	.mode(EvalMode::Strict)
	.build()
	.evaluate(&expr, None);
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	serde_json::from_str(
		result
			.value
			.unwrap()
			.to_contextful_str()
			.unwrap()
			.to_str()
			.unwrap(),
	)
	.unwrap()
}

#[test]
fn github_gitlab_and_sourcehut_apis_lock_offline_with_reference_attributes() {
	let dir = tempfile::tempdir().unwrap();
	let mut tar_bytes = Vec::new();
	{
		let mut tar = tar::Builder::new(&mut tar_bytes);
		let mut header = tar::Header::new_gnu();
		header.set_mode(0o644);
		header.set_size(5);
		header.set_mtime(123);
		header.set_cksum();
		tar.append_data(&mut header, "root/file", &b"hello"[..])
			.unwrap();
		tar.finish().unwrap();
	}
	let archive = dir.path().join("source.tar");
	fs::write(&archive, &tar_bytes).unwrap();
	let reference = Path::new(env!("CARGO_MANIFEST_DIR"))
		.join("../../target/nix-reference/bin/nix-instantiate");
	let expression = projection(&format!(
		"builtins.fetchTree {{ type = \"tarball\"; url = {}; rev = \"{REV}\"; }}",
		serde_json::json!(format!("file://{}", archive.display()))
	));
	let home = tempfile::tempdir().unwrap();
	let store = tempfile::tempdir().unwrap();
	let output = Command::new(reference)
		.env_clear()
		.env("PATH", std::env::var_os("PATH").unwrap())
		.env("HOME", home.path())
		.env("NIX_CONF_DIR", home.path())
		.env(
			"NIX_CONFIG",
			"experimental-features = flakes nix-command\nsubstituters =\nflake-registry =\n",
		)
		.args([
			"--store",
			&format!("local?root={}", store.path().display()),
			"--eval",
			"--json",
			"--strict",
			"--expr",
			&expression,
		])
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	let expected: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
	for input in [
		"github:owner/repo/main",
		"gitlab:owner/repo/main",
		"sourcehut:~owner/repo",
	] {
		let cache = tempfile::tempdir().unwrap();
		let fixture = Rc::new(Forge {
			tar: tar_bytes.clone(),
			calls: RefCell::new(Vec::new()),
		});
		assert_eq!(
			evaluate(cache.path(), fixture.clone(), input, false),
			expected
		);
		let requests = fixture.calls.borrow().len();
		assert!(requests >= 2);
		assert_eq!(
			evaluate(cache.path(), fixture.clone(), input, true),
			expected
		);
		assert_eq!(fixture.calls.borrow().len(), requests);
	}
}
