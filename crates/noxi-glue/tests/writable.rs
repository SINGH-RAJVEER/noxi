use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::rc::Rc;

use nix_compat::store_path::StorePath;
use noxi_eval::{EvalMode, Evaluation, Value};
use noxi_glue::{EvaluatorStore, configure};

fn evaluate(store: &Rc<EvaluatorStore>, cwd: &Path, expr: &str) -> Value {
	let result = configure(Evaluation::builder_pure(), store.clone())
		.mode(EvalMode::Strict)
		.build()
		.evaluate_in_dir(expr, cwd.to_owned());
	assert!(result.errors.is_empty(), "{:?}", result.errors);
	result.value.unwrap()
}

fn path(value: Value) -> StorePath {
	let value = value.to_contextful_str().unwrap();
	StorePath::from_absolute_path(value.as_bytes()).unwrap()
}

#[test]
fn materializes_text_sources_and_derivations_with_references() {
	let root = tempfile::tempdir().unwrap();
	let source = tempfile::tempdir().unwrap();
	fs::write(source.path().join("source"), "hello").unwrap();
	fs::set_permissions(
		source.path().join("source"),
		fs::Permissions::from_mode(0o755),
	)
	.unwrap();
	symlink("source", source.path().join("link")).unwrap();
	let store = EvaluatorStore::writable(root.path()).unwrap();
	let local = store.store().unwrap();
	let copied = path(evaluate(&store, source.path(), r#""${./source}""#));
	assert_eq!(fs::read(local.real_path(&copied)).unwrap(), b"hello");
	assert_ne!(
		fs::metadata(local.real_path(&copied))
			.unwrap()
			.permissions()
			.mode()
			& 0o100,
		0
	);
	let link = path(evaluate(&store, source.path(), r#""${./link}""#));
	assert_eq!(
		fs::read_link(local.real_path(&link)).unwrap(),
		Path::new("source")
	);
	let flat = path(evaluate(
		&store,
		source.path(),
		r#"builtins.path { path = ./source; recursive = false; }"#,
	));
	assert_eq!(fs::read(local.real_path(&flat)).unwrap(), b"hello");
	let checked_flat = path(evaluate(
		&store,
		source.path(),
		r#"builtins.path { path = ./source; name = "checked"; recursive = false; sha256 = builtins.hashFile "sha256" ./source; }"#,
	));
	assert_eq!(fs::read(local.real_path(&checked_flat)).unwrap(), b"hello");
	let filtered = path(evaluate(
		&store,
		source.path(),
		r#"builtins.path { path = ./.; name = "filtered"; filter = p: t: t != "symlink"; }"#,
	));
	assert!(local.real_path(&filtered).join("source").is_file());
	assert!(!local.real_path(&filtered).join("link").exists());
	let text = path(evaluate(
		&store,
		source.path(),
		r#"builtins.toFile "text" "${./source}""#,
	));
	assert_eq!(
		fs::read(local.real_path(&text)).unwrap(),
		copied.to_absolute_path().as_bytes()
	);
	assert_eq!(
		local.query_path_info(&text).unwrap().unwrap().references,
		[copied.clone()].into()
	);
	assert_eq!(
		evaluate(
			&store,
			source.path(),
			r#"builtins.readFile (builtins.toFile "readable" "content")"#
		)
		.to_str()
		.unwrap()
		.as_bytes(),
		b"content"
	);
	let drv_path = path(evaluate(
		&store,
		source.path(),
		r#"(derivation { name = "test"; system = "x"; builder = "/bin/sh"; src = ./source; text = builtins.toFile "text" "${./source}"; }).drvPath"#,
	));
	let known = store.known_paths();
	let drv = known.get_drv_by_drvpath(&drv_path.as_ref()).unwrap();
	assert_eq!(
		fs::read(local.real_path(&drv_path)).unwrap(),
		drv.to_aterm_bytes()
	);
	assert_eq!(
		local
			.query_path_info(&drv_path)
			.unwrap()
			.unwrap()
			.references,
		[copied, text].into()
	);
	assert!(
		local
			.query_derivation_outputs(&drv_path)
			.unwrap()
			.contains_key("out")
	);
	for output in drv.outputs().iter().map(|(_, path)| path) {
		assert!(!local.is_valid_path(output).unwrap());
		assert!(!local.real_path(output).exists());
	}
}

#[test]
fn readonly_computes_the_same_paths_without_opening_a_store() {
	let root = tempfile::tempdir().unwrap();
	let source = tempfile::tempdir().unwrap();
	fs::write(source.path().join("source"), "hello").unwrap();
	let readonly = EvaluatorStore::new();
	assert!(readonly.store().is_none());
	let expressions = [
		r#"builtins.toFile "x" "y""#,
		r#""${./source}""#,
		r#"(derivation { name = "a"; system = "x"; builder = "/bin/sh"; }).drvPath"#,
	];
	let paths: Vec<_> = expressions
		.iter()
		.map(|expr| path(evaluate(&readonly, source.path(), expr)))
		.collect();
	assert!(!root.path().join("nix").exists());
	assert_eq!(
		paths[0].to_absolute_path(),
		"/nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x"
	);
	assert_eq!(
		paths[2].to_absolute_path(),
		"/nix/store/l9ldjc53ry43x24hvq73jx7dx0489mhj-a.drv"
	);
	let writable = EvaluatorStore::writable(root.path()).unwrap();
	for (expr, expected) in expressions.iter().zip(paths) {
		assert_eq!(path(evaluate(&writable, source.path(), expr)), expected);
	}
}

#[test]
fn writable_drvpath_dependencies_use_the_diverted_database() {
	let root = tempfile::tempdir().unwrap();
	let store = EvaluatorStore::writable(root.path()).unwrap();
	let expr = r#"let a = derivation { name = "a"; system = "x"; builder = "/bin/sh"; }; in (derivation { name = "b"; system = "x"; builder = "/bin/sh"; dep = a.drvPath; }).drvPath"#;
	let result = path(evaluate(&store, root.path(), expr));
	assert!(store.store().unwrap().is_valid_path(&result).unwrap());
	drop(store);
	let readonly = EvaluatorStore::read_only(root.path()).unwrap();
	assert!(readonly.store().is_none());
	assert_eq!(path(evaluate(&readonly, root.path(), expr)), result);
}

#[test]
fn dependency_outputs_register_the_input_drv_not_the_unbuilt_output_as_a_reference() {
	let root = tempfile::tempdir().unwrap();
	let store = EvaluatorStore::writable(root.path()).unwrap();
	let result = path(evaluate(
		&store,
		root.path(),
		r#"let a = derivation { name = "a"; system = "x"; builder = "/bin/sh"; outputs = [ "out" "dev" ]; }; in (derivation { name = "b"; system = "x"; builder = "/bin/sh"; dep = a.dev; }).drvPath"#,
	));
	let local = store.store().unwrap();
	let known = store.known_paths();
	let drv = known.get_drv_by_drvpath(&result.as_ref()).unwrap();
	assert_eq!(drv.input_derivations().len(), 1);
	let (input_path, requested_outputs) = drv.input_derivations().first_key_value().unwrap();
	assert_eq!(
		requested_outputs
			.iter()
			.map(ToString::to_string)
			.collect::<Vec<_>>(),
		["dev"]
	);
	assert_eq!(
		local.query_path_info(&result).unwrap().unwrap().references,
		[input_path.clone()].into()
	);
	assert!(local.is_valid_path(input_path).unwrap());
	let input = known.get_drv_by_drvpath(&input_path.as_ref()).unwrap();
	assert_eq!(
		fs::read(local.real_path(input_path)).unwrap(),
		input.to_aterm_bytes()
	);
	assert_eq!(
		local.read_derivation(&result).unwrap().to_aterm_bytes(),
		drv.to_aterm_bytes()
	);
	for (_, output) in input.outputs().iter() {
		assert!(!local.is_valid_path(output).unwrap());
		assert!(!local.real_path(output).exists());
	}
}

#[test]
fn diverted_source_filters_receive_logical_paths_in_both_store_modes() {
	let root = tempfile::tempdir().unwrap();
	let logical = "/nix/store/00000000000000000000000000000000-logical-source";
	let physical = root.path().join(logical.trim_start_matches('/'));
	fs::create_dir_all(physical.join("sub")).unwrap();
	fs::write(physical.join("keep"), "root").unwrap();
	fs::write(physical.join("sub/keep"), "nested").unwrap();
	fs::write(physical.join("drop"), "excluded").unwrap();
	let name = "00000000000000000000000000000000-logical-source";
	let expected = path(evaluate(
		&EvaluatorStore::new(),
		root.path(),
		&format!(
			r#"builtins.path {{ path = "{}"; name = "{name}"; filter = p: t: builtins.baseNameOf p != "drop"; }}"#,
			physical.display()
		),
	));
	let filter = format!(
		r#"p: t: if builtins.substring 0 {} p != "{logical}" then throw "physical filter path leaked" else builtins.baseNameOf p != "drop""#,
		logical.len()
	);
	for store in [
		EvaluatorStore::read_only(root.path()).unwrap(),
		EvaluatorStore::writable(root.path()).unwrap(),
	] {
		for expr in [
			format!(r#"builtins.path {{ path = "{logical}"; filter = {filter}; }}"#),
			format!(r#"builtins.filterSource ({filter}) "{logical}""#),
		] {
			assert_eq!(path(evaluate(&store, root.path(), &expr)), expected);
		}
		if let Some(local) = store.store() {
			assert_eq!(
				fs::read(local.real_path(&expected).join("sub/keep")).unwrap(),
				b"nested"
			);
			assert!(!local.real_path(&expected).join("drop").exists());
		} else {
			assert!(
				!root
					.path()
					.join(expected.to_absolute_path().trim_start_matches('/'))
					.exists()
			);
		}
	}
}

#[test]
fn rejects_live_and_symlink_redirected_roots_before_opening() {
	assert!(EvaluatorStore::writable(Path::new("/")).is_err());
	let root = tempfile::tempdir().unwrap();
	symlink("/nix", root.path().join("nix")).unwrap();
	assert!(EvaluatorStore::writable(root.path()).is_err());
	let root = tempfile::tempdir().unwrap();
	fs::create_dir_all(root.path().join("nix/var/nix")).unwrap();
	symlink("/nix/var/nix/db", root.path().join("nix/var/nix/db")).unwrap();
	assert!(EvaluatorStore::writable(root.path()).is_err());
	assert!(!root.path().join("nix/store").exists());
}

#[test]
fn hash_mismatch_does_not_register_the_source() {
	let root = tempfile::tempdir().unwrap();
	let source = tempfile::tempdir().unwrap();
	fs::write(source.path().join("source"), "hello").unwrap();
	let store = EvaluatorStore::writable(root.path()).unwrap();
	let result = configure(Evaluation::builder_pure(), store.clone()).build().evaluate_in_dir(
		r#"builtins.path { path = ./source; sha256 = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="; }"#, source.path().to_owned());
	assert!(!result.errors.is_empty());
	let config = noxi_store::LocalStoreConfig::with_root(&root.path().canonicalize().unwrap());
	assert!(
		noxi_store::LocalStore::open(config)
			.unwrap()
			.query_all_valid_paths()
			.unwrap()
			.is_empty()
	);
}

#[test]
fn store_path_validity_is_checked_only_in_writable_mode() {
	let root = tempfile::tempdir().unwrap();
	let expr = r#"builtins.storePath "/nix/store/lfngsssysp6h1v4ccqg23c52s9sjl779-x""#;
	let readonly = EvaluatorStore::new();
	let expected = path(evaluate(&readonly, root.path(), expr));
	let writable = EvaluatorStore::writable(root.path()).unwrap();
	let result = configure(Evaluation::builder_pure(), writable.clone())
		.build()
		.evaluate(expr, None);
	assert!(!result.errors.is_empty());
	assert_eq!(
		path(evaluate(
			&writable,
			root.path(),
			r#"builtins.toFile "x" "y""#
		)),
		expected
	);
	assert_eq!(path(evaluate(&writable, root.path(), expr)), expected);
}

#[test]
fn imports_directories_and_symlinks_in_the_diverted_store() {
	let root = tempfile::tempdir().unwrap();
	let source = tempfile::tempdir().unwrap();
	fs::write(source.path().join("default.nix"), "import ./child.nix").unwrap();
	fs::write(source.path().join("child.nix"), "[ 17 ./. ]").unwrap();
	symlink("child.nix", source.path().join("link.nix")).unwrap();
	fs::write(source.path().join("scope.nix"), "x + import ./value.nix").unwrap();
	fs::write(source.path().join("value.nix"), "4").unwrap();
	let writable = EvaluatorStore::writable(root.path()).unwrap();
	let imported = path(evaluate(&writable, source.path(), r#""${./.}""#));
	// The logical path is absent from the host's /nix/store. Each mode must
	// resolve its directory and symlinks through the evaluator's mapped I/O.
	for store in [writable, EvaluatorStore::read_only(root.path()).unwrap()] {
		for suffix in ["", "/default.nix", "/link.nix"] {
			let expression = format!("import {}{suffix}", imported.to_absolute_path());
			let result = evaluate(&store, source.path(), &expression)
				.to_list()
				.unwrap();
			assert_eq!(result[0].as_int().unwrap(), 17);
			assert_eq!(
				result[1].to_path().unwrap().as_path(),
				Path::new(&imported.to_absolute_path())
			);
		}
		let expression = format!(
			"scopedImport {{ x = 8; }} {}/scope.nix",
			imported.to_absolute_path()
		);
		assert_eq!(
			evaluate(&store, source.path(), &expression)
				.as_int()
				.unwrap(),
			12
		);
	}
}

#[test]
fn derivation_import_ignores_scope_and_preserves_output_contexts() {
	let root = tempfile::tempdir().unwrap();
	let store = EvaluatorStore::writable(root.path()).unwrap();
	let drv = path(evaluate(
		&store,
		root.path(),
		r#"(derivation {
		name = "imported"; system = "x"; builder = "/bin/sh"; outputs = [ "out" "dev" ];
	}).drvPath"#,
	));
	for store in [store, EvaluatorStore::read_only(root.path()).unwrap()] {
		let expression = format!(
			r#"let d = scopedImport (abort "unused scope") {}; in [
			d.name d.outputName (builtins.attrNames (builtins.getContext d.out.outPath))
			(d.dev.all == d.all)
		]"#,
			drv.to_absolute_path()
		);
		let values = evaluate(&store, root.path(), &expression)
			.to_list()
			.unwrap();
		assert_eq!(values[0].to_str().unwrap(), "imported");
		assert_eq!(values[1].to_str().unwrap(), "dev");
		assert_eq!(
			values[2].to_list().unwrap()[0].to_str().unwrap(),
			drv.to_absolute_path().as_str()
		);
		assert!(values[3].as_bool().unwrap());
	}
}

#[test]
fn unregistered_drv_files_are_imported_as_expressions() {
	let root = tempfile::tempdir().unwrap();
	let store = EvaluatorStore::writable(root.path()).unwrap();
	let fake: StorePath = "00000000000000000000000000000000-expression.drv"
		.parse()
		.unwrap();
	fs::write(store.store().unwrap().real_path(&fake), "41").unwrap();
	let expression = format!("import {}", fake.to_absolute_path());
	assert_eq!(
		evaluate(&store, root.path(), &expression).as_int().unwrap(),
		41
	);
}

#[test]
fn read_file_retains_only_references_that_occur_in_its_contents() {
	let root = tempfile::tempdir().unwrap();
	let store = EvaluatorStore::writable(root.path()).unwrap();
	let expression = r#"let
		a = builtins.toFile "a" "present";
		b = builtins.toFile "b" "absent";
		doc = builtins.toFile "document" (a + builtins.substring 0 0 b);
	in [ a b (builtins.attrNames (builtins.getContext (builtins.readFile doc))) ]"#;
	let values = evaluate(&store, root.path(), expression).to_list().unwrap();
	let context = values[2].to_list().unwrap();
	assert_eq!(context.len(), 1);
	assert_eq!(
		context[0].to_str().unwrap(),
		values[0].to_contextful_str().unwrap()
	);
	assert_ne!(
		context[0].to_str().unwrap(),
		values[1].to_contextful_str().unwrap()
	);
}
