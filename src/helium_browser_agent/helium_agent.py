"""Profile-aware agent-browser CLI over an owned-target CDP relay."""

import hashlib
import json
import math
import os
import re
import socket
import subprocess
import sys
import time
from pathlib import Path

from helium_browser_agent import helium_cdp as H
from helium_browser_agent.helium_agent_proxy import Store
from helium_browser_agent.helium_session import State, close_owned

from helium_browser_agent import helium_controller as controller

LIB = Path(__file__).parent
ROOT = H.CACHE_DIR / "agent"
PRIVACY = {
    "BH_TELEMETRY": "0",
    "BROWSER_HARNESS_TELEMETRY": "0",
    "ANONYMIZED_TELEMETRY": "false",
    "BROWSER_USE_CLOUD_SYNC": "false",
    "DO_NOT_TRACK": "1",
}
# These options can replace the verified endpoint, bypass the pin or replay state.
PROTECTED = {
    "--cdp",
    "--auto-connect",
    "--no-pin-tab",
    "--namespace",
    "--config",
    "--session",
    "--profile",
    "--provider",
    "-p",
    "--restore",
    "--restore-save",
    "--session-name",
    "--state",
    "--executable-path",
    "--args",
    "--extension",
    "--init-script",
    "--enable",
    "--plugin",
    "--ca-cert",
    "--no-ca-cert",
    "--download-path",
    "--stream-port",
    "--engine",
    "--pin-tab",
    "--no-auto-dialog",
    "--proxy",
    "--proxy-bypass",
    "--ignore-https-errors",
    "--allow-file-access",
}
COMMANDS = {
    "open",
    "read",
    "click",
    "dblclick",
    "type",
    "fill",
    "press",
    "keyboard",
    "hover",
    "focus",
    "check",
    "uncheck",
    "select",
    "drag",
    "upload",
    "download",
    "scroll",
    "scrollintoview",
    "wait",
    "screenshot",
    "pdf",
    "snapshot",
    "eval",
    "back",
    "forward",
    "reload",
    "get",
    "is",
    "find",
    "mouse",
    "set",
    "network",
    "cookies",
    "storage",
    "tab",
    "diff",
    "trace",
    "profiler",
    "record",
    "console",
    "errors",
    "highlight",
    "inspect",
    "clipboard",
    "webmcp",
    "react",
    "vitals",
    "a11y",
    "pushstate",
    "dialog",
}


def parse(argv):
    session = os.environ.get("AGENT_BROWSER_SESSION")
    profile = None
    timeout = 30.0
    output_json = False
    args = list(argv)
    while args and args[0].startswith("-"):
        flag = args.pop(0)
        if flag in ("--personal", "--work"):
            if profile is not None:
                raise ValueError("Choose one profile option")
            profile = H.SETTINGS.personal_profile if flag == "--personal" else H.SETTINGS.work_profile
        elif flag in ("--profile-name", "--profile", "--session", "--lock-timeout"):
            if not args:
                raise ValueError(flag + " requires a value")
            value = args.pop(0)
            if flag == "--session":
                session = value
            elif flag == "--lock-timeout":
                timeout = float(value)
            else:
                if profile is not None:
                    raise ValueError("Choose one profile option")
                profile = value
        elif flag == "--json":
            output_json = True
        elif flag in ("--help", "-h", "--version", "--ws"):
            return session, profile, timeout, output_json, [flag]
        elif flag == "--close":
            args.insert(0, "close")
            break
        else:
            raise ValueError("Place agent-browser options after the command: " + flag)
    if not math.isfinite(timeout) or timeout < 0:
        raise ValueError("--lock-timeout must be finite and nonnegative")
    if not args:
        args = ["--help"]
    if "--help" in args or "-h" in args:
        return session, profile, timeout, output_json, args
    if "--json" in args:
        output_json = True
        args = [a for a in args if a != "--json"]
    for arg in args:
        if arg.split("=", 1)[0] in PROTECTED:
            raise ValueError("This option is controlled by the Helium wrapper: " + arg.split("=", 1)[0])
    if args[0] not in COMMANDS | {"close", "detach", "session", "--help", "-h", "--version", "--ws"}:
        raise ValueError("Unsupported shared-Helium command: " + args[0])
    if args[0] in ("close", "detach") and len(args) != 1:
        raise ValueError("Use close/detach without arguments; global --all is not supported")
    if args[0] not in ("--help", "-h", "--version", "--ws") and (
        not session or not re.fullmatch(r"[A-Za-z0-9_-]{1,48}", session)
    ):
        raise ValueError("Choose --session TASK or AGENT_BROWSER_SESSION (1–48 letters, digits, _ or -)")
    return session, profile, timeout, output_json, args


def environment(session):
    H.atomic_json(ROOT / "agent-config.json", {})
    env = {k: v for k, v in os.environ.items() if not k.startswith("AGENT_BROWSER_")}
    key = "ab-" + hashlib.sha256(session.encode()).hexdigest()[:16]
    env.update(
        PRIVACY,
        AGENT_BROWSER_SESSION=key,
        AGENT_BROWSER_SOCKET_DIR=str(ROOT / "run"),
        AGENT_BROWSER_CONFIG=str(ROOT / "agent-config.json"),
        AGENT_BROWSER_PIN_TAB="1",
        AGENT_BROWSER_NO_AUTO_DIALOG="1",
        AGENT_BROWSER_RESTORE_SAVE="never",
        AGENT_BROWSER_AUTOSAVE_INTERVAL_MS="0",
        AGENT_BROWSER_IDLE_TIMEOUT_MS="900000",
    )
    # Only operational preferences with no profile/endpoint or persistence effect.
    for name in ("AGENT_BROWSER_DEFAULT_TIMEOUT", "AGENT_BROWSER_MAX_OUTPUT", "AGENT_BROWSER_CONTENT_BOUNDARIES"):
        if name in os.environ:
            env[name] = os.environ[name]
    return env


def stop_backend(env):
    path = Path(env["AGENT_BROWSER_SOCKET_DIR"]) / (env["AGENT_BROWSER_SESSION"] + ".sock")
    if not path.exists():
        return
    with socket.socket(socket.AF_UNIX) as conn:
        conn.settimeout(12)
        try:
            conn.connect(str(path))
        except (ConnectionRefusedError, FileNotFoundError):
            return
        conn.sendall(b'{"id":"helium-close","action":"close"}\n')
        result = json.loads(conn.makefile("rb").readline(1024 * 1024))
        if not result.get("success"):
            raise RuntimeError("Backend did not stop; ownership state retained")
    deadline = time.monotonic() + 5
    while path.exists():
        if time.monotonic() >= deadline:
            raise TimeoutError("Backend shutdown still pending; retry cleanup")
        time.sleep(0.05)


def clear_backend_metadata(env):
    root = Path(env["AGENT_BROWSER_SOCKET_DIR"])
    for suffix in (".target", ".config", ".stream"):
        (root / (env["AGENT_BROWSER_SESSION"] + suffix)).unlink(missing_ok=True)


def relay_request(record, action):
    from websockets.exceptions import WebSocketException
    from websockets.sync.client import connect

    try:
        data = json.loads(record.read_text())
        if not re.fullmatch(r"ws://127\.0\.0\.1:\d{1,5}/[0-9a-f]{32}", data["url"]):
            return None
        with connect(data["url"] + "/" + action, open_timeout=2, close_timeout=1, proxy=None) as ws:
            return ws.recv(timeout=2)
    except (OSError, ValueError, KeyError, WebSocketException):
        return None


def relay_url(store, endpoint, record, env):
    if record.exists():
        data = json.loads(record.read_text())
        if data.get("generation") == store.generation and relay_request(record, "health") == "ready":
            return data["url"]
        stop_backend(env)
        relay_request(record, "stop")
        record.unlink(missing_ok=True)
    process = subprocess.Popen(
        [
            sys.executable,
            "-m",
            "helium_browser_agent.helium_agent_proxy",
            str(store.path),
            endpoint,
            store.profile,
            str(record),
        ],
        env=env,
        stdin=subprocess.DEVNULL,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        start_new_session=True,
    )
    deadline = time.monotonic() + 8
    while time.monotonic() < deadline:
        if record.exists() and relay_request(record, "health") == "ready":
            return json.loads(record.read_text())["url"]
        if process.poll() is not None:
            raise RuntimeError("Owned-target relay failed to start")
        time.sleep(0.05)
    process.terminate()
    raise TimeoutError("Owned-target relay did not become ready")


def migrate_legacy(session, path, generation, profile):
    legacy = H.CACHE_DIR / "bound" / session
    owned = legacy.with_suffix(".owned")
    marker = legacy.with_suffix(".migrated")
    if path.exists() or marker.exists() or not (legacy.exists() or owned.exists()):
        return
    if not legacy.exists() or not legacy.with_suffix(".profile").exists():
        raise RuntimeError("Incomplete legacy ownership files retained; use a new session or repair the legacy binding")
    parts = legacy.read_text().split()
    if len(parts) != 2:
        raise RuntimeError("Invalid legacy binding; retained for recovery")
    name = H.canonical_profile(legacy.with_suffix(".profile").read_text().strip())
    if name != profile:
        raise RuntimeError("Legacy task belongs to another profile")
    tids = owned.read_text().split() if owned.exists() else [parts[1]]
    if parts[1] not in tids:
        tids.append(parts[1])
    old_run = (
        Path(os.environ["XDG_RUNTIME_DIR"]) / "agent-browser"
        if os.environ.get("XDG_RUNTIME_DIR")
        else Path.home() / ".agent-browser"
    )
    if (old_run / (session + ".sock")).exists():
        binding = old_run / (session + ".target")
        if not binding.exists() or json.loads(binding.read_text()).get("targetId") not in tids:
            raise RuntimeError("Cannot verify legacy daemon binding; legacy state retained")
        stop_backend({"AGENT_BROWSER_SOCKET_DIR": str(old_run), "AGENT_BROWSER_SESSION": session})
    H.atomic_json(path, {"generation": parts[0], "profile": name, "owned": tids, "target": parts[1]})
    # State invalidates old generation IDs without erasing profile binding.
    State(path, generation, profile).save()
    H.atomic_json(marker, {"migrated": True})


class Adapter:
    def __init__(self, store, client, endpoint, record, env):
        self.store, self.client, self.endpoint, self.record, self.env = store, client, endpoint, record, env
        self.url = None

    def sync(self):
        return self.store.sync(H.targets(self.client.send))

    def backend(self, args, capture=True):
        if self.url is None:
            self.url = relay_url(self.store, self.endpoint, self.record, self.env)
        result = subprocess.run(
            [H.SETTINGS.agent_browser, "--cdp", self.url, "--pin-tab", *args],
            env=self.env,
            text=True,
            capture_output=capture,
            check=False,
        )
        if result.returncode and capture:
            raise RuntimeError((result.stderr or result.stdout).strip())
        return result

    def current(self):
        value = self.sync()
        live = {t["targetId"] for t in H.targets(self.client.send)}
        tid = value.get("target")
        if tid not in live:
            tid = next((t for t in value["owned"] if t in live), None)
        if tid is None:
            tid = self.store.create(self.endpoint)
        self.bind(tid)
        return tid

    def bind(self, tid):
        value = self.sync()
        info = next((t for t in H.targets(self.client.send) if t["targetId"] == tid), None)
        if tid not in value["owned"] or not info or info.get("browserContextId") != value.get("context"):
            raise RuntimeError("Target is not owned by this task/profile")
        run = Path(self.env["AGENT_BROWSER_SOCKET_DIR"])
        key = self.env["AGENT_BROWSER_SESSION"]
        if self.url is None:
            self.url = relay_url(self.store, self.endpoint, self.record, self.env)
        binding = run / (key + ".target")
        if (
            (run / (key + ".sock")).exists()
            and binding.exists()
            and json.loads(binding.read_text()).get("targetId") == tid
        ):
            with self.store.edit() as state:
                state.value["target"] = tid
            return  # Re-selecting the same tab would invalidate snapshot refs.
        if not (run / (key + ".sock")).exists():
            # Seed the backend's documented durable binding before its first
            # connection, so --pin-tab doesn't create a second bootstrap page.
            H.atomic_json(binding, {"targetId": tid, "url": "", "pinned": True})
        self.backend(["tab", tid])
        with self.store.edit() as state:
            state.value["target"] = tid

    def new(self, url="about:blank", label=None):
        if label:
            raise ValueError("Use target IDs from tab list; custom tab labels are not supported by this wrapper")
        previous = self.store.read().get("target")
        tid = self.store.create(self.endpoint, url)
        try:
            self.bind(tid)
            return tid
        except BaseException as error:
            H._close_failed(self.client.send, tid, error)
            if previous and H.target_exists(self.client.send, previous):
                self.bind(previous)
            raise

    def tab_rows(self):
        value = self.sync()
        tabs = H.targets(self.client.send)
        return [
            {
                "targetId": t["targetId"],
                "title": t.get("title", ""),
                "url": t.get("url", ""),
                "active": t["targetId"] == value.get("target"),
            }
            for t in tabs
            if t["targetId"] in value["owned"]
        ]

    def resolve(self, ref):
        if ref in {t["targetId"] for t in self.tab_rows()}:
            return ref
        # Backend IDs/labels are resolved only within the relay's owned inventory.
        result = json.loads(self.backend(["tab", "list", "--json"]).stdout)
        for tab in result.get("data", {}).get("tabs", []):
            if ref in (tab.get("id"), tab.get("label"), tab.get("targetId")):
                tid = tab["targetId"]
                if tid in {t["targetId"] for t in self.tab_rows()}:
                    return tid
        raise ValueError("Unknown or unowned tab; use a targetId from tab list")

    def execute(self, args, as_json=False):
        if args[0] == "tab":
            op = args[1] if len(args) > 1 else "list"
            if op == "new":
                if len(args) > 3:
                    raise ValueError("Usage: tab new [url]")
                tid = self.new(args[2] if len(args) > 2 else "about:blank")
                emit({"targetId": tid}, as_json)
            elif op == "list":
                emit({"tabs": self.tab_rows()}, as_json)
            elif op == "close":
                if len(args) > 3:
                    raise ValueError("Usage: tab close [targetId]")
                tid = self.resolve(args[2]) if len(args) > 2 else self.store.read().get("target")
                if not tid:
                    raise RuntimeError("No current owned tab to close")
                result = self.client.send("Target.closeTarget", {"targetId": tid})
                if result.get("success") is False:
                    raise RuntimeError("Browser refused to close owned tab")
                with self.store.edit() as state:
                    if state.value.get("target") == tid:
                        state.value.pop("target", None)
                emit({"closed": tid}, as_json)
            elif len(args) == 2:
                self.bind(self.resolve(op))
                emit({"targetId": self.store.read()["target"]}, as_json)
            else:
                raise ValueError("Unsupported tab command")
            return 0
        self.current()
        try:
            if args[0] == "download":
                from helium_browser_agent.helium_download import download

                if len(args) != 3:
                    raise ValueError("Usage: download SELECTOR PATH")
                emit(download(self, args[1], args[2]), as_json)
                return 0
            result = self.backend([*args, *(["--json"] if as_json else [])], capture=False)
            return result.returncode
        finally:
            value = self.sync()
            binding = Path(self.env["AGENT_BROWSER_SOCKET_DIR"]) / (self.env["AGENT_BROWSER_SESSION"] + ".target")
            if binding.exists():
                tid = json.loads(binding.read_text()).get("targetId")
                if tid in value["owned"] and H.target_exists(self.client.send, tid):
                    with self.store.edit() as state:
                        state.value["target"] = tid


def emit(data, as_json):
    print(json.dumps({"success": True, "data": data}) if as_json else json.dumps(data, indent=2))


def main(argv=None):
    session, explicit, timeout, as_json, args = parse(sys.argv[1:] if argv is None else argv)
    if args[0] in ("--help", "-h"):
        print(
            "agent-browser-helium --session TASK [--personal|--work|--profile NAME] COMMAND\n"
            "Owned tabs only. close cleans up; detach stops the backend and preserves tabs.\n"
            "Profile/session options precede COMMAND. --lock-timeout defaults to 30 seconds.\n"
            "AGENT_BROWSER_SESSION may supply TASK. Use --json for structured results.\n\n"
            "Common commands: open, snapshot, click, fill, eval, get, wait, screenshot,\n"
            "upload, download, dialog, tab new/list/close/TARGET_ID, session, detach, close.\n"
            "Run COMMAND --help for native command syntax. Connection/config overrides,\n"
            "cloud providers, global close, batch, MCP and chat are outside this adapter.\n"
            "Downloads support HTTP attachments; browser-wide settings are refused.\n"
            "See docs/agent-browser.md in the project repository."
        )
        return 0
    if args[0] == "--version" or "--help" in args or "-h" in args:
        return subprocess.run([H.SETTINGS.agent_browser, *args], env=environment("help"), check=False).returncode
    if args == ["--ws"]:
        raise ValueError("Use helium-controller endpoint for explicit raw CDP; the adapter does not expose it")
    ROOT.mkdir(mode=0o700, parents=True, exist_ok=True)
    (ROOT / "run").mkdir(mode=0o700, exist_ok=True)
    os.chmod(ROOT, 0o700)
    env = environment(session)
    path, record = ROOT / (session + ".json"), ROOT / (session + ".relay.json")
    with H.file_lock(ROOT / (session + ".lock"), timeout):
        if args[0] == "session":
            emit({"session": session, "state_exists": path.exists()}, as_json)
            return 0
        if args[0] in ("close", "detach"):
            stop_backend(env)
            relay_request(record, "stop")
            record.unlink(missing_ok=True)
            legacy = H.CACHE_DIR / "bound" / session
            if args[0] == "detach" or (
                not path.exists()
                and (
                    legacy.with_suffix(".migrated").exists()
                    or (not legacy.exists() and not legacy.with_suffix(".owned").exists())
                )
            ):
                if args[0] == "close":
                    clear_backend_metadata(env)
                emit({"detached": True}, as_json)
                return 0
        saved = json.loads(path.read_text()) if path.exists() else {}
        if args[0] == "close" and not saved:
            legacy_profile = (H.CACHE_DIR / "bound" / session).with_suffix(".profile")
            if legacy_profile.exists():
                saved = {"profile": legacy_profile.read_text().strip()}
        profile = H.canonical_profile(
            explicit
            or (saved.get("profile") if args[0] == "close" else None)
            or os.environ.get("HELIUM_PROFILE_NAME")
            or H.default_profile()
        )
        # Fail before touching the browser if the session belongs to another profile.
        if saved.get("profile", profile) != profile:
            raise RuntimeError("Task belongs to another profile; use a new session")
        endpoint = H.verified_endpoint(controller, profile, launch=args[0] != "close")["url"]
        generation = endpoint.rsplit("/", 1)[-1]
        migrate_legacy(session, path, generation, profile)
        store = Store(path, generation, profile)
        with store.edit():
            pass
        client = H.Client(endpoint)
        try:
            # Migrated bindings have IDs/profile but no cached context field.
            value = store.read()
            if value["owned"] and not value.get("context"):
                context = H.context_for(client.send, profile)
                if any(
                    t["targetId"] in value["owned"] and t.get("browserContextId") != context
                    for t in H.targets(client.send)
                ):
                    raise RuntimeError("Legacy ownership profile mismatch; state retained")
                with store.edit() as state:
                    state.value["context"] = context
            if args[0] == "close":
                with H.file_lock(path.with_suffix(".metadata.lock")):
                    close_owned(State(path, generation, profile), client.send)
                clear_backend_metadata(env)
                emit({"closed": True}, as_json)
                return 0
            adapter = Adapter(store, client, endpoint, record, env)
            return adapter.execute(args, as_json)
        finally:
            client.close()


def cli():
    try:
        raise SystemExit(main())
    except (OSError, ValueError, KeyError, RuntimeError) as error:
        print("agent-browser-helium: " + str(error), file=sys.stderr)
        raise SystemExit(1)


if __name__ == "__main__":
    cli()
