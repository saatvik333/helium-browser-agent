"""Portable configuration and verified endpoint failures without a real browser."""

import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from helium_browser_agent.config import Settings, load
from helium_browser_agent import helium_controller as C, helium_cdp as H


class ConfigurationTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.env = patch.dict(os.environ, {"XDG_CONFIG_HOME": str(self.root)}, clear=True)
        self.env.start()
        self.addCleanup(self.env.stop)

    def config(self, source):
        path = self.root / "config.toml"
        path.write_text(source)
        return load(path)

    def test_missing_default_file_uses_defaults(self):
        self.assertEqual(load().default_profile, "Personal")

    def test_missing_explicit_file_fails(self):
        with self.assertRaises(FileNotFoundError):
            load(self.root / "missing.toml")

    def test_unknown_key_fails(self):
        with self.assertRaisesRegex(ValueError, "Unknown"):
            self.config('defaut_profile = "typo"')

    def test_non_string_profile_fails(self):
        with self.assertRaisesRegex(ValueError, "nonempty string"):
            self.config("default_profile = 123")

    def test_workspaces_validate_values(self):
        with self.assertRaisesRegex(ValueError, "workspaces"):
            self.config('[workspaces]\n"/tmp/work" = false')

    def test_longest_workspace_wins(self):
        settings = Settings(workspaces={str(self.root): "Base", str(self.root / "nested"): "Nested"})
        self.assertEqual(settings.profile_for(self.root / "nested/app"), "Nested")
        self.assertEqual(settings.profile_for(self.root / "nested-other"), "Base")

    def test_environment_profile_overrides_workspace(self):
        settings = Settings(workspaces={str(self.root): "Base"})
        with patch.dict(os.environ, HELIUM_PROFILE_NAME="Explicit"):
            self.assertEqual(settings.profile_for(self.root), "Explicit")

    def test_custom_aliases_and_paths(self):
        settings = self.config('personal_profile = "My browser"\nwork_profile = "Office"\nprofile_dir = "~/custom"')
        self.assertEqual(settings.personal_profile, "My browser")
        self.assertEqual(settings.work_profile, "Office")
        self.assertEqual(settings.profile_dir, Path.home() / "custom")

    def test_environment_data_root_override(self):
        with patch.dict(os.environ, HELIUM_PROFILE_DIR=str(self.root / "env-root")):
            self.assertEqual(self.config('profile_dir = "~/other"').profile_dir, self.root / "env-root")

    def test_endpoint_rejects_wrong_socket_owner(self):
        with (
            patch.object(C, "_helium_pid", return_value=123),
            patch.object(H, "ws_url", return_value="ws://127.0.0.1:9222/devtools/browser/fixture"),
            patch.object(C, "_listener", return_value="foreign"),
            patch.object(C, "_owned_sockets", return_value={"owned"}),
        ):
            with self.assertRaisesRegex(C.Error, "owner does not match"):
                C.endpoint()

    def test_endpoint_rejects_non_loopback(self):
        with (
            patch.object(C, "_helium_pid", return_value=123),
            patch.object(H, "ws_url", return_value="ws://example.com:9222/devtools/browser/fixture"),
        ):
            with self.assertRaisesRegex(C.Error, "not loopback"):
                C.endpoint()

    def test_public_endpoint_omits_connection_url(self):
        value = C.public_endpoint({"pid": 1, "start": "1", "inode": "2", "url": "sensitive"})
        self.assertNotIn("url", value)
        self.assertNotIn("sensitive", str(value))


if __name__ == "__main__":
    unittest.main()
