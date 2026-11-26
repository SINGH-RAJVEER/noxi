//! Opening the machine a build hook builds on: `Machine::openStore` and the
//! store types it can name.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use nix_compat::nixcpp::machines::Machine;
use nix_compat::nixcpp::settings::Settings;
use nix_compat::nixcpp::store_reference::{StoreReference, Variant};
use nix_compat::nixcpp::url;
use noxi_build::backend::LocalBackend;
use noxi_build::settings::BuildSettings;
use noxi_daemon::client::DaemonStore;
use noxi_daemon::legacy_ssh::{BuildOptions, LegacySshStore};
use noxi_daemon::peer::Peer;
use noxi_daemon::ssh::{SshConfig, SshMaster};
use noxi_store::LocalStoreConfig;

/// `Machine::completeStoreReference`.
pub fn complete_reference(machine: &Machine) -> StoreReference {
	let mut reference = machine.store_uri.clone();
	let scheme = match &reference.variant {
		Variant::Specified { scheme, .. } => scheme.clone(),
		_ => String::new(),
	};
	let params = &mut reference.params;
	if scheme == "ssh" {
		params.insert("max-connections".to_owned(), "1".to_owned());
		params.insert("log-fd".to_owned(), "4".to_owned());
	}
	if scheme == "ssh" || scheme == "ssh-ng" {
		if let Some(key) = &machine.ssh_key {
			params.insert("ssh-key".to_owned(), key.to_string_lossy().into_owned());
		}
		if !machine.ssh_public_host_key.is_empty() {
			params.insert(
				"base64-ssh-public-host-key".to_owned(),
				machine.ssh_public_host_key.clone(),
			);
		}
	}
	let features = params.entry("system-features".to_owned()).or_default();
	for feature in machine
		.supported_features
		.iter()
		.chain(&machine.mandatory_features)
	{
		if !features.is_empty() {
			features.push(' ');
		}
		features.push_str(feature);
	}
	reference
}

/// Where a store reference leads.
enum Kind {
	Local(Option<PathBuf>),
	Daemon(PathBuf, Option<PathBuf>),
	Ssh,
	SshNg,
}

fn daemon_socket() -> PathBuf {
	std::env::var_os("NIX_DAEMON_SOCKET_PATH")
		.filter(|path| !path.is_empty())
		.map(PathBuf::from)
		.unwrap_or_else(|| {
			LocalStoreConfig::system()
				.state_dir
				.join("daemon-socket/socket")
		})
}

/// Resolve `reference`, refusing parameters that are not understood.
fn kind(reference: &StoreReference) -> Result<Kind, String> {
	let scheme = match &reference.variant {
		Variant::Specified { scheme, .. } => scheme.as_str(),
		_ => "",
	};
	let known: &[&str] = match scheme {
		"ssh" | "ssh-ng" | "mounted-ssh-ng" => &[
			"base64-ssh-public-host-key",
			"compress",
			"log-fd",
			"max-connections",
			"remote-program",
			"remote-store",
			"ssh-key",
			"system-features",
		],
		_ => &["root", "system-features"],
	};
	if let Some(name) = reference
		.params
		.keys()
		.find(|name| !known.contains(&name.as_str()))
	{
		return Err(format!(
			"unsupported store parameter '{name}' in '{}'",
			reference.render(true)
		));
	}
	let root = reference
		.params
		.get("root")
		.filter(|root| !root.is_empty())
		.map(PathBuf::from);
	Ok(match &reference.variant {
		Variant::Auto if root.is_none() && !crate::store::resolves_to_local("auto")? => {
			Kind::Daemon(daemon_socket(), None)
		}
		Variant::Auto | Variant::Local => Kind::Local(root),
		Variant::Daemon => Kind::Daemon(daemon_socket(), None),
		Variant::Specified { scheme, authority } => match scheme.as_str() {
			"local" if authority.is_empty() => Kind::Local(root),
			"local" => Kind::Local(Some(PathBuf::from(
				url::percent_decode(authority).map_err(|e| e.to_string())?,
			))),
			"unix" if authority.is_empty() => Kind::Daemon(daemon_socket(), root),
			"unix" => Kind::Daemon(
				PathBuf::from(url::percent_decode(authority).map_err(|e| e.to_string())?),
				root,
			),
			"ssh" => Kind::Ssh,
			"ssh-ng" | "mounted-ssh-ng" => Kind::SshNg,
			scheme => {
				return Err(format!(
					"don't know how to open Nix store with scheme '{scheme}'"
				));
			}
		},
	})
}

/// The local store `reference` names, with its builder.
pub fn open_local(
	root: Option<&Path>,
	reference: &StoreReference,
	settings: &Settings,
) -> Result<LocalBackend, String> {
	let uri = match root {
		Some(root) => root.to_string_lossy().into_owned(),
		None => "local".to_owned(),
	};
	backend(&uri, reference, settings)
}

/// The local store `uri` names, opened as every command opens it, with its
/// builder and `reference`'s `system-features`.
pub fn backend(
	uri: &str,
	reference: &StoreReference,
	settings: &Settings,
) -> Result<LocalBackend, String> {
	let (store, _) = crate::store::open_local(uri, settings)?
		.ok_or_else(|| format!("store '{uri}' cannot build"))?;
	let mut build = BuildSettings::from_settings(settings);
	build.show_log_tail = !nix_compat::log::print_build_logs();
	build.store_uri = Some(reference.render(true));
	if let Some(features) = reference.params.get("system-features") {
		build.exact_system_features = true;
		build.system_features = features
			.split_ascii_whitespace()
			.map(str::to_owned)
			.collect::<BTreeSet<_>>();
	}
	let substituters = settings
		.get_list("substituters", &["https://cache.nixos.org/"])
		.into_iter()
		.map(str::to_owned)
		.collect();
	Ok(LocalBackend::new(store, build, substituters))
}

/// Open the store of `reference`.
pub fn open(reference: &StoreReference, settings: &Settings) -> Result<Box<dyn Peer>, String> {
	Ok(match kind(reference)? {
		Kind::Local(root) => Box::new(open_local(root.as_deref(), reference, settings)?),
		Kind::Daemon(socket, root) => Box::new(DaemonStore::new(
			&socket,
			root.as_deref(),
			&crate::store::client_options(settings),
		)),
		Kind::Ssh | Kind::SshNg => match ssh_store(reference, settings)? {
			SshStore::Legacy(store) => Box::new(store),
			SshStore::Ng(store) => Box::new(store),
		},
	})
}

/// An `ssh://` or `ssh-ng://` store.
pub enum SshStore {
	Legacy(LegacySshStore),
	Ng(DaemonStore),
}

/// Open the `ssh://` or `ssh-ng://` store `reference` names, for building on
/// it or as the store of a command.
pub fn ssh_store(reference: &StoreReference, settings: &Settings) -> Result<SshStore, String> {
	let Variant::Specified { scheme, authority } = &reference.variant else {
		return Err(format!("'{}' is not an SSH store", reference.render(true)));
	};
	let legacy = scheme == "ssh";
	let mounted = scheme == "mounted-ssh-ng";
	if mounted && !settings.experimental_features().contains("mounted-ssh-store") {
		return Err("experimental Nix feature 'mounted-ssh-store' is disabled; add '--extra-experimental-features mounted-ssh-store' to enable it".to_owned());
	}
	let program = if legacy { "nix-store" } else { "nix-daemon" };
	let master = SshConfig::new(authority, &reference.params, program)
		.and_then(SshMaster::new)
		.map_err(|e| e.to_string())?;
	Ok(if legacy {
		let int = |name| settings.get_int(name, 0).max(0) as u64;
		SshStore::Legacy(LegacySshStore::new(
			master,
			BuildOptions {
				max_silent_time: int("max-silent-time"),
				build_timeout: int("timeout"),
				max_log_size: int("max-build-log-size"),
				keep_failed: settings.get_bool("keep-failed", false),
			},
		))
	} else {
		let mut command = master.config().remote_command(false);
		if mounted {
			// `MountedSSHStore`'s `extraRemoteProgramArgs`.
			command.push("--process-ops".into());
		}
		let store = DaemonStore::over_ssh(master, command, reference.render(false));
		SshStore::Ng(if mounted { store.mounted() } else { store })
	})
}

/// The daemon that `reference` names.
pub fn daemon(reference: &StoreReference, settings: &Settings) -> Result<DaemonStore, String> {
	match kind(reference)? {
		Kind::Daemon(socket, root) => Ok(DaemonStore::new(
			&socket,
			root.as_deref(),
			&crate::store::client_options(settings),
		)),
		_ => Err(format!(
			"store '{}' cannot be served",
			reference.render(true)
		)),
	}
}

/// The local store that `reference` names, if it names one.
pub fn local(
	reference: &StoreReference,
	settings: &Settings,
) -> Result<Option<LocalBackend>, String> {
	match &reference.variant {
		Variant::Specified { scheme, .. } if scheme == "file" => Ok(None),
		_ => match kind(reference)? {
			Kind::Local(root) => open_local(root.as_deref(), reference, settings).map(Some),
			_ => Ok(None),
		},
	}
}
