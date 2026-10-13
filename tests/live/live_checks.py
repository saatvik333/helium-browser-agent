"""Personal-only local integration tests for the installed Helium wrapper."""

import json
import sys
import os
import subprocess
import tempfile
import threading
import time
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from helium_browser_agent import helium_cdp as H

HTML = """<!doctype html><html><head><meta charset="utf-8"><title>Helium hardening fixture</title>
<style>body{background:rgb(23,74,51);color:white}input,textarea{display:block}#pad{height:3000px}</style></head><body>
<label>Name<input id="name"></label><textarea id="text"></textarea>
<input id="readonly" readonly value="unchanged"><input id="disabled" disabled>
<div id="edit" contenteditable="true">old</div><input id="upload" type="file">
<button id="apply">Apply</button><output id="out"></output>
<a id="popup" target="_blank" href="/popup">Open fixture popup</a>
<div id="shadow"></div><iframe src="/frame" id="frame"></iframe><div id="pad"></div>
<script>
window.inputEvents=[];
document.querySelector('#name').addEventListener('input',e=>inputEvents.push({trusted:e.isTrusted,value:e.target.value}));
document.querySelector('#apply').onclick=async e=>{
 const data=await (await fetch('/api')).json();
 document.querySelector('#out').textContent=document.querySelector('#name').value+':'+data.ok;
 document.querySelector('#out').dataset.trusted=String(e.isTrusted);
};
document.querySelector('#shadow').attachShadow({mode:'open'}).innerHTML='<button id="inner">Shadow</button>';
</script></body></html>"""


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        body = (
            b'{"ok":true}'
            if self.path == "/api"
            else (b'<h1 id="frame-content">Frame fixture</h1>' if self.path == "/frame" else HTML.encode())
        )
        self.send_response(200)
        self.send_header("Content-Type", "application/json" if self.path == "/api" else "text/html; charset=utf-8")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


def main():
    Path("artifacts").mkdir(exist_ok=True)
    os.environ["BH_TELEMETRY"] = "0"
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    url = f"http://127.0.0.1:{server.server_port}/"
    tag = "hardening-" + uuid.uuid4().hex[:8]
    sessions = set()
    owned = set()
    reports = []
    client = H.Client(H.ws_url())

    def invoke(name, source="", close=False, timeout=30):
        sessions.add(name)
        argv = ["helium-browser-use", "--session", name, "--personal"] + (["--close"] if close else [])
        start = time.monotonic()
        result = subprocess.run(argv, input=source, text=True, capture_output=True, timeout=timeout, check=False)
        state = H.CACHE_DIR / "harness" / (name + ".json")
        if state.exists():
            owned.update(json.loads(state.read_text())["owned"])
        if result.returncode:
            raise AssertionError((result.stdout + result.stderr)[-2500:])
        return result.stdout.strip(), round(time.monotonic() - start, 3)

    def check(label, source, name=tag):
        output, elapsed = invoke(name, source)
        row = {"check": label, "seconds": elapsed, "result": output or "PASS"}
        reports.append(row)
        print(json.dumps(row), flush=True)
        return output

    try:
        check(
            "profile, owned blank recovery",
            "assert helium_profile_of()=='Personal'\nassert len(list_tabs())==1\nassert ensure_real_tab()['targetId']==current_tab()['targetId']",
        )
        check(
            "HTTP fixture ready",
            f'goto_url({url!r});assert wait_for_load();assert wait_for_element("#apply");assert js("navigator.webdriver") is False',
        )
        check(
            "literal text, native input event and append",
            """
text='first\\nsecond\\tend é हिंदी 😀'
fill_input('#text',text)
assert js('document.querySelector("#text").value')==text
fill_input('#text','\\nappend',clear_first=False)
assert js('document.querySelector("#text").value')==text+'\\nappend'
fill_input('#text','')
assert js('document.querySelector("#text").value')==''
fill_input('#name','Audit - !? é हिंदी 😀')
assert js('inputEvents.length>0 && inputEvents.every(e=>e.trusted)')
fill_input('#edit','first\\nsecond\\tend')
assert input_value('#edit')=='first\\nsecond\\tend'
for text in ['', 'first\\n', '\\nsecond', 'first\\n\\nlast\\tend']:
    fill_input('#edit',text)
    assert input_value('#edit')==text
for selector in ['#readonly','#disabled','#missing','#apply']:
    try: fill_input(selector,'must not enter')
    except RuntimeError: pass
    else: raise AssertionError('invalid field accepted: '+selector)
assert js('document.querySelector("#readonly").value')=='unchanged'
""",
        )
        check(
            "trusted click and network",
            """
cdp('Network.enable');drain_events()
nodes=cdp('Accessibility.getFullAXTree')['nodes']
node=next(n for n in nodes if n.get('role',{}).get('value')=='button' and n.get('name',{}).get('value')=='Apply')
q=cdp('DOM.getBoxModel',backendNodeId=node['backendDOMNodeId'])['model']['content']
click_at_xy(sum(q[0::2])/4,sum(q[1::2])/4)
assert wait_for_element('#out[data-trusted="true"]',timeout=3)
assert js('document.querySelector("#out").textContent')=='Audit - !? é हिंदी 😀:true'
assert any(e.get('method')=='Network.responseReceived' and e['params']['response']['url'].endswith('/api') for e in drain_events())
""",
        )
        other = tag + "-sentinel"
        sentinel = check("separate Personal sentinel", "print(current_tab()['targetId'])", other)
        check(
            "unowned convenience operations rejected",
            f"""
for action in (switch_tab,activate_tab,close_tab):
    try: action({sentinel!r})
    except RuntimeError as e: assert 'not owned' in str(e)
    else: raise AssertionError('unowned target accepted')
""",
        )
        check(
            "failed navigation rolls back and restores target",
            """
old=current_tab()['targetId'];before={t['targetId'] for t in list_tabs()}
try: new_tab('http://[')
except RuntimeError: pass
else: raise AssertionError('invalid URL accepted')
assert current_tab()['targetId']==old
assert {t['targetId'] for t in list_tabs()}==before
""",
        )
        check(
            "popup discovered through opener",
            """
before={t['targetId'] for t in list_tabs()}
q=js('(()=>{const r=document.querySelector("#popup").getBoundingClientRect();return {x:r.x+r.width/2,y:r.y+r.height/2}})()')
click_at_xy(q['x'],q['y'])
import time
deadline=time.monotonic()+3
while time.monotonic()<deadline and len(list_tabs())==len(before): time.sleep(.05)
assert len(list_tabs())==len(before)+1,'popup not attributed'
""",
        )
        check(
            "iframe, shadow DOM and JS exception",
            """
assert js('document.querySelector("#frame").contentDocument.querySelector("#frame-content").textContent')=='Frame fixture'
assert js('document.querySelector("#shadow").shadowRoot.querySelector("#inner").textContent')=='Shadow'
try: js('throw new Error("expected fixture error")')
except RuntimeError: pass
else: raise AssertionError('JS error swallowed')
""",
        )
        check(
            "cross-origin iframe with owned ancestry",
            f"""
js("document.querySelector('#frame').src='http://localhost:{server.server_port}/frame'")
import time
deadline=time.monotonic()+4
frame=None
while time.monotonic()<deadline:
    frame=iframe_target('localhost:{server.server_port}/frame')
    if frame: break
    time.sleep(.05)
assert frame,'cross-origin target missing'
assert js('document.querySelector("#frame-content").textContent',target_id=frame)=='Frame fixture'
""",
        )
        check(
            "background screenshot and scroll",
            """
import base64,time
png=base64.b64decode(cdp('Page.captureScreenshot',format='png',captureBeyondViewport=False,_response_timeout=6)['data'])
assert png.startswith(b'\\x89PNG\\r\\n\\x1a\\n') and len(png)>1000
before=js('scrollY')
scroll(100,100,dy=500)
deadline=time.monotonic()+2
while time.monotonic()<deadline and js('scrollY')<=before: time.sleep(.05)
assert js('scrollY')>before
scroll(100,100,dy=-500)
""",
        )
        with tempfile.TemporaryDirectory(prefix="helium-upload-fixture-") as temp:
            fixture = Path(temp) / "fixture.txt"
            fixture.write_text("fixture upload")
            check(
                "file upload",
                f'upload_file("#upload",{str(fixture)!r});assert js("document.querySelector(\\"#upload\\").files[0].name")=="fixture.txt"',
            )
        check(
            "native dialog metadata and accept",
            """
js('setTimeout(()=>{document.body.dataset.answer=String(confirm("Helium fixture confirmation"))},0)')
import time
deadline=time.monotonic()+3
while time.monotonic()<deadline:
    info=page_info()
    if info.get('dialog'): break
    time.sleep(.05)
assert info.get('dialog',{}).get('message')=='Helium fixture confirmation'
cdp('Page.handleJavaScriptDialog',accept=True)
assert wait_for_element('body[data-answer="true"]',timeout=3)
""",
        )
        check(
            "lost page recovers only into owned page",
            """
close_tab()
recovered=ensure_real_tab()
assert recovered['targetId'] in {t['targetId'] for t in list_tabs()}
assert helium_profile_of()=='Personal'
""",
        )
        subprocess.run(
            [sys.executable, "-m", "browser_harness.run", "--reload"],
            env=dict(os.environ, BU_NAME="hba-" + tag),
            capture_output=True,
            check=True,
            timeout=25,
        )
        check("daemon restart and tab reuse", "assert helium_profile_of()=='Personal'\nassert len(list_tabs())>=1")
        check(
            "telemetry and cloud sync remain disabled",
            """
from browser_harness import telemetry
import os
assert not telemetry.is_enabled()
assert os.environ['ANONYMIZED_TELEMETRY']=='false' and os.environ['BROWSER_USE_CLOUD_SYNC']=='false'
""",
        )
    finally:
        errors = []
        for name in sessions:
            try:
                invoke(name, close=True)
            except Exception as error:  # noqa: BLE001 - cleanup must continue for every test session
                errors.append(str(error))
        remaining = owned & {t["targetId"] for t in H.targets(client.send)}
        cleanup = {"check": "cleanup", "owned_pages_remaining": len(remaining), "errors": errors}
        reports.append(cleanup)
        print(json.dumps(cleanup), flush=True)
        client.close()
        server.shutdown()
        Path("artifacts/live-results.json").write_text(json.dumps(reports, indent=2) + "\n")
        if remaining or errors:
            raise AssertionError("Cleanup was incomplete")


if __name__ == "__main__":
    main()
