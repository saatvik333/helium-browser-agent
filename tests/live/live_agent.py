"""Personal-only owned-agent-browser regression fixture, never a signed-in site."""

import concurrent.futures
import hashlib
import json
import re
import subprocess
import tempfile
import threading
import time
import uuid
from pathlib import Path

from helium_browser_agent import helium_agent as A
from helium_browser_agent import helium_cdp as H
from live_checks import HTML, Handler, ThreadingHTTPServer

PAYLOAD = bytes(range(256)) * 257
PAGE = HTML.replace(
    '<div id="pad">',
    '<button id="dialog" onclick="document.body.dataset.answer=String(confirm(\'Agent fixture\'))">Confirm fixture</button><a id="download" href="/download">Download fixture</a><div id="pad">',
)


class Fixture(Handler):
    def do_GET(self):
        if self.path == "/download":
            self.send_response(200)
            self.send_header("Content-Type", "application/octet-stream")
            self.send_header("Content-Disposition", 'attachment; filename="fixture.bin"')
            self.send_header("Content-Length", str(len(PAYLOAD)))
            self.end_headers()
            self.wfile.write(PAYLOAD)
        elif self.path in ("/", "/popup"):
            self.send_response(200)
            self.send_header("Content-Type", "text/html; charset=utf-8")
            self.end_headers()
            self.wfile.write(PAGE.encode())
        else:
            super().do_GET()


def main():
    Path("artifacts").mkdir(exist_ok=True)
    server = ThreadingHTTPServer(("127.0.0.1", 0), Fixture)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}/"
    tag = "ab-hardening-" + uuid.uuid4().hex[:8]
    sessions = {tag}
    primary = tag + "-primary"
    owned = set()
    reports = []
    client = H.Client(A.controller.endpoint()["url"])

    def run(*args, session=tag, ok=True, timeout=45):
        sessions.add(session)
        r = subprocess.run(
            ["agent-browser-helium", "--session", session, "--personal", *args],
            text=True,
            capture_output=True,
            timeout=timeout,
            check=False,
        )
        path = A.ROOT / (session + ".json")
        if path.exists():
            owned.update(json.loads(path.read_text())["owned"])
        if ok and r.returncode:
            raise AssertionError((r.stdout + r.stderr)[-2000:])
        if not ok:
            assert r.returncode, "Expected refusal"
            return r.stderr + r.stdout
        return r.stdout

    def evaluate(source, session=tag):
        return json.loads(run("eval", source, "--json", session=session))["data"]["result"]

    def tabs(session=tag):
        return json.loads(run("tab", "list", "--json", session=session))["data"]["tabs"]

    def check(label, fn):
        start = time.monotonic()
        fn()
        row = {"check": label, "result": "PASS", "seconds": round(time.monotonic() - start, 3)}
        reports.append(row)
        print(json.dumps(row), flush=True)

    def assert_that(value):
        assert value

    def primary_run(source):
        r = subprocess.run(
            ["helium-browser-use", "--session", primary, "--personal"],
            input=source,
            text=True,
            capture_output=True,
            timeout=30,
            check=False,
        )
        assert r.returncode == 0, (r.stdout + r.stderr)[-1800:]
        return r.stdout.strip()

    try:

        def ready():
            run("open", url)
            run("wait", "#apply")
            assert len(tabs()) == 1, "Duplicate bootstrap tab"
            assert evaluate("navigator.webdriver") is False
            tid = tabs()[0]["targetId"]
            assert H.profile_of(client.send, tid) == "Personal"

        check("single owned Personal page, readiness and webdriver", ready)

        def text():
            literal = "first\nsecond\tend é हिंदी 😀"
            run("fill", "#text", literal)
            assert evaluate('document.querySelector("#text").value') == literal
            run("fill", "#name", "Fixture é हिंदी 😀")
            assert evaluate("inputEvents.length") > 0
            assert evaluate("inputEvents.every(e=>e.trusted)")
            run("fill", "#name", "first\nsecond", ok=False)
            assert evaluate('document.querySelector("#name").value') == "Fixture é हिंदी 😀"
            run("fill", "#readonly", "unchanged", ok=False)
            assert evaluate('document.querySelector("#readonly").value') == "unchanged"
            run("fill", "#disabled", "must not enter", ok=False)
            run("fill", "#edit", literal)
            assert evaluate('document.querySelector("#edit").textContent').endswith("end é हिंदी 😀")
            run("fill", "#text", "")
            assert evaluate('document.querySelector("#text").value') == ""

        check("literal multiline Unicode input and readonly refusal", text)

        def refs():
            snapshot = run("snapshot", "-i")
            match = re.search(r'button "Apply" \[ref=(e\d+)\]', snapshot)
            assert match, snapshot
            run("click", "@" + match.group(1))
            run("wait", '#out[data-trusted="true"]')
            assert evaluate('document.querySelector("#out").textContent') == "Fixture é हिंदी 😀:true"

        check("snapshot refs survive calls, trusted click and fetch", refs)
        primary_tid = primary_run("print(current_tab()['targetId'])")

        def isolation():
            assert primary_tid not in {t["targetId"] for t in tabs()}
            run("tab", primary_tid, ok=False)
            run("tab", "close", primary_tid, ok=False)
            assert H.target_exists(client.send, primary_tid)
            record = json.loads((A.ROOT / (tag + ".relay.json")).read_text())
            from websockets.exceptions import InvalidStatus
            from websockets.sync.client import connect

            try:
                with connect(record["url"], origin="https://fixture.invalid", proxy=None):
                    raise AssertionError("Browser-origin relay connection accepted")
            except InvalidStatus:
                pass
            relay = H.Client(record["url"])
            try:
                ids = {t["targetId"] for t in H.targets(relay.send)}
                assert primary_tid not in ids
                try:
                    relay.send("Target.attachToTarget", {"targetId": primary_tid, "flatten": True})
                except RuntimeError:
                    pass
                else:
                    raise AssertionError("Foreign attachment accepted")
                try:
                    relay.send("Browser.close")
                except RuntimeError:
                    pass
                else:
                    raise AssertionError("Global browser close accepted")
            finally:
                relay.close()

        check("primary-wrapper sentinel protected at CLI and relay", isolation)

        def rollback():
            before = tabs()
            run("tab", "new", "http://[", ok=False)
            assert tabs() == before

        check("invalid navigation creates no leaked tab", rollback)

        def popup():
            before = {t["targetId"] for t in tabs()}
            run("click", "#popup")
            deadline = time.monotonic() + 3
            while time.monotonic() < deadline and len(tabs()) == len(before):
                time.sleep(0.05)
            assert len(tabs()) == len(before) + 1

        check("page-created popup attribution", popup)

        def native_new():
            run("click", "#popup", "--new-tab")
            rows = tabs()
            assert len(rows) == 3
            assert len([t for t in rows if t["active"]]) == 1
            assert rows[[t["active"] for t in rows].index(True)]["url"].endswith("/popup")
            run("get", "title")
            assert tabs() == rows

        check("backend-created tab inherits Personal and keeps selection", native_new)

        def parallel():
            other = tag + "-other"
            with concurrent.futures.ThreadPoolExecutor() as pool:
                futures = [pool.submit(run, "open", url, session=s) for s in (tag, other)]
                for future in futures:
                    future.result()
                futures = [
                    pool.submit(run, "fill", "#name", value, session=s)
                    for s, value in [(tag, "Alpha"), (other, "Beta")]
                ]
                for future in futures:
                    future.result()
            assert evaluate('document.querySelector("#name").value') == "Alpha"
            assert evaluate('document.querySelector("#name").value', other) == "Beta"

        check("concurrent independent sessions keep separate fields", parallel)
        with tempfile.TemporaryDirectory(prefix="helium-agent-fixture-") as temp:
            root = Path(temp)

            def artifacts():
                file = root / "upload.txt"
                file.write_text("owned fixture")
                run("upload", "#upload", str(file))
                assert evaluate('document.querySelector("#upload").files[0].name') == "upload.txt"
                picture = root / "screen.png"
                run("screenshot", str(picture))
                assert picture.read_bytes().startswith(b"\x89PNG\r\n\x1a\n")

            check("upload and screenshot artifact", artifacts)

            def download():
                dest = root / "download.bin"
                run("download", "#download", str(dest))
                assert hashlib.sha256(dest.read_bytes()).digest() == hashlib.sha256(PAYLOAD).digest()
                assert not list(root.glob(".download.bin.*"))

            check("HTTP attachment downloaded to disk with exact byte hash", download)

        def dialog():
            evaluate('setTimeout(()=>{document.body.dataset.answer=String(confirm("Agent fixture"))},0)')
            status = json.loads(run("dialog", "status", "--json"))
            assert "Agent fixture" in json.dumps(status)
            run("dialog", "accept")
            assert evaluate("document.body.dataset.answer") == "true"

        check("owned confirm dialog remains for deliberate acceptance", dialog)

        def foreign_dialog():
            primary_run("js('setTimeout(()=>alert(\"Primary fixture\"),0)')")
            run("get", "title")
            primary_run(
                'assert page_info().get("dialog",{}).get("message")=="Primary fixture"\ncdp("Page.handleJavaScriptDialog",accept=True)'
            )

        check("other tool dialog is not auto-dismissed", foreign_dialog)

        def resume():
            before = {t["targetId"] for t in tabs()}
            run("detach")
            assert {t["targetId"] for t in tabs()} == before
            run("get", "title")
            assert {t["targetId"] for t in tabs()} == before

        check("daemon and relay detach/restart reuse owned tabs", resume)

        def relay_failure():
            before = {t["targetId"] for t in tabs()}
            assert A.relay_request(A.ROOT / (tag + ".relay.json"), "stop") == "stopping"
            time.sleep(0.15)
            run("get", "title")
            assert {t["targetId"] for t in tabs()} == before

        check("relay connection loss recovers without duplicate pages", relay_failure)

        def closed():
            rows = tabs()
            active = next(t["targetId"] for t in rows if t["active"])
            client.send("Target.closeTarget", {"targetId": active})
            run("get", "title")
            assert primary_tid not in {t["targetId"] for t in tabs()}

        check("closed target recovers only into owned tab", closed)

        def guards():
            run("close", "--all", ok=False)
            run("open", url, "--cdp", "9222", ok=False)
            run("open", url, "--no-pin-tab", ok=False)
            r = subprocess.run(
                ["agent-browser-helium", "--session", tag, "--work", "get", "title"],
                text=True,
                capture_output=True,
                timeout=10,
                check=False,
            )
            assert r.returncode and "another profile" in r.stderr
            with H.file_lock(A.ROOT / (tag + ".lock")):
                r = subprocess.run(
                    ["agent-browser-helium", "--session", tag, "--personal", "--lock-timeout", "0", "get", "title"],
                    text=True,
                    capture_output=True,
                    timeout=5,
                    check=False,
                )
                assert r.returncode and "holds" in r.stderr

        check("profile, endpoint, pin and lock guards", guards)
    finally:
        errors = []
        for session in sessions:
            try:
                run("close", session=session)
            except Exception as error:  # noqa: BLE001 - continue cleanup for all fixture sessions
                errors.append(str(error))
        r = subprocess.run(
            ["helium-browser-use", "--session", primary, "--personal", "--close"],
            capture_output=True,
            text=True,
            timeout=30,
            check=False,
        )
        if r.returncode:
            errors.append(r.stderr)
        remaining = owned & {t["targetId"] for t in H.targets(client.send)}
        row = {"check": "cleanup", "owned_pages_remaining": len(remaining), "errors": errors}
        reports.append(row)
        print(json.dumps(row), flush=True)
        client.close()
        server.shutdown()
        Path("artifacts/agent-live-results.json").write_text(json.dumps(reports, indent=2) + "\n")
        assert not remaining and not errors, "Incomplete fixture cleanup"


if __name__ == "__main__":
    main()
