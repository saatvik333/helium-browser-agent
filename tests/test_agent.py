"""Failure and scope regressions for the owned agent-browser adapter."""

import asyncio
import json
import os
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import AsyncMock, patch

from helium_browser_agent import helium_agent as A
from helium_browser_agent import helium_cdp as H
from helium_browser_agent import helium_download as D
from helium_browser_agent.helium_agent_proxy import NATIVE_FILL_CLEAR, Relay, Store, allowed_targets
from test_helium import Browser


class Tests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.store = Store(self.root / "state.json", "generation", "Personal")
        with self.store.edit() as state:
            state.value.update(owned=["owned"], target="owned", context="personal")
        self.pages = [Browser.page("owned", "personal"), Browser.page("user", "personal"), Browser.page("work", "work")]
        self.patches = [
            patch.object(A, "ROOT", self.root),
            patch.object(H, "CACHE_DIR", self.root),
            patch.object(H, "profiles", return_value={"Personal": "Default", "Work": "Profile 3"}),
        ]
        for p in self.patches:
            p.start()

    def tearDown(self):
        for p in reversed(self.patches):
            p.stop()
        self.tmp.cleanup()

    def relay(self):
        relay = Relay(None, None, self.store, "ws://fixture/generation")
        relay.infos = {t["targetId"]: t for t in self.pages}
        relay.refresh()
        relay.rpc = AsyncMock(return_value={})
        return relay

    def test_backend_inventory_excludes_user_and_other_profiles(self):
        relay = self.relay()
        relay.rpc.return_value = {"targetInfos": self.pages}
        result = asyncio.run(relay.dispatch({"method": "Target.getTargets"}))
        self.assertEqual([t["targetId"] for t in result["targetInfos"]], ["owned"])

    def test_foreign_target_attach_close_activate_are_refused(self):
        for method in ["Target.attachToTarget", "Target.closeTarget", "Target.activateTarget", "Target.getTargetInfo"]:
            relay = self.relay()
            with self.assertRaisesRegex(RuntimeError, "not owned"):
                asyncio.run(relay.dispatch({"method": method, "params": {"targetId": "user"}}))
            relay.rpc.assert_not_called()

    def test_global_browser_changes_are_refused(self):
        for method in [
            "Browser.close",
            "Target.createBrowserContext",
            "Browser.setDownloadBehavior",
            "Storage.clearCookies",
            "Target.setAutoAttach",
        ]:
            relay = self.relay()
            with self.assertRaisesRegex(RuntimeError, "Browser-wide"):
                asyncio.run(relay.dispatch({"method": method}))
            relay.rpc.assert_not_called()

    def test_unknown_sessions_and_unscoped_page_commands_are_refused(self):
        relay = self.relay()
        for msg in [{"method": "Page.navigate"}, {"method": "Page.navigate", "sessionId": "foreign"}]:
            with self.assertRaises(RuntimeError):
                asyncio.run(relay.dispatch(msg))
        relay.rpc.assert_not_called()

    def test_owned_attachment_authorizes_only_returned_session(self):
        relay = self.relay()
        relay.rpc.return_value = {"sessionId": "sid-owned"}
        asyncio.run(relay.dispatch({"method": "Target.attachToTarget", "params": {"targetId": "owned"}}))
        asyncio.run(
            relay.dispatch({"method": "Runtime.evaluate", "sessionId": "sid-owned", "params": {"expression": "1"}})
        )
        self.assertEqual(relay.rpc.call_args.args, ("Runtime.evaluate", {"expression": "1"}, "sid-owned"))

    def test_detach_refuses_foreign_session(self):
        relay = self.relay()
        with self.assertRaises(RuntimeError):
            asyncio.run(relay.dispatch({"method": "Target.detachFromTarget", "params": {"sessionId": "foreign"}}))
        relay.rpc.assert_not_called()

    def test_auto_attach_excludes_shared_service_workers(self):
        relay = self.relay()
        relay.sessions["sid"] = "owned"
        asyncio.run(
            relay.dispatch({"method": "Target.setAutoAttach", "sessionId": "sid", "params": {"autoAttach": True}})
        )
        filters = relay.rpc.call_args.args[1]["filter"]
        self.assertEqual({f["type"] for f in filters if "type" in f}, {"iframe", "worker"})
        self.assertEqual(filters[-1], {"exclude": True})

    def test_profile_wide_page_commands_are_refused(self):
        relay = self.relay()
        relay.sessions["sid"] = "owned"
        for method in [
            "Network.clearBrowserCookies",
            "Network.getAllCookies",
            "Page.setDownloadBehavior",
            "Security.setIgnoreCertificateErrors",
        ]:
            with self.assertRaises(RuntimeError):
                asyncio.run(relay.dispatch({"method": method, "sessionId": "sid"}))
        relay.rpc.assert_not_called()

    def test_popup_ownership_excludes_another_tasks_popup(self):
        self.pages.extend(
            [Browser.page("child", "personal", "owned"), Browser.page("foreign-child", "personal", "user")]
        )
        relay = self.relay()
        self.assertEqual(relay.allowed, {"owned", "child"})
        self.assertEqual(set(self.store.read()["owned"]), {"owned", "child"})

    def test_iframe_ancestry_remains_scoped(self):
        self.pages.extend(
            [
                dict(Browser.page("frame", "personal"), type="iframe", parentId="owned"),
                dict(Browser.page("nested", "personal"), type="iframe", parentId="frame"),
                dict(Browser.page("foreign", "personal"), type="iframe", parentId="user"),
            ]
        )
        self.assertEqual(allowed_targets(self.pages, self.store.read()), {"owned", "frame", "nested"})

    def test_target_creation_cannot_select_another_context(self):
        relay = self.relay()
        with patch.object(self.store, "create") as create:
            with self.assertRaises(RuntimeError):
                asyncio.run(relay.dispatch({"method": "Target.createTarget", "params": {"browserContextId": "work"}}))
            create.assert_not_called()

    def test_fill_validates_readonly_before_clearing(self):
        relay = self.relay()
        relay.sessions["sid"] = "owned"
        relay.rpc.return_value = {"exceptionDetails": {"exception": {"description": "Field is readonly"}}}
        with self.assertRaisesRegex(RuntimeError, "readonly"):
            asyncio.run(
                relay.dispatch(
                    {
                        "method": "Runtime.callFunctionOn",
                        "sessionId": "sid",
                        "params": {"objectId": "field", "functionDeclaration": NATIVE_FILL_CLEAR},
                    }
                )
            )
        self.assertIsNone(relay.fill)
        self.assertNotIn("this.value = ''", relay.rpc.call_args.args[1]["functionDeclaration"])

    def test_fill_rejects_multiline_single_line_before_inserting(self):
        relay = self.relay()
        relay.sessions["sid"] = "owned"
        relay.fill = ("sid", "field", {"multiline": False})
        with self.assertRaisesRegex(RuntimeError, "single-line"):
            asyncio.run(
                relay.dispatch({"method": "Input.insertText", "sessionId": "sid", "params": {"text": "first\nsecond"}})
            )
        relay.rpc.assert_not_called()

    def test_fill_value_mismatch_is_reported_without_retry(self):
        relay = self.relay()
        relay.sessions["sid"] = "owned"
        relay.fill = ("sid", "field", {"multiline": True})
        relay.rpc.side_effect = [{}, {"result": {"value": "transformed"}}]
        with self.assertRaisesRegex(RuntimeError, "exact requested text"):
            asyncio.run(
                relay.dispatch({"method": "Input.insertText", "sessionId": "sid", "params": {"text": "original"}})
            )
        self.assertEqual(relay.rpc.await_count, 2)

    def test_cli_requires_named_session(self):
        with patch.dict(os.environ, {}, clear=True), self.assertRaisesRegex(ValueError, "Choose --session"):
            A.parse(["open", "about:blank"])

    def test_cli_preserves_profile_and_command_values(self):
        result = A.parse(["--session", "fixture", "--personal", "fill", "#field", "line1\nline2\tend"])
        self.assertEqual(result[1], "Personal")
        self.assertEqual(result[-1][-1], "line1\nline2\tend")

    def test_cli_cannot_override_guarded_backend(self):
        for flag in ["--cdp", "--namespace", "--config", "--session", "--no-pin-tab", "--restore", "--provider"]:
            with self.assertRaisesRegex(ValueError, "controlled"):
                A.parse(["--session", "fixture", "open", "about:blank", flag, "bad"])

    def test_cli_blocks_global_close_and_nested_execution_bypasses(self):
        for args in [
            ["close", "--all"],
            ["batch", '["connect 9222"]'],
            ["mcp"],
            ["chat", "hello"],
            ["connect", "9222"],
        ]:
            with self.assertRaises(ValueError):
                A.parse(["--session", "fixture", *args])

    def test_invalid_lock_timeouts_are_refused(self):
        for val in ["nan", "inf", "-1"]:
            with self.assertRaises(ValueError):
                A.parse(["--lock-timeout", val, "--session", "fixture", "open", "about:blank"])

    def test_inherited_provider_profile_and_persistence_are_scrubbed(self):
        with patch.dict(
            os.environ,
            {
                "AGENT_BROWSER_PROVIDER": "cloud",
                "AGENT_BROWSER_PROFILE": "other",
                "AGENT_BROWSER_RESTORE": "secret",
                "AGENT_BROWSER_CONFIG": "untrusted",
                "AGENT_BROWSER_NAMESPACE": "foreign",
                "AGENT_BROWSER_SESSION_NAME": "saved",
            },
        ):
            env = A.environment("fixture")
        self.assertNotIn("AGENT_BROWSER_PROVIDER", env)
        self.assertNotIn("AGENT_BROWSER_PROFILE", env)
        self.assertNotIn("AGENT_BROWSER_RESTORE", env)
        self.assertNotIn("AGENT_BROWSER_NAMESPACE", env)
        self.assertEqual(env["AGENT_BROWSER_RESTORE_SAVE"], "never")
        self.assertEqual(env["BH_TELEMETRY"], "0")
        self.assertEqual(env["AGENT_BROWSER_NO_AUTO_DIALOG"], "1")

    def test_session_backend_names_do_not_collide_with_generic_sessions(self):
        one, two = A.environment("one"), A.environment("two")
        self.assertNotEqual(one["AGENT_BROWSER_SESSION"], two["AGENT_BROWSER_SESSION"])
        self.assertEqual(one["AGENT_BROWSER_SOCKET_DIR"], str(self.root / "run"))

    def test_command_help_does_not_need_a_browser_or_session(self):
        with patch.object(A.controller, "endpoint") as endpoint, patch.object(A.subprocess, "run") as run:
            run.return_value.returncode = 0
            self.assertEqual(A.main(["snapshot", "--help"]), 0)
            endpoint.assert_not_called()

    def test_relay_metadata_cannot_redirect_to_external_host(self):
        record = self.root / "relay.json"
        record.write_text('{"url":"ws://example.invalid/token"}')
        with patch("websockets.sync.client.connect") as connect:
            self.assertIsNone(A.relay_request(record, "health"))
            connect.assert_not_called()

    def test_empty_close_never_contacts_browser_or_starts_backend(self):
        with patch.object(A.controller, "endpoint") as endpoint, patch.object(A.subprocess, "Popen") as popen:
            self.assertEqual(A.main(["--session", "missing", "--personal", "close"]), 0)
            endpoint.assert_not_called()
            popen.assert_not_called()

    def test_close_preserves_state_when_browser_unavailable(self):
        dest = self.root / "fixture.json"
        H.atomic_json(dest, self.store.read())
        with (
            patch.object(A.controller, "endpoint", side_effect=RuntimeError("Unavailable")),
            self.assertRaisesRegex(RuntimeError, "Unavailable"),
        ):
            A.main(["--session", "fixture", "--personal", "close"])
        self.assertTrue(dest.exists())

    def test_corrupt_ownership_fails_closed(self):
        self.store.path.write_text("{")
        with self.assertRaisesRegex(RuntimeError, "damaged"):
            self.store.read()
        self.assertEqual(self.store.path.read_text(), "{")

    def test_legacy_migration_preserves_owned_ids_and_profile(self):
        legacy = self.root / "bound" / "legacy"
        legacy.parent.mkdir()
        legacy.write_text("generation owned\n")
        legacy.with_suffix(".profile").write_text("Personal\n")
        legacy.with_suffix(".owned").write_text("owned\nchild\n")
        dest = self.root / "new.json"
        A.migrate_legacy("legacy", dest, "generation", "Personal")
        self.assertEqual(json.loads(dest.read_text())["owned"], ["owned", "child"])
        self.assertTrue(legacy.exists())

    def test_legacy_missing_binding_retains_evidence(self):
        legacy = self.root / "bound" / "legacy.owned"
        legacy.parent.mkdir()
        legacy.write_text("owned\n")
        with self.assertRaisesRegex(RuntimeError, "Incomplete"):
            A.migrate_legacy("legacy", self.root / "new.json", "generation", "Personal")
        self.assertTrue(legacy.exists())

    def test_completed_migration_cannot_resurrect_old_ownership(self):
        legacy = self.root / "bound" / "legacy"
        legacy.parent.mkdir()
        legacy.write_text("generation owned\n")
        legacy.with_suffix(".profile").write_text("Personal\n")
        legacy.with_suffix(".migrated").write_text('{"migrated":true}')
        dest = self.root / "new.json"
        A.migrate_legacy("legacy", dest, "generation", "Personal")
        self.assertFalse(dest.exists())

    def test_download_connection_failure_preserves_destination_and_removes_temp(self):
        dest = self.root / "artifact.bin"
        dest.write_bytes(b"previous artifact")
        adapter = SimpleNamespace(store=self.store, sync=lambda: self.store.read(), endpoint="ws://fixture")
        with patch.object(D, "Events", side_effect=OSError("Connection failed")), self.assertRaises(OSError):
            D.download(adapter, "#download", dest)
        self.assertEqual(dest.read_bytes(), b"previous artifact")
        self.assertFalse(list(self.root.glob(".artifact.bin.*")))


if __name__ == "__main__":
    unittest.main()
