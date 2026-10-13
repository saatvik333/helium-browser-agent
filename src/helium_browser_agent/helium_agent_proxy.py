"""Owned-target CDP view for the agent-browser backend; no global tab attachment."""

import asyncio
import contextlib
import json
import sys
import time
import uuid
from pathlib import Path

from helium_browser_agent import helium_cdp as H
from helium_browser_agent.helium_session import State

NATIVE_FILL_CLEAR = """function() {
    this.select && this.select();
    this.value = '';
    this.dispatchEvent(new Event('input', { bubbles: true }));
}"""
CHECKED_SELECT = """function() {
    if (this.disabled || this.readOnly) throw Error('Field is disabled or readonly');
    const field=this instanceof HTMLTextAreaElement ||
        (this instanceof HTMLInputElement && ['text','search','url','tel','email','password','number'].includes(this.type));
    if (!field && !this.isContentEditable) throw Error('Target is not a text field');
    this.focus();
    if (this.ownerDocument.activeElement!==this) throw Error('Could not focus field');
    if (this.isContentEditable) {
        const range=this.ownerDocument.createRange();range.selectNodeContents(this);
        const selection=this.ownerDocument.getSelection();selection.removeAllRanges();selection.addRange(range);
    } else this.select();
    return {multiline:this instanceof HTMLTextAreaElement || this.isContentEditable};
}"""
READ_TEXT = """function() {
    if (!this.isContentEditable) return this.value;
    const blocks=new Set(['DIV','P','LI','PRE','BLOCKQUOTE','H1','H2','H3','H4','H5','H6']);
    function read(parent) {
        const children=Array.from(parent.childNodes);
        if (children.length===1 && children[0].nodeName==='BR') return '';
        let out='',previousBlock=false;
        children.forEach((node,index)=>{
            const block=blocks.has(node.nodeName);
            if (index && (block || previousBlock)) out+='\\n';
            out+=node.nodeType===Node.TEXT_NODE ? node.data : node.nodeName==='BR' ? '\\n' : read(node);
            previousBlock=block;
        });
        return out;
    }
    return read(this);
}"""


class Store:
    def __init__(self, path, generation, profile):
        self.path, self.generation, self.profile = Path(path), generation, profile

    @contextlib.contextmanager
    def edit(self):
        with H.file_lock(self.path.with_suffix(".metadata.lock")):
            state = State(self.path, self.generation, self.profile)
            yield state
            state.save()

    def read(self):
        with self.edit() as state:
            return dict(state.value)

    def sync(self, infos):
        with self.edit() as state:
            state.sync_children(lambda *args: {"targetInfos": infos})
            return dict(state.value)

    def create(self, endpoint, url="about:blank"):
        client = H.Client(endpoint)

        def register(tid):
            with self.edit() as state:
                state.add(tid)

        try:
            tid = H.new_tab(client.send, self.profile, url, on_created=register)
            info = client.send("Target.getTargetInfo", {"targetId": tid})["targetInfo"]
            with self.edit() as state:
                state.value["context"] = info["browserContextId"]
            return tid
        finally:
            client.close()


def allowed_targets(infos, state):
    context = state.get("context")
    allowed = {t["targetId"] for t in infos if t["targetId"] in state["owned"] and t.get("browserContextId") == context}
    while True:
        children = {
            t["targetId"]
            for t in infos
            if t["type"] == "iframe"
            and t.get("browserContextId") == context
            and (t.get("parentId") in allowed or t.get("parentFrameId") in allowed)
        }
        if children <= allowed:
            return allowed
        allowed.update(children)


class Relay:
    def __init__(self, upstream, downstream, store, endpoint):
        self.up, self.down, self.store, self.endpoint = upstream, downstream, store, endpoint
        self.pending, self.sessions, self.infos = {}, {}, {}
        self.pending_methods, self.dialogs = {}, set()
        self.counter = 0
        self.state = store.read()
        self.allowed = set()
        self.fill = None

    def refresh(self):
        self.state = self.store.sync(list(self.infos.values()))
        self.allowed = allowed_targets(list(self.infos.values()), self.state)

    async def rpc(self, method, params=None, sid=None):
        if sid in self.dialogs and method in ("Runtime.evaluate", "Runtime.callFunctionOn"):
            raise RuntimeError("A JavaScript dialog is open; use dialog accept/dismiss")
        self.counter += 1
        mid = self.counter
        future = asyncio.get_running_loop().create_future()
        self.pending[mid] = future
        self.pending_methods[mid] = (method, sid)
        msg = {"id": mid, "method": method, "params": params or {}}
        if sid:
            msg["sessionId"] = sid
        try:
            await self.up.send(json.dumps(msg))
            result = await asyncio.wait_for(future, 40)
            if "error" in result:
                raise RuntimeError(result["error"].get("message", "CDP failure"))
            return result.get("result", {})
        finally:
            self.pending.pop(mid, None)
            self.pending_methods.pop(mid, None)

    async def receive(self):
        try:
            async for raw in self.up:
                msg = json.loads(raw)
                if "id" in msg:
                    future = self.pending.get(msg["id"])
                    if future and not future.done():
                        future.set_result(msg)
                    continue
                method, params = msg.get("method", ""), msg.get("params", {})
                event_sid = msg.get("sessionId")
                if method == "Page.javascriptDialogOpening" and event_sid in self.sessions:
                    self.dialogs.add(event_sid)
                    for mid, (pending_method, pending_sid) in self.pending_methods.items():
                        future = self.pending[mid]
                        if (
                            pending_sid == event_sid
                            and pending_method in ("Runtime.evaluate", "Runtime.callFunctionOn")
                            and not future.done()
                        ):
                            future.set_exception(RuntimeError("A JavaScript dialog is open; use dialog accept/dismiss"))
                elif method == "Page.javascriptDialogClosed":
                    self.dialogs.discard(event_sid)
                info = params.get("targetInfo")
                if info:
                    self.infos[info["targetId"]] = info
                    self.refresh()
                if method == "Target.attachedToTarget":
                    tid = info["targetId"]
                    parent = self.sessions.get(msg.get("sessionId"))
                    # Workers attached beneath an owned page stay in that page's scope.
                    if tid in self.allowed or (parent in self.allowed and info["type"] in ("worker", "iframe")):
                        self.allowed.add(tid)
                        self.sessions[params["sessionId"]] = tid
                    else:
                        continue
                if method.startswith("Target."):
                    tid = (info or {}).get("targetId") or params.get("targetId")
                    if tid and tid not in self.allowed:
                        continue
                    if not tid and params.get("sessionId") not in self.sessions:
                        continue
                elif msg.get("sessionId") not in self.sessions:
                    continue  # No unsolicited global events, URLs or download metadata.
                await self.down.send(json.dumps(msg))
                if method == "Target.targetDestroyed":
                    self.infos.pop(params["targetId"], None)
                    self.allowed.discard(params["targetId"])
                elif method == "Target.detachedFromTarget":
                    self.sessions.pop(params["sessionId"], None)
        finally:
            for future in self.pending.values():
                if not future.done():
                    future.set_exception(RuntimeError("Helium connection closed; completion may be uncertain"))
            await self.down.close()

    async def dispatch(self, msg):
        method, p, sid = msg["method"], dict(msg.get("params") or {}), msg.get("sessionId")
        self.state = self.store.read()
        if sid is not None and sid not in self.sessions:
            raise RuntimeError("Session is not attached to an owned target")
        if method == "Runtime.callFunctionOn" and "".join(p.get("functionDeclaration", "").split()) == "".join(
            NATIVE_FILL_CLEAR.split()
        ):
            self.fill = None
            result = await self.rpc(method, dict(p, functionDeclaration=CHECKED_SELECT), sid)
            if result.get("exceptionDetails"):
                raise RuntimeError(
                    "fill: "
                    + result["exceptionDetails"].get("exception", {}).get("description", "Field cannot be edited")
                )
            self.fill = (sid, p["objectId"], result.get("result", {}).get("value", {}))
            return result
        if method == "Input.insertText" and self.fill:
            fill_sid, obj, info = self.fill
            self.fill = None
            text = p.get("text", "").replace("\r\n", "\n").replace("\r", "\n")
            if "\n" in text and not info.get("multiline"):
                raise RuntimeError("fill: single-line fields cannot preserve newlines")
            if text:
                result = await self.rpc(method, dict(p, text=text), sid)
            else:
                for kind in ("keyDown", "keyUp"):
                    await self.rpc(
                        "Input.dispatchKeyEvent",
                        {"type": kind, "key": "Backspace", "code": "Backspace", "windowsVirtualKeyCode": 8},
                        sid,
                    )
                result = {}
            value = await self.rpc(
                "Runtime.callFunctionOn",
                {"objectId": obj, "functionDeclaration": READ_TEXT, "returnByValue": True},
                fill_sid,
            )
            if value.get("exceptionDetails") or value.get("result", {}).get("value") != text:
                raise RuntimeError("fill: page did not retain the exact requested text; no automatic retry")
            return result
        if method == "Target.getTargets":
            result = await self.rpc(method, p)
            self.infos = {t["targetId"]: t for t in result["targetInfos"]}
            self.refresh()
            return {"targetInfos": [t for tid, t in self.infos.items() if tid in self.allowed]}
        if method == "Target.createTarget":
            if p.get("browserContextId", self.state.get("context")) != self.state.get("context"):
                raise RuntimeError("Cannot create a target in another profile")
            tid = await asyncio.to_thread(self.store.create, self.endpoint, p.get("url", "about:blank"))
            result = await self.rpc("Target.getTargetInfo", {"targetId": tid})
            self.infos[tid] = result["targetInfo"]
            self.refresh()
            return {"targetId": tid}
        if method in (
            "Target.attachToTarget",
            "Target.getTargetInfo",
            "Target.activateTarget",
            "Target.closeTarget",
            "Browser.getWindowForTarget",
        ):
            if p.get("targetId") not in self.allowed:
                raise RuntimeError("Target is not owned by this task")
            result = await self.rpc(method, p, sid)
            if method == "Target.attachToTarget":
                self.sessions[result["sessionId"]] = p["targetId"]
            return result
        if method == "Target.detachFromTarget":
            if p.get("sessionId") not in self.sessions:
                raise RuntimeError("Cannot detach an unowned session")
            result = await self.rpc(method, p)
            self.sessions.pop(p["sessionId"], None)
            return result
        if method == "Target.setDiscoverTargets":
            return await self.rpc(method, p)
        if method == "Target.setAutoAttach" and sid:
            # Attach only beneath this owned page, never at browser scope.
            # Shared/service workers can serve unrelated tabs; never pause them.
            p["filter"] = [
                {"type": "iframe", "exclude": False},
                {"type": "worker", "exclude": False},
                {"exclude": True},
            ]
            return await self.rpc(method, p, sid)
        if method == "Browser.getVersion":
            return await self.rpc(method, p)
        if method.startswith(("Target.", "Browser.", "Storage.", "SystemInfo.", "Tethering.")):
            raise RuntimeError("Browser-wide operation is unavailable in an owned Helium task: " + method)
        if not sid:
            raise RuntimeError("Page commands require an owned session")
        if method in (
            "Network.clearBrowserCookies",
            "Network.clearBrowserCache",
            "Network.getAllCookies",
            "Page.setDownloadBehavior",
            "Security.setIgnoreCertificateErrors",
        ):
            raise RuntimeError("Profile-wide cookie/cache operations are unavailable in an owned task")
        return await self.rpc(method, p, sid)

    async def answer(self, raw):
        msg = json.loads(raw)
        try:
            result = await self.dispatch(msg)
            response = {"id": msg["id"], "result": result}
        except (RuntimeError, OSError, ValueError, KeyError) as error:
            response = {"id": msg["id"], "error": {"code": -32000, "message": str(error)}}
        await self.down.send(json.dumps(response))

    async def run(self):
        reader = asyncio.create_task(self.receive())
        tasks = set()
        try:
            result = await self.rpc("Target.getTargets")
            self.infos = {t["targetId"]: t for t in result["targetInfos"]}
            self.refresh()
            async for raw in self.down:
                task = asyncio.create_task(self.answer(raw))
                tasks.add(task)
                task.add_done_callback(tasks.discard)
        finally:
            reader.cancel()
            for task in tasks:
                task.cancel()
            await asyncio.gather(reader, *tasks, return_exceptions=True)


async def serve(state_path, endpoint, profile, record):
    from websockets.asyncio.client import connect
    from websockets.asyncio.server import serve as ws_serve
    from websockets.exceptions import ConnectionClosed

    store = Store(state_path, endpoint.rsplit("/", 1)[-1], profile)
    token = uuid.uuid4().hex
    stop = asyncio.Event()
    last_client = time.monotonic()
    clients = 0

    async def handler(ws):
        nonlocal last_client, clients
        path = ws.request.path
        if path not in ("/" + token, "/" + token + "/health", "/" + token + "/stop"):
            await ws.close(code=1008)
            return
        if path.endswith("/health"):
            await ws.send("ready")
            return
        if path.endswith("/stop"):
            await ws.send("stopping")
            stop.set()
            return
        clients += 1
        try:
            async with connect(endpoint, max_size=32 * 1024 * 1024, open_timeout=5, proxy=None) as up:
                await Relay(up, ws, store, endpoint).run()
        except (ConnectionClosed, OSError, RuntimeError):
            await ws.close()
        finally:
            clients -= 1
            last_client = time.monotonic()

    async def idle():
        while not stop.is_set():
            await asyncio.sleep(10)
            if not clients and time.monotonic() - last_client > 60:
                stop.set()

    async with ws_serve(handler, "127.0.0.1", 0, max_size=32 * 1024 * 1024, origins=[None]) as server:
        H.atomic_json(
            record,
            {"url": f"ws://127.0.0.1:{server.sockets[0].getsockname()[1]}/{token}", "generation": store.generation},
        )
        watcher = asyncio.create_task(idle())
        try:
            await stop.wait()
        finally:
            watcher.cancel()
            await asyncio.gather(watcher, return_exceptions=True)


if __name__ == "__main__":
    asyncio.run(serve(*sys.argv[1:]))
