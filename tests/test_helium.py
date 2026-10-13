"""Offline regressions: fake browser transport, real profile/session implementation."""

import json
import runpy
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from helium_browser_agent import helium_cdp as H
from helium_browser_agent.helium_session import Session, State, close_owned


class Browser:
    def __init__(self):
        self.pages = {
            "user": self.page("user", "personal"),
            "unrelated": self.page("unrelated", "work"),
        }
        self.calls = []
        self.counter = 0
        self.fail_create = None
        self.fail_close = False
        self.navigation_error = None

    @staticmethod
    def page(tid, context, opener=None):
        return {
            "targetId": tid,
            "type": "page",
            "browserContextId": context,
            "url": "about:blank",
            "title": "fixture",
            "openerId": opener,
        }

    def send(self, method, params=None, session_id=None):
        p = params or {}
        self.calls.append((method, p, session_id))
        if method == "Target.getTargets":
            return {"targetInfos": list(self.pages.values())}
        if method == "Target.getTargetInfo":
            return {"targetInfo": self.pages[p["targetId"]]}
        if method == "Target.createTarget":
            if self.fail_create:
                raise self.fail_create
            self.counter += 1
            tid = "owned-" + str(self.counter)
            self.pages[tid] = self.page(tid, p["browserContextId"])
            return {"targetId": tid}
        if method == "Target.attachToTarget":
            return {"sessionId": "sid-" + p["targetId"]}
        if method == "Target.closeTarget":
            if self.fail_close:
                return {"success": False}
            self.pages.pop(p["targetId"], None)
            return {"success": True}
        if method == "Page.navigate" and self.navigation_error:
            return {"errorText": self.navigation_error}
        if method == "Page.getFrameTree":
            return {"frameTree": {"frame": {"id": "owned-root"}, "childFrames": [{"frame": {"id": "owned-frame"}}]}}
        return {}

    def cdp(self, method, session_id=None, **params):
        return self.send(method, params, session_id)


class Tests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.patches = [
            patch.object(H, "CACHE_DIR", self.root),
            patch.object(H, "browser_uuid", return_value="generation"),
            patch.object(H, "profiles", return_value={"Personal": "Default", "Work": "Profile 3"}),
        ]
        for p in self.patches:
            p.start()
        self.browser = Browser()
        H.atomic_json(H._cache_path(), {"Personal": "personal", "Work": "work"})

    def tearDown(self):
        for p in reversed(self.patches):
            p.stop()
        self.tmp.cleanup()

    def session(self):
        helper = SimpleNamespace(
            cdp=self.browser.cdp,
            list_tabs=lambda **kw: list(self.browser.pages.values()),
            _target_id=lambda target: target.get("targetId") if isinstance(target, dict) else target,
            _send=lambda request: {},
        )
        session = Session(helper, State(self.root / "task.json", "generation", "Personal"))
        helper.goto_url = lambda url: session.cdp("Page.navigate", url=url)
        session.install({})
        session.start()
        return session

    def test_profile_listing_is_observation_only(self):
        H._cache_path().unlink()
        self.assertEqual(H.resolve_contexts(self.browser.send), {})
        self.assertTrue(all(m == "Target.getTargets" for m, p, s in self.browser.calls))
        self.assertEqual(set(self.browser.pages), {"user", "unrelated"})

    def test_unknown_context_never_borrows_a_user_page(self):
        H._cache_path().unlink()
        with self.assertRaisesRegex(RuntimeError, "Unknown profile context"):
            H.open_in_context(self.browser.send, "work")
        self.assertFalse(
            any(m in ("Page.navigate", "Runtime.evaluate", "Target.closeTarget") for m, p, s in self.browser.calls)
        )

    def test_corrupt_cache_recovers(self):
        H._cache_path().write_text("{")
        self.assertEqual(H.resolve_contexts(self.browser.send), {})
        self.assertEqual(json.loads(H._cache_path().read_text()), {})

    def test_invalid_cache_value_recovers(self):
        H.atomic_json(H._cache_path(), {"Personal": ["bad"]})
        self.assertEqual(H.resolve_contexts(self.browser.send), {})

    def test_stale_context_is_discarded(self):
        H.atomic_json(H._cache_path(), {"Personal": "old"})
        self.assertEqual(H.resolve_contexts(self.browser.send), {})

    def test_profile_of_does_not_discover_other_profiles(self):
        H.atomic_json(H._cache_path(), {"Personal": "personal"})
        self.assertEqual(H.profile_of(self.browser.send, "user"), "Personal")
        self.assertTrue(all(m == "Target.getTargets" for m, p, s in self.browser.calls))

    def test_creation_registered_before_navigation(self):
        events = []
        original = self.browser.send

        def send(method, params=None, session_id=None):
            events.append(method)
            return original(method, params, session_id)

        H.new_tab(send, "Personal", on_created=lambda tid: events.append("registered"))
        self.assertLess(events.index("registered"), events.index("Page.navigate"))

    def test_failed_navigation_rolls_back_new_page(self):
        self.browser.navigation_error = "net::ERR_CONNECTION_REFUSED"
        with self.assertRaisesRegex(RuntimeError, "Navigation failed"):
            H.new_tab(self.browser.send, "Personal", "http://fixture.invalid/")
        self.assertEqual(set(self.browser.pages), {"user", "unrelated"})

    def test_failed_registration_rolls_back_new_page(self):
        def fail(tid):
            raise OSError("disk full")

        with self.assertRaisesRegex(OSError, "disk full"):
            H.new_tab(self.browser.send, "Personal", on_created=fail)
        self.assertEqual(set(self.browser.pages), {"user", "unrelated"})

    def test_uncertain_creation_does_not_retry(self):
        self.browser.fail_create = TimeoutError("context timed out")
        with patch.object(H, "_launch_owned") as launch:
            with self.assertRaises(TimeoutError):
                H.new_tab(self.browser.send, "Personal")
            launch.assert_not_called()

    def test_probe_failure_closes_only_marked_page_and_restores_focus(self):
        H._cache_path().unlink()

        def launch(args, **kw):
            self.browser.pages["probe"] = dict(Browser.page("probe", "personal"), url=args[-1])

        with (
            patch.object(H.subprocess, "Popen", side_effect=launch),
            patch.object(H, "_focused_window", return_value={"id": 4}),
            patch.object(H, "_restore_focus") as restore,
            patch.object(H, "_read_profile_dir", return_value="/fixture/Wrong"),
        ):
            with self.assertRaisesRegex(RuntimeError, "identity mismatch"):
                H.context_for(self.browser.send, "Personal")
            restore.assert_called_once_with({"id": 4})
        self.assertEqual(set(self.browser.pages), {"user", "unrelated"})

    def test_blank_owned_page_is_not_replaced_with_user_page(self):
        session = self.session()
        original = session.target
        self.assertEqual(session.ensure_real_tab()["targetId"], original)
        self.assertEqual(len(session.list_tabs()), 1)

    def test_switch_close_activate_refuse_unowned_pages(self):
        session = self.session()
        for action in (session.switch_tab, session.close_tab, session.activate_tab):
            with self.assertRaisesRegex(RuntimeError, "not owned"):
                action("user")
        self.assertIn("user", self.browser.pages)

    def test_stale_page_recovery_creates_owned_page(self):
        session = self.session()
        self.browser.pages.pop(session.target)
        recovered = session.ensure_real_tab()["targetId"]
        self.assertNotIn(recovered, ("user", "unrelated"))

    def test_cdp_uses_explicit_owned_session(self):
        session = self.session()
        session.cdp("Input.insertText", text="fixture")
        self.assertEqual(self.browser.calls[-1][2], session.sid)

    def test_profile_recovery_uses_requested_url(self):
        session = self.session()
        existing = session.target
        self.assertEqual(session.ensure_profile_tab(url="http://unused.invalid/"), existing)
        self.browser.pages.pop(existing)
        recovered = session.ensure_profile_tab(url="http://fixture.invalid/")
        self.assertNotEqual(recovered, existing)
        self.assertTrue(
            any(
                m == "Page.navigate" and p.get("url") == "http://fixture.invalid/" and s == session.sid
                for m, p, s in self.browser.calls
            )
        )

    def test_child_and_grandchild_owned_but_unrelated_popup_excluded(self):
        session = self.session()
        self.browser.pages["child"] = Browser.page("child", "personal", session.target)
        self.browser.pages["grandchild"] = Browser.page("grandchild", "personal", "child")
        self.browser.pages["user-child"] = Browser.page("user-child", "personal", "user")
        self.browser.pages["wrong-profile"] = Browser.page("wrong-profile", "work", session.target)
        listed = {p["targetId"] for p in session.list_tabs()}
        self.assertEqual(listed, {session.target, "child", "grandchild"})
        close_owned(session.state, self.browser.send)
        self.assertEqual(set(self.browser.pages), {"user", "unrelated", "user-child", "wrong-profile"})

    def test_popup_survives_closed_parent_and_is_still_owned(self):
        session = self.session()
        self.browser.pages["child"] = Browser.page("child", "personal", session.target)
        self.browser.pages.pop(session.target)
        close_owned(session.state, self.browser.send)
        self.assertNotIn("child", self.browser.pages)

    def test_failed_close_retains_state(self):
        session = self.session()
        self.browser.fail_close = True
        with self.assertRaisesRegex(RuntimeError, "refused"):
            close_owned(session.state, self.browser.send)
        self.assertTrue(session.state.path.exists())

    def test_state_profile_binding_survives_generation_change(self):
        session = self.session()
        with self.assertRaisesRegex(RuntimeError, "another profile"):
            State(session.state.path, "new-generation", "Work")
        fresh = State(session.state.path, "new-generation", "Personal")
        self.assertEqual(fresh.value["owned"], [])

    def test_corrupt_ownership_state_fails_closed(self):
        path = self.root / "task.json"
        path.write_text("{")
        with self.assertRaisesRegex(RuntimeError, "damaged"):
            State(path, "generation", "Personal")
        self.assertEqual(path.read_text(), "{")

    def test_state_is_private_and_atomically_replaced(self):
        state = State(self.root / "task.json", "generation", "Personal")
        state.save()
        before = state.path.stat().st_ino
        state.add("fixture")
        self.assertNotEqual(before, state.path.stat().st_ino)
        self.assertEqual(state.path.stat().st_mode & 0o777, 0o600)

    def test_current_workspace_defaults_and_prefix_boundary(self):
        from helium_browser_agent.config import Settings

        settings = Settings(workspaces={str(self.root / "work"): "Work"})
        with patch.object(H, "SETTINGS", settings):
            self.assertEqual(H.default_profile(self.root / "work/project"), "Work")
            self.assertEqual(H.default_profile(self.root / "work-other"), "Personal")

    def test_javascript_refuses_foreign_targets(self):
        session = self.session()
        with self.assertRaisesRegex(RuntimeError, "not an owned page"):
            session.js("document.title", target_id="user")

    def test_iframe_lookup_is_scoped_to_owned_page(self):
        session = self.session()
        for tid in ["foreign-frame", "owned-frame"]:
            self.browser.pages[tid] = dict(
                Browser.page(tid, "personal"), type="iframe", url="https://fixture.invalid/frame"
            )
        self.assertEqual(session.iframe_target("fixture.invalid"), "owned-frame")

    def test_dialog_opening_during_page_info_is_reported(self):
        session = self.session()
        session.original_page_info = lambda: (_ for _ in ()).throw(RuntimeError("Runtime.evaluate timed out"))
        session.h._send = lambda req: {"dialog": {"type": "confirm", "message": "fixture"}}
        self.assertEqual(session.page_info()["dialog"]["message"], "fixture")

    def test_out_of_process_frames_require_owned_ancestry(self):
        session = self.session()
        for tid, parent, context in [
            ("foreign", "user", "personal"),
            ("wrong-profile", session.target, "work"),
            ("child", session.target, "personal"),
            ("grandchild", "child", "personal"),
        ]:
            self.browser.pages[tid] = dict(
                Browser.page(tid, context), type="iframe", parentId=parent, url="https://fixture.invalid/" + tid
            )
        self.assertEqual(session.iframe_target("/grandchild"), "grandchild")
        self.assertIsNone(session.iframe_target("/foreign"))
        self.assertIsNone(session.iframe_target("/wrong-profile"))
        session.original_js = lambda expression, target_id: target_id
        self.assertEqual(session.js("document.title", target_id="grandchild"), "grandchild")

    def test_page_info_unrelated_error_is_not_swallowed(self):
        session = self.session()
        session.original_page_info = lambda: (_ for _ in ()).throw(RuntimeError("Disconnected"))
        with self.assertRaisesRegex(RuntimeError, "Disconnected"):
            session.page_info()

    def test_closing_parent_can_already_have_closed_child(self):
        session = self.session()
        parent = session.target
        self.browser.pages["child"] = Browser.page("child", "personal", parent)
        original = self.browser.send

        def send(method, params=None, session_id=None):
            if method == "Target.closeTarget":
                if params["targetId"] == "child" and "child" not in self.browser.pages:
                    raise RuntimeError("No target with given id found")
                if params["targetId"] == parent:
                    self.browser.pages.pop("child", None)
            return original(method, params, session_id)

        close_owned(session.state, send)
        self.assertEqual(set(self.browser.pages), {"user", "unrelated"})

    def test_detaches_previous_daemon_session_on_warm_attach(self):
        session = self.session()
        session.sid = None
        session.h._send = lambda req: {"session_id": "previous-daemon-session"}
        session.switch_tab(session.target)
        self.assertTrue(
            any(
                m == "Target.detachFromTarget" and p.get("sessionId") == "previous-daemon-session"
                for m, p, s in self.browser.calls
            )
        )

    def test_warm_dialog_reuses_attachment_without_focus_emulation(self):
        session = self.session()
        session.sid = None
        target = session.target
        session.h._send = lambda req: {"session_id": "dialog-session", "dialog": {"type": "alert"}, "targetId": target}
        self.browser.calls.clear()
        self.assertEqual(session.switch_tab(target), "dialog-session")
        self.assertFalse(
            any(m in ("Emulation.setFocusEmulationEnabled", "Target.attachToTarget") for m, p, s in self.browser.calls)
        )

    def test_lock_timeout_is_bounded(self):
        path = self.root / "test.lock"
        with H.file_lock(path), self.assertRaises(TimeoutError), H.file_lock(path, timeout=0):
            self.fail("second task acquired lock")

    def test_empty_cleanup_does_not_contact_or_launch_browser(self):
        cli = runpy.run_module("helium_browser_agent.cli")
        with (
            patch.object(sys, "argv", ["helium-browser-use", "--session", "missing", "--personal", "--close"]),
            patch.object(cli["controller"], "endpoint") as endpoint,
            patch.object(cli["subprocess"], "run", return_value=subprocess.CompletedProcess([], 0)) as run,
        ):
            self.assertEqual(cli["main"](), 0)
            endpoint.assert_not_called()
            self.assertEqual(run.call_args.args[0][-1], "--reload")

    def test_endpoint_failure_never_launches_over_existing_browser(self):
        controller = SimpleNamespace(
            endpoint=lambda: (_ for _ in ()).throw(RuntimeError("Wrong endpoint")),
            _helium_pid=lambda: 123,
            Error=RuntimeError,
        )
        with patch.object(H.subprocess, "Popen") as launch, self.assertRaisesRegex(RuntimeError, "refusing"):
            H.verified_endpoint(controller, "Personal")
        launch.assert_not_called()

    def test_absent_browser_launch_uses_requested_profile(self):
        endpoint = SimpleNamespace(count=0)

        def discover():
            endpoint.count += 1
            if endpoint.count == 1:
                raise RuntimeError("Not running")
            return {"url": "ws://fixture/generation"}

        controller = SimpleNamespace(
            endpoint=discover, _helium_pid=lambda: (_ for _ in ()).throw(RuntimeError("found 0")), Error=RuntimeError
        )
        with patch.object(H.subprocess, "Popen") as launch:
            self.assertEqual(H.verified_endpoint(controller, "Personal")["url"], "ws://fixture/generation")
        self.assertIn("--profile-directory=Default", launch.call_args.args[0])

    def test_cleanup_endpoint_failure_does_not_launch(self):
        controller = SimpleNamespace(endpoint=lambda: (_ for _ in ()).throw(RuntimeError("Unavailable")))
        with patch.object(H.subprocess, "Popen") as launch, self.assertRaisesRegex(RuntimeError, "Unavailable"):
            H.verified_endpoint(controller, "Personal", launch=False)
        launch.assert_not_called()


if __name__ == "__main__":
    unittest.main()
