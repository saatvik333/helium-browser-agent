//! `hba`: drive your running Helium, Chrome or Brave from the command line.
//!
//! One binary: a short-lived client per command and a background daemon that keeps the
//! DevTools connection, sessions and attached tabs warm.

mod actions;
mod browser;
mod cdp;
mod cli;
mod client;
mod clip;
mod commands;
mod config;
mod console;
mod daemon;
mod devtools;
mod download;
mod emulate;
mod endpoint;
mod extensions;
mod injected;
mod input;
mod inspect;
mod ipc;
mod keyboard;
mod media;
mod nav;
mod network;
mod niri;
mod output;
mod page;
mod profiles;
mod routes;
mod script;
mod session;
mod snapshot;
mod storage;
mod system;
mod tabs;
mod trace;
mod transfer;
mod undo;

use std::process::ExitCode;

fn main() -> ExitCode {
    if std::env::args_os().nth(1).is_some_and(|arg| arg == ipc::DAEMON_ARG) {
        return daemon::main();
    }
    client::main()
}
