"""Task ownership, safe convenience helpers and durable cleanup for Helium."""

import json
import sys
import time
from pathlib import Path

from helium_browser_agent import helium_cdp as H


class State:
    def __init__(self, path, generation, profile):
        self.path = Path(path)
        try:
            value = json.loads(self.path.read_text())
        except FileNotFoundError:
            value = {}
        except (ValueError, UnicodeError) as error:
            raise RuntimeError(
                "Task ownership state is damaged; retained for recovery. Use a new session name."
            ) from error
        if not isinstance(value, dict):
            raise RuntimeError("Invalid task ownership state")  # noqa: TRY004 - corrupt file, not caller type misuse
        # Profile binding persists across browser restarts.
        if value.get("profile", profile) != profile:
            raise RuntimeError("Task session belongs to another profile; use a new session name")
        if value.get("generation") != generation:
            value = {"generation": generation, "profile": profile, "owned": []}
        owned = value.get("owned")
        if not isinstance(owned, list) or not all(isinstance(t, str) and t for t in owned):
            raise RuntimeError("Invalid task ownership list; state retained")
        if value.get("target") and value["target"] not in owned:
            raise RuntimeError("Task target is not owned; state retained")
        self.value = value

    def save(self):
        H.atomic_json(self.path, self.value)

    def add(self, tid):
        if tid not in self.value["owned"]:
            self.value["owned"].append(tid)
            self.save()

    def sync_children(self, send):
        pages = {t["targetId"]: t for t in H.targets(send) if t["type"] == "page"}
        owned = set(self.value["owned"])
        while True:
            children = {
                tid
                for tid, info in pages.items()
                if info.get("openerId") in owned and info.get("browserContextId") == self.value.get("context")
            }
            if children <= owned:
                break
            owned.update(children)
        if owned != set(self.value["owned"]):
            self.value["owned"] = sorted(owned)
            self.save()
        return pages


def close_owned(state, send):
    """Close confirmed task roots and descendants; retain state on any failure."""
    for _ in range(5):
        pages = state.sync_children(send)
        live = set(state.value["owned"]) & pages.keys()
        if not live:
            state.path.unlink(missing_ok=True)
            return
        # Close openers first so they cannot continue producing popups.
        for tid in sorted(live, key=lambda t: bool(pages[t].get("openerId"))):
            try:
                result = send("Target.closeTarget", {"targetId": tid})
            except RuntimeError:
                if not H.target_exists(send, tid):
                    continue  # Closing a parent can already have closed a child.
                raise
            if result.get("success") is False and H.target_exists(send, tid):
                raise RuntimeError("Browser refused to close an owned tab; state retained")
        time.sleep(0.05)  # Target.getTargets can briefly include closing pages.
    if set(state.value["owned"]) & state.sync_children(send).keys():
        raise RuntimeError("Owned tabs are still closing; retry --close. State retained.")
    state.path.unlink(missing_ok=True)


class Session:
    def __init__(self, helpers, state):
        self.h = helpers
        self.state = state
        self.profile = state.value["profile"]
        self.raw = helpers.cdp
        self.original_tabs = helpers.list_tabs
        self.original_js = getattr(helpers, "js", None)
        self.original_page_info = getattr(helpers, "page_info", None)
        self.sid = None
        self.target = None

    def send(self, method, params=None, session_id=None):
        return self.raw(method, session_id=session_id, **(params or {}))

    def install(self, namespace):
        functions = {
            "cdp": self.cdp,
            "new_tab": self.new_tab,
            "switch_tab": self.switch_tab,
            "close_tab": self.close_tab,
            "current_tab": self.current_tab,
            "list_tabs": self.list_tabs,
            "activate_tab": self.activate_tab,
            "ensure_real_tab": self.ensure_real_tab,
            "ensure_profile_tab": self.ensure_profile_tab,
            "helium_profile_of": self.profile_of,
            "helium_profiles": self.profiles,
            "fill_input": self.fill_input,
            "input_value": self.input_value,
            "js": self.js,
            "iframe_target": self.iframe_target,
            "page_info": self.page_info,
        }
        for name, function in functions.items():
            setattr(self.h, name, function)
            namespace[name] = function

    def start(self):
        self.state.save()
        pages = self.state.sync_children(self.send)
        target = self.state.value.get("target")
        if target in pages:
            self.switch_tab(target)
        else:
            self.new_tab()

    def cdp(self, method, session_id=None, **params):
        # Explicit sessions never trigger the harness daemon's automatic replay
        # on a replacement tab. A closed/stale page fails rather than switching accounts.
        if session_id is None and not method.startswith(("Target.", "Browser.")):
            if not self.sid:
                raise RuntimeError("No owned page is attached; call ensure_real_tab()")
            session_id = self.sid
        result = self.raw(method, session_id=session_id, **params)
        if method == "Page.navigate" and result.get("errorText"):
            raise RuntimeError("Navigation failed: " + result["errorText"])
        return result

    def _owned_info(self, target):
        tid = self.h._target_id(target)
        pages = self.state.sync_children(self.send)
        if tid not in self.state.value["owned"]:
            raise RuntimeError("Target is not owned by this task; refusing to use it")
        if tid not in pages:
            raise RuntimeError("Owned target has closed; call ensure_real_tab()")
        context = self.state.value.get("context")
        if context and pages[tid].get("browserContextId") != context:
            raise RuntimeError("Owned target profile mismatch")
        return pages[tid]

    def switch_tab(self, target, activate=False):
        info = self._owned_info(target)
        tid = info["targetId"]
        context = H.context_for(self.send, self.profile)
        if info.get("browserContextId") != context:
            raise RuntimeError("Task target profile mismatch")
        old_sid = self.sid or self.h._send({"meta": "session"}).get("session_id")
        if (
            old_sid
            and self.h._send({"meta": "pending_dialog"}).get("dialog")
            and self.h._send({"meta": "current_tab"}).get("targetId") == tid
        ):
            # A modal dialog blocks focus emulation. Keep the existing verified
            # attachment so another CLI call can inspect/resolve the dialog.
            self.sid, self.target = old_sid, tid
            self.state.value.update(target=tid, context=context)
            self.state.save()
            return old_sid
        sid = self.raw("Target.attachToTarget", targetId=tid, flatten=True)["sessionId"]
        try:
            self.h._send({"meta": "set_session", "session_id": sid, "target_id": tid})
            self.raw("Emulation.setFocusEmulationEnabled", session_id=sid, enabled=True)
        except BaseException as error:
            try:
                H._detach(self.send, sid)
            except Exception as cleanup:  # noqa: BLE001 - preserve original attach failure
                error.add_note("Could not detach failed attachment: " + str(cleanup))
            raise
        self.sid, self.target = sid, tid
        self.state.value.update(target=tid, context=context)
        self.state.save()
        if old_sid and old_sid != sid:
            H._detach(self.send, old_sid)
        if activate:
            self.activate_tab(tid)
        return sid

    def new_tab(self, url="about:blank", profile=None, activate=False):
        if H.canonical_profile(profile or self.profile) != self.profile:
            raise RuntimeError("Use another session for another profile")
        previous = self.target
        tid = H.new_tab(self.send, self.profile, "about:blank", not activate, self.state.add)
        try:
            self.switch_tab(tid, activate=activate)
            if url != "about:blank":
                self.h.goto_url(url)
            return tid
        except BaseException as error:
            H._close_failed(self.send, tid, error)
            self.sid, self.target = None, None
            if previous and H.target_exists(self.send, previous):
                try:
                    self.switch_tab(previous)
                except Exception as restore:  # noqa: BLE001 - preserve original navigation failure
                    error.add_note("Could not restore previous owned target: " + str(restore))
            raise

    def current_tab(self):
        if not self.target:
            raise RuntimeError("No owned page is attached; call ensure_real_tab()")
        info = self.raw("Target.getTargetInfo", targetId=self.target)["targetInfo"]
        return {
            "targetId": self.target,
            "target_id": self.target,
            "url": info.get("url", ""),
            "title": info.get("title", ""),
        }

    def list_tabs(self, include_chrome=True, all_tabs=False):
        self.state.sync_children(self.send)
        tabs = self.original_tabs(include_chrome=include_chrome)
        return tabs if all_tabs else [t for t in tabs if t["targetId"] in self.state.value["owned"]]

    def activate_tab(self, target):
        tid = self._owned_info(target)["targetId"]
        self.raw("Target.activateTarget", targetId=tid)
        return tid

    def close_tab(self, target=None):
        tid = self._owned_info(target if target is not None else self.target)["targetId"]
        result = self.raw("Target.closeTarget", targetId=tid)
        if result.get("success") is False:
            raise RuntimeError("Browser refused to close the owned target")
        if tid == self.target:
            self.sid, self.target = None, None
            self.state.value.pop("target", None)
            self.state.save()

    def ensure_real_tab(self):
        if self.target and H.target_exists(self.send, self.target):
            # about:blank is a legitimate owned work page, not a reason to borrow.
            return self.current_tab()
        pages = self.state.sync_children(self.send)
        target = next((t for t in self.state.value["owned"] if t in pages), None)
        if target:
            self.switch_tab(target)
        else:
            self.new_tab()
        return self.current_tab()

    def ensure_profile_tab(self, profile=None, url="about:blank"):
        if H.canonical_profile(profile or self.profile) != self.profile:
            raise RuntimeError("Use another session for another profile")
        if self.target and H.target_exists(self.send, self.target):
            return self.target
        return self.new_tab(url)

    def profile_of(self, target=None):
        tid = self._owned_info(target if target is not None else self.target)["targetId"]
        return H.profile_of(self.send, tid)

    def profiles(self):
        return H.resolve_contexts(self.send)

    def page_info(self):
        try:
            return self.original_page_info()
        except (TimeoutError, RuntimeError) as error:
            if "timed out" in str(error).lower():
                dialog = self.h._send({"meta": "pending_dialog"}).get("dialog")
                if dialog:
                    return {"dialog": dialog}
            raise

    def _frame_ids(self):
        tree = self.cdp("Page.getFrameTree")["frameTree"]
        pending, result = [tree], set()
        while pending:
            item = pending.pop()
            result.add(item["frame"]["id"])
            pending.extend(item.get("childFrames", []))
        # Out-of-process frames can be absent from Page.getFrameTree. Follow
        # explicit target ancestry, never URL matching across the browser.
        result.add(self.target)
        targets = H.targets(self.send)
        while True:
            children = {
                t["targetId"]
                for t in targets
                if t["type"] == "iframe"
                and t.get("browserContextId") == self.state.value.get("context")
                and (t.get("parentId") in result or t.get("parentFrameId") in result)
            }
            if children <= result:
                break
            result.update(children)
        return result

    def iframe_target(self, url_substr):
        frames = self._frame_ids()
        return next(
            (
                t["targetId"]
                for t in H.targets(self.send)
                if t["type"] == "iframe" and t["targetId"] in frames and url_substr in t.get("url", "")
            ),
            None,
        )

    def js(self, expression, target_id=None):
        if target_id:
            if target_id in self.state.value["owned"]:
                self._owned_info(target_id)
            elif target_id not in self._frame_ids():
                raise RuntimeError("JavaScript target is not an owned page or its frame")
        return self.original_js(expression, target_id=target_id)

    def input_value(self, selector):
        """Read literal text, preserving editor tabs and native paragraph breaks."""
        return self.h.js(
            """(selector => {
            const e=document.querySelector(selector);
            if (!e) throw Error('input_value: element not found');
            if (!e.isContentEditable) return e.value;
            const blocks=new Set(['DIV','P','LI','PRE','BLOCKQUOTE','H1','H2','H3','H4','H5','H6']);
            function read(parent) {
                const children=Array.from(parent.childNodes);
                if (children.length===1 && children[0].nodeName==='BR') return '';
                let out='',previousBlock=false;
                children.forEach((node,index)=>{
                    const block=blocks.has(node.nodeName);
                    if (index && (block || previousBlock)) out+='\\n';
                    out+=node.nodeType===Node.TEXT_NODE ? node.data :
                        node.nodeName==='BR' ? '\\n' : read(node);
                    previousBlock=block;
                });
                return out;
            }
            return read(e);
        })("""
            + json.dumps(selector)
            + ")"
        )

    def fill_input(self, selector, text, clear_first=True, timeout=0.0):
        """Insert literal text, including textarea tabs/newlines, and verify it.

        Uses Chromium's native text insertion, so input events reach frameworks.
        clear_first=False appends. Selects/checkboxes are not text fields.
        """
        if not isinstance(text, str):
            raise TypeError("fill_input text must be a string")
        if timeout and not self.h.wait_for_element(selector, timeout=timeout):
            raise RuntimeError("fill_input: element not found")
        info = self.h.js(
            """(selector => {
            const e = document.querySelector(selector);
            if (!e) throw Error('fill_input: element not found');
            if (e.disabled || e.readOnly) throw Error('fill_input: field is disabled or readonly');
            const field = e instanceof HTMLTextAreaElement ||
                (e instanceof HTMLInputElement && ['text','search','url','tel','email','password','number'].includes(e.type));
            if (!field && !e.isContentEditable) throw Error('fill_input: target is not a text field');
            e.focus();
            if (document.activeElement !== e) throw Error('fill_input: could not focus the field');
            return {value:field ? e.value : e.innerText, multiline:e instanceof HTMLTextAreaElement || e.isContentEditable};
        })("""
            + json.dumps(selector)
            + ")"
        )
        text = text.replace("\r\n", "\n").replace("\r", "\n")
        if not info["multiline"] and "\n" in text:
            raise ValueError("fill_input: a single-line field cannot preserve newlines")
        expected = ("" if clear_first else self.input_value(selector)) + text
        # Select the field contents without modifying the system clipboard.
        self.h.js(
            """((selector, clear) => {
            const e = document.querySelector(selector);
            if (e.isContentEditable) {
                const r=document.createRange();r.selectNodeContents(e);
                if (!clear) r.collapse(false);
                const s=getSelection();s.removeAllRanges();s.addRange(r);
            } else if (clear) { e.select(); }
            else { try {e.setSelectionRange(e.value.length,e.value.length);} catch (_) {} }
        })("""
            + json.dumps(selector)
            + ","
            + json.dumps(clear_first)
            + ")"
        )
        if text:
            self.cdp("Input.insertText", text=text)
        elif clear_first:
            self.h.press_key("Backspace")
        self.h.js(
            '(selector=>document.querySelector(selector).dispatchEvent(new Event("change",{bubbles:true})))('
            + json.dumps(selector)
            + ")"
        )
        value = self.input_value(selector)
        if value != expected:
            # Do not retry: the field may have validation/formatting side effects.
            raise RuntimeError("fill_input: page did not retain the exact text; inspect the field before retrying")


def run_task(code, namespace, path, generation, profile):
    from browser_harness import helpers

    session = Session(helpers, State(path, generation, profile))
    session.install(namespace)
    try:
        session.start()
        exec(compile(code, "<helium-task>", "exec"), namespace)  # noqa: S102 - this CLI intentionally runs user Python
    finally:
        original_error = sys.exc_info()[1]
        try:
            session.state.sync_children(session.send)
        except Exception as error:
            if original_error is None:
                raise
            original_error.add_note("Could not refresh owned popups; session state retained: " + str(error))
