import importlib.util
import pathlib
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location("publish_crates", pathlib.Path(__file__).resolve().parents[2] / "scripts/publish-crates.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def package(name, version="0.0.2", publish=None):
	return {"name": name, "id": name, "version": version, "publish": publish}


def metadata(*packages):
	return {"packages": list(packages), "workspace_members": [package["id"] for package in packages]}


def versions(*numbers):
	return [{"num": number, "yanked": False} for number in numbers]


class ReleaseTests(unittest.TestCase):
	def test_plan_only_mode_never_uploads(self):
		with mock.patch("sys.argv", ["publish-crates.py"]), mock.patch.object(release.subprocess, "check_output", return_value="{}"), mock.patch.object(release, "release_plan", return_value=[{"name": "crate", "version": "0.0.2"}]), mock.patch.object(release.subprocess, "run") as upload, mock.patch("builtins.print"):
			release.main()
			upload.assert_not_called()

	def test_publish_requires_token_when_versions_are_missing(self):
		with mock.patch("sys.argv", ["publish-crates.py", "--publish"]), mock.patch.object(release.subprocess, "check_output", return_value="{}"), mock.patch.object(release, "release_plan", return_value=[{"name": "crate", "version": "0.0.2"}]), mock.patch.dict(release.os.environ, {}, clear=True), mock.patch.object(release.subprocess, "run") as upload, mock.patch("builtins.print"):
			with self.assertRaises(RuntimeError):
				release.main()
			upload.assert_not_called()

	def test_already_published_workspace_is_a_noop_without_token(self):
		with mock.patch("sys.argv", ["publish-crates.py", "--publish"]), mock.patch.object(release.subprocess, "check_output", return_value="{}"), mock.patch.object(release, "release_plan", return_value=[]), mock.patch.dict(release.os.environ, {}, clear=True), mock.patch.object(release.subprocess, "run") as upload, mock.patch("builtins.print"):
			release.main()
			upload.assert_not_called()

	def test_rerun_skips_uploaded_versions_and_private_crates(self):
		data = metadata(package("done"), package("pending"), package("private", publish=[]), package("other", publish=["other-registry"]))
		lookups = []
		def lookup(name):
			lookups.append(name)
			return versions("0.0.2" if name == "done" else "0.0.1")
		self.assertEqual(release.release_plan(data, lookup), [{"name": "pending", "version": "0.0.2"}])
		self.assertEqual(lookups, ["done", "pending"])

	def test_existing_crates_must_bump_exactly_one_patch(self):
		for version in ["0.0.1", "0.0.4", "0.1.0", "1.0.0", "0.0.3-beta.1"]:
			with self.subTest(version=version), self.assertRaises(ValueError):
				release.release_plan(metadata(package("crate", version)), lambda _: versions("0.0.2"))
		self.assertEqual(release.release_plan(metadata(package("crate", "0.0.3")), lambda _: versions("0.0.2"))[0]["version"], "0.0.3")

	def test_yanked_version_is_not_republished_or_reused(self):
		with self.assertRaises(ValueError):
			release.release_plan(metadata(package("crate")), lambda _: [{"num": "0.0.2", "yanked": True}])
		with self.assertRaises(ValueError):
			release.release_plan(metadata(package("crate", "0.0.4")), lambda _: [{"num": "0.0.4", "yanked": True}])

	def test_new_crate_and_non_workspace_package(self):
		data = metadata(package("new", "0.0.1"))
		data["packages"].append(package("external"))
		self.assertEqual(release.release_plan(data, lambda _: []), [{"name": "new", "version": "0.0.1"}])

	def test_registry_failure_aborts_plan(self):
		def unavailable(_):
			raise OSError("registry unavailable")
		with self.assertRaises(OSError):
			release.release_plan(metadata(package("crate")), unavailable)

	def test_cargo_stages_all_selected_crates_together(self):
		plan = [{"name": "macro", "version": "0.0.2"}, {"name": "library", "version": "0.0.3"}]
		command = release.publish_command(plan)
		self.assertEqual(command.count("--package"), 2)
		self.assertIn("macro@0.0.2", command)
		self.assertIn("library@0.0.3", command)
		self.assertIn("--locked", command)
		self.assertNotIn("--no-verify", command)
		self.assertNotIn("--allow-dirty", command)

	def test_registry_confirmation_waits_for_uploaded_version(self):
		responses = iter([versions("0.0.1"), versions("0.0.1", "0.0.2")])
		sleeps = []
		release.verify_release({"name": "crate", "version": "0.0.2"}, lambda _: next(responses), sleeps.append)
		self.assertEqual(sleeps, [5])

	def test_missing_upload_is_a_failure(self):
		with self.assertRaises(RuntimeError):
			release.verify_release({"name": "crate", "version": "0.0.2"}, lambda _: [], lambda _: None)


if __name__ == "__main__":
	unittest.main()
