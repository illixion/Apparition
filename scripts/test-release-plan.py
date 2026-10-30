import importlib.util
from pathlib import Path
import tempfile
import unittest
import sys

sys.dont_write_bytecode = True

spec = importlib.util.spec_from_file_location("release_plan", Path(__file__).with_name("release-plan.py"))
module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(module)


class ReleasePlanTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        for part in ("installer", "desktop"):
            (self.root / part).mkdir()
        self.versions("0.4.4", "0.4.3")

    def versions(self, app, desktop):
        (self.root / "project.yml").write_text(f'MARKETING_VERSION: "{app}"\n')
        for part in ("installer", "desktop"):
            (self.root / part / "Cargo.toml").write_text(f'[package]\nversion = "{desktop}"\n')

    def test_mobile_only_tag_selects_only_apps(self):
        result = module.plan(self.root, "v0.4.4", "")
        self.assertEqual(result["parts"], "app")
        self.assertEqual(result["linux"], [])
        self.assertEqual([part["part"] for part in result["apple"]], ["app"])

    def test_full_tag_selects_every_part(self):
        self.versions("0.4.4", "0.4.4")
        self.assertEqual(module.plan(self.root, "v0.4.4", "")["parts"], "app,macos,windows,linux")

    def test_explicit_mismatched_parts_fail_before_building(self):
        for parts in ("all", "macos", "windows,linux", "unknown", ","):
            with self.subTest(parts=parts), self.assertRaises(ValueError):
                module.plan(self.root, "0.4.4", parts)
        self.assertEqual(module.plan(self.root, "0.4.4", "app")["parts"], "app")

    def test_installers_only_release_does_not_require_app_bump(self):
        self.versions("0.4.3", "0.4.4")
        self.assertEqual(module.plan(self.root, "v0.4.4", "")["parts"], "macos,windows,linux")
        self.assertEqual(module.plan(self.root, "0.4.4", "linux")["apple"], [])

    def test_unknown_version_fails(self):
        for version in ("0.4.5", "bad"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                module.plan(self.root, version, "")


if __name__ == "__main__":
    unittest.main()
