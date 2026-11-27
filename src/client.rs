//! The short-lived CLI process: parse, forward to the daemon (starting it when needed), print.

use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{ExitCode, Stdio};
use std::time::{Duration, Instant};
use std::{env, fs};

use anyhow::{Context, Result, bail};
use clap::parser::ValueSource;
use clap::{CommandFactory, FromArgMatches, Parser};

use crate::cli::{Cli, Command, RunArgs};
use crate::ipc::{self, BUILD, DAEMON_ARG, DAEMON_DRAIN, Reply, Request};

/// How long a freshly spawned daemon may take to accept connections.
const DAEMON_START: Duration = Duration::from_secs(5);

/// Size beyond which `daemon.log` is moved to `daemon.log.old` when a daemon starts.
const LOG_LIMIT: u64 = 1 << 20;

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("hba: {error:#}");
            // As for clap's own errors: 2 means the command line was wrong.
            if error.is::<Usage>() { ExitCode::from(2) } else { ExitCode::FAILURE }
        }
    }
}

/// A command line refused before anything runs, such as a bad script line.
#[derive(Debug)]
struct Usage(String);

impl std::fmt::Display for Usage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Usage {}

fn usage(message: String) -> anyhow::Error {
    Usage(message).into()
}

fn run(cli: Cli) -> Result<u8> {
    let mut stdin = StdinOnce::default();
    let mut command = cli.command;
    resolve_stdin(&mut command, &mut stdin)?;
    if let Command::Run(args) = &mut command {
        load_script(args, &mut stdin)?;
    }
    // Reporting on the daemon or stopping it never starts one.
    let about_daemon = matches!(command, Command::Daemon(_));
    let request = Request {
        build: BUILD.to_owned(),
        env: ipc::daemon_env(),
        cwd: env::current_dir().context("reading the working directory")?,
        session: cli.session,
        profile: cli.profile,
        tab: cli.tab,
        json: cli.json,
        timeout_secs: cli.timeout,
        command,
    };
    let runtime = ipc::runtime_dir().context("creating the runtime directory")?;
    let socket = ipc::socket_path(&runtime);
    let line = serde_json::to_string(&request)? + "\n";
    for _ in 0..2 {
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(_) if about_daemon => {
                print_all(no_daemon(&runtime), "")?;
                return Ok(0);
            }
            Err(_) => start_daemon(&socket, &runtime)?,
        };
        match exchange(stream, &line, &runtime)? {
            Reply::Done { stdout, stderr, code } => {
                print_all(&stdout, &stderr)?;
                return Ok(code);
            }
            // The old daemon is shutting down; the next attempt starts a matching one.
            Reply::Restart => wait_gone(&socket),
        }
    }
    bail!("the daemon keeps restarting; see {}", runtime.join("daemon.log").display())
}

fn exchange(mut stream: UnixStream, line: &str, runtime: &Path) -> Result<Reply> {
    stream.write_all(line.as_bytes()).context("sending the request")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).context("reading the reply")?;
    if reply.is_empty() {
        bail!("the daemon closed the connection; see {}", runtime.join("daemon.log").display());
    }
    Ok(serde_json::from_str(&reply)?)
}

/// What `daemon status` and `daemon stop` say when no daemon answers.
fn no_daemon(runtime: &Path) -> &'static str {
    if daemon_locked(runtime) {
        "the daemon is stopping: it exits once its commands finish"
    } else {
        "no daemon running"
    }
}

/// Starts a daemon and connects once it listens.
fn start_daemon(socket: &Path, runtime: &Path) -> Result<UnixStream> {
    spawn_daemon(runtime)?;
    let limit = Instant::now() + DAEMON_DRAIN + 2 * DAEMON_START;
    let mut patience = Instant::now() + DAEMON_START;
    loop {
        match UnixStream::connect(socket) {
            Ok(stream) => return Ok(stream),
            Err(_) if Instant::now() < patience => std::thread::sleep(Duration::from_millis(5)),
            // The new daemon starts listening once a daemon that is shutting down has finished.
            Err(_) if Instant::now() < limit && daemon_locked(runtime) => patience = Instant::now() + DAEMON_START,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("the daemon did not start; see {}", runtime.join("daemon.log").display())
                });
            }
        }
    }
}

/// Whether a daemon holds `daemon.lock`: it is starting, running or finishing its commands.
fn daemon_locked(runtime: &Path) -> bool {
    fs::File::open(runtime.join("daemon.lock"))
        .is_ok_and(|lock| matches!(lock.try_lock(), Err(fs::TryLockError::WouldBlock)))
}

fn spawn_daemon(runtime: &Path) -> Result<()> {
    let path = runtime.join("daemon.log");
    if fs::metadata(&path).is_ok_and(|meta| meta.len() > LOG_LIMIT) {
        let _ = fs::rename(&path, runtime.join("daemon.log.old"));
    }
    let log = fs::OpenOptions::new().create(true).append(true).open(&path)?;
    std::process::Command::new(env::current_exe()?)
        .arg(DAEMON_ARG)
        .current_dir("/")
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Own process group: the daemon outlives the terminal or agent shell that started it.
        .process_group(0)
        .spawn()
        .context("starting the daemon")?;
    Ok(())
}

fn wait_gone(socket: &Path) {
    let deadline = Instant::now() + Duration::from_secs(2);
    while UnixStream::connect(socket).is_ok() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Prints the daemon's output. A reader that stopped reading early (`hba snap | head`) is no
/// failure of the command.
fn print_all(stdout: &str, stderr: &str) -> Result<()> {
    let printed = write_text(&mut io::stdout().lock(), stdout);
    let reported = write_text(&mut io::stderr().lock(), stderr);
    for outcome in [printed, reported] {
        if let Err(error) = outcome
            && error.kind() != io::ErrorKind::BrokenPipe
        {
            return Err(error).context("printing the output");
        }
    }
    Ok(())
}

fn write_text(out: &mut impl Write, text: &str) -> io::Result<()> {
    out.write_all(text.as_bytes())?;
    if !text.is_empty() && !text.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()
}

/// Stdin can be consumed by exactly one `-` argument.
#[derive(Default)]
struct StdinOnce {
    used: bool,
}

impl StdinOnce {
    fn read(&mut self) -> Result<String> {
        if std::mem::replace(&mut self.used, true) {
            return Err(usage("only one argument can read stdin".to_owned()));
        }
        let mut text = String::new();
        io::stdin().read_to_string(&mut text).context("reading stdin")?;
        Ok(text)
    }

    /// Replaces `-` with stdin, dropping the newline a heredoc or `echo` appends.
    fn fill(&mut self, value: &mut String) -> Result<()> {
        if value == "-" {
            let text = self.read()?;
            text.strip_suffix('\n').unwrap_or(&text).clone_into(value);
        }
        Ok(())
    }
}

fn resolve_stdin(command: &mut Command, stdin: &mut StdinOnce) -> Result<()> {
    match command {
        Command::Fill(args) => stdin.fill(&mut args.text),
        Command::Type(args) => stdin.fill(&mut args.text),
        Command::Js(args) => stdin.fill(&mut args.code),
        Command::Clip(args) => args.text.as_mut().map_or(Ok(()), |text| stdin.fill(text)),
        Command::Cdp(args) => args.params.as_mut().map_or(Ok(()), |params| stdin.fill(params)),
        Command::Ext(args) => match &mut args.op {
            Some(crate::cli::ExtOp::Eval { code, .. }) => stdin.fill(code),
            _ => Ok(()),
        },
        _ => Ok(()),
    }
}

/// Parses a batch script client-side so syntax errors surface before anything runs.
fn load_script(args: &mut RunArgs, stdin: &mut StdinOnce) -> Result<()> {
    let text = match args.file.as_deref() {
        None => stdin.read()?,
        Some(path) if path == Path::new("-") => stdin.read()?,
        Some(path) => fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    };
    for (index, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let number = index + 1;
        let words = shlex::split(line).ok_or_else(|| usage(format!("line {number}: unbalanced quotes")))?;
        let clap_error = |error: clap::Error| usage(format!("line {number}: {}", error.render().to_string().trim()));
        let found =
            Cli::command().try_get_matches_from(std::iter::once("hba".to_owned()).chain(words)).map_err(clap_error)?;
        // Global options would be ignored; values from the environment are the `run` command's own.
        if ["session", "profile", "tab", "json", "timeout"]
            .iter()
            .any(|id| found.value_source(id) == Some(ValueSource::CommandLine))
        {
            return Err(usage(format!(
                "line {number}: put -s/-p/-T/--json/--timeout on the `hba run` command, not on script lines"
            )));
        }
        let mut command = Cli::from_arg_matches(&found).map_err(clap_error)?.command;
        if matches!(command, Command::Run(_)) {
            return Err(usage(format!("line {number}: scripts cannot nest `run`")));
        }
        let mut no_stdin = StdinOnce { used: true };
        resolve_stdin(&mut command, &mut no_stdin)
            .map_err(|_| usage(format!("line {number}: `-` cannot read stdin inside a script")))?;
        args.commands.push((line.to_owned(), command));
    }
    if args.commands.is_empty() {
        return Err(usage("the script has no commands".to_owned()));
    }
    Ok(())
}

/// Resolves a user-supplied path against the client's working directory.
pub fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() { path.to_owned() } else { cwd.join(path) }
}
