"""Release ordering, registry failures and restart behavior."""

import importlib.util
import io
from pathlib import Path
import unittest
from unittest.mock import patch
import urllib.error

spec = importlib.util.spec_from_file_location("publish_crates", Path(__file__).with_name("publish-crates.py"))
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


def package(name, dependencies=()):
    return {
        "name": name, "version": "0.1.42", "manifest_path": f"src/{name}/Cargo.toml",
        "dependencies": list(dependencies),
    }


class ReleaseTests(unittest.TestCase):
    def test_optional_platform_dependencies_publish_before_consumers(self):
        dependency = {"name": "msb_krun_hvf", "req": "^0.1.42", "kind": None,
                      "optional": True, "target": 'cfg(target_os = "macos")'}
        packages = [package("msb_krun", [dependency]), package("msb_krun_hvf")]
        order = release.release_order({"packages": packages}, "0.1.42")
        self.assertEqual([p["name"] for p in order], ["msb_krun_hvf", "msb_krun"])

    def test_inconsistent_release_is_rejected_before_upload(self):
        for bad in [
            {**package("msb_krun"), "version": "0.1.41"},
            package("msb_krun", [{"name": "msb_krun_missing", "req": "^0.1.42", "kind": None}]),
            {**package("msb_krun"), "publish": []},
        ]:
            with self.subTest(package=bad), self.assertRaises(ValueError):
                release.release_order({"packages": [bad]}, "0.1.42")

    def test_registry_errors_are_not_treated_as_absent_versions(self):
        for code in [403, 429, 500]:
            error = urllib.error.HTTPError("https://index.crates.io", code, "failed", {}, None)
            with self.subTest(code=code), patch.object(release.urllib.request, "urlopen", side_effect=error):
                with self.assertRaises(urllib.error.HTTPError):
                    release.published("msb_krun", "0.1.42")
        error = urllib.error.HTTPError("https://index.crates.io", 404, "missing", {}, None)
        with patch.object(release.urllib.request, "urlopen", side_effect=error):
            self.assertFalse(release.published("msb_krun", "0.1.42"))

    def test_yanked_version_is_not_a_successful_release(self):
        with patch.object(release.urllib.request, "urlopen", return_value=io.BytesIO(
            b'{"vers":"0.1.42","yanked":true}\n'
        )):
            with self.assertRaises(ValueError):
                release.published("msb_krun", "0.1.42")

    @patch.dict(release.os.environ, {"CARGO_REGISTRY_TOKEN": "test-only"})
    def test_resume_skips_existing_crates_and_waits_before_next_upload(self):
        events = []
        states = iter([True, False, False, True, False, True])

        def available(name, version):
            events.append(("index", name))
            return next(states)

        def upload(command, **kwargs):
            events.append(("upload", command[-1]))

        with patch.object(release, "published", side_effect=available), \
             patch.object(release.subprocess, "run", side_effect=upload), \
             patch.object(release.time, "sleep"):
            release.publish([package("msb_krun_arch_gen"), package("msb_krun_arch"),
                             package("msb_krun")], "0.1.42")
        self.assertEqual(events, [
            ("index", "msb_krun_arch_gen"),
            ("index", "msb_krun_arch"), ("upload", "src/msb_krun_arch/Cargo.toml"),
            ("index", "msb_krun_arch"), ("index", "msb_krun_arch"),
            ("index", "msb_krun"), ("upload", "src/msb_krun/Cargo.toml"),
            ("index", "msb_krun"),
        ])

    @patch.dict(release.os.environ, {"CARGO_REGISTRY_TOKEN": "test-only"})
    def test_failed_upload_stops_dependent_publication(self):
        with patch.object(release, "published", return_value=False), \
             patch.object(release.subprocess, "run", side_effect=RuntimeError("upload failed")) as upload:
            with self.assertRaises(RuntimeError):
                release.publish([package("msb_krun_arch"), package("msb_krun")], "0.1.42")
            self.assertEqual(upload.call_count, 1)


if __name__ == "__main__":
    unittest.main()
