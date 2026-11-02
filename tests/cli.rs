//! Configuration, daemon and endpoint behavior that needs no browser. Each test gets its own
//! runtime, config and data directories, so its daemon is private and tests run in parallel.

use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

const LOCAL_STATE: &str = r#"{"profile": {
    "info_cache": {"Default": {"name": "Personal"}, "Profile 3": {"name": "Modo - Work"}},
    "last_used": "Default", "last_active_profiles": ["Default"]}}"#;

/// Python for a fake browser that only stays alive.
const IDLE: &str = "import time; time.sleep(60)";

/// Python for a fake browser answering `/json/version` on its `--remote-debugging-port`; it
/// creates the file named by its second argument once it listens.
const VERSION_SERVER: &str = "
import http.server, json, sys
port = int(sys.argv[1].split('=')[1])
body = json.dumps({'webSocketDebuggerUrl': f'ws://127.0.0.1:{port}/devtools/browser/abc'}).encode()
class Version(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        self.send_response(200)
        self.send_header('Content-Length', str(len(body)))
        self.end_headers()
        self.wfile.write(body)
    def log_message(self, *args):
        pass
server = http.server.HTTPServer(('127.0.0.1', port), Version)
open(sys.argv[2], 'w').close()
server.serve_forever()
";

/// Python for a fake browser in approval mode. It publishes its port in `DevToolsActivePort` in
/// the data dir (first argument) and holds every WebSocket handshake, as a browser does until its
/// user clicks Allow, until the file `allow` appears in the control dir (second argument). Then
/// it completes them, creates `allowed`, and answers every command (`Browser.getVersion` with
/// product `Fake/1`). The file `connections` counts the connections it accepted.
const APPROVAL_BROWSER: &str = r"
import base64, hashlib, json, os, socket, struct, sys, threading, time
data, control = sys.argv[1], sys.argv[2]
server = socket.socket()
server.bind(('127.0.0.1', 0))
server.listen()

def serve(conn):
    request = b''
    while b'\r\n\r\n' not in request:
        chunk = conn.recv(4096)
        if not chunk:
            return
        request += chunk
    while not os.path.exists(os.path.join(control, 'allow')):
        time.sleep(0.02)
    key = next(line.split(b':', 1)[1].strip() for line in request.split(b'\r\n')
               if line.lower().startswith(b'sec-websocket-key:'))
    accept = base64.b64encode(hashlib.sha1(key + b'258EAFA5-E914-47DA-95CA-C5AB0DC85B11').digest())
    conn.sendall(b'HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n'
                 b'Sec-WebSocket-Accept: ' + accept + b'\r\n\r\n')
    open(os.path.join(control, 'allowed'), 'w').close()
    frames = conn.makefile('rb')
    while True:
        head = frames.read(2)
        if len(head) < 2 or head[0] & 15 == 8:
            return
        length = head[1] & 127
        if length == 126:
            length = struct.unpack('>H', frames.read(2))[0]
        elif length == 127:
            length = struct.unpack('>Q', frames.read(8))[0]
        mask = frames.read(4)
        payload = bytes(byte ^ mask[i % 4] for i, byte in enumerate(frames.read(length)))
        if head[0] & 15 != 1:
            continue
        command = json.loads(payload)
        result = {'product': 'Fake/1'} if command['method'] == 'Browser.getVersion' else {}
        reply = json.dumps({'id': command['id'], 'result': result}).encode()
        size = struct.pack('B', len(reply)) if len(reply) < 126 else struct.pack('>BH', 126, len(reply))
        conn.sendall(b'\x81' + size + reply)

path = os.path.join(data, 'DevToolsActivePort')
with open(path + '.tmp', 'w') as file:
    file.write(f'{server.getsockname()[1]}\n/devtools/browser/abc\n')
os.rename(path + '.tmp', path)
count = 0
while True:
    conn = server.accept()[0]
    count += 1
    with open(os.path.join(control, 'connections'), 'w') as file:
        file.write(str(count))
    threading.Thread(target=serve, args=(conn,), daemon=True).start()
";

/// Temporary directories for one test, the daemon it starts and any fake browser; all removed
/// on drop.
struct Home {
    root: PathBuf,
    browser: Option<Child>,
}

impl Home {
    fn new(name: &str) -> Self {
        let root = env::temp_dir().join(format!("hba-cli-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["runtime", "config/hba", "data"] {
            fs::create_dir_all(root.join(dir)).expect("create test directories");
        }
        fs::write(root.join("data/Local State"), LOCAL_STATE).expect("write Local State");
        Self { root, browser: None }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    fn write_config(&self, text: &str) {
        fs::write(self.path("config/hba/config.toml"), text).expect("write config");
    }

    /// Runs hba with this home's directories plus `vars`.
    fn run(&self, args: &[&str], vars: &[(&str, &Path)]) -> Output {
        self.command(args, vars).stdin(Stdio::null()).output().expect("run hba")
    }

    /// Starts hba in the background, like [`Home::run`].
    fn spawn(&self, args: &[&str], vars: &[(&str, &Path)]) -> Child {
        let mut command = self.command(args, vars);
        command.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        command.spawn().expect("start hba")
    }

    fn command(&self, args: &[&str], vars: &[(&str, &Path)]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_hba"));
        command.args(args).env("XDG_RUNTIME_DIR", self.path("runtime")).env("XDG_CONFIG_HOME", self.path("config"));
        for name in [
            "HBA_CONFIG",
            "HBA_BROWSER",
            "HBA_USER_DATA_DIR",
            "HBA_LAUNCHER",
            "HBA_HUMANIZE",
            "HBA_CDP",
            "HBA_TRACE",
            "HBA_SESSION",
            "HBA_PROFILE",
        ] {
            command.env_remove(name);
        }
        for (name, value) in vars {
            command.env(name, value);
        }
        command
    }

    /// Waits until the daemon log (with `HBA_TRACE`) has a line containing `text`.
    fn wait_for_log(&self, text: &str) {
        let log = self.path("runtime/hba/daemon.log");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fs::read_to_string(&log).is_ok_and(|log| log.lines().any(|line| line.contains(text))) {
            assert!(Instant::now() < deadline, "no {text:?} in the daemon log");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn ok(&self, args: &[&str], vars: &[(&str, &Path)]) -> String {
        let output = self.run(args, vars);
        assert!(output.status.success(), "hba {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn fails(&self, args: &[&str], vars: &[(&str, &Path)]) -> String {
        let output = self.run(args, vars);
        assert!(!output.status.success(), "hba {args:?} unexpectedly succeeded");
        String::from_utf8_lossy(&output.stderr).into_owned()
    }

    /// Starts a process named `process` that holds this home's data dir, as its `SingletonLock`
    /// says: Python running `code` with `args`. False when Python is not installed.
    fn fake_browser(&mut self, process: &str, code: &str, args: &[&str]) -> bool {
        let paths = env::var_os("PATH").unwrap_or_default();
        let Some(python) = env::split_paths(&paths).map(|dir| dir.join("python3")).find(|path| path.is_file()) else {
            eprintln!("skipped: no python3 for a fake browser");
            return false;
        };
        let program = self.path(process);
        std::os::unix::fs::symlink(python, &program).expect("link fake browser");
        let child = Command::new(&program)
            .arg("-c")
            .arg(code)
            .args(args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start fake browser");
        let host = fs::read_to_string("/proc/sys/kernel/hostname").expect("hostname");
        std::os::unix::fs::symlink(format!("{}-{}", host.trim(), child.id()), self.path("data/SingletonLock"))
            .expect("write SingletonLock");
        self.browser = Some(child);
        true
    }

    /// Whether the fake browser is still running.
    fn fake_alive(&mut self) -> bool {
        self.browser.as_mut().is_some_and(|browser| matches!(browser.try_wait(), Ok(None)))
    }

    /// Waits until `path` exists, while the fake browser runs.
    fn wait_for_file(&mut self, path: &Path) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(Instant::now() < deadline && self.fake_alive(), "{} never appeared", path.display());
            thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = self.run(&["daemon", "stop"], &[]);
        if let Some(mut browser) = self.browser.take() {
            let _ = browser.kill();
            let _ = browser.wait();
        }
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn profiles_are_read_from_local_state_without_a_browser() {
    let home = Home::new("profiles");
    let data = home.path("data");
    let listing = home.ok(&["profiles"], &[("HBA_USER_DATA_DIR", &data)]);
    assert!(listing.contains("Personal (Default) [open, last used]"), "{listing}");
    assert!(listing.contains("Modo - Work (Profile 3)"), "{listing}");
}

#[test]
fn the_config_file_sets_the_data_dir_and_aliases() {
    let home = Home::new("file");
    home.write_config(&format!("user_data_dir = {:?}\n[aliases]\nwork = \"Modo - Work\"\n", home.path("data")));
    let listing = home.ok(&["profiles"], &[]);
    assert!(listing.contains("Modo - Work (Profile 3) -p work"), "{listing}");
}

#[test]
fn invalid_config_is_reported_by_the_command() {
    let home = Home::new("invalid");
    home.write_config("bogus = 1\n");
    let unknown = home.fails(&["profiles"], &[]);
    assert!(unknown.contains("invalid config") && unknown.contains("bogus"), "{unknown}");
    // The same daemon picks up the corrected file.
    home.write_config("default_profile = 5\n");
    let wrong_type = home.fails(&["profiles"], &[]);
    assert!(wrong_type.contains("invalid type"), "{wrong_type}");
    home.write_config(&format!("user_data_dir = {:?}\n", home.path("data")));
    assert!(home.ok(&["profiles"], &[]).contains("Personal (Default)"));
}

#[test]
fn a_missing_explicit_config_file_is_an_error() {
    let home = Home::new("explicit");
    let missing = home.path("nope.toml");
    let error = home.fails(&["profiles"], &[("HBA_CONFIG", &missing)]);
    assert!(error.contains("HBA_CONFIG"), "{error}");
}

#[test]
fn a_daemon_started_with_other_settings_is_replaced() {
    let home = Home::new("takeover");
    let data = home.path("data");
    home.ok(&["profiles"], &[("HBA_USER_DATA_DIR", &data)]);
    // A client with a different data directory must not be served by the running daemon.
    let other = home.path("other");
    fs::create_dir_all(&other).expect("create other data dir");
    let listing = home.ok(&["profiles"], &[("HBA_USER_DATA_DIR", &other)]);
    assert!(!listing.contains("Modo - Work"), "{listing}");
}

#[test]
fn a_fixed_debugging_port_is_found_from_the_browser_command_line() {
    // Helium writes no DevToolsActivePort for a fixed port; the port comes from its flags. The
    // port is picked before the fake binds it, so a parallel test may take it first: retry.
    for _ in 0..5 {
        let mut home = Home::new("fixed");
        let data = home.path("data");
        let listening = home.path("listening");
        let port = TcpListener::bind("127.0.0.1:0").expect("bind").local_addr().expect("address").port();
        let flag = format!("--remote-debugging-port={port}");
        if !home.fake_browser("helium", VERSION_SERVER, &[&flag, listening.to_str().expect("utf-8 path")]) {
            return;
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while !listening.exists() && home.fake_alive() {
            assert!(Instant::now() < deadline, "the fake Helium never listened");
            thread::sleep(Duration::from_millis(20));
        }
        if !listening.exists() {
            continue; // It could not bind the port.
        }
        let url = home.ok(&["browser", "endpoint"], &[("HBA_USER_DATA_DIR", &data)]);
        assert_eq!(url.trim(), format!("ws://127.0.0.1:{port}/devtools/browser/abc"));
        return;
    }
    panic!("no free port for the fake Helium after 5 tries");
}

#[test]
fn an_endpoint_owned_by_another_process_is_refused() {
    let mut home = Home::new("owner");
    let data = home.path("data");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    fs::write(data.join("DevToolsActivePort"), format!("{port}\n/devtools/browser/abc\n")).expect("write port file");
    if !home.fake_browser("helium", IDLE, &["--remote-debugging-port=0"]) {
        return;
    }
    let error = home.fails(&["browser", "endpoint"], &[("HBA_USER_DATA_DIR", &data)]);
    assert!(error.contains("is not owned by Helium"), "{error}");
}

#[test]
fn an_endpoint_listening_beyond_loopback_is_refused() {
    let mut home = Home::new("exposed");
    let data = home.path("data");
    let listener = TcpListener::bind("0.0.0.0:0").expect("bind");
    let port = listener.local_addr().expect("address").port();
    fs::write(data.join("DevToolsActivePort"), format!("{port}\n/devtools/browser/abc\n")).expect("write port file");
    if !home.fake_browser("helium", IDLE, &["--remote-debugging-port=0"]) {
        return;
    }
    let error = home.fails(&["browser", "endpoint"], &[("HBA_USER_DATA_DIR", &data)]);
    assert!(error.contains("beyond loopback"), "{error}");
}

#[test]
fn no_browser_holding_the_data_dir_means_not_running() {
    let home = Home::new("absent");
    let data = home.path("data");
    // A port file left by an earlier run is not trusted without its browser.
    fs::write(data.join("DevToolsActivePort"), "9\n/devtools/browser/abc\n").expect("write port file");
    let error = home.fails(&["browser", "endpoint"], &[("HBA_USER_DATA_DIR", &data)]);
    assert!(error.contains("Helium is not running"), "{error}");
}

#[test]
fn the_configured_browser_kind_names_the_process() {
    let mut home = Home::new("kind");
    let data = home.path("data");
    if !home.fake_browser("helium", IDLE, &["--remote-debugging-port=0"]) {
        return;
    }
    let error =
        home.fails(&["browser", "endpoint"], &[("HBA_USER_DATA_DIR", &data), ("HBA_BROWSER", Path::new("chrome"))]);
    assert!(error.contains("Chrome is not running with user data dir"), "{error}");
}

#[test]
fn chrome_on_its_default_data_dir_ignores_the_flag() {
    let mut home = Home::new("chrome-default");
    // The home's data dir stands in for Chrome's default one under XDG_CONFIG_HOME.
    std::os::unix::fs::symlink(home.path("data"), home.path("config/google-chrome")).expect("link data dir");
    if !home.fake_browser("chrome", IDLE, &["--remote-debugging-port=9222"]) {
        return;
    }
    let error = home.fails(&["browser", "endpoint"], &[("HBA_BROWSER", Path::new("chrome"))]);
    assert!(error.contains("Chrome ignores --remote-debugging-port on its default user data dir"), "{error}");
    assert!(error.contains("chrome://inspect/#remote-debugging"), "{error}");
}

#[test]
fn a_browser_without_remote_debugging_says_how_to_turn_it_on() {
    let mut home = Home::new("off");
    let data = home.path("data");
    if !home.fake_browser("helium", IDLE, &[]) {
        return;
    }
    let error = home.fails(&["browser", "endpoint"], &[("HBA_USER_DATA_DIR", &data)]);
    assert!(error.contains("Helium runs without remote debugging"), "{error}");
    assert!(error.contains("--remote-debugging-port=9222"), "{error}");
    assert!(error.contains("chrome://inspect/#remote-debugging"), "{error}");
}

/// A home with an [`APPROVAL_BROWSER`] that has published its port; `None` without Python.
fn approval_home(name: &str) -> Option<Home> {
    let mut home = Home::new(name);
    let (data, control) = (home.path("data"), home.path("control"));
    fs::create_dir_all(&control).expect("create control dir");
    let args = [data.to_str().expect("utf-8 path"), control.to_str().expect("utf-8 path")];
    if !home.fake_browser("chrome", APPROVAL_BROWSER, &args) {
        return None;
    }
    home.wait_for_file(&data.join("DevToolsActivePort"));
    Some(home)
}

#[test]
fn approval_mode_keeps_one_request_open_and_takes_a_late_allow() {
    let Some(mut home) = approval_home("approval") else { return };
    let (data, control) = (home.path("data"), home.path("control"));
    let vars = [("HBA_USER_DATA_DIR", data.as_path()), ("HBA_BROWSER", Path::new("chrome"))];
    // Without a flag, the endpoint comes from the port file, verified like any other.
    let url = home.ok(&["browser", "endpoint"], &vars);
    assert!(url.starts_with("ws://127.0.0.1:") && url.trim().ends_with("/devtools/browser/abc"), "{url}");
    // The connection waits for the user's Allow for the command's timeout, not a fixed one.
    let error = home.fails(&["--timeout", "1", "browser", "launch"], &vars);
    assert!(error.contains("Chrome did not allow the DevTools connection within 1s; click Allow"), "{error}");
    // The next command waits for the same request: one dialog, not one per command.
    let report = home.ok(&["--timeout", "1", "doctor"], &vars);
    assert!(report.contains("owned by Chrome pid") && report.contains("approval mode"), "{report}");
    assert!(report.contains("✗ connect: Chrome did not allow"), "{report}");
    let status = home.ok(&["daemon", "status"], &vars);
    assert!(status.contains("waiting for Allow in Chrome's dialog"), "{status}");
    let connections = || fs::read_to_string(control.join("connections")).expect("read the connection count");
    assert_eq!(connections(), "1");
    // An Allow after every command gave up still connects, and the next command uses it.
    fs::write(control.join("allow"), "").expect("allow");
    home.wait_for_file(&control.join("allowed"));
    let status = home.ok(&["daemon", "status"], &vars);
    assert!(status.contains("Chrome allowed the connection, which the next command takes"), "{status}");
    let launched = home.ok(&["--timeout", "1", "browser", "launch"], &vars);
    assert!(launched.starts_with("Fake/1 pid "), "{launched}");
    assert_eq!(connections(), "1");
}

#[test]
fn a_daemon_stopped_while_its_request_waits_stays_for_the_answer() {
    let Some(home) = approval_home("stop-pending") else { return };
    let (data, control) = (home.path("data"), home.path("control"));
    let vars = [("HBA_USER_DATA_DIR", data.as_path()), ("HBA_BROWSER", Path::new("chrome"))];
    home.fails(&["--timeout", "1", "browser", "launch"], &vars);
    // Closing the request would leave its dialog in front of the next daemon's, where Allow
    // connects nothing.
    let stopping = home.ok(&["daemon", "stop"], &vars);
    assert!(stopping.contains("daemon stopping once you answer Chrome's"), "{stopping}");
    home.wait_for_log("waiting for the answer to the approval request before exiting");
    fs::write(control.join("allow"), "").expect("allow");
    // The next daemon starts once the old one has its answer, asks once and connects.
    let launched = home.ok(&["--timeout", "5", "browser", "launch"], &vars);
    assert!(launched.starts_with("Fake/1 pid "), "{launched}");
    assert_eq!(fs::read_to_string(control.join("connections")).expect("read the connection count"), "2");
    assert!(!home.path("runtime/hba/abandoned-approval").exists(), "no request was left unanswered");
}

#[test]
fn closing_without_a_browser_never_starts_one() {
    let home = Home::new("close");
    let data = home.path("data");
    let launcher = home.path("launcher-must-not-run");
    let closed = home.ok(&["close"], &[("HBA_USER_DATA_DIR", &data), ("HBA_LAUNCHER", &launcher)]);
    assert!(closed.contains("nothing to close"), "{closed}");
    let status = home.ok(&["status"], &[("HBA_USER_DATA_DIR", &data), ("HBA_LAUNCHER", &launcher)]);
    assert!(status.contains("Helium is not running"), "{status}");
}

#[test]
fn page_commands_without_a_tab_never_start_helium() {
    let home = Home::new("notab");
    let data = home.path("data");
    let launcher = home.path("launcher-must-not-run");
    let error =
        home.fails(&["-s", "fresh", "get", "url"], &[("HBA_USER_DATA_DIR", &data), ("HBA_LAUNCHER", &launcher)]);
    assert!(error.contains("session \"fresh\" has no tab"), "{error}");
}

#[test]
fn closing_while_helium_is_away_keeps_the_session_for_later() {
    let home = Home::new("keep");
    let data = home.path("data");
    let state = r#"{"sessions": {"task": {"tabs": [{"id": "AAA"}], "current": "AAA"}}}"#;
    fs::create_dir_all(home.path("runtime/hba")).expect("create runtime dir");
    fs::write(home.path("runtime/hba/state.json"), state).expect("write state");
    let error = home.fails(&["-s", "task", "close"], &[("HBA_USER_DATA_DIR", &data)]);
    assert!(error.contains("session state kept"), "{error}");
    let kept = fs::read_to_string(home.path("runtime/hba/state.json")).expect("read state");
    assert!(kept.contains("AAA"), "{kept}");
}

#[test]
fn a_timeout_longer_than_a_day_is_refused() {
    let home = Home::new("timeout");
    let error = home.fails(&["--timeout", "1e20", "wait", "1"], &[]);
    assert!(error.contains("at most 86400"), "{error}");
    assert!(home.ok(&["--timeout", "86400", "wait", "1"], &[]).contains("waited 1 ms"));
}

#[test]
fn waits_and_key_delays_are_bounded() {
    let home = Home::new("bounds");
    // Refused at once; accepted, it would hold the session for over a day.
    let mut client = home.spawn(&["wait", "86400001"], &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while client.try_wait().expect("poll hba").is_none() {
        if Instant::now() >= deadline {
            client.kill().expect("kill hba");
            panic!("`wait 86400001` was accepted");
        }
        thread::sleep(Duration::from_millis(20));
    }
    let refused = client.wait_with_output().expect("collect hba");
    let error = String::from_utf8_lossy(&refused.stderr);
    assert!(!refused.status.success() && error.contains("wait at most 86400000 ms"), "{error}");
    let error = home.fails(&["type", "x", "--delay", "10001"], &[]);
    assert!(error.contains("10001 is not in 0..=10000"), "{error}");
}

#[test]
fn script_lines_cannot_carry_global_options() {
    let home = Home::new("script");
    let script = home.path("script.hba");
    for line in ["wait 1 --json", "--timeout 5 wait 1", "wait 1 -s other", "-T t1 wait 1"] {
        fs::write(&script, format!("wait 1\n{line}\n")).expect("write script");
        let error = home.fails(&["run", script.to_str().expect("utf-8 path")], &[]);
        assert!(error.contains("line 2: put -s/-p/-T/--json/--timeout on the `hba run` command"), "{line}: {error}");
    }
    // A session from the environment belongs to the `run` command, not to its lines.
    fs::write(&script, "wait 1\n").expect("write script");
    let ran = home.ok(&["run", script.to_str().expect("utf-8 path")], &[("HBA_SESSION", Path::new("from-env"))]);
    assert!(ran.contains("waited 1 ms"), "{ran}");
}

#[test]
fn a_client_that_goes_away_frees_its_session() {
    let home = Home::new("hangup");
    let trace = [("HBA_TRACE", Path::new("1"))];
    let mut client = home.spawn(&["-s", "held", "wait", "60000"], &trace);
    home.wait_for_log("held Wait(WaitArgs { what: Some(\"60000\")");
    client.kill().expect("kill the client");
    client.wait().expect("reap the client");
    let started = Instant::now();
    assert!(home.ok(&["-s", "held", "wait", "1"], &trace).contains("waited 1 ms"));
    assert!(started.elapsed() < Duration::from_secs(5), "the session stayed busy for {:?}", started.elapsed());
}

#[test]
fn a_new_daemon_takes_over_after_the_old_one_finishes_its_commands() {
    let home = Home::new("drain");
    let trace = [("HBA_TRACE", Path::new("1"))];
    // Longer than a client waits for a daemon to start.
    let busy = home.spawn(&["-s", "busy", "wait", "7000"], &trace);
    home.wait_for_log("busy Wait(WaitArgs { what: Some(\"7000\")");
    // Other settings restart the daemon while the old one still runs the command.
    assert!(home.ok(&["-s", "other", "wait", "1"], &[]).contains("waited 1 ms"));
    let finished = busy.wait_with_output().expect("wait for the busy client");
    assert!(String::from_utf8_lossy(&finished.stdout).contains("waited 7000 ms"), "{finished:?}");
}

#[test]
fn the_fallback_runtime_dir_must_be_private() {
    let home = Home::new("fallback");
    let tmp = home.path("tmp");
    let base = tmp.join(format!("hba-{}", fs::metadata("/proc/self").expect("own process").uid()));
    let refused = |what: &str| {
        let mut command = home.command(&["profiles"], &[("TMPDIR", &tmp)]);
        let output = command.env_remove("XDG_RUNTIME_DIR").stdin(Stdio::null()).output().expect("run hba");
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(!output.status.success() && error.contains("must be a directory of yours"), "{what}: {error}");
    };
    // Another user could have created it, or swapped it for a link, before hba ran.
    fs::create_dir_all(&base).expect("create base");
    fs::set_permissions(&base, fs::Permissions::from_mode(0o755)).expect("loosen base");
    refused("a shared directory");
    fs::remove_dir(&base).expect("remove base");
    let elsewhere = home.path("elsewhere");
    fs::create_dir_all(&elsewhere).expect("create link target");
    fs::set_permissions(&elsewhere, fs::Permissions::from_mode(0o700)).expect("restrict link target");
    std::os::unix::fs::symlink(&elsewhere, &base).expect("link base");
    refused("a symlink");
}
