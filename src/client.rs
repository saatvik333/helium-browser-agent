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

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(code) => ExitCode::from(code),
        Err(error) => {
            eprintln!("hba: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<u8> {
    let mut stdin = StdinOnce::default();
    let mut command = cli.command;
    resolve_stdin(&mut command, &mut stdin)?;
    if let Command::Run(args) = &mut command {
        load_script(args, &mut stdin)?;
    }
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
        match exchange(&socket, &line, &runtime)? {
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

fn exchange(socket: &Path, line: &str, runtime: &Path) -> Result<Reply> {
    let mut stream = connect(socket, runtime)?;
    stream.write_all(line.as_bytes()).context("sending the request")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).context("reading the reply")?;
    if reply.is_empty() {
        bail!("the daemon closed the connection; see {}", runtime.join("daemon.log").display());
    }
    Ok(serde_json::from_str(&reply)?)
}

fn connect(socket: &Path, runtime: &Path) -> Result<UnixStream> {
    if let Ok(stream) = UnixStream::connect(socket) {
        return Ok(stream);
    }
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
    let log = fs::OpenOptions::new().create(true).append(true).open(runtime.join("daemon.log"))?;
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

fn print_all(stdout: &str, stderr: &str) -> Result<()> {
    let mut out = io::stdout().lock();
    out.write_all(stdout.as_bytes())?;
    if !stdout.is_empty() && !stdout.ends_with('\n') {
        out.write_all(b"\n")?;
    }
    out.flush()?;
    let mut err = io::stderr().lock();
    err.write_all(stderr.as_bytes())?;
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        err.write_all(b"\n")?;
    }
    Ok(())
}

/// Stdin can be consumed by exactly one `-` argument.
#[derive(Default)]
struct StdinOnce {
    used: bool,
}

impl StdinOnce {
    fn read(&mut self) -> Result<String> {
        if std::mem::replace(&mut self.used, true) {
            bail!("only one argument can read stdin");
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
        let words = shlex::split(line).with_context(|| format!("line {}: unbalanced quotes", index + 1))?;
        let clap_error =
            |error: clap::Error| anyhow::anyhow!("line {}: {}", index + 1, error.render().to_string().trim());
        let found =
            Cli::command().try_get_matches_from(std::iter::once("hba".to_owned()).chain(words)).map_err(clap_error)?;
        // Global options would be ignored; values from the environment are the `run` command's own.
        if ["session", "profile", "tab", "json", "timeout"]
            .iter()
            .any(|id| found.value_source(id) == Some(ValueSource::CommandLine))
        {
            bail!("line {}: put -s/-p/-T/--json/--timeout on the `hba run` command, not on script lines", index + 1);
        }
        let mut command = Cli::from_arg_matches(&found).map_err(clap_error)?.command;
        if matches!(command, Command::Run(_)) {
            bail!("line {}: scripts cannot nest `run`", index + 1);
        }
        let mut no_stdin = StdinOnce { used: true };
        resolve_stdin(&mut command, &mut no_stdin)
            .map_err(|_| anyhow::anyhow!("line {}: `-` cannot read stdin inside a script", index + 1))?;
        args.commands.push((line.to_owned(), command));
    }
    if args.commands.is_empty() {
        bail!("the script has no commands");
    }
    Ok(())
}

/// Resolves a user-supplied path against the client's working directory.
pub fn absolute(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() { path.to_owned() } else { cwd.join(path) }
}
