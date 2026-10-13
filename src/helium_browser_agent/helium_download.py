"""Save an owned page's HTTP attachment without changing profile download policy."""

import base64
import json
import os
import subprocess
import sys
import tempfile
import time
from collections import deque
from pathlib import Path

from helium_browser_agent import helium_cdp as H


class Events(H.Client):
    def __init__(self, endpoint):
        super().__init__(endpoint)
        self.events = deque()

    def send(self, method, params=None, session_id=None):
        mid = next(self.ids)
        msg = {"id": mid, "method": method, "params": params or {}}
        if session_id:
            msg["sessionId"] = session_id
        self.ws.send(json.dumps(msg))
        deadline = time.monotonic() + 10
        while True:
            response = json.loads(self.ws.recv(timeout=max(0.001, deadline - time.monotonic())))
            if response.get("id") == mid:
                if "error" in response:
                    raise RuntimeError(response["error"]["message"])
                return response.get("result", {})
            if "method" in response:
                self.events.append(response)
            if time.monotonic() >= deadline:
                raise TimeoutError(method + " timed out")

    def event(self, timeout=1):
        if self.events:
            return self.events.popleft()
        return json.loads(self.ws.recv(timeout=timeout))


def download(adapter, selector, destination, timeout=35):
    """Click once, intercept an HTTP attachment response, stream to an atomic file."""
    tid = adapter.store.read()["target"]
    if tid not in adapter.sync()["owned"]:
        raise RuntimeError("Download target is not owned")
    path = Path(destination).expanduser().resolve()
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix="." + path.name + ".", dir=path.parent)
    client = None
    sid, stream, click, request = None, None, None, None
    try:
        client = Events(adapter.endpoint)
        sid = client.send("Target.attachToTarget", {"targetId": tid, "flatten": True})["sessionId"]
        client.send("Fetch.enable", {"patterns": [{"urlPattern": "*", "requestStage": "Response"}]}, sid)
        # Backend is already pinned; this click performs no browser-wide download override.
        click = subprocess.Popen(
            [H.SETTINGS.agent_browser, "--cdp", adapter.url, "--pin-tab", "click", selector],
            env=adapter.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            try:
                event = client.event(min(1, max(0.01, deadline - time.monotonic())))
            except TimeoutError:
                if click.poll() not in (None, 0):
                    out, err = click.communicate()
                    raise RuntimeError("Download click failed: " + (err or out).strip())
                continue
            if event.get("method") != "Fetch.requestPaused" or event.get("sessionId") != sid:
                continue
            p = event["params"]
            disposition = next(
                (h["value"] for h in p.get("responseHeaders", []) if h["name"].lower() == "content-disposition"), ""
            )
            if not disposition.lower().startswith("attachment") or not 200 <= p.get("responseStatusCode", 0) < 300:
                client.send("Fetch.continueRequest", {"requestId": p["requestId"]}, sid)
                continue
            request = p["requestId"]
            stream = client.send("Fetch.takeResponseBodyAsStream", {"requestId": request}, sid)["stream"]
            break
        else:
            raise TimeoutError("No HTTP attachment response received; blob/data downloads need a different export path")
        size = 0
        with os.fdopen(fd, "wb") as output:
            fd = None
            while True:
                if time.monotonic() >= deadline:
                    raise TimeoutError("Download body timed out")
                chunk = client.send("IO.read", {"handle": stream, "size": 65536}, sid)
                data = base64.b64decode(chunk["data"]) if chunk.get("base64Encoded") else chunk["data"].encode()
                output.write(data)
                size += len(data)
                if chunk.get("eof"):
                    break
            output.flush()
            os.fsync(output.fileno())
        # Consume the navigation as no-content so the ordinary browser downloader
        # cannot save another copy or alter the user's current download directory.
        client.send("Fetch.fulfillRequest", {"requestId": request, "responseCode": 204, "body": ""}, sid)
        request = None
        os.replace(temporary, path)
        return {"path": str(path), "bytes": size}
    finally:
        original = sys.exc_info()[1]
        if fd is not None:
            os.close(fd)
        Path(temporary).unlink(missing_ok=True)
        failures = []
        if client:
            cleanup = []
            if request:
                cleanup.append(("Fetch.failRequest", {"requestId": request, "errorReason": "Aborted"}))
            if stream:
                cleanup.append(("IO.close", {"handle": stream}))
            if sid:
                cleanup.append(("Fetch.disable", {}))
            for method, params in cleanup:
                try:
                    client.send(method, params, sid)
                except (OSError, RuntimeError) as error:
                    failures.append(str(error))
            try:
                if sid:
                    H._detach(client.send, sid)
            except (OSError, RuntimeError) as error:
                failures.append(str(error))
            finally:
                client.close()
        if click:
            try:
                click.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                click.terminate()
                click.communicate(timeout=5)
        if failures:
            detail = "Download cleanup needs attention: " + "; ".join(failures)
            if original:
                original.add_note(detail)
            else:
                raise RuntimeError(detail)
