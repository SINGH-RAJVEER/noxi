"""Plan workspace releases against crates.io; upload only with --publish."""

import argparse
import json
import os
import re
import subprocess
import time
import urllib.error
import urllib.request


def stable_version(version):
	if not re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", version):
		raise ValueError(f"Only stable major.minor.patch releases are supported: {version}")
	return tuple(map(int, version.split(".")))


def registry_versions(name):
	request = urllib.request.Request(
		f"https://crates.io/api/v1/crates/{name}",
		headers={"User-Agent": "noxi-release-ci (https://github.com/SINGH-RAJVEER/nix-rs)"},
	)
	for attempt in range(4):
		try:
			with urllib.request.urlopen(request, timeout=30) as response:
				return json.load(response)["versions"]
		except urllib.error.HTTPError as error:
			if error.code == 404:
				return []
			if error.code != 429 and error.code < 500:
				raise
			if attempt == 3:
				raise
		except urllib.error.URLError:
			if attempt == 3:
				raise
		time.sleep(2 ** attempt)


def release_plan(metadata, lookup=registry_versions):
	"""Validate the entire plan before Cargo can upload any package."""
	members = set(metadata["workspace_members"])
	plan = []
	for package in sorted(metadata["packages"], key=lambda package: package["name"]):
		if package["id"] not in members or package["publish"] == []:
			continue
		if package["publish"] is not None and "crates-io" not in package["publish"]:
			continue
		name, version = package["name"], package["version"]
		current = stable_version(version)
		versions = lookup(name)
		existing = next((item for item in versions if item["num"] == version), None)
		if existing:
			if existing["yanked"]:
				raise ValueError(f"{name}@{version} is yanked; bump its patch version")
			continue
		stable = [stable_version(item["num"]) for item in versions if re.fullmatch(r"[0-9]+\.[0-9]+\.[0-9]+", item["num"])]
		if stable:
			latest = max(stable)
			expected = (latest[0], latest[1], latest[2] + 1)
			if current != expected:
				raise ValueError(f"{name}@{version}: next release must be {'.'.join(map(str, expected))}")
		plan.append({"name": name, "version": version})
	return plan


def publish_command(plan):
	# Cargo stages selected workspace packages together, then publishes in
	# dependency order. This also supports the proc-macro dev-dependency cycles.
	command = ["cargo", "publish", "--registry", "crates-io", "--locked", "--all-features"]
	for package in plan:
		command.extend(["--package", f"{package['name']}@{package['version']}"])
	return command


def verify_release(package, lookup=registry_versions, sleep=time.sleep):
	for attempt in range(12):
		versions = lookup(package["name"])
		if any(item["num"] == package["version"] and not item["yanked"] for item in versions):
			return
		if attempt < 11:
			sleep(5)
	raise RuntimeError(f"Registry did not confirm {package['name']}@{package['version']}; rerun CI to reconcile")


def main():
	parser = argparse.ArgumentParser(description=__doc__)
	parser.add_argument("--publish", action="store_true", help="Upload missing versions; otherwise only print the plan")
	args = parser.parse_args()
	metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--no-deps", "--format-version", "1", "--locked"], text=True))
	plan = release_plan(metadata)
	print(json.dumps({"releases": plan}, indent="\t"), flush=True)
	if not args.publish or not plan:
		return
	if not os.environ.get("CARGO_REGISTRY_TOKEN"):
		raise RuntimeError("CARGO_REGISTRY_TOKEN must contain a crates.io token with publish access")
	# Cargo can time out after a successful upload. Check the registry even if
	# Cargo fails; a rerun skips uploaded versions and resumes the remainder.
	result = subprocess.run(publish_command(plan))
	for package in plan:
		verify_release(package)
	result.check_returncode()


if __name__ == "__main__":
	main()
