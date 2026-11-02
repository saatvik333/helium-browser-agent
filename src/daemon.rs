//! The background process: owns the browser connection, sessions and attached tabs.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc, watch};

use crate::browser::Browser;
use crate::cdp::{Cdp, Event};
use crate::cli::MAX_TIMEOUT;
use crate::config::Config;
use crate::injected::Source;
use crate::ipc::{self, BUILD, DAEMON_DRAIN, Reply, Request};
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

/// The environment this daemon started with; see [`ipc::daemon_env`].
static DAEMON_ENV: LazyLock<String> = LazyLock::new(ipc::daemon_env);

pub fn main() -> ExitCode {
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
    let lock = std::fs::File::create(runtime.join("daemon.lock"))?;
    // A daemon that is shutting down holds the lock until its last command finishes.
    let deadline = Instant::now() + DAEMON_DRAIN + Duration::from_secs(5);
    while lock.try_lock().is_err() {
        if Instant::now() >= deadline {
            return Ok(()); // Another daemon is running.
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let socket = ipc::socket_path(&runtime);
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
    drop(listener);
    let _ = std::fs::remove_file(&socket);
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
            if let Some(browser) = slot.as_ref().filter(|browser| browser.is_connected()) {
                return Ok(Arc::clone(browser));
            }
            let config = self.config()?;
            let profile: &Option<String> = profile.get_or_insert_with(|| launch_profile.take().and_then(|hint| hint()));
            let endpoint = crate::endpoint::ensure_running(&config, profile.as_deref()).await?;
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
        let reply = match serde_json::from_str::<Request>(&line) {
            Ok(request) if request.build != BUILD || request.env != *DAEMON_ENV => {
                self.stop.notify_one();
                Reply::Restart
            }
            Ok(request) => match self.execute(request, hung_up(read)).await {
                Some(reply) => reply,
                None => return,
            },
            Err(error) => Reply::Done { stdout: String::new(), stderr: format!("hba: bad request: {error}"), code: 2 },
        };
        let mut text = serde_json::to_string(&reply).unwrap_or_default();
        text.push('\n');
        let _ = write.write_all(text.as_bytes()).await;
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
        let lock = Arc::clone(self.locks.lock().expect("locks lock").entry(session.clone()).or_default());
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
        let name = if *crate::trace::ENABLED {
            format!("{:?}", request.command).chars().take(100).collect::<String>()
        } else {
            String::new()
        };
        let mut hung_up = std::pin::pin!(hung_up);
        // Commands of one session run in order.
        let locked = if request.command.is_local() {
            Some(Ok(None))
        } else {
            tokio::select! {
                locked = tokio::time::timeout(Duration::from_secs(120), lock.lock_owned()) => Some(locked.map(Some)),
                () = &mut hung_up => None,
            }
        };
        let outcome = match locked {
            None => None,
            Some(Err(_)) => Some(Err(anyhow::anyhow!("session {:?} is busy with another hba command", cx.session))),
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

    /// A new file in the runtime directory, e.g. for screenshots.
    pub fn artifact(&self, stem: &str, extension: &str) -> Result<PathBuf> {
        let dir = self.hub.runtime.join("files");
        std::fs::create_dir_all(&dir)?;
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis();
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

    /// Releases the remote objects the command held; a busy renderer's answer is not awaited.
    fn release(&mut self) {
        for page in self.touched.drain(..) {
            page.release();
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
    pub async fn adopt_popups(&mut self, browser: &Browser, page: &Page, mut watch: PopupWatch) -> Result<()> {
        let Some(opener) = page.page_target() else { return Ok(()) };
        let mut popups = Vec::new();
        while let Ok(event) = watch.events.try_recv() {
            if let crate::browser::TargetEvent::Created(info) = event
                && info.kind == "page"
                && info.opener_id.as_deref() == Some(opener.as_str())
            {
                popups.push(info);
            }
        }
        if popups.is_empty() {
            return Ok(());
        }
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
            }
        }
        let now = shown_tabs(&browser.tabs().await?);
        for hidden in watch.shown.iter().filter(|tab| !now.contains(tab)) {
            browser.cdp.send("Target.activateTarget", serde_json::json!({"targetId": hidden}), None).await.ok();
        }
        Ok(())
    }
}

/// Tabs opened during an action, and the tabs that were on screen before it.
pub struct PopupWatch {
    events: tokio::sync::broadcast::Receiver<crate::browser::TargetEvent>,
    shown: Vec<String>,
}

fn shown_tabs(tabs: &[crate::browser::TargetInfo]) -> Vec<String> {
    tabs.iter()
        .filter(|tab| tab.embedder_data.as_ref().is_some_and(|strip| strip.active))
        .map(|tab| tab.target_id.clone())
        .collect()
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
}
