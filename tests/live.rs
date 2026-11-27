//! End-to-end tests against a throwaway headless browser. They never touch the user's browser.
//!
//! Run with `cargo test --test live -- --ignored`. `HBA_HELIUM` overrides the browser binary
//! (default `/opt/helium-browser-bin/helium`); for another browser set `HBA_BROWSER` too, e.g.
//! `HBA_BROWSER=chrome HBA_HELIUM=/opt/google/chrome/chrome`. The test is ignored by default, so
//! a plain `cargo test` reports it as ignored instead of passing without a browser.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs, thread};

/// A headless browser, a fixture web server and an isolated hba daemon; torn down on drop.
struct Rig {
    root: PathBuf,
    site: String,
    browser: Child,
}

impl Rig {
    fn start() -> Self {
        let root = env::temp_dir().join(format!("hba-live-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        for dir in ["profile/Default", "runtime", "config", "downloads"] {
            fs::create_dir_all(root.join(dir)).expect("create test directories");
        }
        // Downloads stay inside the rig instead of the user's download folder.
        let preferences = format!(
            r#"{{"download": {{"default_directory": "{}", "prompt_for_download": false}}}}"#,
            root.join("downloads").display()
        );
        fs::write(root.join("profile/Default/Preferences"), preferences).expect("write browser preferences");
        let helium = env::var("HBA_HELIUM").unwrap_or_else(|_| "/opt/helium-browser-bin/helium".to_owned());
        let browser = Command::new(helium)
            .args(["--headless=new", "--no-first-run", "--no-default-browser-check", "--remote-debugging-port=0"])
            .arg(format!("--user-data-dir={}", root.join("profile").display()))
            // Headless Chromium paints background tabs at ~1 fps; the user's headful browser does not.
            .args([
                "--disable-background-timer-throttling",
                "--disable-backgrounding-occluded-windows",
                "--disable-renderer-backgrounding",
            ])
            // `remote.example` reaches the fixture server but is a website to hba, not a local page.
            .arg("--host-resolver-rules=MAP *.example 127.0.0.1")
            .arg("about:blank")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start the headless browser (set HBA_HELIUM)");
        let site = serve(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/site"));
        // Built before waiting so a startup failure still tears the browser down.
        let rig = Self { root, site, browser };
        let port_file = rig.root.join("profile/DevToolsActivePort");
        let deadline = Instant::now() + Duration::from_secs(20);
        while !port_file.exists() {
            assert!(Instant::now() < deadline, "the browser did not open a DevTools port");
            thread::sleep(Duration::from_millis(50));
        }
        rig.settle();
        rig
    }

    /// Waits until two fresh tabs in a row open and follow a link. While a fresh profile's
    /// bundled uBlock Origin starts, or after developer mode reloads it, it holds the requests
    /// it sees and then reloads their tabs, swallowing navigations.
    fn settle(&self) {
        let deadline = Instant::now() + Duration::from_secs(90);
        let mut settled = 0;
        while settled < 2 {
            assert!(Instant::now() < deadline, "navigations kept failing in the fresh profile");
            let opened = self.raw("warmup", &["--timeout", "5", "open", &self.url("index.html")]);
            let clicked = self.raw("warmup", &["--timeout", "5", "click", "text=Second page"]);
            self.raw("warmup", &["close"]);
            let moved = String::from_utf8_lossy(&clicked.stdout).contains("page2.html");
            settled = if opened.status.success() && moved { settled + 1 } else { 0 };
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.site)
    }

    fn raw(&self, session: &str, args: &[&str]) -> Output {
        self.raw_with(session, args, &[])
    }

    /// Like `raw`, with more variables for the daemon (a daemon with other variables is replaced).
    fn raw_with(&self, session: &str, args: &[&str], vars: &[(&str, &str)]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_hba"))
            .args(["-s", session])
            .args(args)
            .env("HBA_USER_DATA_DIR", self.root.join("profile"))
            .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            // If the rig's browser is gone, hba must fail instead of starting the user's browser.
            .env("HBA_LAUNCHER", self.root.join("launcher-must-not-run"))
            // The daemon log carries the commands' progress, which tests wait for.
            .env("HBA_TRACE", "1")
            .env_remove("HBA_CONFIG")
            .env_remove("HBA_HUMANIZE")
            .env_remove("HBA_SESSION")
            .env_remove("HBA_PROFILE")
            .env_remove("HBA_CDP")
            .envs(vars.iter().copied())
            .stdin(Stdio::null())
            .output()
            .expect("run hba")
    }

    /// Waits until the daemon log has a line containing every one of `parts`.
    fn wait_for_log(&self, parts: &[&str]) {
        let log = self.root.join("runtime/hba/daemon.log");
        let deadline = Instant::now() + Duration::from_secs(10);
        while !fs::read_to_string(&log)
            .is_ok_and(|log| log.lines().any(|line| parts.iter().all(|part| line.contains(part))))
        {
            assert!(Instant::now() < deadline, "no log line with {parts:?}");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// Runs hba and returns stdout, failing with stderr when the command fails.
    fn ok(&self, session: &str, args: &[&str]) -> String {
        self.ok_with(session, args, &[])
    }

    fn ok_with(&self, session: &str, args: &[&str], vars: &[(&str, &str)]) -> String {
        let output = self.raw_with(session, args, vars);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        assert!(output.status.success(), "hba {args:?} failed: {}{stdout}", String::from_utf8_lossy(&output.stderr));
        stdout
    }

    /// Runs hba expecting failure and returns stderr.
    fn fails(&self, session: &str, args: &[&str]) -> String {
        let output = self.raw(session, args);
        assert!(
            !output.status.success(),
            "hba {args:?} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        String::from_utf8_lossy(&output.stderr).into_owned()
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        if thread::panicking() {
            // The daemon log (per-command timings) explains most failures.
            let log = fs::read_to_string(self.root.join("runtime/hba/daemon.log")).unwrap_or_default();
            eprintln!("--- daemon.log ---\n{log}");
        }
        // A closed browser stops its helper processes, which a killed one leaves writing into the
        // profile while it is removed.
        let _ = self.raw("-", &["cdp", "--browser", "Browser.close"]);
        let _ = self.raw("-", &["daemon", "stop"]);
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(self.browser.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
        }
        let _ = self.browser.kill();
        let _ = self.browser.wait();
        for _ in 0..20 {
            let _ = fs::remove_dir_all(&self.root);
            if !self.root.exists() {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
    }
}

/// Minimal fixture server (GET only): files, plus `/no-content` (204), `/redirect?to=URL` (302),
/// `/attachment/FILE` (the file as a download) and `/slow/FILE` (the file after 300 ms).
fn serve(dir: PathBuf) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture server");
    let address = format!("http://127.0.0.1:{}", listener.local_addr().expect("address").port());
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let dir = dir.clone();
            thread::spawn(move || respond(stream, &dir));
        }
    });
    address
}

fn respond(mut stream: TcpStream, dir: &Path) {
    let mut request = String::new();
    if BufReader::new(&stream).read_line(&mut request).is_err() {
        return;
    }
    let target = request.split_whitespace().nth(1).unwrap_or("/");
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let mut headers = String::new();
    let (status, body, kind) = if path == "/no-content" {
        ("204 No Content", Vec::new(), "text/plain")
    } else if let Some(to) = query.strip_prefix("to=").filter(|_| path == "/redirect") {
        headers = format!("Location: {to}\r\n");
        ("302 Found", Vec::new(), "text/plain")
    } else {
        let name = if let Some(name) = path.strip_prefix("/attachment/") {
            headers = format!("Content-Disposition: attachment; filename=\"{name}\"\r\n");
            name
        } else if let Some(name) = path.strip_prefix("/slow/") {
            // A website's answer takes a network round trip; the fixture server's would not.
            thread::sleep(Duration::from_millis(300));
            name
        } else {
            path.trim_start_matches('/')
        };
        let file = dir.join(name);
        match fs::read(&file) {
            Ok(body) if !path.contains("..") => {
                let kind = match file.extension().and_then(|e| e.to_str()) {
                    Some("html") => "text/html; charset=utf-8",
                    Some("js") => "text/javascript",
                    Some("json") => "application/json",
                    _ => "application/octet-stream",
                };
                ("200 OK", body, kind)
            }
            _ => ("404 Not Found", b"not found".to_vec(), "text/plain"),
        }
    };
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n{headers}Connection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes()).and_then(|()| stream.write_all(&body));
}

/// The ref of the first snapshot line containing `needle`.
fn ref_of(snapshot: &str, needle: &str) -> String {
    let line =
        snapshot.lines().find(|line| line.contains(needle)).unwrap_or_else(|| panic!("{needle:?} not in:\n{snapshot}"));
    let start = line.find("[ref=").expect("line has a ref") + 5;
    line[start..].split(']').next().expect("ref closes").to_owned()
}

#[test]
#[ignore = "needs a browser: cargo test --test live -- --ignored"]
fn live() {
    let rig = Rig::start();
    open_snapshot_click_and_navigate(&rig);
    fill_type_and_submit(&rig);
    dialogs_block_until_answered(&rig);
    covered_elements_and_late_content(&rig);
    popups_join_the_session_and_close_with_it(&rig);
    frames_are_stitched_and_actionable(&rig);
    cross_process_frames_are_stitched_and_actionable(&rig);
    uploads_reach_inputs_and_file_choosers(&rig);
    drags_move_html5_items_and_pointer_handles(&rig);
    downloads_are_saved_where_asked(&rig);
    console_and_errors_report_what_the_page_did(&rig);
    network_log_bodies_blocking_and_har(&rig);
    performance_reports(&rig);
    errors_from_local_pages_are_reported(&rig);
    screenshots_keep_the_page_as_it_was(&rig);
    labels_land_on_their_elements(&rig);
    permission_overrides_go_with_the_last_tab(&rig);
    extension_pages_let_go_of_websites(&rig);
    extensions_are_developed_from_the_shell(&rig);
    humanized_input_selects_fills_and_runs_in_frames(&rig);
    mocks_answer_cross_origin_preflights(&rig);
    scripts_run_many_commands_in_one_call(&rig);
    pointer_and_form_controls(&rig);
    keyboard_and_prompts(&rig);
    scrolling_and_waiting(&rig);
    chords_offsets_and_waits_that_outlast_documents(&rig);
    states_drags_and_interrupted_typing(&rig);
    sessions_do_not_share_tabs(&rig);
    adopted_tabs_are_released_not_closed(&rig);
    navigations_that_never_commit_return_at_once(&rig);
    console_capture_stays_off_on_websites(&rig);
    history_restores_pages_without_repeating_their_console(&rig);
}

/// Runs hba with `args` until its output contains `needle` (at most 10 s) and returns it.
fn until(rig: &Rig, session: &str, args: &[&str], needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let output = rig.ok(session, args);
        if output.contains(needle) || Instant::now() > deadline {
            assert!(output.contains(needle), "{args:?} never printed {needle:?}: {output}");
            return output;
        }
        thread::sleep(Duration::from_millis(50));
    }
}

/// The id of the extension a `ext load` line reports.
fn extension_id(loaded: &str) -> String {
    loaded
        .split_whitespace()
        .find(|word| word.len() == 32 && word.bytes().all(|c| (b'a'..=b'p').contains(&c)))
        .unwrap_or_else(|| panic!("no extension id in {loaded:?}"))
        .to_owned()
}

/// The `#log` line of the controls fixture.
fn log(rig: &Rig, session: &str) -> String {
    rig.ok(session, &["text", "#log"]).trim().to_owned()
}

fn mocks_answer_cross_origin_preflights(rig: &Rig) {
    let s = "cors";
    rig.ok(s, &["open", &rig.url("routes-cors.html")]);
    // The fixture server is `localhost` too: another origin, so the JSON POST with a custom
    // header needs a CORS preflight, which the mock must allow.
    let api = rig.url("api/items").replace("127.0.0.1", "localhost");
    rig.ok(s, &["net", "mock", "*://localhost:*/api/*", "--body", r#"{"ok":true}"#]);
    assert_eq!(rig.ok(s, &["js", &format!("callApi({api:?})")]).trim(), r#"200 {"ok":true}"#);
    let requests = rig.ok(s, &["net"]);
    assert!(requests.contains("OPTIONS") && requests.contains("→ 204"), "{requests}");
    rig.ok(s, &["close"]);
}

fn states_drags_and_interrupted_typing(rig: &Rig) {
    let s = "states";
    rig.ok(s, &["open", &rig.url("controls.html")]);
    // A missing element is hidden; a selector matching several elements is an error, not an answer.
    assert_eq!(rig.ok(s, &["is", "hidden", "#missing"]).trim(), "true");
    assert_eq!(rig.ok(s, &["is", "visible", "#missing"]).trim(), "false");
    let ambiguous = rig.fails(s, &["is", "hidden", "button"]);
    assert!(ambiguous.contains("matched"), "{ambiguous}");
    let not_gone = rig.fails(s, &["--timeout", "1", "wait", "button", "--gone"]);
    assert!(not_gone.contains("matched"), "{not_gone}");
    // Pointer moves between `mouse down` and `mouse up` are drags.
    let track =
        "addEventListener('mousemove', e => document.getElementById('log').textContent = 'buttons ' + e.buttons)";
    rig.ok(s, &["js", track]);
    rig.ok(s, &["mouse", "down", "5", "5"]);
    rig.ok(s, &["mouse", "move", "40", "40"]);
    assert_eq!(log(rig, s), "buttons 1");
    rig.ok(s, &["mouse", "up", "40", "40"]);
    rig.ok(s, &["mouse", "move", "60", "60"]);
    assert_eq!(log(rig, s), "buttons 0");
    // Typing stops at a dialog and says how much was left.
    let stop = "document.getElementById('keys').addEventListener('keyup', e => e.key === 'b' && confirm('Stop?'))";
    rig.ok(s, &["js", stop]);
    let stopped = rig.fails(s, &["type", "abcd", "--into", "#keys"]);
    assert!(stopped.contains("2 of 4 characters were not typed"), "{stopped}");
    rig.ok(s, &["dialog", "dismiss"]);
    assert_eq!(rig.ok(s, &["get", "value", "#keys"]).trim(), "ab");
    rig.ok(s, &["close"]);
}

fn pointer_and_form_controls(rig: &Rig) {
    let s = "controls";
    rig.ok(s, &["open", &rig.url("controls.html")]);
    rig.ok(s, &["hover", "#hover"]);
    assert_eq!(log(rig, s), "hovered");
    rig.ok(s, &["click", "#double", "--count", "2"]);
    assert_eq!(log(rig, s), "double clicked");
    rig.ok(s, &["click", "#context", "--button", "right"]);
    assert_eq!(log(rig, s), "context menu");
    rig.ok(s, &["click", "#modifier", "--modifiers", "Control"]);
    assert_eq!(log(rig, s), "control click");
    rig.ok(s, &["select", "#plan", "Pro"]);
    assert_eq!(log(rig, s), "plan pro");
    rig.ok(s, &["select", "#plan", "team"]);
    assert_eq!(rig.ok(s, &["get", "value", "#plan"]).trim(), "team");
    let missing = rig.fails(s, &["select", "#plan", "Enterprise"]);
    assert!(missing.contains("no option matching Enterprise"), "{missing}");
    rig.ok(s, &["check", "internal:label=\"Accept terms\"i"]);
    assert_eq!(log(rig, s), "terms true");
    let again = rig.ok(s, &["check", "#terms"]);
    assert!(again.contains("already checked"), "{again}");
    rig.ok(s, &["uncheck", "#terms"]);
    assert_eq!(rig.ok(s, &["is", "checked", "#terms"]).trim(), "false");
    rig.ok(s, &["check", "role=radio[name=\"Large\"]"]);
    assert_eq!(log(rig, s), "size large");
    assert_eq!(rig.ok(s, &["is", "enabled", "#off"]).trim(), "false");
    assert_eq!(rig.ok(s, &["is", "visible", "#hidden"]).trim(), "false");
    let disabled = rig.fails(s, &["--timeout", "1", "click", "#off"]);
    assert!(disabled.contains("is not enabled"), "{disabled}");
    let readonly = rig.fails(s, &["--timeout", "1", "fill", "#fixed", "new"]);
    assert!(readonly.contains("editable"), "{readonly}");
    assert_eq!(rig.ok(s, &["get", "value", "#fixed"]).trim(), "fixed");
    // A field that changes what was typed says so.
    let short = rig.ok(s, &["fill", "#short", "abcdef"]);
    assert!(short.contains("now reads \"abc\""), "{short}");
    rig.ok(s, &["close"]);
}

fn keyboard_and_prompts(rig: &Rig) {
    let s = "keys";
    rig.ok(s, &["open", &rig.url("controls.html")]);
    rig.ok(s, &["type", "abc", "--into", "#keys"]);
    rig.ok(s, &["press", "Control+a"]);
    assert_eq!(log(rig, s), "key Control+a");
    rig.ok(s, &["press", "Backspace"]);
    assert_eq!(rig.ok(s, &["get", "value", "#keys"]).trim(), "");
    rig.ok(s, &["click", "#ask"]);
    let shown = rig.ok(s, &["dialog"]);
    assert!(shown.contains("prompt \"Name?\" (default \"anon\")"), "{shown}");
    // A page behind a dialog answers nothing; commands must say so at once.
    let started = Instant::now();
    let blocked = rig.fails(s, &["--timeout", "10", "text", "#log"]);
    assert!(blocked.contains("prompt dialog is open"), "{blocked}");
    assert!(started.elapsed() < Duration::from_secs(5), "blocked command took {:?}", started.elapsed());
    rig.ok(s, &["dialog", "accept", "Ada"]);
    assert_eq!(log(rig, s), "hello Ada");
    rig.ok(s, &["close"]);
}

fn scrolling_and_waiting(rig: &Rig) {
    let s = "scroll";
    rig.ok(s, &["open", &rig.url("controls.html")]);
    let page = rig.ok(s, &["scroll", "down", "400"]);
    assert!(page.contains("y=400/"), "{page}");
    let inner = rig.ok(s, &["scroll", "down", "300", "-e", "#box"]);
    assert!(inner.contains("y=300/"), "{inner}");
    rig.ok(s, &["scroll", "top"]);
    let into = rig.ok(s, &["scroll", "-e", "#far", "--into-view"]);
    assert!(into.contains("into view"), "{into}");
    rig.ok(s, &["click", "#far"]);
    assert_eq!(log(rig, s), "far clicked");
    rig.ok(s, &["js", "setTimeout(() => document.title = 'Later', 300)"]);
    rig.ok(s, &["wait", "--until", "document.title === 'Later'"]);
    rig.ok(s, &["js", "setTimeout(() => document.getElementById('hover').remove(), 300)"]);
    rig.ok(s, &["wait", "#hover", "--gone"]);
    rig.ok(s, &["js", "setTimeout(() => log('eventually'), 300)"]);
    rig.ok(s, &["wait", "--text", "eventually"]);
    rig.ok(s, &["js", "setTimeout(() => history.pushState({}, '', '/done'), 300)"]);
    rig.ok(s, &["wait", "--url", "*/done"]);
    let never = rig.fails(s, &["--timeout", "1", "wait", "text=Never shown"]);
    assert!(never.contains("Never shown"), "{never}");
    rig.ok(s, &["close"]);
}

fn chords_offsets_and_waits_that_outlast_documents(rig: &Rig) {
    let s = "edges";
    rig.ok(s, &["open", &rig.url("input-edges.html")]);
    // A chord with an unknown key fails before any key goes down, so no modifier stays held.
    for keys in [&["press", "Control+Typo"][..], &["press", "a", "Control+Typo"]] {
        let typo = rig.fails(s, keys);
        assert!(typo.contains("unknown key \"Typo\""), "{typo}");
    }
    assert_eq!(rig.ok(s, &["js", "keys.join() || 'none'"]).trim(), "none");
    rig.ok(s, &["press", "Control+a"]);
    assert_eq!(rig.ok(s, &["js", "keys.join()"]).trim(), "keydown Control,keydown a,keyup a,keyup Control");
    // `--at` counts from the element's top-left corner, also when that is scrolled out of view.
    rig.ok(s, &["scroll", "down", "1000"]);
    rig.ok(s, &["click", "#tall", "--at", "10,10"]);
    assert_eq!(log(rig, s), "at 10,10");
    // A poll stuck behind a busy renderer still ends at the wait's deadline. The loop starts later
    // than the `js` call that sets it up, or that call would wait it out instead.
    rig.ok(s, &["js", "setTimeout(() => { const end = Date.now() + 4000; while (Date.now() < end); }, 300)"]);
    let started = Instant::now();
    let busy = rig.fails(s, &["--timeout", "1", "wait", "#never"]);
    assert!(busy.contains("#never did not appear within 1s"), "{busy}");
    assert!(started.elapsed() < Duration::from_secs(3), "the wait took {:?}", started.elapsed());
    // Waits outlast navigations: a poll the old document cannot answer asks the new one.
    let until = "location.pathname.endsWith('/page2.html') \
                 || new Promise(() => setTimeout(() => location.href = '/page2.html', 100))";
    rig.ok(s, &["wait", "--until", until]);
    rig.ok(s, &["js", "setTimeout(() => location.href = '/hops.html?left=8', 50)"]);
    rig.ok(s, &["wait", "--text", "Arrived"]);
    rig.ok(s, &["close"]);
}

fn adopted_tabs_are_released_not_closed(rig: &Rig) {
    let s = "adopt";
    // The rig's start page stands in for a tab the user opened.
    let all = rig.ok(s, &["tabs", "--all"]);
    let user_tab = all
        .lines()
        .find(|line| line.contains("about:blank") && !line.contains("hba:"))
        .and_then(|line| line.split_whitespace().next())
        .unwrap_or_else(|| panic!("no user tab in:\n{all}"))
        .to_owned();
    let refused = rig.fails(s, &["tab", "close", &user_tab]);
    assert!(refused.contains("not in session"), "{refused}");
    let adopted = rig.ok(s, &["tab", "use", &user_tab]);
    assert!(adopted.contains("current") && adopted.contains("adopted"), "{adopted}");
    rig.ok(s, &["open", &rig.url("index.html")]);
    rig.ok(s, &["tab", "new", &rig.url("page2.html")]);
    assert!(rig.ok(s, &["get", "url"]).contains("page2.html"));
    assert!(rig.ok(s, &["-T", &user_tab, "get", "url"]).contains("index.html"));
    // The handover is saved to disk, so a new daemon still knows the tab is the session's.
    rig.ok(s, &["daemon", "stop"]);
    assert!(rig.ok(s, &["-T", &user_tab, "get", "url"]).contains("index.html"));
    // A release queues behind the session's running command, then takes the tab away.
    let released = thread::scope(|scope| {
        let waiting = scope.spawn(|| rig.ok(s, &["-T", &user_tab, "wait", "1500"]));
        rig.wait_for_log(&["adopt Wait", " started"]);
        let started = Instant::now();
        let released = rig.ok(s, &["tab", "release", &user_tab]);
        assert!(started.elapsed() > Duration::from_millis(800), "release did not wait: {released}");
        waiting.join().expect("waiting command");
        released
    });
    assert!(released.contains("released"), "{released}");
    let refused = rig.fails(s, &["-T", &user_tab, "get", "url"]);
    assert!(refused.contains("not in session"), "{refused}");
    rig.ok(s, &["tab", "use", &user_tab]);
    let closed = rig.ok(s, &["close"]);
    assert!(closed.contains("closed 1 tab, released 1"), "{closed}");
    let left = rig.ok(s, &["tabs", "--all"]);
    assert!(left.contains("index.html") && !left.contains("page2.html"), "{left}");
}

fn open_snapshot_click_and_navigate(rig: &Rig) {
    let opened = rig.ok("nav", &["open", &rig.url("index.html")]);
    assert!(opened.contains("\"Fixture home\""), "{opened}");
    let snap = rig.ok("nav", &["snap", "-i"]);
    assert!(snap.contains("button \"Open popup\""), "{snap}");
    let link = ref_of(&snap, "link \"Second page\"");
    let clicked = rig.ok("nav", &["click", &link]);
    assert!(clicked.contains("→ ") && clicked.contains("page2.html"), "{clicked}");
    // Refs from the previous document never resolve in the new one.
    let stale = rig.fails("nav", &["click", &link]);
    assert!(stale.contains("earlier page") || stale.contains("not on the page"), "{stale}");
    // Back and forward restore from the back/forward cache, which fires no load events.
    let started = Instant::now();
    let back = rig.ok("nav", &["--timeout", "10", "back"]);
    assert!(back.contains("index.html"), "{back}");
    let forward = rig.ok("nav", &["--timeout", "10", "forward"]);
    assert!(forward.contains("page2.html"), "{forward}");
    assert!(started.elapsed() < Duration::from_secs(5), "history took {:?}", started.elapsed());
    rig.ok("nav", &["close"]);
}

fn fill_type_and_submit(rig: &Rig) {
    let user = "internal:label=\"User\"";
    rig.ok("form", &["open", &rig.url("index.html")]);
    rig.ok("form", &["fill", user, "alice"]);
    let typed = rig.ok("form", &["type", " smith", "--into", user]);
    assert!(typed.contains("typed 6 characters"), "{typed}");
    assert_eq!(rig.ok("form", &["get", "value", user]).trim(), "alice smith");
    let refused = rig.fails("form", &["fill", user, "two\nlines"]);
    assert!(refused.contains("single-line"), "{refused}");
    rig.ok("form", &["fill", "#editor", "line one\nline two"]);
    assert_eq!(rig.ok("form", &["get", "value", "#editor"]).trim(), "line one\nline two");
    rig.ok("form", &["press", "Enter", "--into", user]);
    assert_eq!(rig.ok("form", &["text", "#status"]).trim(), "submitted alice smith");
    rig.ok("form", &["close"]);
}

fn dialogs_block_until_answered(rig: &Rig) {
    rig.ok("dlg", &["open", &rig.url("index.html")]);
    let alert = rig.ok("dlg", &["click", "#alert"]);
    assert!(alert.contains("alert \"Saved!\" (accepted)"), "{alert}");
    let confirm = rig.ok("dlg", &["click", "#confirm"]);
    assert!(confirm.contains("confirm dialog"), "{confirm}");
    let blocked = rig.fails("dlg", &["text", "#status"]);
    assert!(blocked.contains("dialog is open"), "{blocked}");
    rig.ok("dlg", &["dialog", "accept"]);
    assert_eq!(rig.ok("dlg", &["text", "#status"]).trim(), "confirmed");
    rig.ok("dlg", &["close"]);
}

fn covered_elements_and_late_content(rig: &Rig) {
    rig.ok("cover", &["open", &rig.url("index.html")]);
    let covered = rig.fails("cover", &["--timeout", "2", "click", "#covered"]);
    assert!(covered.contains("intercepts pointer events"), "{covered}");
    rig.ok("cover", &["click", "#overlay"]);
    rig.ok("cover", &["click", "#covered"]);
    assert_eq!(rig.ok("cover", &["text", "#status"]).trim(), "covered clicked");
    rig.ok("cover", &["click", "#later"]);
    rig.ok("cover", &["click", "text=Appeared"]);
    assert_eq!(rig.ok("cover", &["text", "#status"]).trim(), "appeared clicked");
    rig.ok("cover", &["close"]);
}

fn popups_join_the_session_and_close_with_it(rig: &Rig) {
    rig.ok("pop", &["open", &rig.url("index.html")]);
    let clicked = rig.ok("pop", &["click", "#popup"]);
    assert!(clicked.contains("+ popup t"), "{clicked}");
    let tabs = rig.ok("pop", &["tabs"]);
    assert_eq!(tabs.lines().count(), 2, "{tabs}");
    let closed = rig.ok("pop", &["close"]);
    assert!(closed.contains("closed 2 tabs"), "{closed}");
}

fn frames_are_stitched_and_actionable(rig: &Rig) {
    rig.ok("frame", &["open", &rig.url("page2.html")]);
    let snap = rig.ok("frame", &["snap"]);
    let button = ref_of(&snap, "button \"Inner button\"");
    assert!(button.starts_with('f'), "{snap}");
    rig.ok("frame", &["click", &button]);
    let diff = rig.ok("frame", &["snap", "-d"]);
    assert!(diff.contains("+ ") && diff.contains("inner clicked"), "{diff}");
    rig.ok("frame", &["close"]);
}

fn errors_from_local_pages_are_reported(rig: &Rig) {
    rig.ok("err", &["open", &rig.url("index.html")]);
    let broken = rig.ok("err", &["click", "#broken"]);
    assert!(broken.contains("boom from fixture"), "{broken}");
    assert!(broken.contains("missing.json → 404"), "{broken}");
    rig.ok("err", &["close"]);
}

/// Pixel size from a `shot` line: `PATH (WIDTHxHEIGHT, SIZE)`.
fn shot_size(line: &str) -> (u32, u32) {
    let inside = line.split_once(" (").and_then(|(_, rest)| rest.split_once(',')).map(|(size, _)| size);
    let (width, height) = inside.and_then(|size| size.split_once('x')).unwrap_or_else(|| panic!("no size in {line:?}"));
    (width.parse().expect("width"), height.parse().expect("height"))
}

fn screenshots_keep_the_page_as_it_was(rig: &Rig) {
    let s = "shot";
    rig.ok(s, &["open", &rig.url("media-shot.html")]);
    // The visible viewport: the window's inner size without scrollbars.
    let size = "[visualViewport.width, visualViewport.height].map(Math.round).join('x')";
    let viewport = rig.ok(s, &["js", size]).trim().to_owned();
    let dom = "document.documentElement.outerHTML + document.adoptedStyleSheets.length";
    let before = rig.ok(s, &["js", dom]);
    let dir = rig.root.join("shots");
    let path = |name: &str| dir.join(name).to_str().expect("utf-8 path").to_owned();
    let shot = shot_size(&rig.ok(s, &["shot", &path("view.png")]));
    assert_eq!(format!("{}x{}", shot.0, shot.1), viewport);
    let full = shot_size(&rig.ok(s, &["shot", "--full", &path("full.png")]));
    assert!(full.1 > 2000, "full page is {full:?}");
    assert_eq!(shot_size(&rig.ok(s, &["shot", "-e", "#card", &path("card.png")])), (356, 133));
    let labelled = rig.ok(s, &["shot", "-a", &path("labels.png")]);
    assert!(labelled.contains("elements labelled"), "{labelled}");
    // A capture beyond the viewport resizes the page while it runs; afterwards it is as before.
    assert_eq!(rig.ok(s, &["js", size]).trim(), viewport);
    assert_eq!(rig.ok(s, &["js", dom]), before, "shots leave the page's DOM as it was");
    rig.ok(s, &["close"]);
}

/// RGB of a PNG at whole-pixel `points`, decoded in the page.
fn pixels(rig: &Rig, session: &str, image: &Path, points: &[(f64, f64)]) -> Vec<[i64; 3]> {
    use base64::Engine as _;
    let data = base64::engine::general_purpose::STANDARD.encode(fs::read(image).expect("read the image"));
    let points: Vec<String> = points.iter().map(|(x, y)| format!("[{x}, {y}]")).collect();
    let code = format!(
        "(async () => {{ const image = new Image(); image.src = 'data:image/png;base64,{data}'; await image.decode(); \
           const g = new OffscreenCanvas(image.width, image.height).getContext('2d'); g.drawImage(image, 0, 0); \
           return [{}].map(([x, y]) => Array.from(g.getImageData(x, y, 1, 1).data.slice(0, 3)).join(' ')).join(' '); }})()",
        points.join(", ")
    );
    let channels: Vec<i64> =
        rig.ok(session, &["js", &code]).split_whitespace().map(|value| value.parse().expect("a channel")).collect();
    channels.chunks(3).map(|rgb| [rgb[0], rgb[1], rgb[2]]).collect()
}

fn labels_land_on_their_elements(rig: &Rig) {
    let s = "labels";
    rig.ok(s, &["open", &rig.url("media-shot.html")]);
    let shot = rig.root.join("shots/labelled-card.png");
    let labelled = rig.ok(s, &["shot", "-a", "-e", "#card", shot.to_str().expect("utf-8 path")]);
    assert!(labelled.contains("2 elements labelled"), "{labelled}");
    // The image starts at the card's whole-pixel corner.
    let boxes = "['#card', '#save'].map(s => { const r = document.querySelector(s).getBoundingClientRect(); \
                 return [r.x, r.y, r.width, r.height].join(' '); }).join(' ')";
    let numbers: Vec<f64> =
        rig.ok(s, &["js", boxes]).split_whitespace().map(|value| value.parse().expect("a number")).collect();
    let (x, y) = (numbers[4] - numbers[0].floor(), numbers[5] - numbers[1].floor());
    let (width, height) = (numbers[6], numbers[7]);
    // The box's left edge, the ref label above the button, and the button's middle.
    let points = [
        ((x + 1.0).floor(), (y + height / 2.0).floor()),
        ((x + 1.0).floor(), (y - 13.0).floor()),
        ((x + width / 2.0).floor(), (y + height / 2.0).floor()),
    ];
    let colors = pixels(rig, s, &shot, &points);
    let label_red = |rgb: [i64; 3]| (rgb[0] - 200).abs() < 16 && (rgb[1] - 16).abs() < 16 && (rgb[2] - 79).abs() < 16;
    assert!(label_red(colors[0]) && label_red(colors[1]) && !label_red(colors[2]), "{points:?}: {colors:?}");
    rig.ok(s, &["close"]);
}

fn permission_overrides_go_with_the_last_tab(rig: &Rig) {
    let state = |session: &str, name: &str| {
        let query = format!("navigator.permissions.query({{name: '{name}'}}).then(status => status.state)");
        rig.ok(session, &["js", &query]).trim().to_owned()
    };
    let own = rig.url("index.html");
    let other = own.replace("127.0.0.1", "localhost");
    let other_origin = rig.site.replace("127.0.0.1", "localhost");
    // The profile's own settings; a fresh profile allows background sync.
    let probe = "perms-before";
    rig.ok(probe, &["open", &own]);
    let geolocation = state(probe, "geolocation");
    rig.ok(probe, &["open", &other]);
    let sync = state(probe, "background-sync");
    assert_eq!(sync, "granted", "background sync is allowed by default");
    rig.ok(probe, &["close"]);
    // Two tabs of the profile hold grants, one for another origin.
    rig.ok("perms-a", &["open", &own]);
    rig.ok("perms-a", &["perms", "grant", "geolocation"]);
    rig.ok("perms-a", &["perms", "grant", "background-sync", "--origin", &other_origin]);
    assert_eq!(state("perms-a", "geolocation"), "granted");
    rig.ok("perms-b", &["open", &rig.url("page2.html")]);
    rig.ok("perms-b", &["perms", "grant", "notifications"]);
    // Overrides apply to the whole profile, so they stay while a tab hba holds keeps one.
    rig.ok("perms-a", &["close"]);
    let probe = "perms-after";
    rig.ok(probe, &["open", &own]);
    assert_eq!(state(probe, "geolocation"), "granted");
    // With the last one gone the profile's own settings apply again, not "ask" overrides.
    rig.ok("perms-b", &["close"]);
    rig.ok(probe, &["open", "-n", &own]);
    assert_eq!(state(probe, "geolocation"), geolocation);
    rig.ok(probe, &["open", "-n", &other]);
    assert_eq!(state(probe, "background-sync"), sync);
    rig.ok(probe, &["close"]);
}

fn extension_pages_let_go_of_websites(rig: &Rig) {
    let s = "ext";
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension");
    let loaded = rig.ok(s, &["ext", "load", dir.to_str().expect("utf-8 path")]);
    let id = extension_id(&loaded);
    rig.ok(s, &["ext", "open", &id, "options.html"]);
    let capturing = rig.ok(s, &["ext", "logs", &id]);
    assert!(capturing.contains("1 page"), "{capturing}");
    rig.ok(s, &["js", "console.log('options page log')"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !rig.ok(s, &["ext", "logs", &id]).contains("options page log") {
        assert!(Instant::now() < deadline, "the extension page's log was not captured");
        thread::sleep(Duration::from_millis(50));
    }
    // The tab leaving for a website ends the page's session (and its `Runtime`) before the
    // website's script logs.
    let website = rig.url("slow/console-remote.html").replace("127.0.0.1", "remote.example");
    rig.ok(s, &["open", &website]);
    let logs = rig.ok(s, &["ext", "logs", &id]);
    assert!(!logs.contains("remote page log"), "{logs}");
    rig.ok(s, &["ext", "rm", &id]);
    rig.ok(s, &["close"]);
}

fn scripts_run_many_commands_in_one_call(rig: &Rig) {
    let script = rig.root.join("script.hba");
    let lines = format!("open {}\nfill 'internal:label=\"User\"' bob\njs 'document.title'\n", rig.url("index.html"));
    fs::write(&script, lines).expect("write script");
    let out = rig.ok("batch", &["run", script.to_str().expect("utf-8 path")]);
    assert!(out.contains("> js 'document.title'\nFixture home"), "{out}");
    let json = rig.ok("batch", &["--json", "js", "({a: [1, 2]})"]);
    assert_eq!(json.trim(), r#"{"a":[1,2]}"#);
    rig.ok("batch", &["close"]);
}

fn sessions_do_not_share_tabs(rig: &Rig) {
    rig.ok("one", &["open", &rig.url("index.html")]);
    rig.ok("two", &["open", &rig.url("page2.html")]);
    assert!(rig.ok("one", &["get", "url"]).contains("index.html"));
    assert!(rig.ok("two", &["get", "url"]).contains("page2.html"));
    let listing = rig.ok("one", &["tabs"]);
    let tab = listing.split_whitespace().next().expect("tab number").to_owned();
    let refused = rig.fails("two", &["tab", "use", &tab]);
    assert!(refused.contains("is used by session \"one\""), "{refused}");
    rig.ok("one", &["close"]);
    rig.ok("two", &["close"]);
}

fn navigations_that_never_commit_return_at_once(rig: &Rig) {
    let s = "ends";
    rig.ok(s, &["open", &rig.url("nav-ends.html")]);
    // Each of these starts no navigation of this page, or one that never commits; waiting
    // for a commit would take the whole 30 s timeout. The new tab joins the session.
    let clicks: [(&[&str], &str); 4] = [
        (&["click", "#attachment"], "+ download"),
        (&["click", "#no-content"], ""),
        (&["click", "#download"], ""),
        (&["click", "#new-tab"], "+ popup"),
    ];
    for (args, report) in clicks {
        let started = Instant::now();
        let out = rig.ok(s, args);
        assert!(started.elapsed() < Duration::from_secs(10), "{args:?} took {:?}: {out}", started.elapsed());
        assert!(out.contains(report), "{args:?}: {out}");
        assert!(rig.ok(s, &["get", "url"]).contains("nav-ends.html"), "{args:?} moved the page: {out}");
    }
    let saved = rig.root.join("downloads/media-download.txt");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !saved.exists() {
        assert!(Instant::now() < deadline, "the attachment did not land in {}", saved.display());
        thread::sleep(Duration::from_millis(50));
    }
    // Opening an attachment URL downloads it and leaves the page where it was.
    let started = Instant::now();
    rig.ok(s, &["open", &rig.url("attachment/media-download.txt")]);
    assert!(started.elapsed() < Duration::from_secs(10), "open of an attachment took {:?}", started.elapsed());
    assert!(rig.ok(s, &["get", "url"]).contains("nav-ends.html"));
    rig.ok(s, &["close"]);
}

fn console_capture_stays_off_on_websites(rig: &Rig) {
    let s = "stealth";
    let website = rig.url("slow/console-remote.html").replace("127.0.0.1", "remote.example");
    rig.ok(s, &["open", &rig.url("console-leave.html")]);
    let local = rig.ok(s, &["console"]);
    assert!(local.contains("local page log"), "{local}");
    // Heading for a website turns capture off; when that navigation never commits it comes back
    // on without repeating what the page logged before.
    rig.ok(s, &["click", "#no-content"]);
    rig.ok(s, &["js", "console.log('after the 204')"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let console = loop {
        let console = rig.ok(s, &["console"]);
        if console.contains("after the 204") || Instant::now() > deadline {
            break console;
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(console.contains("after the 204"), "capture did not come back: {console}");
    assert_eq!(console.matches("local page log").count(), 1, "{console}");
    // The website logs while it loads; with Runtime on at that moment the message would be
    // captured, so its absence shows capture went off before the website's script ran.
    let assert_off = |how: &str| {
        assert!(rig.ok(s, &["get", "url"]).contains("remote.example"), "{how} did not reach the website");
        let console = rig.ok(s, &["console"]);
        assert!(!console.contains("remote page log") && console.contains("off on websites"), "{how}: {console}");
    };
    rig.ok(s, &["click", "#redirect"]);
    assert_off("a server redirect");
    rig.ok(s, &["open", &rig.url("console-leave.html")]);
    rig.ok(s, &["click", "#script"]);
    assert_off("a script navigation");
    rig.ok(s, &["open", &format!("{}/redirect?to={website}", rig.site)]);
    assert_off("opening a local URL that redirects");
    // A website coming back from the back/forward cache does not load again: capture must be
    // off before the history navigation even starts.
    rig.ok(s, &["open", &rig.url("console-leave.html")]);
    rig.ok(s, &["back"]);
    assert_off("going back");
    rig.ok(s, &["back"]);
    rig.ok(s, &["back"]);
    assert!(rig.ok(s, &["get", "url"]).contains("console-leave.html"), "the history is not as expected");
    rig.ok(s, &["forward"]);
    assert_off("going forward");
    rig.ok(s, &["close"]);
}

fn history_restores_pages_without_repeating_their_console(rig: &Rig) {
    let s = "bfcache";
    rig.ok(s, &["open", &rig.url("console-history.html")]);
    rig.ok(s, &["open", &rig.url("index.html")]);
    // The page comes back from the back/forward cache (its `pageshow` says so) and replays
    // what it logged, which was captured the first time.
    rig.ok(s, &["back"]);
    let deadline = Instant::now() + Duration::from_secs(5);
    let console = loop {
        let console = rig.ok(s, &["console"]);
        if console.contains("history page restored") || Instant::now() > deadline {
            break console;
        }
        thread::sleep(Duration::from_millis(50));
    };
    assert!(console.contains("history page restored"), "not restored from the back/forward cache: {console}");
    assert_eq!(console.matches("history page log").count(), 1, "{console}");
    rig.ok(s, &["close"]);
}

fn cross_process_frames_are_stitched_and_actionable(rig: &Rig) {
    let s = "oopif";
    rig.ok(s, &["open", &rig.url("oopif.html")]);
    // The frame is another site (`localhost`), so it lives in its own process and loads later.
    let snap = until(rig, s, &["snap"], "Inner submit");
    let name = ref_of(&snap, "textbox \"Inner name\"");
    let submit = ref_of(&snap, "button \"Inner submit\"");
    assert!(name.starts_with('f') && submit.starts_with('f'), "{snap}");
    rig.ok(s, &["fill", &name, "ada"]);
    rig.ok(s, &["click", &submit]);
    let diff = rig.ok(s, &["snap", "-d"]);
    assert!(diff.contains("inner clicked ada"), "{diff}");
    let frame = ref_of(&snap, "iframe");
    assert_eq!(
        rig.ok(s, &["js", "--frame", &frame, "document.getElementById('result').textContent"]).trim(),
        "inner clicked ada"
    );
    rig.ok(s, &["close"]);
}

fn uploads_reach_inputs_and_file_choosers(rig: &Rig) {
    let s = "upload";
    let file = rig.root.join("report.txt");
    fs::write(&file, "hello upload").expect("write the upload");
    let path = file.to_str().expect("utf-8 path");
    rig.ok(s, &["open", &rig.url("media-upload.html")]);
    let direct = rig.ok(s, &["upload", "#file", path]);
    assert!(direct.contains("uploaded report.txt"), "{direct}");
    assert_eq!(rig.ok(s, &["text", "#status"]).trim(), "file: report.txt 12");
    // The button opens a file chooser for a hidden input; hba answers it.
    let chooser = rig.ok(s, &["upload", "text=Choose a photo", path]);
    assert!(chooser.contains("uploaded report.txt"), "{chooser}");
    assert_eq!(rig.ok(s, &["text", "#status"]).trim(), "hidden: report.txt 12");
    let missing = rig.fails(s, &["upload", "#file", "/nonexistent/hba-upload.txt"]);
    assert!(missing.contains("hba-upload.txt"), "{missing}");
    rig.ok(s, &["close"]);
}

fn drags_move_html5_items_and_pointer_handles(rig: &Rig) {
    let s = "drag";
    rig.ok(s, &["open", &rig.url("media-drag.html")]);
    let html5 = rig.ok(s, &["drag", "#item", "#zone"]);
    assert!(html5.contains("HTML5 drag and drop"), "{html5}");
    assert_eq!(rig.ok(s, &["text", "#status"]).trim(), "html5 dropped payload-42 (dragend move)");
    // Without a draggable item the pointer path runs: real moves between press and release.
    rig.ok(s, &["drag", "#handle", "#bin"]);
    let status = rig.ok(s, &["text", "#status"]);
    assert!(status.trim().starts_with("mouse dropped after "), "{status}");
    rig.ok(s, &["close"]);
}

fn downloads_are_saved_where_asked(rig: &Rig) {
    let s = "download";
    let dir = rig.root.join("saved");
    fs::create_dir_all(&dir).expect("create the download directory");
    let dir_arg = dir.to_str().expect("utf-8 path");
    rig.ok(s, &["open", &rig.url("media-download.html")]);
    let linked = rig.ok(s, &["download", "#file", "-o", dir_arg]);
    assert!(linked.contains("media-report.txt"), "{linked}");
    assert_eq!(
        fs::read_to_string(dir.join("media-report.txt")).expect("saved file"),
        "hba download fixture\nline two\n"
    );
    // A file the page builds from a blob is saved too.
    let generated = rig.ok(s, &["download", "#blob", "-o", dir_arg]);
    assert!(generated.contains("media-generated.txt"), "{generated}");
    assert_eq!(fs::read_to_string(dir.join("media-generated.txt")).expect("saved file"), "generated in the page\n");
    let none = rig.fails(s, &["--timeout", "2", "download", "#nothing"]);
    assert!(none.contains("started no download"), "{none}");
    rig.ok(s, &["close"]);
}

fn console_and_errors_report_what_the_page_did(rig: &Rig) {
    let s = "console";
    rig.ok(s, &["open", &rig.url("devtools-console.html")]);
    let logged = rig.ok(s, &["click", "#log"]);
    assert!(logged.contains("! console.error: boom from devtools fixture"), "{logged}");
    let console = rig.ok(s, &["console"]);
    for line in [
        "log loaded devtools console fixture",
        "debug debug detail",
        "info info message",
        "warn first line",
        "\n  second line",
    ] {
        assert!(console.contains(line), "{line:?} not in:\n{console}");
    }
    let thrown = rig.ok(s, &["click", "#throw"]);
    assert!(thrown.contains("! Uncaught Error: kaboom from fixture"), "{thrown}");
    let rejected = rig.ok(s, &["click", "#reject"]);
    assert!(rejected.contains("! Uncaught (in promise) TypeError: rejected in fixture"), "{rejected}");
    let failed = rig.ok(s, &["click", "#fail"]);
    assert!(failed.contains("devtools-missing.json → 404"), "{failed}");
    let errors = rig.ok(s, &["errors"]);
    for line in [
        "error Uncaught Error: kaboom from fixture",
        "request #",
        "devtools-missing.json → 404",
        "http://127.0.0.1:9/unreachable",
    ] {
        assert!(errors.contains(line), "{line:?} not in:\n{errors}");
    }
    rig.ok(s, &["errors", "--clear"]);
    assert!(!rig.ok(s, &["errors"]).contains("kaboom"), "errors survived --clear");
    rig.ok(s, &["close"]);
}

fn network_log_bodies_blocking_and_har(rig: &Rig) {
    let s = "net";
    let har = rig.root.join("capture.har");
    rig.ok(s, &["open", &rig.url("devtools-net.html")]);
    until(rig, s, &["text", "#out"], "real data from the server");
    rig.ok(s, &["net", "har", "start"]);
    rig.ok(s, &["click", "#load"]);
    rig.ok(s, &["click", "#post"]);
    until(rig, s, &["text", "#posted"], "posted 200");
    let fetched = rig.ok(s, &["net", "--type", "fetch", "--filter", "devtools-data.json"]);
    let line = fetched.lines().find(|line| line.contains("GET")).unwrap_or_else(|| panic!("no GET in:\n{fetched}"));
    let id = line.trim_start_matches('#').split_whitespace().next().expect("request number").to_owned();
    let shown = rig.ok(s, &["net", "show", &id]);
    assert!(shown.contains("GET") && shown.contains("application/json"), "{shown}");
    assert!(rig.ok(s, &["net", "body", &id]).contains("real data from the server"));
    let saved = rig.ok(s, &["net", "har", "stop", har.to_str().expect("utf-8 path")]);
    assert!(saved.contains("saved"), "{saved}");
    let recorded: serde_json::Value = serde_json::from_slice(&fs::read(&har).expect("HAR file")).expect("HAR is JSON");
    let entries = recorded["log"]["entries"].as_array().expect("HAR entries");
    let methods: Vec<_> = entries
        .iter()
        .filter(|e| e["request"]["url"].as_str().is_some_and(|u| u.contains("devtools-data.json")))
        .map(|e| e["request"]["method"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert!(methods.contains(&"GET".to_owned()) && methods.contains(&"POST".to_owned()), "{methods:?}");
    // A blocked request fails in the page; removing the rule restores it.
    rig.ok(s, &["net", "block", "*devtools-data.json*"]);
    rig.ok(s, &["click", "#load"]);
    until(rig, s, &["text", "#out"], "failed:");
    rig.ok(s, &["net", "unroute"]);
    rig.ok(s, &["click", "#load"]);
    until(rig, s, &["text", "#out"], "real data from the server");
    rig.ok(s, &["close"]);
}

fn performance_reports(rig: &Rig) {
    let s = "perf";
    let artifact = |name: &str| rig.root.join(name);
    rig.ok(s, &["open", &rig.url("devtools-perf.html")]);
    let vitals = until(rig, s, &["perf", "vitals"], "(1 shift)");
    assert!(vitals.contains("TTFB") && vitals.contains("LCP"), "{vitals}");
    assert!(rig.ok(s, &["perf", "metrics"]).contains("js heap"));
    rig.ok(s, &["perf", "coverage", "start"]);
    rig.ok(s, &["reload"]);
    let coverage = rig.ok(s, &["perf", "coverage", "stop"]);
    assert!(coverage.contains("devtools-perf.js") && coverage.contains("devtools-perf.css"), "{coverage}");
    let trace = artifact("trace.json");
    rig.ok(s, &["perf", "trace", "start"]);
    rig.ok(s, &["click", "#work"]);
    let traced = rig.ok(s, &["perf", "trace", "stop", trace.to_str().expect("utf-8 path")]);
    assert!(traced.contains("saved") && fs::metadata(&trace).is_ok_and(|m| m.len() > 0), "{traced}");
    let profile = artifact("work.cpuprofile");
    rig.ok(s, &["perf", "profile", "start"]);
    rig.ok(s, &["click", "#work"]);
    let profiled = rig.ok(s, &["perf", "profile", "stop", profile.to_str().expect("utf-8 path")]);
    assert!(profiled.contains("fib (devtools-perf.js"), "{profiled}");
    let heap = artifact("page.heapsnapshot");
    let snapshot = rig.ok(s, &["perf", "heap", heap.to_str().expect("utf-8 path")]);
    assert!(snapshot.contains("saved") && fs::metadata(&heap).is_ok_and(|m| m.len() > 0), "{snapshot}");
    rig.ok(s, &["close"]);
}

fn extensions_are_developed_from_the_shell(rig: &Rig) {
    let s = "extdev";
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/extension");
    let dir = dir.to_str().expect("utf-8 path");
    let id = extension_id(&rig.ok(s, &["ext", "load", dir]));
    let listed = rig.ok(s, &["ext"]);
    assert!(listed.contains(&id) && listed.contains("service worker running"), "{listed}");
    assert_eq!(rig.ok(s, &["ext", "eval", &id, "chrome.runtime.getManifest().name"]).trim(), "hba test extension");
    // Storage, by hand and through the extension's own code.
    rig.ok(s, &["ext", "storage", &id, "local", "set", "key", r#"{"a":1}"#]);
    assert_eq!(rig.ok(s, &["ext", "storage", &id, "local", "get", "key"]).trim(), r#"{"a":1}"#);
    assert!(rig.ok(s, &["ext", "eval", &id, "await chrome.storage.local.get('key')"]).contains(r#""a":1"#));
    rig.ok(s, &["ext", "storage", &id, "local", "rm", "key"]);
    // A stopped worker starts again on request.
    assert!(rig.ok(s, &["ext", "sw", &id, "stop"]).contains("stopped"));
    assert!(rig.ok(s, &["ext", "sw", &id, "start"]).contains("started"));
    // Recording runtime errors needs developer mode, which the first call turns on; that reloads
    // the profile's extensions and holds navigations for a while.
    let first = rig.ok(s, &["ext", "errors", &id]);
    assert!(first.contains("developer mode"), "{first}");
    rig.settle();
    rig.ok(s, &["ext", "eval", &id, "hbaFail('from eval')"]);
    until(rig, s, &["ext", "errors", &id], "hba fixture failure: from eval");
    assert!(until(rig, s, &["ext", "logs", &id], "hba sw started").contains("sw.js"));
    // The popup opens as a page of the current tab and works like one.
    rig.ok(s, &["open", &rig.url("index.html")]);
    let popup = rig.ok(s, &["ext", "popup", &id]);
    assert!(popup.contains("popup.html"), "{popup}");
    rig.ok(s, &["click", "text=Count click"]);
    until(rig, s, &["text", "#clicks"], "1");
    assert_eq!(rig.ok(s, &["ext", "storage", &id, "local", "get", "clicks"]).trim(), "1");
    rig.ok(s, &["click", "text=Fail in worker"]);
    until(rig, s, &["ext", "errors", &id], "popup button");
    // The content script's world holds what the page's own world cannot see.
    rig.ok(s, &["open", &rig.url("ext-page.html")]);
    assert_eq!(rig.ok(s, &["js", "--world", &id, "hbaContent.title"]).trim(), "hba extension fixture");
    assert_eq!(rig.ok(s, &["js", "typeof hbaContent"]).trim(), "undefined");
    let reloaded = rig.ok(s, &["ext", "reload", dir]);
    assert!(reloaded.contains("reloaded"), "{reloaded}");
    assert!(rig.ok(s, &["ext", "sw", &id]).contains("running"));
    rig.ok(s, &["ext", "rm", &id]);
    rig.ok(s, &["close"]);
}

/// With `HBA_HUMANIZE=always` input goes through real mouse and key events even on local pages.
fn humanized_input_selects_fills_and_runs_in_frames(rig: &Rig) {
    let s = "human";
    let human = |args: &[&str]| rig.ok_with(s, args, &[("HBA_HUMANIZE", "always")]);
    human(&["open", &rig.url("controls.html")]);
    human(&["select", "#plan", "Pro"]);
    assert_eq!(human(&["text", "#log"]).trim(), "plan pro");
    assert_eq!(human(&["get", "value", "#plan"]).trim(), "pro");
    human(&["select", "#plan", "team"]);
    assert_eq!(human(&["get", "value", "#plan"]).trim(), "team");
    human(&["open", &rig.url("index.html")]);
    // `fill` clicks the field to focus it, like a person.
    human(&["fill", "internal:label=\"User\"", "alice"]);
    assert_eq!(human(&["get", "value", "internal:label=\"User\""]).trim(), "alice");
    // Statements, not only an expression, run in a frame.
    human(&["open", &rig.url("page2.html")]);
    let typed = human(&[
        "js",
        "--frame",
        "iframe[title=Embedded]",
        "const input = document.getElementById('inner'); input.value = 'typed'; input.value",
    ]);
    assert_eq!(typed.trim(), "typed");
    human(&["close"]);
}
