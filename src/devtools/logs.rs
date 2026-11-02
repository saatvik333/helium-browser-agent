//! `hba console` and `hba errors`: what the page logged and what broke.

use std::fmt::Write as _;
use std::time::Instant;

use anyhow::Result;
use serde_json::Value;

use super::net::request_json;
use crate::cli::{ConsoleArgs, ErrorsArgs};
use crate::console::{Capture, Entry, Level};
use crate::daemon::Cx;
use crate::output;
use crate::page::Page;

/// Characters printed before the rest of a listing goes to a file.
const BUDGET: usize = 32_000;

/// `hba console`: captured messages, newest last; `--on`/`--off` change capture, `--clear` forgets.
pub async fn console(cx: &mut Cx<'_>, args: ConsoleArgs) -> Result<()> {
    let page = cx.page().await?;
    let tab = cx.tab_number(&page.tab);
    if args.clear {
        let cleared = {
            let mut log = page.console.lock().expect("console lock");
            let count = log.entries.len();
            log.clear();
            count
        };
        cx.say(format!("cleared {}", output::count(cleared, "console message")));
    }
    if args.on || args.off {
        let capture = if args.on { Capture::On } else { Capture::Off };
        page.console.lock().expect("console lock").capture = capture;
        crate::console::apply(&page).await?;
        if args.off {
            cx.say(format!("console capture off for {tab}"));
        } else if page.is_local() {
            cx.say(format!("console capture on for {tab}"));
        } else {
            cx.say(format!(
                "console capture on for {tab}; Runtime is enabled, which anti-bot scripts can detect (`hba console --off` stops it)"
            ));
        }
    }
    if args.clear || args.on || args.off {
        return Ok(());
    }

    let min = args.level.map_or(Level::Debug, Level::from);
    let (lines, json, earlier) = {
        let log = page.console.lock().expect("console lock");
        let matching: Vec<&Entry> = log.entries.iter().filter(|entry| entry.level >= min).collect();
        let earlier = matching.len().saturating_sub(args.last);
        let shown = &matching[earlier..];
        let json: Vec<Value> = if cx.json { shown.iter().map(|entry| entry.json()).collect() } else { Vec::new() };
        (shown.iter().map(|entry| entry.line()).collect::<Vec<String>>(), json, earlier)
    };
    if cx.json {
        cx.say(serde_json::to_string(&json)?);
        return Ok(());
    }
    let mut text = String::new();
    if earlier > 0 {
        let _ = writeln!(text, "… {} (raise -n)", output::count(earlier, "earlier message"));
    }
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    if lines.is_empty() && earlier == 0 && capturing(&page) {
        text.push_str(if min == Level::Debug {
            "no console messages\n"
        } else {
            "no console messages at this level or above\n"
        });
    }
    if let Some(note) = capture_note(&page) {
        text.push_str(&note);
    }
    crate::inspect::emit_budgeted(cx, &text, BUDGET, "console", "log", "use -n or --level to narrow it")
}

/// `hba errors`: exceptions, console errors and failed requests in the order they happened.
pub async fn errors(cx: &mut Cx<'_>, args: ErrorsArgs) -> Result<()> {
    let page = cx.page().await?;
    if args.clear {
        let console = page.console.lock().expect("console lock").clear_errors();
        let network = page.network.lock().expect("network lock").clear_failures();
        cx.say(format!("cleared {}", output::count(console + network, "error")));
        return Ok(());
    }
    let with_json = cx.json;
    let mut items: Vec<(Instant, String, Value)> = Vec::new();
    {
        let log = page.console.lock().expect("console lock");
        for entry in log.entries.iter().filter(|entry| entry.level == Level::Error) {
            let json = if with_json { entry.json() } else { Value::Null };
            items.push((entry.at, entry.line(), json));
        }
    }
    {
        let log = page.network.lock().expect("network lock");
        for (at, request) in log.failures() {
            let json = if with_json { request_json(request) } else { Value::Null };
            items.push((at, format!("request {}", request.summary()), json));
        }
    }
    items.sort_by_key(|(at, ..)| *at);
    let earlier = items.len().saturating_sub(args.last);
    let shown = &items[earlier..];
    if with_json {
        let json: Vec<&Value> = shown.iter().map(|(_, _, json)| json).collect();
        cx.say(serde_json::to_string(&json)?);
        return Ok(());
    }
    let mut text = String::new();
    if earlier > 0 {
        let _ = writeln!(text, "… {} (raise -n)", output::count(earlier, "earlier error"));
    }
    for (_, line, _) in shown {
        text.push_str(line);
        text.push('\n');
    }
    if shown.is_empty() {
        text.push_str("no errors\n");
    }
    if let Some(note) = capture_note(&page) {
        text.push_str(&note);
    }
    crate::inspect::emit_budgeted(cx, &text, BUDGET, "errors", "log", "use -n to narrow it")
}

fn capturing(page: &Page) -> bool {
    page.console.lock().expect("console lock").active
}

/// One line saying how to start capturing when it is off on this page.
fn capture_note(page: &Page) -> Option<String> {
    if capturing(page) {
        return None;
    }
    Some(if page.is_local() {
        "console capture is off on this tab; `hba console --on` starts it\n".to_owned()
    } else {
        "console capture is off on websites because it needs Runtime.enable, which anti-bot scripts can detect; \
         `hba console --on` starts it on this tab\n"
            .to_owned()
    })
}
