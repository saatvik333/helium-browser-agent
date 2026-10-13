"""Compare old/new fill algorithms inside one current owned Personal session."""

import json
import subprocess
import sys
import threading
import uuid
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "tests/live"))
from live_checks import Handler, ThreadingHTTPServer

server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
threading.Thread(target=server.serve_forever, daemon=True).start()
Path("artifacts").mkdir(exist_ok=True)
tag = "bench-" + uuid.uuid4().hex[:8]
argv = ["helium-browser-use", "--session", tag, "--personal"]
report = {
    "profile": "Personal",
    "date": __import__("datetime").date.today().isoformat(),
    "method": "Old installed fill_input body vs hardened helper; same owned HTTP fixture, alternating order, seven samples per size after warmup. Durations include an identical external exact-value read/check; exclude CLI startup and navigation. Old body uses current scoped transports, so this measures algorithm changes, not whole historical wrapper.",
}
source = (
    f"goto_url('http://127.0.0.1:{server.server_port}/');assert wait_for_load();assert wait_for_element('#text')\n"
    + r"""
import ast, json, statistics, time
from pathlib import Path
import browser_harness.helpers as helpers
assert helium_profile_of() == 'Personal'
tree = ast.parse(Path(helpers.__file__).read_text())
node = next(n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == 'fill_input')
old_namespace = dict(vars(helpers))
exec(compile(ast.Module(body=[node], type_ignores=[]), '<original-fill>', 'exec'), old_namespace)
functions = {'old': old_namespace['fill_input'], 'new': fill_input}
read = 'document.querySelector("#text").value'
for fun in functions.values():
    fun('#text', 'warmup')
    assert js(read) == 'warmup'
rows = []
for size in [32, 512, 2048]:
    value = ('Abc 123!?-' * ((size+9)//10))[:size]
    samples = {key: [] for key in functions}
    for iteration in range(7):
        order = ['old', 'new'] if iteration % 2 == 0 else ['new', 'old']
        for key in order:
            start = time.perf_counter()
            functions[key]('#text', value)
            assert js(read) == value, key
            samples[key].append(time.perf_counter()-start)
    medians = {key: statistics.median(values) for key, values in samples.items()}
    rows.append({'characters': size, 'seconds': samples, 'median_seconds': medians, 'speedup': medians['old']/medians['new'], 'exact_value_all_samples': True})
correctness = []
for name, value in [('multiline', 'first\nsecond'), ('literal_tab', 'first\tsecond'), ('unicode', 'é हिंदी 😀')]:
    results = {}
    for key, fun in functions.items():
        try:
            fun('#text', value)
            actual = js(read)
            results[key] = {'exact': actual == value, 'actual': actual}
        except Exception as exc:
            results[key] = {'exact': False, 'error': str(exc)}
    correctness.append({'case': name, 'expected': value, 'results': results})
print(json.dumps({'timings': rows, 'correctness': correctness}))
"""
)
try:
    result = subprocess.run(argv, input=source, text=True, capture_output=True, timeout=180)
    if result.returncode:
        raise RuntimeError((result.stdout + result.stderr)[-3000:])
    report.update(json.loads(result.stdout.strip().splitlines()[-1]))
finally:
    closed = subprocess.run(argv + ["--close"], text=True, capture_output=True, timeout=30)
    report["cleanup_exit_code"] = closed.returncode
    report["cleanup"] = closed.stdout.strip()
    server.shutdown()
    Path("artifacts/performance-results.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report, indent=2))
