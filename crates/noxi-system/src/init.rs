//! `noxi system init`: the native `nixos-generate-config`.
//!
//! The hardware detection is a port of nixpkgs'
//! `nixos/modules/installer/tools/nixos-generate-config.pl` at the revision
//! recorded in PROVENANCE.md, probe for probe and in the same order, so the
//! generated `hardware-configuration.nix` matches upstream's for the same
//! machine:
//!
//! - `nixpkgs.hostPlatform` from the platform noxi was built for;
//! - `kvm-intel`/`kvm-amd` from the `vmx`/`svm` flags in `/proc/cpuinfo`;
//! - initrd modules for mass-storage, FireWire and USB controllers in
//!   `/sys/bus/pci/devices`, USB storage and keyboards in
//!   `/sys/bus/usb/devices`, and every block and MMC host device in
//!   `/sys/class/{block,mmc_host}`, read from their `driver/module` links;
//!   Broadcom FullMac, virtio-scsi, Intel 2200BG/3945ABG firmware and Intel
//!   NPU devices by PCI id; `bcache` for `/dev/bcache*`; `dm-snapshot`
//!   when `lsblk -o TYPE` lists LVM;
//! - guest support from `systemd-detect-virt` (VirtualBox, Parallels,
//!   QEMU/KVM/Bochs, Hyper-V, systemd-nspawn), and on bare metal
//!   `not-detected.nix` and AMD/Intel microcode updates by CPU vendor;
//! - `swapDevices` from the partitions in `/proc/swaps`, skipping zram and
//!   swap files;
//! - `fileSystems` from `/proc/self/mountinfo` under the root: bind mounts
//!   of earlier file systems (unless they are btrfs subvolumes), loop
//!   devices with their backing file, btrfs `subvol=`, vfat `fmask`/`dmask`,
//!   Stratis pools, bcachefs UUIDs, LUKS devices with one slave as
//!   `boot.initrd.luks.devices`, and `boot.swraid.enable` for md devices;
//!   devices by their `/dev/stratis`, `/dev/mapper`, `/dev/disk/by-uuid` or
//!   `/dev/disk/by-label` name.
//!
//! The `/proc`, `/sys`, `/dev` and `/boot` trees are read below
//! [`Host::root`] and the external programs run through [`Tools`], so
//! tests run the detection against fixture trees.
//!
//! The files are written as upstream writes them: `hardware-configuration.nix`
//! is always regenerated, while an existing `configuration.nix` or
//! `flake.nix` is kept with a warning unless `--force` is given. The
//! templates are nixpkgs' defaults for `system.nixos-generate-config`
//! (`tools.nix`), with these differences: the header names
//! `noxi system init`; the X server line is always the commented default,
//! because the running system's `services.xserver.enable` is not known
//! outside its evaluation; and with `--flake` the `nixpkgs` input is pinned
//! to the running system's Nixpkgs revision when `noxi system version`
//! knows it, instead of following the unstable channel.
//!
//! On a noxios system (`ID=noxios` in os-release), `--flake` writes a flake
//! with the noxios live image's inputs whose configuration imports the
//! noxios modules, so the installed system keeps noxi as its package
//! manager, and copies the running system's `/etc/nixos/flake.lock` next to
//! it when that lock pins those inputs, so the flake evaluates from the
//! same sources, offline from the image's store.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use noxi_events::Styling;

pub const USAGE: &str = "usage: noxi system init [--root ROOT] [--dir DIR] [--flake] [--force] [--no-filesystems] [--show-hardware-config] [--kernel lts|latest]";

/// The defaults file upstream reads before the command line.
pub const CONFIG_FILE: &str = "/etc/nixos-generate-config.conf";

/// Where the probed `/proc`, `/sys`, `/dev` and `/boot` trees are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
	/// `/` on a real machine; a fixture tree in tests.
	pub root: PathBuf,
}

impl Default for Host {
	fn default() -> Self {
		Host {
			root: PathBuf::from("/"),
		}
	}
}

impl Host {
	/// The host path of the absolute path `path`.
	pub fn path(&self, path: impl AsRef<Path>) -> PathBuf {
		let path = path.as_ref();
		self.root.join(path.strip_prefix("/").unwrap_or(path))
	}

	/// `path` without the host root, as an absolute path string.
	fn unhost(&self, path: &Path) -> String {
		match path.strip_prefix(&self.root) {
			Ok(rest) => format!("/{}", rest.display()),
			Err(_) => path.display().to_string(),
		}
	}

	fn read(&self, path: impl AsRef<Path>) -> Option<String> {
		fs::read_to_string(self.path(path)).ok()
	}

	/// The entries matching `DIR/*`, sorted as Perl's `glob` sorts them,
	/// as host paths. Hidden entries are skipped.
	fn glob(&self, dir: impl AsRef<Path>) -> Vec<PathBuf> {
		let Ok(entries) = fs::read_dir(self.path(dir)) else {
			return Vec::new();
		};
		let mut paths: Vec<PathBuf> = entries
			.filter_map(Result::ok)
			.filter(|entry| !entry.file_name().as_encoded_bytes().starts_with(b"."))
			.map(|entry| entry.path())
			.collect();
		paths.sort();
		paths
	}
}

/// The external programs the detection runs.
pub trait Tools {
	/// `systemd-detect-virt`'s output, such as `none`, `kvm` or `oracle`.
	fn detect_virt(&self) -> String;
	/// `btrfs subvol show PATH`: whether it succeeded, and its output.
	fn btrfs_subvol_show(&self, path: &Path) -> (bool, String);
	/// `bcachefs fs usage PATH`: whether it succeeded, and its output.
	fn bcachefs_fs_usage(&self, path: &Path) -> (bool, String);
	/// `lsblk -o TYPE`.
	fn lsblk_types(&self) -> String;
	/// `stratis pool list`.
	fn stratis_pool_list(&self) -> String;
}

/// [`Tools`] running the programs from `PATH`. A missing program reads
/// as empty output and failure, as a Perl backtick does.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemTools;

fn run_tool(program: &str, args: &[&dyn AsRef<std::ffi::OsStr>], stderr: bool) -> (bool, String) {
	let mut command = Command::new(program);
	for arg in args {
		command.arg(arg);
	}
	let output = command
		.stdin(Stdio::null())
		.stderr(if stderr {
			Stdio::piped()
		} else {
			Stdio::null()
		})
		.output();
	match output {
		Ok(output) => {
			let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
			text.push_str(&String::from_utf8_lossy(&output.stderr));
			(output.status.success(), text)
		}
		Err(_) => (false, String::new()),
	}
}

impl Tools for SystemTools {
	fn detect_virt(&self) -> String {
		run_tool("systemd-detect-virt", &[], false).1
	}

	fn btrfs_subvol_show(&self, path: &Path) -> (bool, String) {
		run_tool("btrfs", &[&"subvol", &"show", &path], true)
	}

	fn bcachefs_fs_usage(&self, path: &Path) -> (bool, String) {
		run_tool("bcachefs", &[&"fs", &"usage", &path], true)
	}

	fn lsblk_types(&self) -> String {
		run_tool("lsblk", &[&"-o", &"TYPE"], false).1
	}

	fn stratis_pool_list(&self) -> String {
		run_tool("stratis", &[&"pool", &"list"], false).1
	}
}

/// What the detection found, before rendering.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Detection {
	pub attrs: Vec<String>,
	pub imports: Vec<String>,
	pub kernel_modules: Vec<String>,
	pub initrd_kernel_modules: Vec<String>,
	pub initrd_available_kernel_modules: Vec<String>,
	pub module_packages: Vec<String>,
	/// The rendered `fileSystems` and LUKS entries.
	pub file_systems: String,
	/// The rendered `swapDevices` entries.
	pub swap_devices: Vec<String>,
	/// `systemd-detect-virt`'s answer.
	pub virt: String,
	pub warnings: Vec<String>,
}

/// The Nix system double noxi was built for, as `@hostPlatformSystem@`.
pub fn host_platform() -> String {
	format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
}

/// The text after the colon of each `/proc/cpuinfo` line named `key`.
fn cpuinfo_values<'a>(cpuinfo: &'a str, key: &'a str) -> impl Iterator<Item = &'a str> + 'a {
	cpuinfo.lines().filter_map(move |line| {
		let rest = line.strip_prefix(key)?;
		let rest = rest.trim_start_matches([' ', '\t']);
		rest.strip_prefix(':')
	})
}

/// `/^flags\s*:.* FEATURE( |$)/m`.
pub fn has_cpu_feature(cpuinfo: &str, feature: &str) -> bool {
	let needle = format!(" {feature}");
	cpuinfo_values(cpuinfo, "flags").any(|flags| {
		flags.match_indices(&needle).any(|(at, _)| {
			let after = &flags[at + needle.len()..];
			after.is_empty() || after.starts_with(' ')
		})
	})
}

/// `/^vendor_id\s*:.* ID$/m`.
pub fn cpu_manufacturer(cpuinfo: &str, id: &str) -> bool {
	let needle = format!(" {id}");
	cpuinfo_values(cpuinfo, "vendor_id").any(|vendor| vendor.ends_with(&needle))
}

/// The basename of the module a `driver/module` link points at.
fn driver_module(path: &Path) -> Option<String> {
	let link = path.join("driver/module");
	if !link.exists() {
		return None;
	}
	fs::canonicalize(&link)
		.ok()?
		.file_name()
		.map(|name| name.to_string_lossy().into_owned())
}

fn read_trim(path: &Path) -> String {
	fs::read_to_string(path)
		.map(|text| text.trim_end_matches('\n').to_owned())
		.unwrap_or_default()
}

/// Remove duplicates, keeping the first occurrence of each.
pub fn uniq(items: &[String]) -> Vec<String> {
	let mut seen = std::collections::HashSet::new();
	items
		.iter()
		.filter(|item| seen.insert(item.as_str()))
		.cloned()
		.collect()
}

/// `$d1 eq $d2 || substr($d1, 0, length($d2) + 1) eq "$d2/"`.
fn is_in(d1: &str, d2: &str) -> bool {
	d1 == d2
		|| d1
			.strip_prefix(d2)
			.is_some_and(|rest| rest.starts_with('/'))
}

fn unescape_mountinfo(field: &str) -> String {
	field.replace("\\040", " ").replace("\\011", "\t")
}

impl Detection {
	fn pci_check(&mut self, path: &Path) {
		let vendor = read_trim(&path.join("vendor"));
		let device = read_trim(&path.join("device"));
		let class = read_trim(&path.join("class"));
		let module = driver_module(path);
		if let Some(module) = module
			&& (class.starts_with("0x01")
				|| class.starts_with("0x0c00")
				|| class.starts_with("0x0c03"))
		{
			self.initrd_available_kernel_modules.push(module);
		}
		let device = device.as_str();
		if vendor == "0x14e4"
			&& [
				"0x43a3", "0x43df", "0x43ec", "0x43d3", "0x43d9", "0x43e9", "0x43ba", "0x43bb",
				"0x43bc", "0xaa52", "0x43ca", "0x43cb", "0x43cc", "0x43c3", "0x43c4", "0x43c5",
			]
			.contains(&device)
		{
			self.imports
				.push("(modulesPath + \"/hardware/network/broadcom-43xx.nix\")".to_owned());
		}
		if vendor == "0x1af4" && (device == "0x1004" || device == "0x1048") {
			self.initrd_available_kernel_modules
				.push("virtio_scsi".to_owned());
		}
		if vendor == "0x8086" {
			if ["0x1043", "0x104f", "0x4220", "0x4221", "0x4223", "0x4224"].contains(&device) {
				self.attrs
					.push("networking.enableIntel2200BGFirmware = true;".to_owned());
			}
			if ["0x4229", "0x4230", "0x4222", "0x4227"].contains(&device) {
				self.attrs
					.push("networking.enableIntel3945ABGFirmware = true;".to_owned());
			}
			if ["0x7d1d", "0xad1d", "0x643e", "0xb03e", "0xfd3e", "0xd71d"].contains(&device) {
				self.attrs
					.push("hardware.cpu.intel.npu.enable = true;".to_owned());
			}
		}
	}

	fn usb_check(&mut self, path: &Path) {
		let class = read_trim(&path.join("bInterfaceClass"));
		let protocol = read_trim(&path.join("bInterfaceProtocol"));
		if let Some(module) = driver_module(path)
			&& (class == "08" || (class == "03" && protocol == "01"))
		{
			self.initrd_available_kernel_modules.push(module);
		}
	}
}

/// `findStableDevPath`: a `/dev/stratis`, `/dev/mapper`, `/dev/disk/by-uuid`
/// or `/dev/disk/by-label` name for `dev`, or `dev` itself. Names match by
/// device number, or, for fixture trees without device nodes, by the file
/// they resolve to.
pub fn find_stable_dev_path(host: &Host, dev: &str) -> String {
	if !dev.starts_with('/') {
		return dev.to_owned();
	}
	let path = host.path(dev);
	let Ok(metadata) = fs::metadata(&path) else {
		return dev.to_owned();
	};
	let real = fs::canonicalize(&path).ok();
	let mut candidates = Vec::new();
	for pool in host.glob("/dev/stratis") {
		candidates.extend(host.glob(host.unhost(&pool)));
	}
	candidates.extend(host.glob("/dev/mapper"));
	candidates.extend(host.glob("/dev/disk/by-uuid"));
	candidates.extend(host.glob("/dev/disk/by-label"));
	for candidate in candidates {
		let Ok(other) = fs::metadata(&candidate) else {
			continue;
		};
		let same_device = metadata.rdev() != 0 && metadata.rdev() == other.rdev();
		let same_file = real.is_some() && fs::canonicalize(&candidate).ok() == real;
		if same_device || same_file {
			return host.unhost(&candidate);
		}
	}
	dev.to_owned()
}

/// The first line of `btrfs subvol show` when the mount is a subvolume
/// other than the top level, as upstream's
/// `m/^(?!\/\n).*Subvolume ID:[ \t\n]*([0-9]+)/s` decides it.
fn btrfs_subvolume(info: &str) -> Option<String> {
	let first = info.split('\n').next().unwrap_or("");
	if first == "/" {
		return None;
	}
	let at = info.find("Subvolume ID:")?;
	let rest = info[at + "Subvolume ID:".len()..].trim_start_matches([' ', '\t', '\n']);
	rest.starts_with(|c: char| c.is_ascii_digit())
		.then(|| first.to_owned())
}

/// Run the detection for the installation mounted at `root_dir` (empty
/// for `/`).
pub fn detect(host: &Host, tools: &dyn Tools, root_dir: &str) -> Result<Detection, String> {
	let mut d = Detection::default();
	d.attrs.push(format!(
		"nixpkgs.hostPlatform = lib.mkDefault \"{}\";",
		host_platform()
	));
	let cpuinfo = host.read("/proc/cpuinfo").unwrap_or_default();
	if has_cpu_feature(&cpuinfo, "vmx") {
		d.kernel_modules.push("kvm-intel".to_owned());
	}
	if has_cpu_feature(&cpuinfo, "svm") {
		d.kernel_modules.push("kvm-amd".to_owned());
	}
	for path in host.glob("/sys/bus/pci/devices") {
		d.pci_check(&path);
	}
	for path in host.glob("/sys/bus/usb/devices") {
		if path.join("bInterfaceClass").exists() {
			d.usb_check(&path);
		}
	}
	for path in host
		.glob("/sys/class/block")
		.into_iter()
		.chain(host.glob("/sys/class/mmc_host"))
	{
		if let Some(module) = driver_module(&path.join("device")) {
			d.initrd_available_kernel_modules.push(module);
		}
	}
	let bcache = host.glob("/dev").into_iter().any(|path| {
		let name = path.file_name().unwrap_or_default().to_string_lossy();
		name.starts_with("bcache") && !name.starts_with("bcachefs")
	});
	if bcache {
		d.initrd_available_kernel_modules.push("bcache".to_owned());
	}
	if tools.lsblk_types().contains("lvm") {
		d.initrd_kernel_modules.push("dm-snapshot".to_owned());
	}

	d.virt = tools.detect_virt().trim_end_matches('\n').to_owned();
	match d.virt.as_str() {
		"oracle" => d
			.attrs
			.push("virtualisation.virtualbox.guest.enable = true;".to_owned()),
		"parallels" => {
			d.attrs.push("hardware.parallels.enable = true;".to_owned());
			d.attrs
				.push("nixpkgs.config.allowUnfreePackages = [ \"prl-tools\" ];".to_owned());
		}
		"qemu" | "kvm" | "bochs" => d
			.imports
			.push("(modulesPath + \"/profiles/qemu-guest.nix\")".to_owned()),
		"microsoft" => d
			.attrs
			.push("virtualisation.hypervGuest.enable = true;".to_owned()),
		"systemd-nspawn" => d.attrs.push("boot.isNspawnContainer = true;".to_owned()),
		"none" => {
			d.imports
				.push("(modulesPath + \"/installer/scan/not-detected.nix\")".to_owned());
			if cpu_manufacturer(&cpuinfo, "AuthenticAMD") {
				d.attrs.push("hardware.cpu.amd.updateMicrocode = lib.mkDefault config.hardware.enableRedistributableFirmware;".to_owned());
			}
			if cpu_manufacturer(&cpuinfo, "GenuineIntel") {
				d.attrs.push("hardware.cpu.intel.updateMicrocode = lib.mkDefault config.hardware.enableRedistributableFirmware;".to_owned());
			}
		}
		_ => {}
	}

	detect_swaps(host, &mut d)?;
	detect_file_systems(host, tools, root_dir, &mut d)?;
	Ok(d)
}

fn detect_swaps(host: &Host, d: &mut Detection) -> Result<(), String> {
	let Some(swaps) = host.read("/proc/swaps") else {
		return Ok(());
	};
	for line in swaps.lines().skip(1) {
		let fields: Vec<&str> = line.split_whitespace().collect();
		let (Some(file), Some(kind)) = (fields.first(), fields.get(1)) else {
			continue;
		};
		if !host.path(file).exists() {
			continue;
		}
		let dev = find_stable_dev_path(host, file);
		if kind.contains("partition") {
			// zram devices are more likely created by configuration.nix.
			if file.starts_with("/dev/zram") {
				continue;
			}
			d.swap_devices.push(format!("{{ device = \"{dev}\"; }}"));
		} else if kind.contains("file") {
			// Swap files are more likely specified in configuration.nix.
		} else {
			return Err(format!("unsupported swap type: {kind}"));
		}
	}
	Ok(())
}

fn detect_file_systems(
	host: &Host,
	tools: &dyn Tools,
	root_dir: &str,
	d: &mut Detection,
) -> Result<(), String> {
	let mountinfo = host.read("/proc/self/mountinfo").unwrap_or_default();
	let mut by_dev: BTreeMap<String, String> = BTreeMap::new();
	let mut use_swraid = false;
	for line in mountinfo.lines() {
		let fields: Vec<&str> = line.split(' ').collect();
		if fields.len() < 7 {
			continue;
		}
		let mount_point = unescape_mountinfo(fields[4]);
		if !host.path(&mount_point).is_dir() {
			continue;
		}
		let mount_options: Vec<&str> = fields[5].split(',').collect();
		if !is_in(&mount_point, root_dir) {
			continue;
		}
		let mut mount_point = mount_point[root_dir.len()..].to_owned();
		if mount_point.is_empty() {
			mount_point = "/".to_owned();
		}
		if ["/proc", "/dev", "/sys", "/run"]
			.iter()
			.any(|special| is_in(&mount_point, special))
			|| mount_point == "/var/lib/nfs/rpc_pipefs"
		{
			continue;
		}
		let Some(separator) = fields.iter().skip(6).position(|field| *field == "-") else {
			continue;
		};
		let n = 6 + separator + 1;
		let fs_type = fields.get(n).copied().unwrap_or_default();
		let mut device = unescape_mountinfo(fields.get(n + 1).copied().unwrap_or_default());
		let super_options: Vec<&str> = fields
			.get(n + 2)
			.copied()
			.unwrap_or_default()
			.split(',')
			.collect();

		// The read-only bind mount on /nix/store.
		if mount_point == "/nix/store"
			&& super_options.contains(&"rw")
			&& mount_options.contains(&"ro")
		{
			continue;
		}

		let target = PathBuf::from(format!("{root_dir}{mount_point}"));
		if let Some(base) = by_dev.get(fields[2]) {
			// A bind mount of an earlier file system, unless it is a btrfs
			// subvolume.
			let (ok, message) = tools.btrfs_subvol_show(&target);
			if !ok || message.contains("ERROR:") {
				let path = if fields[3] == "/" { "" } else { fields[3] };
				let base = if base == "/" { "" } else { base.as_str() };
				d.file_systems.push_str(&format!(
					"  fileSystems.\"{mount_point}\" =\n    {{ device = \"{base}{path}\";\n      fsType = \"none\";\n      options = [ \"bind\" ];\n    }};\n\n"
				));
				continue;
			}
		}
		by_dev.insert(fields[2].to_owned(), mount_point.clone());

		if fs_type == "fuseblk" || fs_type == "fuse" {
			d.warnings.push(format!(
				"don't know how to emit ‘fileSystem’ option for FUSE filesystem ‘{mount_point}’"
			));
			continue;
		}

		let mut extra_options = Vec::new();
		if let Some(at) = device.find("/dev/loop") {
			let digits: String = device[at + "/dev/loop".len()..]
				.chars()
				.take_while(char::is_ascii_digit)
				.collect();
			if !digits.is_empty()
				&& let Some(backer) =
					host.read(format!("/sys/block/loop{digits}/loop/backing_file"))
			{
				device = backer.trim_end_matches('\n').to_owned();
				extra_options.push("loop".to_owned());
			}
		}

		if fs_type == "btrfs" {
			let (ok, info) = tools.btrfs_subvol_show(&target);
			if !ok || info.contains("ERROR:") {
				return Err(format!(
					"failed to retrieve subvolume info for {mount_point}"
				));
			}
			if let Some(subvolume) = btrfs_subvolume(&info) {
				extra_options.push(format!("subvol={subvolume}"));
			}
		}

		// Keep the umask of vfat file systems: the default mounts them
		// world-readable, a risk for the EFI system partition.
		if fs_type == "vfat" {
			extra_options.extend(
				super_options
					.iter()
					.filter(|option| option.contains("fmask") || option.contains("dmask"))
					.map(|option| (*option).to_owned()),
			);
		}

		let mut stable = find_stable_dev_path(host, &device);
		let mut stratis_pool = None;
		if let Some(rest) = stable.strip_prefix("/dev/stratis/")
			&& let Some((pool, _)) = rest.rsplit_once('/')
		{
			let list = tools.stratis_pool_list();
			let mut lines = list.split('\n');
			let header = lines.next().unwrap_or("");
			if let Some(uuid_index) = header.find("UUID")
				&& let Some(line) = lines.find(|line| line.starts_with(&format!("{pool} ")))
			{
				let start = uuid_index.saturating_sub(32);
				stratis_pool = line.get(start..start + 36).map(str::to_owned);
			}
		}

		// A tmpfs /tmp most likely comes from boot.tmp.useTmpfs.
		if mount_point == "/tmp" && fs_type == "tmpfs" {
			continue;
		}

		if fs_type == "bcachefs" {
			let (ok, info) = tools.bcachefs_fs_usage(&target);
			let uuid = info.lines().next().and_then(|line| {
				let rest = line
					.strip_prefix("Filesystem:")?
					.trim_start_matches([' ', '\t']);
				let uuid: String = rest
					.chars()
					.take_while(|c| c.is_ascii_digit() || c.is_ascii_lowercase() || *c == '-')
					.collect();
				(!uuid.is_empty()).then_some(uuid)
			});
			match uuid {
				Some(uuid) if ok => stable = format!("UUID={uuid}"),
				_ => d
					.warnings
					.push("can't find bcachefs mount UUID falling back to device-path".to_owned()),
			}
		}

		d.file_systems.push_str(&format!(
			"  fileSystems.\"{mount_point}\" =\n    {{ device = \"{stable}\";\n      fsType = \"{fs_type}\";\n"
		));
		if !extra_options.is_empty() {
			let options: Vec<String> = uniq(&extra_options)
				.iter()
				.map(|option| format!("\"{option}\""))
				.collect();
			d.file_systems
				.push_str(&format!("      options = [ {} ];\n", options.join(" ")));
		}
		if let Some(pool) = stratis_pool {
			d.file_systems
				.push_str(&format!("      stratis.poolUuid = \"{pool}\";\n"));
		}
		d.file_systems.push_str("    };\n\n");

		// A file system on a LUKS device needs its boot.initrd.luks entry.
		let device_path = host.path(&device);
		if device.starts_with('/') && device_path.exists() {
			let name = fs::canonicalize(&device_path)
				.ok()
				.and_then(|path| path.file_name().map(|n| n.to_string_lossy().into_owned()))
				.unwrap_or_default();
			let block = format!("/sys/class/block/{name}");
			let dm_uuid = host.read(format!("{block}/dm/uuid")).unwrap_or_default();
			if dm_uuid.starts_with("CRYPT-LUKS") {
				let slaves = host.glob(format!("{block}/slaves"));
				if let [slave] = slaves.as_slice() {
					let slave = format!(
						"/dev/{}",
						slave.file_name().unwrap_or_default().to_string_lossy()
					);
					if host.path(&slave).exists() {
						let dm_name = host
							.read(format!("{block}/dm/name"))
							.unwrap_or_default()
							.trim_end_matches('\n')
							.to_owned();
						let luks = format!("  boot.initrd.luks.devices.\"{dm_name}\".device");
						if !d.file_systems.lines().any(|line| line.starts_with(&luks)) {
							d.file_systems.push_str(&format!(
								"{luks} = \"{}\";\n\n",
								find_stable_dev_path(host, &slave)
							));
						}
					}
				}
			}
			if host.path(format!("{block}/md/uuid")).exists() {
				use_swraid = true;
			}
		}
	}
	if use_swraid {
		d.attrs.push("boot.swraid.enable = true;\n\n".to_owned());
	}
	Ok(())
}

fn nix_string_list(items: &[String]) -> String {
	uniq(items)
		.iter()
		.map(|item| format!(" \"{item}\""))
		.collect()
}

/// `multiLineList`.
fn multi_line_list(indent: &str, items: &[String]) -> String {
	if items.is_empty() {
		return " [ ]".to_owned();
	}
	let mut out = format!("\n{indent}[ ");
	for (i, item) in items.iter().enumerate() {
		if i > 0 {
			out.push_str(&format!("{indent}  "));
		}
		out.push_str(item);
		out.push('\n');
	}
	out.push_str(&format!("{indent}]"));
	out
}

/// The text of `hardware-configuration.nix`.
pub fn hardware_configuration(d: &Detection, no_filesystems: bool) -> String {
	let fs_and_swap = if no_filesystems {
		String::new()
	} else {
		format!(
			"\n{}  swapDevices ={};\n",
			d.file_systems,
			multi_line_list("    ", &d.swap_devices)
		)
	};
	let attrs: String = uniq(&d.attrs)
		.iter()
		.map(|attr| format!("  {attr}\n"))
		.collect();
	format!(
		"# Do not modify this file!  It was generated by ‘noxi system init’\n# and may be overwritten by future invocations.  Please make changes\n# to /etc/nixos/configuration.nix instead.\n{{ config, lib, pkgs, modulesPath, ... }}:\n\n{{\n  imports ={};\n\n  boot.initrd.availableKernelModules = [{} ];\n  boot.initrd.kernelModules = [{} ];\n  boot.kernelModules = [{} ];\n  boot.extraModulePackages = [{} ];\n{fs_and_swap}\n{attrs}}}\n",
		multi_line_list("    ", &d.imports),
		nix_string_list(&d.initrd_available_kernel_modules),
		nix_string_list(&d.initrd_kernel_modules),
		nix_string_list(&d.kernel_modules),
		uniq(&d.module_packages)
			.iter()
			.map(|item| format!(" {item}"))
			.collect::<String>(),
	)
}

/// The boot loader block of `configuration.nix`: systemd-boot with EFI
/// variables, extlinux when `/boot/extlinux` exists, GRUB otherwise, and
/// nothing in a systemd-nspawn container.
pub fn boot_loader_config(host: &Host, virt: &str, kernel: Kernel) -> String {
	let mut config = if host.path("/sys/firmware/efi/efivars").exists() {
		"  # Use the systemd-boot EFI boot loader.\n  boot.loader.systemd-boot.enable = true;\n  boot.loader.efi.canTouchEfiVariables = true;\n".to_owned()
	} else if host.path("/boot/extlinux").exists() {
		"  # Use the extlinux boot loader. (NixOS wants to enable GRUB by default)\n  boot.loader.grub.enable = false;\n  # Enables the generation of /boot/extlinux/extlinux.conf\n  boot.loader.generic-extlinux-compatible.enable = true;\n".to_owned()
	} else if virt != "systemd-nspawn" {
		"  # Use the GRUB 2 boot loader.\n  boot.loader.grub.enable = true;\n  # boot.loader.grub.efiSupport = true;\n  # boot.loader.grub.efiInstallAsRemovable = true;\n  # boot.loader.efi.efiSysMountPoint = \"/boot/efi\";\n  # Define on which hard drive you want to install Grub.\n  # boot.loader.grub.device = \"/dev/sda\"; # or \"nodev\" for efi only\n".to_owned()
	} else {
		String::new()
	};
	if kernel == Kernel::Latest {
		config.push_str(
			"\n  # Use latest kernel.\n  boot.kernelPackages = pkgs.linuxPackages_latest;\n",
		);
	}
	config
}

/// The templates are kept with tab indentation; the generated files use
/// upstream's two spaces per level.
fn spaces(template: &str) -> String {
	template
		.lines()
		.map(|line| {
			let tabs = line.len() - line.trim_start_matches('\t').len();
			format!("{}{}\n", "  ".repeat(tabs), &line[tabs..])
		})
		.collect()
}

/// nixpkgs' default `system.nixos-generate-config.configuration`.
const CONFIGURATION_TEMPLATE: &str = include_str!("init/configuration.nix.in");
/// nixpkgs' default `system.nixos-generate-config.flake`.
const FLAKE_TEMPLATE: &str = include_str!("init/flake.nix.in");

/// The text of `configuration.nix` for NixOS release `release`.
pub fn configuration(boot_loader: &str, release: &str) -> String {
	let xserver = "  # Enable the X11 windowing system.\n  # services.xserver.enable = true;\n";
	// The empty `desktopConfiguration` list, indented as upstream does.
	let desktop = "  \n";
	let text = spaces(CONFIGURATION_TEMPLATE)
		.replace("$bootLoaderConfig", boot_loader)
		.replace("$xserverConfig", xserver)
		.replace("$desktopConfiguration", desktop)
		.replace("@release@", release);
	format!("{text}\n")
}

/// The `flake.nix` written on a noxios system: the live image's inputs and
/// the noxios modules, so the installed system keeps noxi as its package
/// manager.
const NOXIOS_FLAKE_TEMPLATE: &str = include_str!("init/noxios-flake.nix.in");

/// Where a noxios system keeps its flake; its `flake.lock` pins the inputs
/// [`noxios_flake`] declares.
pub const NOXIOS_FLAKE_LOCK: &str = "/etc/nixos/flake.lock";

/// The text of `flake.nix` on a noxios system, for the configuration named
/// `hostname`.
pub fn noxios_flake(hostname: &str) -> String {
	let text = spaces(NOXIOS_FLAKE_TEMPLATE).replace("@hostname@", hostname);
	format!("{text}\n")
}

/// The text of `flake.nix` for the configuration named `hostname`, with
/// Nixpkgs pinned to `revision` when it is known.
pub fn flake(hostname: &str, revision: Option<&str>) -> String {
	let inputs = match revision {
		Some(revision) => format!(
			"    # Pinned to the Nixpkgs revision of the system that ran `noxi system init`.\n    # To upgrade, change the revision, or follow a release channel such as\n    # \"https://channels.nixos.org/nixos-unstable/nixexprs.tar.zst\", then run\n    # `noxi flake update`.\n    nixpkgs.url = \"github:NixOS/nixpkgs/{revision}\";\n"
		),
		None => "    # This is pointing to an unstable release.\n    # If you prefer a stable release instead, you can change the word unstable to the latest number shown here: https://nixos.org/download\n    # i.e. nixos-24.11\n    # Use `nix flake update` to update the flake to the latest revision of the chosen release channel.\n    nixpkgs.url = \"https://channels.nixos.org/nixos-unstable/nixexprs.tar.zst\";\n".to_owned(),
	};
	let text = spaces(FLAKE_TEMPLATE)
		.replace("@inputs@\n", &inputs)
		.replace("@hostname@", hostname);
	format!("{text}\n")
}

/// `--kernel`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Kernel {
	#[default]
	Lts,
	Latest,
}

/// A parsed `noxi system init` command line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Options {
	/// The output directory; `/etc/nixos` is placed under the root.
	pub dir: String,
	/// The root of the installation, without trailing slashes; empty for
	/// `/`.
	pub root: String,
	pub force: bool,
	pub no_filesystems: bool,
	pub flake: bool,
	pub show_hardware_config: bool,
	pub kernel: Kernel,
}

impl Default for Options {
	fn default() -> Self {
		Options {
			dir: "/etc/nixos".to_owned(),
			root: String::new(),
			force: false,
			no_filesystems: false,
			flake: false,
			show_hardware_config: false,
			kernel: Kernel::Lts,
		}
	}
}

fn absolute(path: &str) -> String {
	let trimmed = path.trim_end_matches('/');
	let trimmed = if trimmed.is_empty() { "/" } else { trimmed };
	let cwd = std::env::current_dir().unwrap_or_default();
	noxi_commands::instantiate::absolute_path(&cwd, Path::new(trimmed))
		.to_string_lossy()
		.into_owned()
}

fn parse_kernel(value: &str) -> Result<Kernel, String> {
	match value {
		"lts" => Ok(Kernel::Lts),
		"latest" => Ok(Kernel::Latest),
		other => Err(format!("invalid kernel: '{other}'")),
	}
}

/// Apply the `[Defaults]` section of `/etc/nixos-generate-config.conf`.
pub fn apply_config_file(options: &mut Options, text: &str) -> Result<(), String> {
	let mut section = String::new();
	for line in text.lines() {
		let line = line.trim();
		if line.is_empty() || line.starts_with(['#', ';']) {
			continue;
		}
		if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
			section = name.trim().to_owned();
			continue;
		}
		if section != "Defaults" {
			continue;
		}
		let Some((key, value)) = line.split_once('=') else {
			continue;
		};
		let value = value.trim();
		match key.trim() {
			"Directory" => options.dir = value.to_owned(),
			"RootDirectory" => options.root = absolute(value).trim_end_matches('/').to_owned(),
			"Kernel" => options.kernel = parse_kernel(value)?,
			// Perl truth: empty and "0" are false.
			"Flake" => options.flake = !(value.is_empty() || value == "0"),
			_ => {}
		}
	}
	Ok(())
}

/// Parse the arguments after `noxi system init` over `options`; `None` for
/// `--help`.
pub fn parse(args: Vec<String>, mut options: Options) -> Result<Option<Options>, String> {
	let mut args = args.into_iter();
	let value = |flag: &str, args: &mut dyn Iterator<Item = String>| {
		args.next()
			.ok_or_else(|| format!("'{flag}' requires an argument"))
	};
	while let Some(arg) = args.next() {
		match arg.as_str() {
			"-h" | "--help" => return Ok(None),
			"--dir" => options.dir = value(&arg, &mut args)?,
			"--root" => {
				let root = value(&arg, &mut args)?;
				if root == "/" {
					return Err(
						"no need to specify '/' with '--root', it is the default".to_owned()
					);
				}
				options.root = absolute(&root).trim_end_matches('/').to_owned();
			}
			"--force" => options.force = true,
			"--no-filesystems" => options.no_filesystems = true,
			"--show-hardware-config" => options.show_hardware_config = true,
			"--flake" => options.flake = true,
			"--kernel" => options.kernel = parse_kernel(&value(&arg, &mut args)?)?,
			other => return Err(format!("unrecognized argument '{other}'")),
		}
	}
	Ok(Some(options))
}

/// What the generated files depend on besides the hardware.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Distribution {
	/// `system.stateVersion`: the running release, such as `26.11`.
	pub release: String,
	/// The default host name, which names the flake's configuration.
	pub hostname: String,
	/// The running system's Nixpkgs revision, pinned by `--flake`.
	pub revision: Option<String>,
	/// Whether the running system is noxios (`ID=noxios`), whose `--flake`
	/// output imports the noxios modules instead of the stock template.
	pub noxios: bool,
}

impl Distribution {
	/// Read from the running system as `noxi system version` does.
	pub fn running(host: &Host) -> Distribution {
		let os_release = host
			.read(crate::version::OS_RELEASE)
			.map(|text| crate::version::parse_os_release(&text))
			.unwrap_or_default();
		let info = crate::version::read(&crate::version::Sources {
			system: host.path(crate::snapshot::CURRENT_SYSTEM),
			os_release: host.path(crate::version::OS_RELEASE),
		})
		.ok();
		let release = os_release
			.get("VERSION_ID")
			.cloned()
			.or_else(|| {
				let version = info.as_ref()?.version.clone()?;
				let mut parts = version.split('.');
				Some(format!("{}.{}", parts.next()?, parts.next()?))
			})
			.unwrap_or_else(|| "26.11".to_owned());
		Distribution {
			release,
			hostname: os_release
				.get("DEFAULT_HOSTNAME")
				.cloned()
				.unwrap_or_else(|| "nixos".to_owned()),
			revision: info.and_then(|info| info.revision),
			noxios: os_release.get("ID").map(String::as_str) == Some("noxios"),
		}
	}
}

/// The running noxios system's `flake.lock` when it locks every input of
/// [`noxios_flake`], so a generated flake evaluates from the same sources,
/// offline when they are in the store.
fn noxios_lock(host: &Host) -> Option<String> {
	let text = host.read(NOXIOS_FLAKE_LOCK)?;
	let lock: serde_json::Value = serde_json::from_str(&text).ok()?;
	let root = lock.get("root")?.as_str()?;
	let inputs = lock.get("nodes")?.get(root)?.get("inputs")?;
	["nixpkgs", "noxi", "rust-overlay"]
		.iter()
		.all(|name| inputs.get(name).is_some())
		.then_some(text)
}

fn write_file(path: &Path, text: &str) -> Result<(), String> {
	fs::write(path, text)
		.and_then(|()| fs::set_permissions(path, fs::Permissions::from_mode(0o644)))
		.map_err(|e| format!("cannot write \"{}\": {e}", path.display()))
}

fn create_dir(path: &Path) -> Result<(), String> {
	fs::create_dir_all(path).map_err(|e| format!("cannot create \"{}\": {e}", path.display()))
}

/// Detect the hardware and write (or print) the configuration.
pub fn run(
	options: &Options,
	host: &Host,
	tools: &dyn Tools,
	distribution: &Distribution,
	out: &mut dyn Write,
	err: &mut dyn Write,
	styling: &Styling,
) -> ExitCode {
	let warning = |err: &mut dyn Write, text: &str| {
		let _ = writeln!(
			err,
			"{}: {text}",
			styling.paint(noxi_events::style::WARNING, "warning")
		);
	};
	let detection = match detect(host, tools, &options.root) {
		Ok(detection) => detection,
		Err(message) => return crate::fail(err, styling, None, &message),
	};
	for text in &detection.warnings {
		warning(err, text);
	}
	let hardware = hardware_configuration(&detection, options.no_filesystems);
	if options.show_hardware_config {
		let _ = out.write_all(hardware.as_bytes());
		return ExitCode::SUCCESS;
	}
	let dir = if options.dir == "/etc/nixos" {
		PathBuf::from(format!("{}{}", options.root, options.dir))
	} else {
		PathBuf::from(absolute(&options.dir))
	};
	let result = (|| -> Result<(), String> {
		let file = dir.join("hardware-configuration.nix");
		let _ = writeln!(err, "writing {}...", file.display());
		create_dir(&dir)?;
		write_file(&file, &hardware)?;

		let file = dir.join("flake.nix");
		if options.flake {
			if options.force || !file.exists() {
				let _ = writeln!(err, "writing {}...", file.display());
				if distribution.noxios {
					write_file(&file, &noxios_flake(&distribution.hostname))?;
					let lock = dir.join("flake.lock");
					let source = host.path(NOXIOS_FLAKE_LOCK);
					// `noxi system init` run in /etc/nixos itself keeps its lock.
					let same = matches!(
						(fs::canonicalize(&source), fs::canonicalize(&lock)),
						(Ok(a), Ok(b)) if a == b
					);
					if !same && (options.force || !lock.exists()) {
						if let Some(text) = noxios_lock(host) {
							let _ = writeln!(err, "writing {}...", lock.display());
							write_file(&lock, &text)?;
						}
					}
				} else {
					write_file(
						&file,
						&flake(&distribution.hostname, distribution.revision.as_deref()),
					)?;
				}
			} else {
				warning(err, &format!("not overwriting existing {}", file.display()));
			}
		}

		let file = dir.join("configuration.nix");
		if options.force || !file.exists() {
			let _ = writeln!(err, "writing {}...", file.display());
			let boot_loader = boot_loader_config(host, &detection.virt, options.kernel);
			write_file(&file, &configuration(&boot_loader, &distribution.release))?;
			let _ = writeln!(
				err,
				"For more hardware-specific settings, see https://github.com/NixOS/nixos-hardware."
			);
		} else {
			warning(err, &format!("not overwriting existing {}", file.display()));
		}
		Ok(())
	})();
	match result {
		Ok(()) => ExitCode::SUCCESS,
		Err(message) => crate::fail(err, styling, None, &message),
	}
}

pub fn main(args: Vec<String>, out: &mut dyn Write, err: &mut dyn Write) -> ExitCode {
	let host = Host::default();
	let mut defaults = Options::default();
	if let Some(text) = host.read(CONFIG_FILE)
		&& let Err(message) = apply_config_file(&mut defaults, &text)
	{
		let _ = writeln!(err, "error: {CONFIG_FILE}: {message}");
		return ExitCode::from(2);
	}
	match parse(args, defaults) {
		Ok(Some(options)) => {
			// SAFETY: `umask` has no preconditions.
			unsafe { libc::umask(0o022) };
			let distribution = Distribution::running(&host);
			run(
				&options,
				&host,
				&SystemTools,
				&distribution,
				out,
				err,
				&Styling::for_stderr(),
			)
		}
		Ok(None) => {
			let _ = writeln!(out, "{USAGE}");
			ExitCode::SUCCESS
		}
		Err(message) => {
			let _ = writeln!(err, "error: {message}\n{USAGE}");
			ExitCode::from(2)
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn cpuinfo_probes() {
		let cpuinfo = "vendor_id\t: GenuineIntel\nflags\t\t: fpu vme vmx sse\n";
		assert!(has_cpu_feature(cpuinfo, "vmx"));
		assert!(has_cpu_feature(cpuinfo, "sse"));
		assert!(!has_cpu_feature(cpuinfo, "svm"));
		assert!(!has_cpu_feature(cpuinfo, "vm"));
		assert!(cpu_manufacturer(cpuinfo, "GenuineIntel"));
		assert!(!cpu_manufacturer(cpuinfo, "AuthenticAMD"));
	}

	#[test]
	fn btrfs_subvolume_lines() {
		assert_eq!(
			btrfs_subvolume("@home\n\tName: \t\t\t@home\n\tSubvolume ID: \t\t257\n"),
			Some("@home".to_owned())
		);
		assert_eq!(
			btrfs_subvolume("/\n\tName: \t\t\t<FS_TREE>\n\tSubvolume ID: \t\t5\n"),
			None
		);
		assert_eq!(btrfs_subvolume("ERROR: not a subvolume\n"), None);
	}

	#[test]
	fn lists_render_like_upstream() {
		assert_eq!(multi_line_list("    ", &[]), " [ ]");
		assert_eq!(
			multi_line_list("    ", &["a".to_owned(), "b".to_owned()]),
			"\n    [ a\n      b\n    ]"
		);
		assert_eq!(
			nix_string_list(&["x".to_owned(), "y".to_owned(), "x".to_owned()]),
			" \"x\" \"y\""
		);
	}

	#[test]
	fn command_line_and_config_file() {
		let mut defaults = Options::default();
		apply_config_file(
			&mut defaults,
			"[Defaults]\nDirectory = /tmp/out\nKernel=latest\nFlake=1\n[Other]\nKernel=bogus\n",
		)
		.unwrap();
		assert_eq!(defaults.dir, "/tmp/out");
		assert_eq!(defaults.kernel, Kernel::Latest);
		assert!(defaults.flake);
		let options = parse(
			vec!["--root".into(), "/mnt/".into(), "--force".into()],
			Options::default(),
		)
		.unwrap()
		.unwrap();
		assert_eq!(options.root, "/mnt");
		assert!(options.force);
		assert!(parse(vec!["--root".into(), "/".into()], Options::default()).is_err());
		assert!(parse(vec!["--kernel".into(), "rt".into()], Options::default()).is_err());
		assert!(parse(vec!["--bogus".into()], Options::default()).is_err());
	}
}
