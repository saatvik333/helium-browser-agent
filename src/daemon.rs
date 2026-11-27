//! The background process: owns the browser connection, sessions and attached tabs.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, broadcast, mpsc, watch};

use crate::browser::{Browser, TargetEvent, TargetInfo};
use crate::cdp::{Cdp, Event};
use crate::cli::{Command, MAX_TIMEOUT};
use crate::config::Config;
use crate::injected::Source;
use crate::ipc::{self, BUILD, DAEMON_DRAIN, Header, Reply, Request};
use crate::page::Page;
use crate::profiles::{Profile, Profiles};
use crate::session::{Session, Store};

/// Default per-command timeout.
const TIMEOUT: Duration = Duration::from_secs(30);

/// How long a browser that needs no approval may take to accept the DevTools connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// File in the runtime directory naming the endpoint of an approval request a daemon left
/// unanswered when it exited: the browser still shows its dialog, before any later one.
const ABANDONED: &str = "abandoned-approval";

/// Directory in the runtime directory for screenshots, PDFs, HARs and other artifacts.
const FILES: &str = "files";

/// How long artifacts are kept: the runtime directory is usually in RAM.
const ARTIFACT_LIFETIME: Duration = Duration::from_secs(86_400);

/// The environment this daemon started with; see [`ipc::daemon_env`].
static DAEMON_ENV: LazyLock<String> = LazyLock::new(ipc::daemon_env);

pub fn main() -> ExitCode {
    // Trace times count from here.
    LazyLock::force(&crate::trace::START);
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
    let outcome = runtime.map_err(anyhow::Error::from).and_then(|runtime| runtime.block_on(serve()));
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("hba daemon: {error:#}");
            ExitCode::FAILURE
        }
    }
}

async fn serve() -> Result<()> {
    let runtime = ipc::runtime_dir()?;
    let socket = ipc::socket_path(&runtime);
    let lock = std::fs::File::create(runtime.join("daemon.lock"))?;
    // A daemon that is shutting down holds the lock until its last command finishes.
    let deadline = Instant::now() + DAEMON_DRAIN + Duration::from_secs(5);
    while lock.try_lock().is_err() {
        // Another daemon serves: one started at the same time won, or one never stops.
        if Instant::now() >= deadline || UnixStream::connect(&socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    prune_artifacts(&runtime.join(FILES), SystemTime::now());
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    let hub = Arc::new(Hub::new(runtime.clone()));
    eprintln!("hba daemon {BUILD} pid {} listening on {}", std::process::id(), socket.display());
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut hangup = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?;
    let mut tick = tokio::time::interval(Duration::from_secs(30));
    let mut connections = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                if let Ok((stream, _)) = accepted {
                    connections.spawn(Arc::clone(&hub).connection(stream));
                }
            }
            Some(_) = connections.join_next(), if !connections.is_empty() => {}
            _ = tick.tick() => {
                let limit = hub.config().map_or(Some(crate::config::DEFAULT_IDLE), |config| config.idle);
                let idle = hub.activity.lock().expect("activity lock").idle(Instant::now());
                // Every new connection in approval mode is another dialog for the user.
                if limit.is_some_and(|limit| idle > limit) && !hub.holds_approval() {
                    break;
                }
            }
            () = hub.stop.notified() => break,
            _ = terminate.recv() => break,
            _ = hangup.recv() => {} // Survive the terminal that started us.
        }
    }
    // New clients start a fresh daemon, which takes over once the commands in flight finish.
    // Clients already queued are told to retry, which reaches that daemon.
    let _ = std::fs::remove_file(&socket);
    refuse_queued(listener, &mut connections);
    let drained = async {
        while connections.join_next().await.is_some() {}
        // The browser shows its approval dialogs one at a time, oldest first, and closing a
        // request does not close its dialog: an unanswered one would sit in front of the next
        // daemon's, where Allow does nothing. So the answer is waited for too.
        if matches!(hub.approval_request(), Some(ApprovalRequest::Waiting { .. })) {
            eprintln!("hba daemon: waiting for the answer to the approval request before exiting");
            hub.approval_answered().await;
        }
    };
    let _ = tokio::time::timeout(DAEMON_DRAIN, drained).await;
    hub.record_abandoned();
    drop(lock);
    Ok(())
}

pub struct Hub {
    /// Loaded on first use, so a bad config file is reported by the command, not a dead daemon.
    config: Mutex<Option<Arc<Config>>>,
    pub runtime: PathBuf,
    browser: tokio::sync::Mutex<Option<Arc<Browser>>>,
    /// The approval-mode handshake in flight or not yet taken; at most one.
    handshake: Mutex<Option<Handshake>>,
    pub store: Mutex<Store>,
    locks: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    source: Arc<Source>,
    pub started: Instant,
    activity: Mutex<Activity>,
    pub stop: Notify,
}

impl Hub {
    fn new(runtime: PathBuf) -> Self {
        Self {
            config: Mutex::new(None),
            store: Mutex::new(Store::load(runtime.join("state.json"))),
            runtime,
            browser: tokio::sync::Mutex::new(None),
            handshake: Mutex::new(None),
            locks: Mutex::new(HashMap::new()),
            source: Arc::new(Source::new()),
            started: Instant::now(),
            activity: Mutex::new(Activity { running: 0, since: Instant::now() }),
            stop: Notify::new(),
        }
    }

    /// The current configuration, loaded again when the file changed.
    pub fn config(&self) -> Result<Arc<Config>> {
        let mut config = self.config.lock().expect("config lock");
        if let Some(current) = config.as_ref().filter(|current| !current.is_stale()) {
            return Ok(Arc::clone(current));
        }
        let loaded = Arc::new(Config::load()?);
        *config = Some(Arc::clone(&loaded));
        Ok(loaded)
    }

    /// The connected browser; connects (and launches the browser when absent, in the profile
    /// `launch_profile` names) on first use. A browser in approval mode gets until
    /// `approval_wait` to accept the connection, but the handshake goes on without the command.
    pub async fn browser(
        &self,
        launch_profile: impl FnOnce() -> Option<String>,
        approval_wait: Duration,
    ) -> Result<Arc<Browser>> {
        let deadline = tokio::time::Instant::now() + approval_wait;
        let mut launch_profile = Some(launch_profile);
        let mut profile = None;
        // The approval handshake this command waited for, whose failure it reports.
        let mut waited = None;
        loop {
            let mut slot = self.browser.lock().await;
            let config = self.config()?;
            if let Some(browser) = slot.as_ref().filter(|browser| browser.is_connected()) {
                if browser.config().same_browser(&config) {
                    browser.set_config(config);
                    return Ok(Arc::clone(browser));
                }
                // The config file now names another browser or data directory.
                browser.cdp.close();
            }
            let profile: &Option<String> = profile.get_or_insert_with(|| launch_profile.take().and_then(|hint| hint()));
            let endpoint = crate::endpoint::ensure_running(&config, profile.as_deref(), deadline).await?;
            let name = config.browser.name();
            let socket = if endpoint.approval {
                match self.approval(&endpoint.ws_url, waited.as_ref()) {
                    Approval::Allowed(socket) => socket,
                    Approval::Refused(why) => bail!(
                        "{name} refused the DevTools connection ({why}); run the command again and click Allow in the \
                         dialog {name} shows"
                    ),
                    Approval::Pending { mut ended, behind } => {
                        // Others may connect or start the browser meanwhile.
                        drop(slot);
                        waited = Some(ended.clone());
                        let answered = tokio::time::timeout_at(deadline, ended.wait_for(Option::is_some)).await;
                        if answered.is_err() {
                            let mut message = format!(
                                "{name} did not allow the DevTools connection within {}s; click Allow in the \"Allow \
                                 remote debugging?\" dialog in the {name} window you used last, and run the command \
                                 again (the request stays open, so a later Allow still counts)",
                                approval_wait.as_secs_f32()
                            );
                            if behind {
                                message.push_str(&abandoned_note(name));
                            }
                            bail!(message);
                        }
                        continue;
                    }
                }
            } else {
                match tokio::time::timeout(CONNECT_TIMEOUT, Cdp::connect(&endpoint.ws_url)).await {
                    Ok(connected) => connected.with_context(|| format!("connecting to {name}'s DevTools endpoint"))?,
                    Err(_) => bail!("timed out connecting to {name}'s DevTools endpoint"),
                }
            };
            let browser = Browser::start(config, Arc::clone(&self.source), endpoint, socket).await?;
            let known = {
                let mut store = self.store.lock().expect("store lock");
                store.bind_browser(&browser.endpoint.browser_id);
                store.contexts.clone()
            };
            browser.restore_contexts(known);
            browser.infer_contexts().await.ok();
            self.save_contexts(&browser);
            *slot = Some(Arc::clone(&browser));
            return Ok(browser);
        }
    }

    /// The approval-mode connection to `url` if the user allowed it, else the handshake to wait
    /// for: the one in flight, or a new one. `waited` is the handshake the caller already waited
    /// for; only its callers hear why it failed, and later ones ask again.
    fn approval(&self, url: &str, waited: Option<&watch::Receiver<Ended>>) -> Approval {
        let mut current = self.handshake.lock().expect("handshake lock");
        if let Some(handshake) = current.as_ref().filter(|handshake| handshake.url == url) {
            let ended = handshake.ended.borrow().clone();
            match ended {
                None => return Approval::Pending { ended: handshake.ended.clone(), behind: handshake.behind },
                Some(Ok(())) => {
                    let socket = handshake.socket.lock().expect("socket lock").take();
                    if let Some(socket) = socket {
                        *current = None;
                        return Approval::Allowed(socket);
                    }
                }
                Some(Err(why)) if waited.is_some_and(|waited| waited.same_channel(&handshake.ended)) => {
                    return Approval::Refused(why);
                }
                Some(Err(_)) => {}
            }
        }
        // An earlier daemon's unanswered request keeps its dialog in front of this one's. The
        // marker is read once: before this daemon asks again, the user answered both dialogs.
        let marker = self.runtime.join(ABANDONED);
        let behind = std::fs::read_to_string(&marker).is_ok_and(|left| left == url);
        let _ = std::fs::remove_file(&marker);
        // Replaces a failed handshake, or one to a browser that is gone.
        let handshake = Handshake::start(url, behind);
        let pending = Approval::Pending { ended: handshake.ended.clone(), behind };
        *current = Some(handshake);
        pending
    }

    /// Whether an approval-mode connection is open, or waits for the user or for a command to
    /// take it.
    fn holds_approval(&self) -> bool {
        self.handshake
            .lock()
            .expect("handshake lock")
            .as_ref()
            .is_some_and(|handshake| handshake.ended.borrow().as_ref().is_none_or(Result::is_ok))
            || self.browser.try_lock().is_ok_and(|slot| {
                slot.as_ref().is_some_and(|browser| browser.endpoint.approval && browser.is_connected())
            })
    }

    /// Where this daemon's approval-mode request stands, if it has one no command took yet.
    pub fn approval_request(&self) -> Option<ApprovalRequest> {
        let current = self.handshake.lock().expect("handshake lock");
        let handshake = current.as_ref()?;
        let ended = handshake.ended.borrow();
        match ended.as_ref() {
            None => Some(ApprovalRequest::Waiting { behind: handshake.behind }),
            Some(Ok(())) => Some(ApprovalRequest::Allowed),
            Some(Err(_)) => None,
        }
    }

    /// Resolves once no approval request waits for the user's answer.
    async fn approval_answered(&self) {
        let pending = self.handshake.lock().expect("handshake lock").as_ref().map(|handshake| handshake.ended.clone());
        if let Some(mut ended) = pending {
            let _ = ended.wait_for(Option::is_some).await;
        }
    }

    /// Records the endpoint of an approval request still unanswered as this daemon exits, so
    /// the next daemon can say that its dialog comes first and connects nothing.
    fn record_abandoned(&self) {
        let current = self.handshake.lock().expect("handshake lock");
        if let Some(handshake) = current.as_ref().filter(|handshake| handshake.ended.borrow().is_none())
            && let Err(error) = std::fs::write(self.runtime.join(ABANDONED), &handshake.url)
        {
            eprintln!("hba daemon: recording the unanswered approval request: {error}");
        }
    }

    /// The browser if this daemon is already connected.
    pub async fn connected(&self) -> Option<Arc<Browser>> {
        self.browser.lock().await.as_ref().filter(|browser| browser.is_connected()).cloned()
    }

    /// Where the browser connection stands, without waiting for a command that is connecting.
    pub fn link(&self) -> Link {
        match self.browser.try_lock() {
            Ok(slot) => slot
                .as_ref()
                .filter(|browser| browser.is_connected())
                .map_or(Link::Unconnected, |browser| Link::Connected(Arc::clone(browser))),
            Err(_) => Link::Connecting,
        }
    }

    pub fn save_contexts(&self, browser: &Browser) {
        let mut store = self.store.lock().expect("store lock");
        store.contexts = browser.contexts();
        if let Err(error) = store.save() {
            eprintln!("hba: {error:#}");
        }
    }

    async fn connection(self: Arc<Self>, stream: UnixStream) {
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut line = String::new();
        if read.read_line(&mut line).await.is_err() || line.is_empty() {
            return;
        }
        let reply = match admit(&line, &DAEMON_ENV) {
            Ok(request) => match self.execute(request, hung_up(read)).await {
                Some(reply) => reply,
                None => return,
            },
            Err(Reply::Restart) => {
                self.stop.notify_one();
                Reply::Restart
            }
            Err(refused) => refused,
        };
        let _ = write.write_all(reply.line().as_bytes()).await;
    }

    /// The lock that keeps a session's commands in order. Locks of sessions no command holds or
    /// waits for are dropped meanwhile.
    fn session_lock(&self, session: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut locks = self.locks.lock().expect("locks lock");
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        Arc::clone(locks.entry(session.to_owned()).or_default())
    }

    /// Runs a request. `None` when the client went away first (Ctrl-C, killed by its caller):
    /// the command is cancelled, so it no longer holds the session.
    async fn execute(self: &Arc<Self>, request: Request, hung_up: impl Future<Output = ()>) -> Option<Reply> {
        let _running = Running::new(&self.activity);
        let timeout = match request.timeout_secs {
            None => TIMEOUT,
            Some(secs) => match Duration::try_from_secs_f64(secs) {
                Ok(timeout) if !timeout.is_zero() && timeout <= MAX_TIMEOUT => timeout,
                _ => {
                    let stderr = format!(
                        "hba: --timeout must be a positive number of seconds, at most {}",
                        MAX_TIMEOUT.as_secs()
                    );
                    return Some(Reply::Done { stdout: String::new(), stderr, code: 2 });
                }
            },
        };
        let session = request.session.clone().unwrap_or_else(|| default_session(&request.cwd));
        let lock = self.session_lock(&session);
        let mut cx = Cx {
            hub: self,
            cwd: request.cwd,
            session,
            profile_flag: request.profile,
            tab_flag: request.tab,
            json: request.json,
            timeout,
            out: String::new(),
            touched: Vec::new(),
        };
        let started = Instant::now();
        let name = if *crate::trace::ENABLED { command_name(&request.command) } else { String::new() };
        let mut hung_up = std::pin::pin!(hung_up);
        // Commands of one session run in order.
        let locked = if request.command.is_local() {
            Some(Ok(None))
        } else {
            tokio::select! {
                locked = tokio::time::timeout(timeout, lock.lock_owned()) => Some(locked.map(Some)),
                () = &mut hung_up => None,
            }
        };
        let outcome = match locked {
            None => None,
            Some(Err(_)) => Some(Err(anyhow::anyhow!(
                "session {:?} stayed busy with another hba command for {}s; run this again once it finishes, or in \
                 another session (-s NAME)",
                cx.session,
                timeout.as_secs_f32()
            ))),
            Some(Ok(_guard)) => {
                crate::trace!("{} {name} started", cx.session);
                let outcome = tokio::select! {
                    outcome = crate::commands::run(&mut cx, request.command) => Some(outcome),
                    () = &mut hung_up => None,
                };
                cx.release();
                outcome
            }
        };
        let ending = if outcome.is_some() { "took" } else { "was cancelled by its client after" };
        crate::trace!("{} {name} {ending} {:?}", cx.session, started.elapsed());
        Some(match outcome? {
            Ok(()) => Reply::Done { stdout: cx.out, stderr: String::new(), code: 0 },
            Err(error) => Reply::Done { stdout: cx.out, stderr: format!("hba: {error:#}"), code: 1 },
        })
    }
}

/// A DevTools connection and its browser-level events.
type Socket = (Cdp, mpsc::UnboundedReceiver<Event>);

/// How a handshake ended: `None` while the browser holds it, then whether it connected and, if
/// not, why.
type Ended = Option<Result<(), String>>;

/// A WebSocket handshake that a browser in approval mode holds until its user answers its
/// dialog. It runs on a task of its own, so it outlives the commands that wait for it: an Allow
/// that comes after their timeout still connects, and the next command takes the connection.
struct Handshake {
    url: String,
    ended: watch::Receiver<Ended>,
    /// The connection once made, until a command takes it.
    socket: Arc<Mutex<Option<Socket>>>,
    /// Its dialog waits behind one that an earlier daemon's unanswered request left.
    behind: bool,
    task: tokio::task::AbortHandle,
}

impl Handshake {
    fn start(url: &str, behind: bool) -> Self {
        let (tell, ended) = watch::channel(None);
        let socket = Arc::new(Mutex::new(None));
        let made = Arc::clone(&socket);
        let target = url.to_owned();
        let task = tokio::spawn(async move {
            let outcome = match Cdp::connect(&target).await {
                Ok(connected) => {
                    *made.lock().expect("socket lock") = Some(connected);
                    Ok(())
                }
                Err(error) => Err(format!("{error:#}")),
            };
            tell.send_replace(Some(outcome));
        });
        Self { url: url.to_owned(), ended, socket, behind, task: task.abort_handle() }
    }
}

impl Drop for Handshake {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// An approval-mode request no command has taken yet, for status reports.
pub enum ApprovalRequest {
    /// The browser has not answered; `behind` as in [`Handshake`].
    Waiting { behind: bool },
    /// The user allowed it; the next command takes the connection.
    Allowed,
}

/// Where an approval-mode connection stands for a command.
enum Approval {
    Allowed(Socket),
    Refused(String),
    /// Wait for this handshake to end; `behind` as in [`Handshake`].
    Pending {
        ended: watch::Receiver<Ended>,
        behind: bool,
    },
}

/// What to do about a dialog an earlier daemon's request left in front of this daemon's.
pub fn abandoned_note(name: &str) -> String {
    format!(
        "; {name} shows these dialogs one at a time, and the first one belongs to a request an earlier hba daemon \
         left unanswered, which connects nothing: click Cancel on it, then Allow on the next"
    )
}

/// Where this daemon's browser connection stands, for status reports.
pub enum Link {
    Connected(Arc<Browser>),
    /// A command is connecting, or starting the browser.
    Connecting,
    Unconnected,
}

/// The request a client sent, or the reply refusing it. The build is checked before the rest is
/// parsed: a client of another build may send a command this one does not know. Daemon commands
/// are served whatever environment their client has; others need the one this daemon has.
fn admit(line: &str, env: &str) -> Result<Request, Reply> {
    let bad = |error: serde_json::Error| Reply::Done {
        stdout: String::new(),
        stderr: format!("hba: bad request: {error}"),
        code: 1,
    };
    let header: Header = serde_json::from_str(line).map_err(bad)?;
    if header.build != BUILD {
        return Err(Reply::Restart);
    }
    let request: Request = serde_json::from_str(line).map_err(bad)?;
    if request.env != env && !request.command.is_local() {
        return Err(Reply::Restart);
    }
    Ok(request)
}

/// Answers [`Reply::Restart`] to the clients that connected before the listener closed: their
/// next attempt reaches the daemon that takes over.
fn refuse_queued(listener: UnixListener, connections: &mut tokio::task::JoinSet<()>) {
    let Ok(listener) = listener.into_std() else { return };
    while let Ok((stream, _)) = listener.accept() {
        if stream.set_nonblocking(true).is_ok()
            && let Ok(stream) = UnixStream::from_std(stream)
        {
            connections.spawn(refuse(stream));
        }
    }
}

async fn refuse(stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut line = String::new();
    if BufReader::new(read).read_line(&mut line).await.is_ok_and(|read| read > 0) {
        let _ = write.write_all(Reply::Restart.line().as_bytes()).await;
    }
}

/// A command's name for the trace, without its arguments: those can hold typed text and secrets.
fn command_name(command: &Command) -> String {
    match serde_json::to_value(command) {
        Ok(serde_json::Value::String(name)) => name,
        Ok(serde_json::Value::Object(fields)) => fields.into_iter().next().map(|(name, _)| name).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Deletes the artifacts in `dir` last changed more than [`ARTIFACT_LIFETIME`] before `now`.
fn prune_artifacts(dir: &Path, now: SystemTime) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let Ok(meta) = entry.metadata() else { continue };
        let expired = meta.modified().is_ok_and(|at| now.duration_since(at).is_ok_and(|age| age > ARTIFACT_LIFETIME));
        if expired {
            let path = entry.path();
            let _ = if meta.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
        }
    }
}

/// Commands in flight and when the last one ended: what the idle exit goes by.
struct Activity {
    running: usize,
    since: Instant,
}

impl Activity {
    /// How long the daemon has had nothing to do as of `now`; none while a command runs,
    /// however long that takes.
    fn idle(&self, now: Instant) -> Duration {
        if self.running > 0 { Duration::ZERO } else { now.saturating_duration_since(self.since) }
    }
}

/// Counts a command as running until dropped.
struct Running<'a>(&'a Mutex<Activity>);

impl<'a> Running<'a> {
    fn new(activity: &'a Mutex<Activity>) -> Self {
        activity.lock().expect("activity lock").running += 1;
        Self(activity)
    }
}

impl Drop for Running<'_> {
    fn drop(&mut self) {
        let mut activity = self.0.lock().expect("activity lock");
        activity.running -= 1;
        activity.since = Instant::now();
    }
}

/// Resolves when the client closes its end of the socket. Clients send one line and then
/// only read, so anything but more data means they are gone.
async fn hung_up(mut read: impl AsyncRead + Unpin) {
    let mut sink = [0_u8; 64];
    while read.read(&mut sink).await.is_ok_and(|n| n > 0) {}
}

/// Session name from the project: the git root's directory name, else the working directory's.
fn default_session(cwd: &Path) -> String {
    let root = cwd.ancestors().find(|dir| dir.join(".git").exists()).unwrap_or(cwd);
    let name: String = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("default")
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') { c } else { '-' })
        .take(48)
        .collect();
    if name.is_empty() { "default".to_owned() } else { name }
}

/// Everything a command needs: the hub, the caller's context and the output buffer.
pub struct Cx<'a> {
    pub hub: &'a Arc<Hub>,
    pub cwd: PathBuf,
    pub session: String,
    pub profile_flag: Option<String>,
    pub tab_flag: Option<String>,
    pub json: bool,
    pub timeout: Duration,
    pub out: String,
    touched: Vec<Arc<Page>>,
}

impl Cx<'_> {
    pub fn say(&mut self, text: impl AsRef<str>) {
        let text = text.as_ref();
        self.out.push_str(text);
        if !text.ends_with('\n') {
            self.out.push('\n');
        }
    }

    pub fn path(&self, path: &Path) -> PathBuf {
        crate::client::absolute(&self.cwd, path)
    }

    pub fn config(&self) -> Result<Arc<Config>> {
        self.hub.config()
    }

    /// A new file in the runtime directory, e.g. for screenshots. Those older than a day go.
    pub fn artifact(&self, stem: &str, extension: &str) -> Result<PathBuf> {
        let dir = self.hub.runtime.join(FILES);
        std::fs::create_dir_all(&dir)?;
        prune_artifacts(&dir, SystemTime::now());
        let stamp = SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
        Ok(dir.join(format!("{}-{stem}-{stamp}.{extension}", self.session)))
    }

    pub fn session_data(&self) -> Session {
        self.hub.store.lock().expect("store lock").sessions.get(&self.session).cloned().unwrap_or_default()
    }

    pub fn update_session(&self, change: impl FnOnce(&mut Session)) -> Result<()> {
        let mut store = self.hub.store.lock().expect("store lock");
        change(store.session(&self.session));
        store.save()
    }

    pub fn tab_number(&self, tab: &str) -> String {
        format!("t{}", self.hub.store.lock().expect("store lock").number(tab))
    }

    pub async fn browser(&self) -> Result<Arc<Browser>> {
        self.hub.browser(|| self.profile_dir_hint(), self.timeout).await
    }

    fn profile_dir_hint(&self) -> Option<String> {
        let config = self.config().ok()?;
        let profiles = Profiles::load(&config.user_data_dir).ok()?;
        self.choose_profile(&config, &profiles).ok().map(|profile| profile.dir)
    }

    /// Profile for new tabs: `-p`/`HBA_PROFILE`, the session's, the workspace rule, the default,
    /// the browser's last used. The session keeps it for its later tabs.
    pub fn profile(&self, browser: &Browser) -> Result<Profile> {
        let profile = self.chosen_profile(browser)?;
        if self.session_data().profile.as_deref() != Some(&profile.dir) {
            let dir = profile.dir.clone();
            self.update_session(|session| session.profile = Some(dir))?;
        }
        Ok(profile)
    }

    /// The profile [`Cx::profile`] picks, without keeping it for the session.
    pub fn chosen_profile(&self, browser: &Browser) -> Result<Profile> {
        self.choose_profile(&*self.config()?, &browser.profiles()?)
    }

    fn choose_profile(&self, config: &Config, profiles: &Profiles) -> Result<Profile> {
        let session = self.session_data().profile;
        let rule = config.workspace_profile(&self.cwd).or(config.default_profile.as_deref());
        profiles.choose(self.profile_flag.as_deref(), session.as_deref(), rule, &config.aliases).cloned()
    }

    /// Live tab target ids, used to prune and resolve tab references.
    pub async fn live_tabs(&self, browser: &Browser) -> Result<Vec<String>> {
        let live: Vec<String> = browser.tabs().await?.into_iter().map(|tab| tab.target_id).collect();
        let mut store = self.hub.store.lock().expect("store lock");
        store.prune(&live);
        for id in &live {
            store.number(id);
        }
        Ok(live)
    }

    /// Resolves a tab reference to a target id among all live tabs.
    pub async fn resolve_any_tab(&self, browser: &Browser, query: &str) -> Result<String> {
        let live = self.live_tabs(browser).await?;
        self.hub.store.lock().expect("store lock").resolve_tab(query, &live)
    }

    /// The tab commands act on: `-T` (must belong to the session) or the session's current tab.
    pub async fn current_tab(&self, browser: &Browser) -> Result<String> {
        if let Some(query) = &self.tab_flag {
            let tab = self.resolve_any_tab(browser, query).await?;
            if !self.session_data().owns(&tab) {
                bail!("{query} is not in session {:?}; take it over with `hba tab use {query}`", self.session);
            }
            return Ok(tab);
        }
        let current = self.session_data().current;
        let Some(tab) = current else {
            bail!("session {:?} has no tab; `hba open URL` creates one", self.session);
        };
        if browser.tab_info(&tab).await?.is_none() {
            self.update_session(|session| session.remove(&tab))?;
            bail!("the session's tab was closed; `hba open URL` creates a new one");
        }
        Ok(tab)
    }

    /// The page commands act on. Fails at once while the page shows a JavaScript dialog: the
    /// page answers nothing until the dialog is closed.
    pub async fn page(&mut self) -> Result<Arc<Page>> {
        let page = self.page_with_dialog().await?;
        match page.dialog() {
            Some(dialog) => Err(Page::dialog_error(&dialog)),
            None => Ok(page),
        }
    }

    /// Like [`Cx::page`], also while a dialog is open: for commands that answer it or only
    /// read what hba recorded.
    pub async fn page_with_dialog(&mut self) -> Result<Arc<Page>> {
        // Checked before connecting, so a session without tabs never starts the browser.
        if self.tab_flag.is_none() && self.session_data().current.is_none() {
            bail!("session {:?} has no tab; `hba open URL` creates one", self.session);
        }
        let browser = self.browser().await?;
        let tab = self.current_tab(&browser).await?;
        let page = browser.attach(&tab).await?;
        self.touch(&page);
        Ok(page)
    }

    /// Opens a new session tab in the session's profile and makes it current.
    pub async fn new_tab(&mut self, front: bool) -> Result<Arc<Page>> {
        let browser = self.browser().await?;
        let profile = self.profile(&browser)?;
        let tab = browser.create_tab(&profile, !front).await?;
        self.hub.save_contexts(&browser);
        self.update_session(|session| {
            session.add(&tab, false);
            session.current = Some(tab.clone());
        })?;
        let page = browser.attach(&tab).await?;
        if let Some(target) = page.page_target() {
            self.group(&browser, &page, &[target]).await;
        }
        if front {
            browser.cdp.send("Target.activateTarget", serde_json::json!({"targetId": tab}), None).await?;
        }
        self.touch(&page);
        Ok(page)
    }

    fn touch(&mut self, page: &Arc<Page>) {
        if !self.touched.iter().any(|p| Arc::ptr_eq(p, page)) {
            self.touched.push(Arc::clone(page));
        }
    }

    /// Releases the remote objects the command held (a busy renderer's answer is not awaited),
    /// and saves the tab numbers it handed out.
    fn release(&mut self) {
        for page in self.touched.drain(..) {
            page.release();
        }
        if let Err(error) = self.hub.store.lock().expect("store lock").save_changes() {
            eprintln!("hba: {error:#}");
        }
    }

    /// Starts watching for tabs an action opens, remembering which tabs the user is looking at.
    pub async fn watch(&self, browser: &Browser) -> PopupWatch {
        let events = browser.subscribe();
        let shown = browser.tabs().await.map(|tabs| shown_tabs(&tabs)).unwrap_or_default();
        PopupWatch { events, shown }
    }

    /// Tabs opened by `page` during the watch join the session as its own tabs. When a popup
    /// took over the user's view, the tab they were looking at is shown again.
    pub async fn adopt_popups(&mut self, browser: &Arc<Browser>, page: &Page, mut watch: PopupWatch) -> Result<()> {
        let Some(opener) = page.page_target() else { return Ok(()) };
        let popups = popups_of(&mut watch.events, &opener);
        if popups.is_empty() {
            return Ok(());
        }
        let mut grouped = Vec::new();
        for popup in &popups {
            if let Some(tab) = browser.tab_of_page(&popup.target_id, &popup.url).await? {
                self.update_session(|session| session.add(&tab, false))?;
                let label = self.tab_number(&tab);
                let url = browser.tab_info(&tab).await?.map_or_else(|| popup.url.clone(), |info| info.url);
                let _ = writeln!(
                    self.out,
                    "+ popup {label} {} — `hba tab use {label}`",
                    crate::output::short_url(&url, 100)
                );
                grouped.push(popup.target_id.clone());
            }
        }
        self.group(browser, page, &grouped).await;
        let now = shown_tabs(&browser.tabs().await?);
        for hidden in watch.shown.iter().filter(|tab| !now.contains(tab)) {
            browser.cdp.send("Target.activateTarget", serde_json::json!({"targetId": hidden}), None).await.ok();
        }
        Ok(())
    }

    /// Puts session tabs (page target ids) in the agents' tab group; `via` is a page in their
    /// profile. Grouping only shows the user where agents work, so a failure is a note.
    async fn group(&mut self, browser: &Arc<Browser>, via: &Page, pages: &[String]) {
        if pages.is_empty() {
            return;
        }
        let note = match crate::extensions::groups::add(browser, via, pages).await {
            Ok(hint) => hint,
            Err(error) => Some(format!("tabs are not grouped: {error:#}")),
        };
        if let Some(note) = note
            && !self.json
        {
            self.say(format!("note: {note}"));
        }
    }
}

/// Tabs opened during an action, and the tabs that were on screen before it.
pub struct PopupWatch {
    events: broadcast::Receiver<TargetEvent>,
    shown: Vec<String>,
}

fn shown_tabs(tabs: &[TargetInfo]) -> Vec<String> {
    tabs.iter()
        .filter(|tab| tab.embedder_data.as_ref().is_some_and(|strip| strip.active))
        .map(|tab| tab.target_id.clone())
        .collect()
}

/// Pages `opener` opened, among the target events received so far. Events a slow reader missed
/// are skipped, not the ones after them.
fn popups_of(events: &mut broadcast::Receiver<TargetEvent>, opener: &str) -> Vec<Arc<TargetInfo>> {
    let mut popups = Vec::new();
    loop {
        match events.try_recv() {
            Ok(TargetEvent::Created(info)) if info.kind == "page" && info.opener_id.as_deref() == Some(opener) => {
                popups.push(info);
            }
            Ok(_) | Err(broadcast::error::TryRecvError::Lagged(_)) => {}
            Err(_) => return popups,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_command_is_never_idle_time() {
        let start = Instant::now();
        let day = Duration::from_secs(86_400);
        let activity = Mutex::new(Activity { running: 0, since: start });
        let idle = |at| activity.lock().expect("activity lock").idle(at);
        assert_eq!(idle(start + day), day);
        let running = Running::new(&activity);
        // A command longer than `idle_minutes` must not let the daemon exit under it.
        assert_eq!(idle(start + day), Duration::ZERO);
        drop(running);
        assert!(idle(start + day) < day, "the idle time restarts when the command ends");
    }

    /// A hub with a fresh runtime directory.
    fn hub(name: &str) -> (Hub, PathBuf) {
        let runtime = std::env::temp_dir().join(format!("hba-daemon-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&runtime);
        std::fs::create_dir_all(&runtime).expect("create runtime dir");
        (Hub::new(runtime.clone()), runtime)
    }

    /// A browser endpoint that takes connections and answers nothing until the test does.
    async fn approval_endpoint() -> (tokio::net::TcpListener, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let url = format!("ws://{}/devtools/browser/abc", listener.local_addr().expect("address"));
        (listener, url)
    }

    #[tokio::test]
    async fn one_approval_request_at_a_time_and_a_late_allow_is_kept() {
        let (listener, url) = approval_endpoint().await;
        let (hub, runtime) = hub("late");
        let Approval::Pending { ended, behind: false } = hub.approval(&url, None) else { panic!("no request") };
        // Another command meanwhile waits for the same request instead of asking again.
        let Approval::Pending { ended: again, .. } = hub.approval(&url, None) else { panic!("not pending") };
        assert!(again.same_channel(&ended));
        let (request, _) = listener.accept().await.expect("the request");
        let second = tokio::time::timeout(Duration::from_millis(100), listener.accept()).await;
        assert!(second.is_err(), "a second request was opened");
        // Every command has given up when the user clicks Allow.
        let _allowed = tokio_tungstenite::accept_async(request).await.expect("handshake");
        let mut ended = ended;
        ended.wait_for(Option::is_some).await.expect("the request ends");
        assert!(matches!(hub.approval(&url, None), Approval::Allowed(_)), "the next command takes the connection");
        hub.record_abandoned();
        assert!(!runtime.join(ABANDONED).exists(), "nothing was left unanswered");
        std::fs::remove_dir_all(runtime).ok();
    }

    #[tokio::test]
    async fn a_refused_request_is_told_to_its_waiters_and_later_commands_ask_again() {
        use tokio::io::AsyncWriteExt as _;
        let (listener, url) = approval_endpoint().await;
        let (hub, runtime) = hub("refused");
        let Approval::Pending { mut ended, .. } = hub.approval(&url, None) else { panic!("no request") };
        let (mut request, _) = listener.accept().await.expect("the request");
        // What the browser answers when the user clicks Cancel.
        request.write_all(b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\n\r\n").await.expect("refuse");
        ended.wait_for(Option::is_some).await.expect("the request ends");
        assert!(matches!(hub.approval(&url, Some(&ended)), Approval::Refused(why) if why.contains("403")));
        assert!(matches!(hub.approval(&url, None), Approval::Pending { .. }), "a later command asks again");
        listener.accept().await.expect("the new request");
        std::fs::remove_dir_all(runtime).ok();
    }

    #[tokio::test]
    async fn a_request_an_earlier_daemon_left_is_named_once() {
        let (_listener, url) = approval_endpoint().await;
        let (hub, runtime) = hub("abandoned");
        let marker = runtime.join(ABANDONED);
        std::fs::write(&marker, &url).expect("mark");
        assert!(matches!(hub.approval(&url, None), Approval::Pending { behind: true, .. }));
        assert!(matches!(hub.approval_request(), Some(ApprovalRequest::Waiting { behind: true })));
        assert!(!marker.exists(), "the marker is read once");
        // Exiting with this request unanswered leaves it for the next daemon.
        hub.record_abandoned();
        assert_eq!(std::fs::read_to_string(&marker).expect("marker"), url);
        std::fs::remove_dir_all(runtime).ok();
    }

    /// A request line as a client of `build` with environment `env` sends it.
    fn request_line(build: &str, env: &str, command: &serde_json::Value) -> String {
        let request = serde_json::json!({
            "build": build, "env": env, "cwd": "/", "session": null, "profile": null, "tab": null,
            "json": false, "timeout_secs": null, "command": command,
        });
        request.to_string()
    }

    #[test]
    fn requests_are_admitted_by_build_then_by_environment() {
        use serde_json::json;
        // A newer client may send a command this build does not know: it needs another daemon.
        let unknown = request_line("0.0.0+newer", "mine", &json!({"Teleport": {"to": "mars"}}));
        assert!(matches!(admit(&unknown, "mine"), Err(Reply::Restart)));
        assert!(matches!(admit(&request_line(BUILD, "theirs", &json!("Back")), "mine"), Err(Reply::Restart)));
        assert!(admit(&request_line(BUILD, "mine", &json!("Back")), "mine").is_ok());
        // Daemon commands are about whichever daemon runs.
        let status = request_line(BUILD, "theirs", &json!({"Daemon": {"op": "Status"}}));
        assert!(admit(&status, "mine").is_ok_and(|request| request.command.is_local()));
        let garbled = request_line(BUILD, "mine", &json!({"Teleport": {}}));
        assert!(matches!(admit(&garbled, "mine"), Err(Reply::Done { code: 1, .. })));
        assert!(matches!(admit("{", "mine"), Err(Reply::Done { code: 1, .. })));
    }

    #[test]
    fn the_trace_names_a_command_without_its_arguments() {
        use clap::Parser as _;
        let command = |args: &[&str]| crate::cli::Cli::try_parse_from(args).expect("parse").command;
        assert_eq!(command_name(&command(&["hba", "fill", "e1", "hunter2"])), "Fill");
        assert_eq!(command_name(&command(&["hba", "back"])), "Back");
    }

    #[test]
    fn popups_after_events_a_slow_reader_missed_are_still_found() {
        let (events, mut watch) = broadcast::channel(2);
        let page = |id: &str, opener: &str| {
            let info = serde_json::json!({"targetId": id, "type": "page", "openerId": opener});
            TargetEvent::Created(Arc::new(serde_json::from_value(info).expect("target info")))
        };
        for event in [page("A", "other"), page("B", "other"), page("P", "me")] {
            events.send(event).expect("send");
        }
        let popups = popups_of(&mut watch, "me");
        let found: Vec<&str> = popups.iter().map(|info| info.target_id.as_str()).collect();
        assert_eq!(found, ["P"]);
    }

    #[test]
    fn artifacts_older_than_a_day_are_deleted() {
        let dir = std::env::temp_dir().join(format!("hba-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("old-trace")).expect("create the directories");
        let now = SystemTime::now();
        let old = now - ARTIFACT_LIFETIME - Duration::from_secs(60);
        for (name, at) in [("old.png", old), ("old-trace", old), ("new.png", now - Duration::from_secs(60))] {
            let path = dir.join(name);
            let file = if path.is_dir() { std::fs::File::open(&path) } else { std::fs::File::create(&path) };
            file.expect("open").set_modified(at).expect("set the time");
        }
        prune_artifacts(&dir, now);
        let left: Vec<String> = std::fs::read_dir(&dir)
            .expect("list")
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, ["new.png"]);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    fn locks_of_sessions_nothing_waits_for_are_dropped() {
        let (hub, runtime) = hub("locks");
        let held = hub.session_lock("busy");
        drop(hub.session_lock("done"));
        drop(hub.session_lock("next"));
        let mut names: Vec<String> = hub.locks.lock().expect("locks lock").keys().cloned().collect();
        names.sort_unstable();
        assert_eq!(names, ["busy", "next"]);
        drop(held);
        std::fs::remove_dir_all(runtime).ok();
    }

    #[tokio::test]
    async fn clients_queued_at_shutdown_are_told_to_restart() {
        use std::io::{BufRead as _, Write as _};
        let (_hub, runtime) = hub("queued");
        let path = runtime.join("queued.sock");
        let listener = UnixListener::bind(&path).expect("bind");
        let mut client = std::os::unix::net::UnixStream::connect(&path).expect("connect");
        client.write_all(request_line(BUILD, "", &serde_json::json!("Back")).as_bytes()).expect("send");
        client.write_all(b"\n").expect("send");
        let mut connections = tokio::task::JoinSet::new();
        refuse_queued(listener, &mut connections);
        while connections.join_next().await.is_some() {}
        let mut reply = String::new();
        std::io::BufReader::new(client).read_line(&mut reply).expect("the reply");
        assert_eq!(reply, Reply::Restart.line(), "not dropped with the listener");
        std::fs::remove_dir_all(runtime).ok();
    }
}
