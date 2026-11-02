//! Command-line surface. The same types travel to the daemon as the request payload.

use std::path::PathBuf;
use std::time::Duration;

use clap::{Args, Parser, Subcommand, ValueEnum};
use serde::{Deserialize, Serialize};

/// Drive your running Helium browser: profiles, tabs, pages, DevTools and extensions.
///
/// TARGET is a ref from `hba snap` (e5, f1e3) or a Playwright selector: css, text=Save,
/// role=button[name="Save"], internal:label="Email", xpath=//a.
#[derive(Debug, Parser)]
#[command(name = "hba", version, disable_help_subcommand = true, max_term_width = 100)]
pub struct Cli {
    /// Task session (default: the project directory name)
    #[arg(short, long, global = true, env = "HBA_SESSION", value_parser = session_name, help_heading = GLOBAL)]
    pub session: Option<String>,
    /// Helium profile for new tabs: display name, directory or alias
    #[arg(short, long, global = true, env = "HBA_PROFILE", help_heading = GLOBAL)]
    pub profile: Option<String>,
    /// Act on this session tab instead of the current one
    #[arg(short = 'T', long, global = true, value_name = "TAB", help_heading = GLOBAL)]
    pub tab: Option<String>,
    /// Print machine-readable JSON
    #[arg(long, global = true, help_heading = GLOBAL)]
    pub json: bool,
    /// Give up after this many seconds (default 30, at most a day)
    #[arg(long, global = true, value_name = "SECS", value_parser = positive_secs, help_heading = GLOBAL)]
    pub timeout: Option<f64>,
    #[command(subcommand)]
    pub command: Command,
}

/// Help section for the options every command accepts.
const GLOBAL: &str = "Global options";

/// Longest `--timeout`: a day.
pub const MAX_TIMEOUT: Duration = Duration::from_secs(86_400);

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum Command {
    /// Navigate the current tab (creating a tab if the session has none)
    Open(OpenArgs),
    /// Go back in history
    Back,
    /// Go forward in history
    Forward,
    /// Reload the page
    Reload(ReloadArgs),
    /// Accessibility snapshot with refs for acting (Playwright AI format)
    Snap(SnapArgs),
    /// Find elements by accessible name or text; prints matching snapshot lines
    Find(FindArgs),
    /// Visible text of the page or an element
    Text(TextArgs),
    /// Read one property: url, title, html, value, attr, count, box, style, locator
    Get(GetArgs),
    /// Check an element state; prints true or false
    Is(IsArgs),
    /// Screenshot the viewport, full page or an element; prints the file path
    Shot(ShotArgs),
    /// Save the page as PDF; prints the file path
    Pdf(PdfArgs),
    /// Click an element
    Click(ClickArgs),
    /// Move the mouse over an element
    Hover(TargetArg),
    /// Focus an element
    Focus(TargetArg),
    /// Replace a field's text (input, textarea, contenteditable)
    Fill(FillArgs),
    /// Type text with real key events, optionally into a field first
    Type(TypeArgs),
    /// Press keys or chords: Enter, Control+a, Shift+Tab
    Press(PressArgs),
    /// Check a checkbox or radio
    Check(TargetArg),
    /// Uncheck a checkbox
    Uncheck(TargetArg),
    /// Choose options of a <select> by value or label
    Select(SelectArgs),
    /// Set files on a file input, or answer the file chooser its click opens
    Upload(UploadArgs),
    /// Drag one element onto another
    Drag(DragArgs),
    /// Scroll the page or an element (wheel events)
    Scroll(ScrollArgs),
    /// Raw mouse input at viewport coordinates
    Mouse(MouseArgs),
    /// Wait for time, an element, text, URL, load state or a JS condition
    Wait(WaitArgs),
    /// Show, accept or dismiss the page's JavaScript dialog
    Dialog(DialogArgs),
    /// Click an element and save the download it starts
    Download(DownloadArgs),
    /// Evaluate JavaScript in the page (top-level await allowed); `-` reads stdin
    Js(JsArgs),
    /// Send a raw DevTools protocol command
    Cdp(CdpArgs),
    /// Run hba commands from a file or stdin, one per line, in one call
    Run(RunArgs),
    /// List this session's tabs, or every tab with --all
    Tabs(TabsArgs),
    /// Create, use (adopt), close, release or show tabs
    Tab(TabArgs),
    /// Resize, maximize, minimize or fullscreen the tab's window
    Window(WindowArgs),
    /// List Helium profiles
    Profiles,
    /// Session, tab and browser summary
    Status,
    /// Close the session's own tabs and release adopted ones
    Close(CloseArgs),
    /// Console messages captured from the page
    Console(ConsoleArgs),
    /// Uncaught exceptions, failed requests and browser errors
    Errors(ErrorsArgs),
    /// Network log, bodies, blocking, mocking and HAR
    Net(NetArgs),
    /// Performance: metrics, web vitals, traces, CPU profiles, heap, coverage
    Perf(PerfArgs),
    /// Open Chrome DevTools for the tab (for a human)
    Devtools(DevtoolsArgs),
    /// Emulate device, viewport, locale, timezone, geolocation, media, network or CPU
    Emulate(EmulateArgs),
    /// Read and change cookies
    Cookies(CookiesArgs),
    /// Read and change localStorage or sessionStorage of the page's origin
    Storage(StorageArgs),
    /// Save or load cookies and storage to a JSON file
    State(StateArgs),
    /// Grant or reset site permissions
    Perms(PermsArgs),
    /// Extensions: list, load, reload, remove, eval, logs, errors, popup, storage
    Ext(ExtArgs),
    /// Read or write the system clipboard
    Clip(ClipArgs),
    /// Launch, restart or locate the browser
    Browser(BrowserArgs),
    /// Manage the background daemon
    Daemon(DaemonArgs),
    /// Check the browser endpoint, profiles and environment
    Doctor,
}

impl Command {
    /// Commands that never touch the browser or a session.
    pub fn is_local(&self) -> bool {
        matches!(self, Command::Daemon(_))
    }
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct TargetArg {
    /// Element: ref from `hba snap` or a Playwright selector
    pub target: String,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct OpenArgs {
    /// URL; a bare host gets https://
    pub url: String,
    /// Open in a new session tab instead of the current one
    #[arg(short, long)]
    pub new: bool,
    /// Show the tab to the user (activates it in its window)
    #[arg(long)]
    pub front: bool,
    /// When to return
    #[arg(long, value_enum, default_value_t = LoadState::Dom)]
    pub wait: LoadState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum LoadState {
    /// The navigation committed
    Commit,
    /// DOMContentLoaded fired and the DOM settled
    Dom,
    /// The load event fired
    Load,
    /// Load fired and the network stayed quiet for 500 ms
    Idle,
    /// Return immediately
    None,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct ReloadArgs {
    /// Bypass the cache
    #[arg(long)]
    pub hard: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct SnapArgs {
    /// Only interactive elements (with their landmarks and headings)
    #[arg(short, long)]
    pub interactive: bool,
    /// Only elements inside the viewport
    #[arg(short, long)]
    pub viewport: bool,
    /// Print only what changed since this tab's previous snapshot
    #[arg(short, long)]
    pub diff: bool,
    /// Limit to the subtree of this ref or selector
    #[arg(short = 'S', long, value_name = "TARGET")]
    pub scope: Option<String>,
    /// Maximum tree depth
    #[arg(long)]
    pub depth: Option<u32>,
    /// Add element boxes [box=x,y,w,h] in viewport pixels
    #[arg(long)]
    pub boxes: bool,
    /// Exact Playwright output without compaction
    #[arg(long)]
    pub raw: bool,
    /// Character budget; the full snapshot spills to a file beyond it
    #[arg(long, default_value_t = 24_000)]
    pub max: usize,
    /// Write the snapshot to this file instead of stdout
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct FindArgs {
    /// Text to look for (case-insensitive), or /regex/
    pub query: String,
    /// Only this ARIA role
    #[arg(long)]
    pub role: Option<String>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct TextArgs {
    /// Element (default: the whole page)
    pub target: Option<String>,
    /// Character budget
    #[arg(long, default_value_t = 12_000)]
    pub max: usize,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct GetArgs {
    /// What to read
    #[arg(value_enum)]
    pub what: GetWhat,
    /// Element (not needed for url and title)
    pub target: Option<String>,
    /// Attribute or CSS property name for attr and style
    pub name: Option<String>,
    /// Character budget for html
    #[arg(long, default_value_t = 12_000)]
    pub max: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum GetWhat {
    Url,
    Title,
    Html,
    Inner,
    Value,
    Attr,
    Count,
    Box,
    Style,
    Locator,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct IsArgs {
    /// State to check
    #[arg(value_enum)]
    pub state: ElementState,
    /// Element: ref from `hba snap` or a Playwright selector
    pub target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum ElementState {
    Visible,
    Hidden,
    Enabled,
    Disabled,
    Checked,
    Unchecked,
    Editable,
    Focused,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct ShotArgs {
    /// Output file (default: a new file in the runtime directory)
    pub path: Option<PathBuf>,
    /// Capture the whole scrollable page
    #[arg(short, long)]
    pub full: bool,
    /// Capture only this element
    #[arg(short, long, value_name = "TARGET")]
    pub element: Option<String>,
    /// Draw ref labels over interactive elements
    #[arg(short, long)]
    pub annotate: bool,
    /// PNG instead of JPEG
    #[arg(long)]
    pub png: bool,
    /// JPEG quality 1-100
    #[arg(long, default_value_t = 80, value_parser = clap::value_parser!(u8).range(1..=100))]
    pub quality: u8,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct PdfArgs {
    /// Output file (default: a new file in the runtime directory)
    pub path: Option<PathBuf>,
    /// Landscape orientation
    #[arg(long)]
    pub landscape: bool,
    /// Paper size: A4, Letter, Legal, A3, Tabloid
    #[arg(long, default_value = "A4")]
    pub format: String,
    /// Include CSS backgrounds
    #[arg(long)]
    pub background: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct ClickArgs {
    /// Element: ref from `hba snap` or a Playwright selector
    pub target: String,
    /// Mouse button
    #[arg(long, value_enum, default_value_t = MouseButton::Left)]
    pub button: MouseButton,
    /// Number of clicks (2 = double click)
    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u8).range(1..=3))]
    pub count: u8,
    /// Hold modifiers: Alt, Control, Meta, Shift (comma-separated)
    #[arg(long, value_delimiter = ',')]
    pub modifiers: Vec<String>,
    /// Click at this offset from the element's top-left corner: X,Y
    #[arg(long, value_parser = parse_point)]
    pub at: Option<(f64, f64)>,
    /// Skip visibility, enabled and hit-target checks
    #[arg(long)]
    pub force: bool,
    /// Print a snapshot diff after the page settles
    #[arg(short, long)]
    pub diff: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct FillArgs {
    /// Field: ref from `hba snap` or a Playwright selector
    pub target: String,
    /// New text; `-` reads stdin
    pub text: String,
    /// Press Enter after filling
    #[arg(long)]
    pub submit: bool,
    /// Print a snapshot diff after the page settles
    #[arg(short, long)]
    pub diff: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct TypeArgs {
    /// Text to type; `-` reads stdin
    pub text: String,
    /// Focus this element first
    #[arg(short, long, value_name = "TARGET")]
    pub into: Option<String>,
    /// Milliseconds between keys, at most 10000 (default: human-like on websites, none on local pages)
    #[arg(long, value_parser = clap::value_parser!(u64).range(..=10_000))]
    pub delay: Option<u64>,
    /// Press Enter afterwards
    #[arg(long)]
    pub submit: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct PressArgs {
    /// Keys or chords, pressed in order
    #[arg(required = true)]
    pub keys: Vec<String>,
    /// Focus this element first
    #[arg(short, long, value_name = "TARGET")]
    pub into: Option<String>,
    /// Print a snapshot diff after the page settles
    #[arg(short, long)]
    pub diff: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct SelectArgs {
    /// The <select>: ref from `hba snap` or a Playwright selector
    pub target: String,
    /// Option values or labels
    #[arg(required = true)]
    pub values: Vec<String>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct UploadArgs {
    /// File input, or the element whose click opens a file chooser
    pub target: String,
    /// Files to upload
    #[arg(required = true)]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct DragArgs {
    /// Element to drag: ref or selector
    pub source: String,
    /// Element to drop onto: ref or selector
    pub destination: String,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct ScrollArgs {
    /// up, down, left, right, top or bottom
    #[arg(value_enum, default_value_t = ScrollDirection::Down)]
    pub direction: ScrollDirection,
    /// Pixels (default: most of a screen)
    pub amount: Option<f64>,
    /// Scroll inside this element, or bring it into view with --into-view
    #[arg(short, long, value_name = "TARGET")]
    pub element: Option<String>,
    /// Scroll the element into view instead of wheeling
    #[arg(long, requires = "element")]
    pub into_view: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum ScrollDirection {
    Up,
    Down,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct MouseArgs {
    /// What to do at the point
    #[arg(value_enum)]
    pub action: MouseAction,
    /// Viewport X in CSS pixels
    pub x: f64,
    /// Viewport Y in CSS pixels
    pub y: f64,
    /// Button for down, up and click
    #[arg(long, value_enum, default_value_t = MouseButton::Left)]
    pub button: MouseButton,
    /// Wheel delta for `wheel` (dy, or dx with --horizontal)
    #[arg(long, default_value_t = 0.0, allow_negative_numbers = true)]
    pub delta: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum MouseAction {
    Move,
    Down,
    Up,
    Click,
    Wheel,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct WaitArgs {
    /// Milliseconds (at most a day), or an element to wait for
    pub what: Option<String>,
    /// With an element: wait until it is gone/hidden instead
    #[arg(long)]
    pub gone: bool,
    /// Wait until the page shows this text
    #[arg(long)]
    pub text: Option<String>,
    /// Wait until the URL contains this, or matches it as a glob when it has `*`
    #[arg(long)]
    pub url: Option<String>,
    /// Wait for a load state
    #[arg(long, value_enum)]
    pub load: Option<LoadState>,
    /// Wait until this JavaScript expression is truthy
    #[arg(long, value_name = "JS")]
    pub until: Option<String>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct DialogArgs {
    /// Omit to show the pending dialog
    #[arg(value_enum)]
    pub action: Option<DialogAction>,
    /// Text for a prompt dialog
    pub text: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum DialogAction {
    Accept,
    Dismiss,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct DownloadArgs {
    /// Element whose click starts the download
    pub target: String,
    /// Destination file or directory (default: where Helium saved it)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct JsArgs {
    /// Expression or statements; `-` reads stdin
    pub code: String,
    /// Run in this frame (an iframe ref or selector)
    #[arg(long, value_name = "TARGET")]
    pub frame: Option<String>,
    /// Run in an extension's content-script world on this page (extension id or name)
    #[arg(long, value_name = "EXTENSION")]
    pub world: Option<String>,
    /// Character budget for the result
    #[arg(long, default_value_t = 16_000)]
    pub max: usize,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct CdpArgs {
    /// Domain.method
    pub method: String,
    /// JSON parameters; `-` reads stdin
    pub params: Option<String>,
    /// Send to the browser instead of the tab
    #[arg(long)]
    pub browser: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct RunArgs {
    /// Script file; omit or `-` for stdin
    pub file: Option<PathBuf>,
    /// Keep going after a failing line
    #[arg(long)]
    pub keep_going: bool,
    /// Parsed lines (filled in by the client)
    #[arg(skip)]
    pub commands: Vec<(String, Command)>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct TabsArgs {
    /// Every tab of every open profile, in tab-strip order
    #[arg(short, long)]
    pub all: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct TabArgs {
    #[command(subcommand)]
    pub op: TabOp,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum TabOp {
    /// Open a new session tab and make it current
    New {
        url: Option<String>,
        #[arg(long)]
        front: bool,
    },
    /// Make a tab current; a tab the session doesn't own is adopted (from `hba tabs --all`)
    Use { id: String },
    /// Close a session tab, adopted ones too (default: the current one); `hba close` only releases those
    Close { id: Option<String> },
    /// Stop controlling an adopted tab; it stays open
    Release { id: Option<String> },
    /// Show the tab to the user (activates it in its window)
    Front { id: Option<String> },
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct WindowArgs {
    /// What to do with the window
    #[arg(value_enum)]
    pub state: WindowState,
    /// New size, for `size`
    #[arg(value_parser = parse_size, value_name = "WIDTHxHEIGHT")]
    pub size: Option<(u32, u32)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum WindowState {
    Normal,
    Maximized,
    Minimized,
    Fullscreen,
    Size,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct CloseArgs {
    /// Close every session of this daemon
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct ConsoleArgs {
    /// Number of most recent messages
    #[arg(short = 'n', long, default_value_t = 50)]
    pub last: usize,
    /// Minimum level
    #[arg(long, value_enum)]
    pub level: Option<ConsoleLevel>,
    /// Forget captured messages
    #[arg(long)]
    pub clear: bool,
    /// Start capturing on this page (detectable by some anti-bot scripts; automatic on local pages)
    #[arg(long, conflicts_with = "off")]
    pub on: bool,
    /// Stop capturing on this page
    #[arg(long)]
    pub off: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, ValueEnum, Serialize, Deserialize)]
pub enum ConsoleLevel {
    Debug,
    Log,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct ErrorsArgs {
    /// Number of most recent errors
    #[arg(short = 'n', long, default_value_t = 50)]
    pub last: usize,
    /// Forget captured errors
    #[arg(long)]
    pub clear: bool,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct NetArgs {
    #[command(subcommand)]
    pub op: Option<NetOp>,
    /// Only URLs containing this
    #[arg(short, long)]
    pub filter: Option<String>,
    /// Only this resource type: document, xhr, fetch, script, stylesheet, image, font, media, websocket, other
    #[arg(long = "type")]
    pub kind: Option<String>,
    /// Only failed requests and HTTP errors
    #[arg(long)]
    pub failed: bool,
    /// Number of most recent requests
    #[arg(short = 'n', long, default_value_t = 50)]
    pub last: usize,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum NetOp {
    /// Request details: headers, timing, status
    Show {
        /// Request number from `hba net`
        id: String,
    },
    /// Response body (text, or saved with -o)
    Body {
        /// Request number from `hba net`
        id: String,
        /// Save the body to this file
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Character budget for printed text
        #[arg(long, default_value_t = 16_000)]
        max: usize,
    },
    /// Fail requests whose URL matches a glob (* and ?)
    Block {
        /// URL globs
        #[arg(required = true)]
        patterns: Vec<String>,
    },
    /// Answer matching requests with a canned response
    Mock {
        /// URL glob
        pattern: String,
        /// HTTP status
        #[arg(long, default_value_t = 200)]
        status: u16,
        /// Response body
        #[arg(long, conflicts_with = "file")]
        body: Option<String>,
        /// Response body from a file
        #[arg(long)]
        file: Option<PathBuf>,
        /// Content-Type header
        #[arg(long = "type")]
        content_type: Option<String>,
        /// Extra header Name:Value (repeatable)
        #[arg(long = "header")]
        headers: Vec<String>,
    },
    /// Remove blocking/mocking rules (all when no pattern)
    Unroute {
        /// Rule pattern to remove
        pattern: Option<String>,
    },
    /// Record a HAR file: start, then stop FILE
    Har {
        #[arg(value_enum)]
        action: StartStop,
        /// HAR file to write on stop
        path: Option<PathBuf>,
    },
    /// Forget the captured log
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum StartStop {
    Start,
    Stop,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct PerfArgs {
    #[command(subcommand)]
    pub op: PerfOp,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum PerfOp {
    /// Chrome performance metrics (layout, script, heap, nodes)
    Metrics,
    /// Web vitals from buffered entries: TTFB, FCP, LCP, CLS, INP
    Vitals,
    /// DevTools performance trace: start, then stop FILE (open in DevTools)
    Trace {
        #[arg(value_enum)]
        action: StartStop,
        /// Trace file to write on stop
        path: Option<PathBuf>,
    },
    /// JavaScript CPU profile: start, then stop FILE (.cpuprofile)
    Profile {
        #[arg(value_enum)]
        action: StartStop,
        /// Profile file to write on stop
        path: Option<PathBuf>,
    },
    /// Heap snapshot to FILE (.heapsnapshot)
    Heap {
        /// Snapshot file (default: a new file in the runtime directory)
        path: Option<PathBuf>,
    },
    /// JS and CSS coverage: start, then stop for a per-file summary
    Coverage {
        #[arg(value_enum)]
        action: StartStop,
        /// Write full coverage JSON here on stop
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct DevtoolsArgs {
    /// elements, console, network, sources, resources, timeline, lighthouse, security
    pub panel: Option<String>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct EmulateArgs {
    /// Device preset (see --list)
    #[arg(long)]
    pub device: Option<String>,
    /// Viewport in CSS pixels
    #[arg(long, value_parser = parse_size, value_name = "WIDTHxHEIGHT")]
    pub viewport: Option<(u32, u32)>,
    /// Device pixel ratio
    #[arg(long)]
    pub dpr: Option<f64>,
    /// Mobile layout and touch
    #[arg(long)]
    pub mobile: bool,
    /// User agent string (client hints follow it)
    #[arg(long)]
    pub ua: Option<String>,
    /// Language, e.g. en-US
    #[arg(long)]
    pub locale: Option<String>,
    /// IANA timezone, e.g. Asia/Kolkata
    #[arg(long)]
    pub tz: Option<String>,
    /// LATITUDE,LONGITUDE
    #[arg(long, value_parser = parse_point, allow_hyphen_values = true)]
    pub geo: Option<(f64, f64)>,
    /// prefers-color-scheme
    #[arg(long, value_enum)]
    pub scheme: Option<ColorScheme>,
    /// CSS media type
    #[arg(long, value_enum)]
    pub media: Option<MediaType>,
    /// prefers-reduced-motion: reduce
    #[arg(long)]
    pub reduced_motion: bool,
    /// Network conditions
    #[arg(long, value_enum)]
    pub network: Option<NetworkPreset>,
    /// CPU slowdown factor (1 = none)
    #[arg(long)]
    pub cpu: Option<f64>,
    /// List device presets
    #[arg(long)]
    pub list: bool,
    /// Remove all emulation from the tab
    #[arg(long)]
    pub reset: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum ColorScheme {
    Light,
    Dark,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum MediaType {
    Screen,
    Print,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum NetworkPreset {
    Offline,
    Slow3g,
    Fast3g,
    Slow4g,
    None,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct CookiesArgs {
    #[command(subcommand)]
    pub op: Option<CookiesOp>,
    /// Cookies sent to this URL (default: the page URL)
    #[arg(long)]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum CookiesOp {
    /// Set a cookie for the page's origin or --url
    Set {
        /// Cookie name
        name: String,
        /// Cookie value
        value: String,
        /// Domain (default: the page's host)
        #[arg(long)]
        domain: Option<String>,
        /// Path (default: /)
        #[arg(long)]
        path: Option<String>,
        /// Send only over HTTPS
        #[arg(long)]
        secure: bool,
        /// Hide from page JavaScript
        #[arg(long)]
        http_only: bool,
        /// Strict, Lax or None
        #[arg(long)]
        same_site: Option<String>,
        /// Expiry as Unix seconds (default: session cookie)
        #[arg(long)]
        expires: Option<f64>,
    },
    /// Delete cookies by name for the page's origin or --url
    Rm {
        /// Cookie name
        name: String,
    },
    /// Delete every cookie of the page's site
    Clear,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct StorageArgs {
    /// Which storage of the page's origin
    #[arg(value_enum, default_value_t = StorageArea::Local)]
    pub area: StorageArea,
    #[command(subcommand)]
    pub op: Option<StorageOp>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum StorageArea {
    Local,
    Session,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum StorageOp {
    /// Print one key's value (default: list every key)
    Get {
        /// Storage key
        key: String,
    },
    /// Set a key (extension storage reads VALUE as JSON when it parses)
    Set {
        /// Storage key
        key: String,
        /// New value
        value: String,
    },
    /// Delete a key
    Rm {
        /// Storage key
        key: String,
    },
    /// Delete every key
    Clear,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct StateArgs {
    /// Save the page site's cookies and storage, or load them back
    #[arg(value_enum)]
    pub action: SaveLoad,
    /// State file (Playwright storage-state format)
    pub path: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum SaveLoad {
    Save,
    Load,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct PermsArgs {
    #[command(subcommand)]
    pub op: PermsOp,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum PermsOp {
    /// Grant permissions (geolocation, notifications, clipboardReadWrite, …) to the page's origin
    Grant {
        /// Permission names
        #[arg(required = true)]
        permissions: Vec<String>,
        /// Origin (default: the page's)
        #[arg(long)]
        origin: Option<String>,
    },
    /// Reset all permission overrides
    Reset,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct ExtArgs {
    #[command(subcommand)]
    pub op: Option<ExtOp>,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum ExtOp {
    /// Install an unpacked extension directory into the active profile (until browser restart)
    Load {
        /// Unpacked extension directory (with manifest.json)
        dir: PathBuf,
    },
    /// Reload an extension by id or unpacked directory
    Reload {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
    },
    /// Uninstall an unpacked extension
    Rm {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
    },
    /// Evaluate JavaScript in the extension's service worker (await allowed)
    Eval {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
        /// JavaScript; `-` reads stdin
        code: String,
    },
    /// Console messages from the extension's service worker and pages
    Logs {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
        /// Number of most recent messages
        #[arg(short = 'n', long, default_value_t = 50)]
        last: usize,
        /// Forget captured messages
        #[arg(long)]
        clear: bool,
    },
    /// Manifest and runtime errors reported by Helium
    Errors {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
        /// Forget recorded errors
        #[arg(long)]
        clear: bool,
    },
    /// Open the toolbar popup (or run the action) on the current tab; the popup becomes current
    Popup {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
    },
    /// Open an extension page in a session tab (default: options page)
    Open {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
        /// Page path inside the extension, e.g. page.html
        page: Option<String>,
    },
    /// Read or change chrome.storage
    Storage {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
        /// chrome.storage area
        #[arg(value_enum, default_value_t = ExtStorageArea::Local)]
        area: ExtStorageArea,
        #[command(subcommand)]
        op: Option<StorageOp>,
    },
    /// Service worker: status, start or stop
    Sw {
        /// Extension id, unpacked directory, or (part of) its name
        ext: String,
        /// Show the worker's state, or start or stop it
        #[arg(value_enum, default_value_t = SwAction::Status)]
        action: SwAction,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum ExtStorageArea {
    Local,
    Sync,
    Session,
    Managed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
pub enum SwAction {
    Status,
    Start,
    Stop,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct ClipArgs {
    /// Text to copy; `-` reads stdin. Omit to print the clipboard.
    pub text: Option<String>,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct BrowserArgs {
    #[command(subcommand)]
    pub op: BrowserOp,
}

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize)]
pub enum BrowserOp {
    /// Start Helium with a profile if it is not running
    Launch,
    /// Quit and relaunch Helium, restoring every open profile's tabs
    Restart,
    /// Print the verified DevTools WebSocket URL (for other CDP tools)
    Endpoint,
}

#[derive(Debug, Clone, Args, Serialize, Deserialize)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub op: DaemonOp,
}

#[derive(Debug, Clone, Copy, Subcommand, Serialize, Deserialize)]
pub enum DaemonOp {
    /// Daemon process, uptime and sessions
    Status,
    /// Stop the daemon; tabs stay open
    Stop,
}

fn session_name(value: &str) -> Result<String, String> {
    let valid = (1..=48).contains(&value.len())
        && value.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if valid { Ok(value.to_owned()) } else { Err("use 1-48 letters, digits, '.', '-' or '_'".into()) }
}

fn positive_secs(value: &str) -> Result<f64, String> {
    match value.parse::<f64>() {
        Ok(secs) if secs > 0.0 && secs <= MAX_TIMEOUT.as_secs_f64() => Ok(secs),
        _ => Err(format!("expected a positive number of seconds, at most {}", MAX_TIMEOUT.as_secs())),
    }
}

fn parse_point(value: &str) -> Result<(f64, f64), String> {
    let (x, y) = value.split_once(',').ok_or("expected X,Y")?;
    let parse = |part: &str| part.trim().parse::<f64>().ok().filter(|n| n.is_finite());
    match (parse(x), parse(y)) {
        (Some(x), Some(y)) => Ok((x, y)),
        _ => Err("expected two numbers: X,Y".into()),
    }
}

fn parse_size(value: &str) -> Result<(u32, u32), String> {
    let (w, h) = value.split_once(['x', 'X']).ok_or("expected WIDTHxHEIGHT")?;
    match (w.parse::<u32>(), h.parse::<u32>()) {
        (Ok(w), Ok(h)) if w > 0 && h > 0 => Ok((w, h)),
        _ => Err("expected positive WIDTHxHEIGHT".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn session_names_are_restricted() {
        assert!(session_name("my-task_1.2").is_ok());
        assert!(session_name("").is_err());
        assert!(session_name("a b").is_err());
        assert!(session_name(&"x".repeat(49)).is_err());
    }

    #[test]
    fn points_and_sizes_parse() {
        assert_eq!(parse_point("12.5, -3"), Ok((12.5, -3.0)));
        assert!(parse_point("1").is_err());
        assert_eq!(parse_size("1280x720"), Ok((1280, 720)));
        assert!(parse_size("0x10").is_err());
    }

    #[test]
    fn timeouts_must_be_positive_and_at_most_a_day() {
        assert_eq!(positive_secs("2.5"), Ok(2.5));
        assert_eq!(positive_secs("86400"), Ok(86_400.0));
        for bad in ["0", "-1", "NaN", "inf", "soon", "86401", "1e20"] {
            assert!(positive_secs(bad).is_err(), "{bad}");
        }
    }
}
