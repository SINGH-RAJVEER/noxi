//! LFS batch requests and shared cache bytes without an external service.
use bstr::ByteSlice;
use nix_rs_eval::{EvalMode, Evaluation};
use nix_rs_glue::fetch::{
	FetchConfig,
	http::{Client, Request, Response},
};
use nix_rs_glue::{EvaluatorStore, configure};
use sha2::{Digest, Sha256};
use std::cell::RefCell;
use std::fs;
use std::path::Path;
use std::process::Command;
use std::rc::Rc;

fn hex(bytes: &[u8]) -> String {
	data_encoding::HEXLOWER.encode(bytes)
}
fn git(path: &Path, args: &[&str]) -> String {
	let output = Command::new("git")
		.arg("-C")
		.arg(path)
		.args(args)
		.env("GIT_CONFIG_GLOBAL", "/dev/null")
		.env("GIT_AUTHOR_DATE", "2020-01-01T00:00:00Z")
		.env("GIT_COMMITTER_DATE", "2020-01-01T00:00:00Z")
		.output()
		.unwrap();
	assert!(
		output.status.success(),
		"{}",
		String::from_utf8_lossy(&output.stderr)
	);
	String::from_utf8(output.stdout).unwrap().trim().into()
}
#[derive(Debug)]
struct Lfs {
	content: Vec<u8>,
	oid: String,
	calls: RefCell<Vec<String>>,
}
impl Client for Lfs {
	fn request(&self, request: &Request) -> nix_rs_flakeref::error::Result<Response> {
		self.calls.borrow_mut().push(request.url.clone());
		let body = if request.url == "https://forge.invalid/repo.git/info/lfs/objects/batch" {
			assert_eq!(request.method, "POST");
			assert_eq!(
				serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
				serde_json::json!({"operation":"download", "objects":[{"oid":self.oid,"size":self.content.len()}]})
			);
			serde_json::to_vec(&serde_json::json!({"objects":[{"oid":self.oid,"size":self.content.len(),"actions":{"download":{"href":"https://objects.invalid/object","header":{"Authorization":"fixture"}}}}]})).unwrap()
		} else {
			assert_eq!(request.url, "https://objects.invalid/object");
			assert!(
				request
					.headers
					.contains(&("Authorization".into(), "fixture".into()))
			);
			self.content.clone()
		};
		Ok(Response {
			status: 200,
			effective_url: request.url.clone(),
			body,
			..Default::default()
		})
	}
}
fn candidate(
	cwd: &Path,
	cache: &Path,
	client: Rc<Lfs>,
	expression: &str,
	offline: bool,
) -> serde_json::Value {
	let store = EvaluatorStore::new();
	store.configure_fetching(FetchConfig {
		cwd: cwd.into(),
		cache_dir: Some(cache.into()),
		http: Some(client),
		offline,
		..Default::default()
	});
	let result = configure(
		Evaluation::builder_pure().experimental_features(["flakes", "fetch-tree"]),
		store,
	)
	.mode(EvalMode::Strict)
	.build()
	.evaluate_in_dir(&format!("builtins.toJSON ({expression})"), cwd.into());
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
fn lfs_batch_and_offline_cache_match_reference() {
	let root = tempfile::tempdir().unwrap();
	git(root.path(), &["init", "-b", "main"]);
	git(root.path(), &["config", "user.name", "Fetcher test"]);
	git(
		root.path(),
		&["config", "user.email", "fetcher@example.invalid"],
	);
	git(
		root.path(),
		&["remote", "add", "origin", "https://forge.invalid/repo"],
	);
	let content = b"LFS content fetched through the fixture\n".to_vec();
	let oid = hex(&Sha256::digest(&content));
	fs::write(root.path().join(".gitattributes"), "object filter=lfs\n").unwrap();
	fs::write(
		root.path().join("object"),
		format!(
			"version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {}\n",
			content.len()
		),
	)
	.unwrap();
	fs::write(root.path().join("flake.nix"), "{ inputs.self.lfs = true; outputs = { self }: { content = builtins.readFile (self.outPath + \"/object\"); info = { inherit (self) outPath narHash lastModifiedDate; }; }; }").unwrap();
	git(root.path(), &["add", "."]);
	git(
		root.path(),
		&[
			"-c",
			"filter.lfs.clean=",
			"-c",
			"filter.lfs.required=false",
			"commit",
			"-m",
			"pointer",
		],
	);
	let rev = git(root.path(), &["rev-parse", "HEAD"]);
	let home = tempfile::tempdir().unwrap();
	let cache = home.path().join("reference-cache");
	let object_dir = cache.join("git-lfs").join(hex(&Sha256::digest(b"object")));
	fs::create_dir_all(&object_dir).unwrap();
	fs::write(object_dir.join(&oid), &content).unwrap();
	let expressions = [
		format!(
			"let t = builtins.fetchTree {{ type = \"git\"; url = {}; rev = \"{rev}\"; lfs = true; }}; in {{ attrs = builtins.map (key: {{ inherit key; value = t.${{key}}; }}) (builtins.attrNames t); content = builtins.readFile (t.outPath + \"/object\"); }}",
			serde_json::json!(format!("file://{}", root.path().display()))
		),
		format!(
			"let f = builtins.getFlake {}; in {{ inherit (f) content info; }}",
			serde_json::json!(format!("git+file://{}?rev={rev}", root.path().display()))
		),
	];
	let client = Rc::new(Lfs {
		content,
		oid,
		calls: RefCell::new(Vec::new()),
	});
	let candidate_cache = tempfile::tempdir().unwrap();
	for expression in expressions {
		let store = tempfile::tempdir().unwrap();
		let output = Command::new(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/nix-reference/bin/nix-instantiate"))
			.env_clear().env("PATH", std::env::var_os("PATH").unwrap()).env("HOME", home.path()).env("NIX_CACHE_HOME", &cache).env("NIX_CONF_DIR",home.path())
			.env("NIX_CONFIG", "experimental-features = flakes nix-command\nsubstituters =\nflake-registry =\nwarn-dirty = false\n")
			.args(["--store", &format!("local?root={}", store.path().display()), "--eval", "--strict", "--json", "--expr", &expression]).output().unwrap();
		assert!(
			output.status.success(),
			"{}",
			String::from_utf8_lossy(&output.stderr)
		);
		let expected: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
		assert_eq!(
			candidate(
				root.path(),
				candidate_cache.path(),
				client.clone(),
				&expression,
				false
			),
			expected
		);
		assert_eq!(
			candidate(
				root.path(),
				candidate_cache.path(),
				client.clone(),
				&expression,
				true
			),
			expected
		);
	}
	assert_eq!(client.calls.borrow().len(), 2);
}
