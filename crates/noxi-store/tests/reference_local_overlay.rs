//! Compare local-overlay operations using a private Linux mount namespace.

#![cfg(target_os = "linux")]

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use nix_compat::nixcpp::settings::Settings;
use nix_compat::store_path::StorePath;
use noxi_store::{
	LocalStore, LocalStoreConfig,
	gc::{GcAction, GcOptions},
	local_overlay,
};

fn settings() -> Settings {
	let mut settings = Settings::default();
	settings
		.apply_config("experimental-features = local-overlay-store", "test")
		.unwrap();
	settings
}

fn reference(root: &Path, program: &str) -> Command {
	let mut command =
		Command::new(PathBuf::from(env::var_os("NOXI_REFERENCE_BIN").unwrap()).join(program));
	command
		.env_clear()
		.env("PATH", env::var_os("PATH").unwrap())
		.env("HOME", root)
		.env("NIX_CONF_DIR", root)
		.env(
			"NIX_CONFIG",
			"experimental-features = nix-command local-overlay-store\nsubstituters =\nbuild-users-group =",
		);
	command
}

fn succeeded(command: &mut Command) -> Output {
	let result = command.output().unwrap();
	assert!(
		result.status.success(),
		"{command:?}: {}",
		String::from_utf8_lossy(&result.stderr)
	);
	result
}

fn uri(root: &Path) -> String {
	format!(
		"local-overlay://?root={0}/merged&lower-store={0}/lower&upper-layer={0}/upper",
		root.display(),
	)
}

#[test]
fn reference_rejects_invalid_mount() {
	let scratch = noxi_test_support::tempdir().unwrap();
	let root = scratch.path();
	let uri = uri(root);
	let error = local_overlay::open(&uri, &settings())
		.unwrap_err()
		.to_string();
	assert_eq!(
		error,
		format!(
			"overlay filesystem {:?} mounted incorrectly",
			root.join("merged/nix/store")
		)
	);
	if env::var_os("NOXI_REFERENCE_BIN").is_some() {
		for features in ["nix-command local-overlay-store", "nix-command"] {
			let reference = reference(root, "nix")
				.env("NIX_CONFIG", format!("experimental-features = {features}"))
				.args(["path-info", "--store", &uri, "--all"])
				.output()
				.unwrap();
			assert_eq!(reference.status.code(), Some(1));
			assert_eq!(reference.stderr, format!("error: {error}\n").as_bytes());
		}
	}
	assert_eq!(
		local_overlay::open(&uri, &Settings::default())
			.unwrap_err()
			.to_string(),
		error
	);
}

#[test]
fn reference_deletion_preserves_lower_store() {
	if env::var_os("NOXI_REFERENCE_BIN").is_none() {
		eprintln!("NOXI_REFERENCE_BIN is unset");
		return;
	}
	let namespace = fs::read_link("/proc/self/ns/mnt").unwrap();
	let Some(parent_namespace) = env::var_os("NOXI_OVERLAY_TEST_PARENT_NS") else {
		let result = Command::new("unshare")
			.args(["-Urnm", "true"])
			.output()
			.unwrap();
		if !result.status.success() {
			eprintln!(
				"user/mount namespace unavailable: {}",
				String::from_utf8_lossy(&result.stderr)
			);
			return;
		}
		let scratch = noxi_test_support::tempdir().unwrap();
		succeeded(
			Command::new("unshare")
				.args(["-Urnm", "--"])
				.arg(env::current_exe().unwrap())
				.args([
					"--exact",
					"reference_deletion_preserves_lower_store",
					"--nocapture",
				])
				.env("NOXI_OVERLAY_TEST_PARENT_NS", namespace)
				.env("NOXI_OVERLAY_TEST_ROOT", scratch.path())
				.env_remove("NIX_STORE_DIR"),
		);
		return;
	};
	assert_ne!(namespace, PathBuf::from(parent_namespace));
	let scratch = PathBuf::from(env::var_os("NOXI_OVERLAY_TEST_ROOT").unwrap());
	for engine in [Engine::Reference, Engine::Noxi] {
		let root = scratch.join(match engine {
			Engine::Reference => "reference",
			Engine::Noxi => "noxi",
		});
		run_overlay_case(engine, &root);
	}
}

#[derive(Clone, Copy)]
enum Engine {
	Reference,
	Noxi,
}

fn run_overlay_case(engine: Engine, root: &Path) {
	let lower = root.join("lower");
	let merged = root.join("merged/nix/store");
	let upper = root.join("upper");
	let work = root.join("work");
	for dir in [&lower.join("nix/store"), &merged, &upper, &work] {
		fs::create_dir_all(dir).unwrap();
	}
	let lower_store = LocalStore::open(LocalStoreConfig::with_root(&lower)).unwrap();
	let seed = |name: &str| {
		let file = root.join(name);
		fs::write(&file, name).unwrap();
		file
	};
	let seed_lower = |name: &str| {
		let path = succeeded(
			reference(root, "nix")
				.args(["store", "add-path", "--store"])
				.arg(&lower)
				.arg(seed(name)),
		);
		StorePath::from_absolute_path(path.stdout.trim_ascii()).unwrap()
	};
	let lower_only = seed_lower("lower-only");
	let unchecked = seed_lower("unchecked");
	succeeded(
		Command::new("mount")
			.args(["-t", "overlay", "overlay", "-o"])
			.arg(format!(
				"lowerdir={}/nix/store,upperdir={},workdir={}",
				lower.display(),
				upper.display(),
				work.display()
			))
			.arg(&merged),
	);
	let hook = root.join("remount.sh");
	fs::write(
		&hook,
		"#!/bin/sh\nprintf '%s' \"$1\" > \"$1/../../../remounted\"\n",
	)
	.unwrap();
	fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).unwrap();
	let uri = format!("{}&remount-hook={}", uri(root), hook.display());
	let add = |name: &str| match engine {
		Engine::Reference => {
			let result = succeeded(
				reference(root, "nix")
					.args(["store", "add-path", "--store", &uri])
					.arg(seed(name)),
			);
			StorePath::from_absolute_path(result.stdout.trim_ascii()).unwrap()
		}
		Engine::Noxi => {
			let file = seed(name);
			let mut nar = Vec::new();
			noxi_store::nar::dump_path(&mut nar, &file).unwrap();
			local_overlay::open(&uri, &settings())
				.unwrap()
				.unwrap()
				.add_to_store_from_dump(
					&mut std::io::Cursor::new(nar),
					name,
					noxi_store::FileIngestionMethod::NixArchive,
					noxi_store::FileIngestionMethod::NixArchive,
					nix_compat::nixhash::HashAlgo::Sha256,
					&BTreeSet::new(),
					false,
				)
				.unwrap()
		}
	};
	let delete = |path: &StorePath| match engine {
		Engine::Reference => {
			succeeded(
				reference(root, "nix-store")
					.args(["--store", &uri, "--delete", "--ignore-liveness"])
					.arg(format!("/nix/store/{path}")),
			);
		}
		Engine::Noxi => {
			let store = local_overlay::open(&uri, &settings()).unwrap().unwrap();
			let mut options = GcOptions::new(GcAction::DeleteSpecific);
			options.paths = Some(BTreeSet::from([path.clone()]));
			options.ignore_liveness = true;
			store.collect_garbage(&options, &mut |_| {}).unwrap();
		}
	};
	assert_eq!(add("lower-only"), lower_only);
	assert!(!upper.join(lower_only.to_string()).exists());
	// Validity checks register lower-only paths for verification. An
	// unqueried lower-only path remains outside the verification set.
	let unchecked_real = lower_store.real_path(&unchecked);
	fs::set_permissions(&unchecked_real, fs::Permissions::from_mode(0o644)).unwrap();
	fs::write(&unchecked_real, "corrupted").unwrap();
	match engine {
		Engine::Reference => {
			succeeded(reference(root, "nix-store").args([
				"--store",
				&uri,
				"--verify",
				"--check-contents",
			]));
		}
		Engine::Noxi => {
			let store = local_overlay::open(&uri, &settings()).unwrap().unwrap();
			assert_eq!(
				store.query_all_valid_paths().unwrap(),
				BTreeSet::from([lower_only.clone()])
			);
			assert!(!store.verify_store(true, None, &mut |_, _| {}).unwrap());
		}
	}

	delete(&lower_only);
	assert_eq!(
		fs::read(lower_store.real_path(&lower_only)).unwrap(),
		b"lower-only"
	);
	assert_eq!(
		fs::read(merged.join(lower_only.to_string())).unwrap(),
		b"lower-only"
	);

	for name in ["duplicate-delete", "duplicate-optimise"] {
		let duplicate = add(name);
		assert_eq!(seed_lower(name), duplicate);
		let upper_path = upper.join(duplicate.to_string());
		assert!(upper_path.exists());
		let marker = root.join("remounted");
		if name == "duplicate-delete" {
			delete(&duplicate);
		} else {
			match engine {
				Engine::Reference => {
					succeeded(reference(root, "nix-store").args(["--store", &uri, "--optimise"]));
				}
				Engine::Noxi => {
					local_overlay::open(&uri, &settings())
						.unwrap()
						.unwrap()
						.optimise()
						.unwrap();
				}
			}
		}
		assert!(!upper_path.exists());
		assert_eq!(
			fs::read_to_string(&marker).unwrap(),
			merged.to_str().unwrap()
		);
		fs::remove_file(marker).unwrap();
		assert_eq!(
			fs::read(lower_store.real_path(&duplicate)).unwrap(),
			name.as_bytes()
		);
	}

	let lower_real = lower_store.real_path(&lower_only);
	assert_eq!(add("lower-only"), lower_only);
	fs::set_permissions(&lower_real, fs::Permissions::from_mode(0o644)).unwrap();
	fs::write(lower_real, "corrupted after registration").unwrap();
	let modified = format!("path '/nix/store/{lower_only}' was modified! expected hash");
	match engine {
		Engine::Reference => {
			let result = reference(root, "nix-store")
				.args(["--store", &uri, "--verify", "--check-contents"])
				.output()
				.unwrap();
			assert_eq!(result.status.code(), Some(1));
			assert!(String::from_utf8_lossy(&result.stderr).contains(&modified));
		}
		Engine::Noxi => {
			let store = local_overlay::open(&uri, &settings()).unwrap().unwrap();
			let mut messages = Vec::new();
			assert!(
				store
					.verify_store(true, None, &mut |_, message| messages
						.push(message.to_owned()))
					.unwrap()
			);
			assert!(messages.iter().any(|message| message.contains(&modified)));
		}
	}

	for self_ref in [false, true] {
		let suffix = if self_ref { "self-ref" } else { "non-self-ref" };
		let leaf = seed_lower(&format!("leaf-{suffix}"));
		let parent = seed_lower(&format!("parent-{suffix}"));
		let mut leaf_info = lower_store.query_path_info(&leaf).unwrap().unwrap();
		let mut parent_info = lower_store.query_path_info(&parent).unwrap().unwrap();
		leaf_info.ca = None;
		parent_info.ca = None;
		parent_info.references.insert(leaf.clone());
		if self_ref {
			leaf_info.references.insert(leaf.clone());
			parent_info.references.insert(parent.clone());
		}
		let registration = |info: &noxi_store::ValidPathInfo| {
			format!(
				"/nix/store/{}\n{}\n{}\n\n{}\n{}",
				info.path,
				info.nar_hash_base16(),
				info.nar_size,
				info.references.len(),
				info.references
					.iter()
					.map(|path| format!("/nix/store/{path}\n"))
					.collect::<String>(),
			)
		};
		let load = |store: &str, data: String| {
			let input = root.join("registration");
			fs::write(&input, data).unwrap();
			succeeded(
				reference(root, "nix-store")
					.args(["--store", store, "--load-db"])
					.stdin(fs::File::open(input).unwrap()),
			);
		};
		load(
			lower.to_str().unwrap(),
			registration(&leaf_info) + &registration(&parent_info),
		);
		let upper_db = LocalStore::open(LocalStoreConfig::with_root(&root.join("merged"))).unwrap();
		assert!(!upper_db.is_valid_path(&leaf).unwrap());
		assert!(!upper_db.is_valid_path(&parent).unwrap());
		match engine {
			Engine::Reference => load(&uri, registration(&parent_info)),
			Engine::Noxi => {
				local_overlay::open(&uri, &settings())
					.unwrap()
					.unwrap()
					.register_valid_paths(std::slice::from_ref(&parent_info))
					.unwrap();
			}
		}
		for expected in [leaf_info, parent_info] {
			let actual = upper_db.query_path_info(&expected.path).unwrap().unwrap();
			assert_eq!(actual.references, expected.references);
			assert_eq!(actual.nar_hash, expected.nar_hash);
			assert!(!upper.join(expected.path.to_string()).exists());
		}
	}

	succeeded(Command::new("umount").arg(merged));
}
