"""Verified Helium profiles and owned tab creation. Never probe an existing page."""

import fcntl
import itertools
import json
import os
import re
import subprocess
import tempfile
import time
import urllib.request
import uuid
from contextlib import contextmanager
from pathlib import Path

from helium_browser_agent.config import SETTINGS

HELIUM_DIR = SETTINGS.profile_dir
CACHE_DIR = SETTINGS.cache_dir
DEFAULT_PROFILE = SETTINGS.default_profile


def atomic_json(path, value):
    path = Path(path)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    try:
        with os.fdopen(fd, "w") as stream:
            json.dump(value, stream)
            stream.flush()
            os.fsync(stream.fileno())
        os.replace(tmp, path)
    finally:
        Path(tmp).unlink(missing_ok=True)


@contextmanager
def file_lock(path, timeout=30):
    path = Path(path)
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    with path.open("a") as lock:
        os.chmod(path, 0o600)
        deadline = time.monotonic() + timeout
        while True:
            try:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                break
            except BlockingIOError:
                if time.monotonic() >= deadline:
                    raise TimeoutError("Another browser task holds " + path.name)
                time.sleep(0.05)
        try:
            yield
        finally:
            fcntl.flock(lock, fcntl.LOCK_UN)


def ws_url():
    flags = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "helium-browser-flags.conf"
    if flags.exists():
        ports = re.findall(r"^\s*--remote-debugging-port=(\d+)\s*$", flags.read_text(), re.MULTILINE)
        if ports and int(ports[-1]):
            port = int(ports[-1])
            opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))
            with opener.open(f"http://127.0.0.1:{port}/json/version", timeout=3) as response:
                value = json.load(response)["webSocketDebuggerUrl"]
            if not value.startswith(f"ws://127.0.0.1:{port}/devtools/browser/"):
                raise RuntimeError("Unexpected Helium debugging endpoint")
            return value
    lines = (HELIUM_DIR / "DevToolsActivePort").read_text().splitlines()
    if len(lines) < 2 or not lines[0].isdigit() or not re.fullmatch(r"/devtools/browser/[\w.-]+", lines[1]):
        raise RuntimeError("Invalid Helium debugging endpoint file")
    return f"ws://127.0.0.1:{lines[0]}{lines[1]}"


def browser_uuid():
    return ws_url().rsplit("/", 1)[-1]


def profiles():
    cache = json.loads((HELIUM_DIR / "Local State").read_text()).get("profile", {}).get("info_cache", {})
    result = {}
    for directory, info in cache.items():
        name = info.get("name", directory)
        if name in result:
            raise RuntimeError("Duplicate Helium profile display name: " + name)
        result[name] = directory
    return result


def default_profile(cwd=None):
    return SETTINGS.profile_for(cwd or Path.cwd())


def verified_endpoint(controller, profile, launch=True):
    """Verify running Helium, or launch only when its main process is absent."""
    try:
        return controller.endpoint()
    except (OSError, RuntimeError):
        if not launch:
            raise
        try:
            controller._helium_pid()
        except controller.Error as error:
            if "found 0" not in str(error):
                raise
        else:
            raise RuntimeError("Running Helium endpoint failed verification; refusing attachment") from None
    subprocess.Popen(
        [SETTINGS.launcher, "--profile-directory=" + profiles()[canonical_profile(profile)]],
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    deadline = time.monotonic() + 15
    while True:
        try:
            return controller.endpoint()
        except (OSError, RuntimeError):
            if time.monotonic() >= deadline:
                raise
            time.sleep(0.25)


def canonical_profile(name=None):
    known = profiles()
    name = (name or DEFAULT_PROFILE).strip()
    if name in known:
        return name
    matches = [n for n in known if n.lower() == name.lower()]
    if not matches and name:
        matches = [n for n in known if name.lower() in n.lower()]
    if len(matches) == 1:
        return matches[0]
    raise ValueError(f"Unknown or ambiguous Helium profile: {name!r}")


def targets(send):
    return send("Target.getTargets")["targetInfos"]


def _cache_path():
    return CACHE_DIR / f"contexts-{browser_uuid()}.json"


def _cached(send):
    try:
        value = json.loads(_cache_path().read_text())
        if not isinstance(value, dict):
            return {}
    except (FileNotFoundError, json.JSONDecodeError, UnicodeError):
        return {}
    live = {t.get("browserContextId") for t in targets(send) if t["type"] == "page"}
    known = profiles()
    return {n: c for n, c in value.items() if n in known and isinstance(c, str) and c in live}


def resolve_contexts(send, force=False):
    """Return known live identities without opening or modifying any page.

    Unknown contexts stay unknown. Call context_for with an explicit profile to
    identify it. force is retained for CLI compatibility; live IDs are always checked.
    """
    with file_lock(CACHE_DIR / "profile-create.lock"):
        result = _cached(send)
        atomic_json(_cache_path(), result)
        return result


def _attach(send, target_id):
    return send("Target.attachToTarget", {"targetId": target_id, "flatten": True})["sessionId"]


def _detach(send, session_id):
    try:
        send("Target.detachFromTarget", {"sessionId": session_id})
    except RuntimeError as error:
        message = str(error).lower()
        if not any(
            text in message for text in ("session with given id not found", "no session with given id", "target closed")
        ):
            raise


def _navigate(send, target_id, url):
    sid = _attach(send, target_id)
    try:
        result = send("Page.navigate", {"url": url}, session_id=sid)
        if result.get("errorText"):
            raise RuntimeError("Navigation failed: " + result["errorText"])
    finally:
        _detach(send, sid)


def _read_profile_dir(send, target_id):
    sid = _attach(send, target_id)
    try:
        send("Page.navigate", {"url": "chrome://version/"}, session_id=sid)
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            result = send(
                "Runtime.evaluate",
                {"expression": "(document.getElementById('profile_path')||{}).textContent||''", "returnByValue": True},
                session_id=sid,
            )
            value = result.get("result", {}).get("value")
            if value:
                return value.strip()
            time.sleep(0.05)
        raise RuntimeError("Profile identity probe did not become ready")
    finally:
        _detach(send, sid)


def _focused_window():
    try:
        result = subprocess.run(
            ["niri", "msg", "-j", "focused-window"], capture_output=True, text=True, timeout=2, check=True
        )
        return json.loads(result.stdout) or {}
    except (OSError, ValueError, subprocess.SubprocessError):
        return {}


def _restore_focus(previous):
    if previous.get("id") is None:
        return
    current = _focused_window()
    if current.get("id") == previous["id"] or "helium" not in str(current.get("app_id", "")).lower():
        return
    try:
        subprocess.run(
            ["niri", "msg", "action", "focus-window", "--id", str(previous["id"])],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=2,
            check=True,
        )
    except (OSError, subprocess.SubprocessError):
        pass


def _close_failed(send, tid, error):
    if tid:
        try:
            send("Target.closeTarget", {"targetId": tid})
        except Exception as cleanup:  # noqa: BLE001 - preserve the original initialization failure
            error.add_note("Could not close newly created target " + tid + ": " + str(cleanup))


def _launch_owned(send, name, on_created=None, background=True):
    """Identify only a uniquely marked page launched by this call."""
    marker = "data:text/html,<title>helium-task-" + uuid.uuid4().hex + "</title>"
    previous = _focused_window() if background else {}
    tid = None
    try:
        subprocess.Popen(
            [SETTINGS.launcher, "--profile-directory=" + profiles()[name], marker],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            start_new_session=True,
        )
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            opened = [t for t in targets(send) if t["type"] == "page" and t.get("url") == marker]
            if opened:
                tid = opened[0]["targetId"]
                if on_created:
                    on_created(tid)
                pdir = _read_profile_dir(send, tid)
                if Path(pdir).name != profiles()[name]:
                    raise RuntimeError("New profile probe identity mismatch")
                ctx = opened[0].get("browserContextId")
                if not ctx:
                    raise RuntimeError("New profile probe has no browser context")
                return tid, ctx
            time.sleep(0.05)
        raise TimeoutError("Could not identify newly launched Helium task page")
    except BaseException as error:
        _close_failed(send, tid, error)
        raise
    finally:
        _restore_focus(previous)


def context_for(send, profile=None):
    """Explicit identity discovery using a short-lived owned probe."""
    name = canonical_profile(profile)
    with file_lock(CACHE_DIR / "profile-create.lock"):
        cached = _cached(send)
        if name in cached:
            return cached[name]
        tid, ctx = _launch_owned(send, name)
        try:
            cached[name] = ctx
            atomic_json(_cache_path(), cached)
            return ctx
        finally:
            send("Target.closeTarget", {"targetId": tid})


def _create_owned(send, name, cached, background, on_created):
    ctx = cached.get(name)
    tid = None
    if ctx:
        try:
            tid = send(
                "Target.createTarget", {"url": "about:blank", "browserContextId": ctx, "background": background}
            )["targetId"]
        except RuntimeError as error:
            # A timeout is uncertain completion, not permission to create another.
            if "context" not in str(error).lower() or "timed out" in str(error).lower():
                raise
    if tid:
        try:
            if on_created:
                on_created(tid)
            info = send("Target.getTargetInfo", {"targetId": tid})["targetInfo"]
            if info.get("browserContextId") != ctx:
                raise RuntimeError("New target profile mismatch")
            return tid, ctx
        except BaseException as error:
            _close_failed(send, tid, error)
            raise
    return _launch_owned(send, name, on_created, background)


def new_tab(send, profile=None, url="about:blank", background=True, on_created=None):
    """Create, register, verify and navigate one owned page; roll back failures."""
    name = canonical_profile(profile)
    with file_lock(CACHE_DIR / "profile-create.lock"):
        cached = _cached(send)
        tid, ctx = _create_owned(send, name, cached, background, on_created)
        try:
            cached[name] = ctx
            atomic_json(_cache_path(), cached)
            _navigate(send, tid, url)
            return tid
        except BaseException as error:
            _close_failed(send, tid, error)
            raise


def open_in_context(send, ctx, url="about:blank", background=True):
    names = [name for name, context in resolve_contexts(send).items() if context == ctx]
    if len(names) != 1:
        raise RuntimeError("Unknown profile context; identify an explicit profile first")
    return new_tab(send, names[0], url, background)


def profile_of(send, target_id):
    info = next((t for t in targets(send) if t["targetId"] == target_id), None)
    if info is None:
        return None
    return next((name for name, ctx in resolve_contexts(send).items() if ctx == info.get("browserContextId")), None)


def target_exists(send, target_id):
    return any(t["targetId"] == target_id and t["type"] == "page" for t in targets(send))


class Client:
    """Small direct transport for cleanup; it never starts a harness daemon."""

    def __init__(self, url):
        from websockets.sync.client import connect

        self.ws = connect(url, max_size=32 * 1024 * 1024, open_timeout=5, proxy=None)
        self.ids = itertools.count(1)

    def send(self, method, params=None, session_id=None):
        mid = next(self.ids)
        message = {"id": mid, "method": method, "params": params or {}}
        if session_id:
            message["sessionId"] = session_id
        self.ws.send(json.dumps(message))
        deadline = time.monotonic() + 10
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(method + " timed out; completion is uncertain")
            response = json.loads(self.ws.recv(timeout=remaining))
            if response.get("id") == mid:
                if "error" in response:
                    raise RuntimeError(response["error"].get("message", "CDP error"))
                return response.get("result", {})

    def close(self):
        self.ws.close()
