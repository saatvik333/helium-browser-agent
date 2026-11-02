//! `hba perf`: Chrome's metrics, performance traces, CPU profiles and heap snapshots, and the
//! tab's state for recordings that span two commands.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use serde_json::value::RawValue;
use tokio::io::AsyncWriteExt;
use tokio::sync::{mpsc, oneshot};

use super::{coverage, vitals};
use crate::cdp::{Cdp, Event};
use crate::cli::{PerfOp, StartStop};
use crate::daemon::Cx;
use crate::output;
use crate::page::Page;
use crate::undo::Undo;

/// Categories the developer tools' Performance panel records, so the trace opens there with every track.
const TRACE_CATEGORIES: [&str; 13] = [
    "devtools.timeline",
    "disabled-by-default-devtools.timeline",
    "disabled-by-default-devtools.timeline.frame",
    "disabled-by-default-devtools.timeline.stack",
    "disabled-by-default-devtools.target-rundown",
    "disabled-by-default-devtools.v8-source-rundown",
    "v8.execute",
    "disabled-by-default-v8.cpu_profiler",
    "toplevel",
    "blink.console",
    "blink.user_timing",
    "latencyInfo",
    "loading",
];

/// CPU sampling interval in microseconds; the developer tools use 1000, finer resolves short functions.
const SAMPLING_INTERVAL_US: u32 = 100;

/// Functions listed after a CPU profile.
const TOP_FUNCTIONS: usize = 10;

/// Per-tab recordings: metric readings, traces, CPU profiles and coverage.
#[derive(Debug, Default)]
pub struct State {
    metrics: Option<Reading>,
    trace: Option<Trace>,
    /// Session where `perf profile start` began sampling.
    profile: Option<Arc<str>>,
    pub(super) coverage: Option<coverage::Run>,
}

impl State {
    /// Whether a CPU profile is recording (coverage must then leave the Profiler enabled).
    pub(super) fn profiling(&self) -> bool {
        self.profile.is_some()
    }
}

/// The last `perf metrics` reading. Chrome counts layout, style, script and task time only
/// while the Performance domain is enabled, so it stays enabled between readings.
#[derive(Debug)]
struct Reading {
    session: Arc<str>,
    at: Instant,
    values: BTreeMap<String, f64>,
}

#[derive(Debug)]
struct Trace {
    session: Arc<str>,
    started: Instant,
    /// Set by `stop`; receives the stream handle of `Tracing.tracingComplete`.
    complete: Option<oneshot::Sender<Option<String>>>,
}

/// Page events other modules do not handle.
pub fn on_event(page: &Page, event: &Event) {
    match event.method.as_str() {
        "Tracing.tracingComplete" => {
            #[derive(Deserialize)]
            struct Complete {
                #[serde(default)]
                stream: Option<String>,
            }
            let Ok(complete) = event.parse::<Complete>() else {
                return;
            };
            let waiter = page
                .devtools
                .lock()
                .expect("devtools lock")
                .trace
                .as_mut()
                .filter(|trace| event.session.as_deref() == Some(&*trace.session))
                .and_then(|trace| trace.complete.take());
            if let Some(waiter) = waiter {
                let _ = waiter.send(complete.stream);
            }
        }
        "CSS.styleSheetAdded" => coverage::on_style_sheet(page, event),
        _ => {}
    }
}

/// `hba perf`: dispatches to metrics, vitals, trace, profile, heap or coverage.
pub async fn perf(cx: &mut Cx<'_>, op: PerfOp) -> Result<()> {
    let page = cx.page().await?;
    match op {
        PerfOp::Metrics => metrics(cx, &page).await,
        PerfOp::Vitals => vitals::report(cx, &page).await,
        PerfOp::Trace { action: StartStop::Start, path } => {
            file_goes_with_stop("trace", path.as_deref())?;
            trace_start(cx, &page).await
        }
        PerfOp::Trace { action: StartStop::Stop, path } => trace_stop(cx, &page, path.as_deref()).await,
        PerfOp::Profile { action: StartStop::Start, path } => {
            file_goes_with_stop("profile", path.as_deref())?;
            profile_start(cx, &page).await
        }
        PerfOp::Profile { action: StartStop::Stop, path } => profile_stop(cx, &page, path.as_deref()).await,
        PerfOp::Heap { path } => heap(cx, &page, path.as_deref()).await,
        PerfOp::Coverage { action: StartStop::Start, output } => {
            if let Some(output) = output {
                bail!("-o goes with stop: `hba perf coverage stop -o {}`", output.display());
            }
            coverage::start(cx, &page).await
        }
        PerfOp::Coverage { action: StartStop::Stop, output } => coverage::stop(cx, &page, output.as_deref()).await,
    }
}

fn file_goes_with_stop(what: &str, path: Option<&Path>) -> Result<()> {
    match path {
        Some(path) => bail!("the file goes with stop: `hba perf {what} stop {}`", path.display()),
        None => Ok(()),
    }
}

/// `path` against the caller's directory, or a new file in the runtime directory.
fn output_path(cx: &Cx<'_>, path: Option<&Path>, stem: &str, extension: &str) -> Result<PathBuf> {
    match path {
        Some(path) => Ok(cx.path(path)),
        None => cx.artifact(stem, extension),
    }
}

async fn metrics(cx: &mut Cx<'_>, page: &Page) -> Result<()> {
    #[derive(Deserialize)]
    struct Metrics {
        metrics: Vec<Metric>,
    }
    #[derive(Deserialize)]
    struct Metric {
        name: String,
        value: f64,
    }
    let session = page.session()?;
    let previous =
        page.devtools.lock().expect("devtools lock").metrics.take().filter(|reading| reading.session == session);
    if previous.is_none() {
        page.cdp.send("Performance.enable", json!({"timeDomain": "timeTicks"}), Some(&session)).await?;
    }
    let metrics: Metrics = page.cdp.call("Performance.getMetrics", json!({}), Some(&session)).await?;
    let values: BTreeMap<String, f64> = metrics.metrics.into_iter().map(|metric| (metric.name, metric.value)).collect();
    let output = if cx.json {
        let since = previous.as_ref().map(|reading| reading.at.elapsed().as_secs_f64());
        vec![serde_json::to_string(&json!({"metrics": values, "previousSecondsAgo": since}))?]
    } else {
        metric_lines(&values, previous.as_ref().map(|reading| (&reading.values, reading.at.elapsed())))
    };
    page.devtools.lock().expect("devtools lock").metrics = Some(Reading { session, at: Instant::now(), values });
    for line in output {
        cx.say(line);
    }
    Ok(())
}

/// The metrics worth reading: sizes in bytes, counts, and the layout, style, script and task
/// time (seconds) added since the previous reading.
fn metric_lines(values: &BTreeMap<String, f64>, previous: Option<(&BTreeMap<String, f64>, Duration)>) -> Vec<String> {
    let get = |metrics: &BTreeMap<String, f64>, name: &str| metrics.get(name).copied().unwrap_or(0.0);
    let size = |name: &str| output::bytes(get(values, name) as u64);
    let mut lines = vec![
        format!("js heap {} used of {} (renderer process)", size("JSHeapUsedSize"), size("JSHeapTotalSize")),
        format!(
            "dom nodes {}, documents {}, frames {}, event listeners {} (renderer process, all its tabs)",
            get(values, "Nodes"),
            get(values, "Documents"),
            get(values, "Frames"),
            get(values, "JSEventListeners")
        ),
    ];
    lines.push(match previous {
        Some((before, elapsed)) => {
            let added = |name: &str| get(values, name) - get(before, name);
            let time = |name: &str| output::millis(added(name) * 1000.0);
            format!(
                "since the last reading {} ago: layouts {} ({}), style recalcs {} ({}), script {}, tasks {}",
                output::millis(elapsed.as_secs_f64() * 1000.0),
                added("LayoutCount"),
                time("LayoutDuration"),
                added("RecalcStyleCount"),
                time("RecalcStyleDuration"),
                time("ScriptDuration"),
                time("TaskDuration")
            )
        }
        None => {
            "layout, style, script and task time count from now; run `hba perf metrics` again after the work".to_owned()
        }
    });
    lines
}

async fn trace_start(cx: &mut Cx<'_>, page: &Page) -> Result<()> {
    if page.devtools.lock().expect("devtools lock").trace.is_some() {
        bail!("a trace is already recording on this tab; `hba perf trace stop FILE` saves it");
    }
    let session = page.session()?;
    let params = json!({
        "transferMode": "ReturnAsStream",
        "streamFormat": "json",
        "streamCompression": "none",
        "traceConfig": {"includedCategories": TRACE_CATEGORIES, "excludedCategories": ["*"]},
    });
    page.cdp
        .send("Tracing.start", params, Some(&session))
        .await
        .context("starting the trace (is DevTools recording a performance profile?)")?;
    page.devtools.lock().expect("devtools lock").trace =
        Some(Trace { session, started: Instant::now(), complete: None });
    cx.say(format!(
        "tracing {}; `hba perf trace stop FILE` saves it for DevTools' Performance panel",
        cx.tab_number(&page.tab)
    ));
    Ok(())
}

async fn trace_stop(cx: &mut Cx<'_>, page: &Arc<Page>, path: Option<&Path>) -> Result<()> {
    let (session, started) = {
        let state = page.devtools.lock().expect("devtools lock");
        let trace =
            state.trace.as_ref().context("no trace is recording on this tab; `hba perf trace start` begins one")?;
        (Arc::clone(&trace.session), trace.started)
    };
    let path = output_path(cx, path, "trace", "json")?;
    let name = cx.config()?.browser.name();
    // Ending the trace is final, so the file is opened first.
    let file = output::create(&path)?;
    let traced = started.elapsed();
    let timeout = cx.timeout;
    let (page, saved_to) = (Arc::clone(page), path.clone());
    // Its own task, so a client that hangs up still gets the trace saved and its stream closed.
    let written = tokio::spawn(async move {
        let complete = {
            let mut state = page.devtools.lock().expect("devtools lock");
            let trace = state.trace.as_mut().context("the trace was stopped meanwhile")?;
            let (sender, receiver) = oneshot::channel();
            trace.complete = Some(sender);
            receiver
        };
        let saved = async {
            page.cdp.send("Tracing.end", json!({}), Some(&session)).await?;
            let stream = tokio::time::timeout(timeout, complete)
                .await
                .with_context(|| format!("{name} did not finish the trace in time; retry with a larger --timeout"))?
                .context("the tab went away before the trace finished")?
                .context("the trace came back without data")?;
            crate::media::save_stream(&page.cdp, &session, &stream, file, &saved_to).await
        }
        .await;
        page.devtools.lock().expect("devtools lock").trace = None;
        saved
    })
    .await??;
    cx.say(format!(
        "saved {} ({}, {} traced); open it in DevTools' Performance panel",
        path.display(),
        output::bytes(written),
        output::millis(traced.as_secs_f64() * 1000.0)
    ));
    Ok(())
}

async fn profile_start(cx: &mut Cx<'_>, page: &Page) -> Result<()> {
    if page.devtools.lock().expect("devtools lock").profiling() {
        bail!("a CPU profile is already recording on this tab; `hba perf profile stop FILE` saves it");
    }
    let session = page.session()?;
    let s = Some(&*session);
    page.cdp.send("Profiler.enable", json!({}), s).await?;
    page.cdp
        .send("Profiler.setSamplingInterval", json!({"interval": SAMPLING_INTERVAL_US}), s)
        .await
        .context("setting the sampling interval (is DevTools profiling this tab?)")?;
    page.cdp.send("Profiler.start", json!({}), s).await?;
    page.devtools.lock().expect("devtools lock").profile = Some(session);
    cx.say(format!("profiling JavaScript on {}; `hba perf profile stop FILE` saves it", cx.tab_number(&page.tab)));
    Ok(())
}

async fn profile_stop(cx: &mut Cx<'_>, page: &Arc<Page>, path: Option<&Path>) -> Result<()> {
    #[derive(Deserialize)]
    struct Stopped {
        profile: Box<RawValue>,
    }
    if !page.devtools.lock().expect("devtools lock").profiling() {
        bail!("no CPU profile is recording on this tab; `hba perf profile start` begins one");
    }
    let path = output_path(cx, path, "profile", "cpuprofile")?;
    // Stopping the profile is final, so the file is opened first.
    let file = output::create(&path)?;
    let (page, saved_to) = (Arc::clone(page), path.clone());
    // Its own task, so a client that hangs up still gets the profile saved and the Profiler off.
    let raw = tokio::spawn(async move {
        let session = page
            .devtools
            .lock()
            .expect("devtools lock")
            .profile
            .take()
            .context("the CPU profile was stopped meanwhile")?;
        let stopped = page.cdp.call::<Stopped>("Profiler.stop", json!({}), Some(&session)).await;
        let covering = page.devtools.lock().expect("devtools lock").coverage.is_some();
        if !covering {
            page.cdp.send("Profiler.disable", json!({}), Some(&session)).await.ok();
        }
        let raw = stopped?.profile;
        let mut file = tokio::fs::File::from_std(file);
        let written = async {
            file.write_all(raw.get().as_bytes()).await?;
            file.flush().await
        };
        written.await.with_context(|| format!("writing {}", saved_to.display()))?;
        anyhow::Ok(raw)
    })
    .await??;
    let profile: Profile = serde_json::from_str(raw.get()).context("reading the CPU profile")?;
    let summary = Summary::of(&profile);
    cx.say(format!(
        "saved {} ({}, {} samples); open it in DevTools' Performance panel",
        path.display(),
        output::millis(summary.total_ms),
        profile.samples.len()
    ));
    for line in summary.lines() {
        cx.say(line);
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Profile {
    nodes: Vec<ProfileNode>,
    start_time: f64,
    end_time: f64,
    #[serde(default)]
    samples: Vec<u64>,
    #[serde(default)]
    time_deltas: Vec<f64>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProfileNode {
    id: u64,
    call_frame: CallFrame,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CallFrame {
    function_name: String,
    url: String,
    line_number: i64,
}

/// Where a CPU profile's time went, in milliseconds.
#[derive(Debug, PartialEq)]
struct Summary {
    total_ms: f64,
    idle_ms: f64,
    gc_ms: f64,
    /// Native browser work outside JavaScript: `(program)`.
    program_ms: f64,
    /// JavaScript functions by self time, slowest first: (name, `file:line`, ms).
    functions: Vec<(String, String, f64)>,
}

impl Summary {
    fn of(profile: &Profile) -> Self {
        let nodes: HashMap<u64, &CallFrame> = profile.nodes.iter().map(|node| (node.id, &node.call_frame)).collect();
        // A sample lasts until the next one; the last one until the profile ends.
        let mut stamps = Vec::with_capacity(profile.time_deltas.len());
        let mut at = profile.start_time;
        for delta in &profile.time_deltas {
            at += delta;
            stamps.push(at);
        }
        let mut self_us: HashMap<u64, f64> = HashMap::new();
        for (index, node) in profile.samples.iter().enumerate() {
            let (Some(start), end) = (stamps.get(index), stamps.get(index + 1).copied().unwrap_or(profile.end_time))
            else {
                continue;
            };
            *self_us.entry(*node).or_default() += (end - start).max(0.0);
        }
        let mut summary = Self {
            total_ms: (profile.end_time - profile.start_time) / 1000.0,
            idle_ms: 0.0,
            gc_ms: 0.0,
            program_ms: 0.0,
            functions: Vec::new(),
        };
        let mut functions: HashMap<(String, String), f64> = HashMap::new();
        for (node, micros) in self_us {
            let Some(frame) = nodes.get(&node) else {
                continue;
            };
            let ms = micros / 1000.0;
            match frame.function_name.as_str() {
                "(root)" => {}
                "(idle)" => summary.idle_ms += ms,
                "(garbage collector)" => summary.gc_ms += ms,
                "(program)" => summary.program_ms += ms,
                name => {
                    let name = if name.is_empty() { "(anonymous)" } else { name };
                    let place = if frame.url.is_empty() {
                        String::new()
                    } else {
                        format!("{}:{}", crate::console::short_source(&frame.url), frame.line_number + 1)
                    };
                    *functions.entry((name.to_owned(), place)).or_default() += ms;
                }
            }
        }
        summary.functions = functions.into_iter().map(|((name, place), ms)| (name, place, ms)).collect();
        summary.functions.sort_by(|a, b| b.2.total_cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
        summary
    }

    fn js_ms(&self) -> f64 {
        // Not `sum()`, which starts at -0.0 and would print "-0.0 ms" for no JavaScript.
        self.functions.iter().fold(0.0, |total, (_, _, ms)| total + ms)
    }

    fn lines(&self) -> Vec<String> {
        let js = self.js_ms();
        let busy = js + self.gc_ms + self.program_ms;
        let mut lines = vec![format!(
            "busy {}: js {}, gc {}, browser {}; idle {}",
            output::millis(busy),
            output::millis(js),
            output::millis(self.gc_ms),
            output::millis(self.program_ms),
            output::millis(self.idle_ms)
        )];
        if self.functions.is_empty() {
            lines.push("no JavaScript ran while profiling".to_owned());
            return lines;
        }
        lines.push("top functions by self time:".to_owned());
        for (name, place, ms) in self.functions.iter().take(TOP_FUNCTIONS) {
            let share = if busy > 0.0 { ms / busy * 100.0 } else { 0.0 };
            let place = if place.is_empty() { String::new() } else { format!(" ({place})") };
            lines.push(format!("  {} {share:.0}% {name}{place}", output::millis(*ms)));
        }
        lines
    }
}

async fn heap(cx: &mut Cx<'_>, page: &Page, path: Option<&Path>) -> Result<()> {
    let target = page.page_target().context("the tab has no page to snapshot")?;
    let path = output_path(cx, path, "heap", "heapsnapshot")?;
    // A session of its own receives exactly the snapshot's chunks, all before the reply.
    let attached: crate::page::Attached =
        page.cdp.call("Target.attachToTarget", json!({"targetId": target, "flatten": true}), None).await?;
    let session = attached.session_id;
    let (sender, events) = mpsc::unbounded_channel();
    page.cdp.route(&session, sender);
    let detach = {
        let (cdp, session) = (page.cdp.clone(), session.clone());
        Undo::new(async move {
            cdp.unroute(&session);
            cdp.send("Target.detachFromTarget", json!({"sessionId": session}), None).await.ok();
        })
    };
    let taken = snapshot(&page.cdp, &session, events, &path, cx.timeout).await;
    detach.finish().await;
    let (written, nodes) = taken?;
    let nodes = nodes.map(|count| format!(", {count} nodes")).unwrap_or_default();
    cx.say(format!("saved {} ({}{nodes}); load it in DevTools' Memory panel", path.display(), output::bytes(written)));
    Ok(())
}

/// Takes a heap snapshot on `session`, streaming its chunks into `path`; returns the bytes
/// written and the node count from the snapshot header.
async fn snapshot(
    cdp: &Cdp,
    session: &str,
    mut events: mpsc::UnboundedReceiver<Event>,
    path: &Path,
    timeout: Duration,
) -> Result<(u64, Option<u64>)> {
    let file = output::create_private(path).with_context(|| format!("creating {}", path.display()))?;
    let mut file = tokio::fs::File::from_std(file);
    let mut sink = Chunks { written: 0, nodes: None };
    let take =
        cdp.raw_within("HeapProfiler.takeHeapSnapshot", json!({"reportProgress": false}), Some(session), timeout);
    tokio::pin!(take);
    loop {
        tokio::select! {
            taken = &mut take => {
                taken?;
                break;
            }
            Some(event) = events.recv() => sink.write(&mut file, &event).await?,
        }
    }
    // Chunk events precede the reply on the socket, so the rest are already queued.
    while let Ok(event) = events.try_recv() {
        sink.write(&mut file, &event).await?;
    }
    file.flush().await?;
    Ok((sink.written, sink.nodes))
}

/// Heap snapshot chunks written so far.
struct Chunks {
    written: u64,
    nodes: Option<u64>,
}

impl Chunks {
    async fn write(&mut self, file: &mut tokio::fs::File, event: &Event) -> Result<()> {
        #[derive(Deserialize)]
        struct Chunk {
            chunk: String,
        }
        if event.method != "HeapProfiler.addHeapSnapshotChunk" {
            return Ok(());
        }
        let Chunk { chunk } = event.parse()?;
        if self.written == 0 {
            self.nodes = node_count(&chunk);
        }
        file.write_all(chunk.as_bytes()).await?;
        self.written += chunk.len() as u64;
        Ok(())
    }
}

/// `node_count` from the header at the start of a heap snapshot.
fn node_count(head: &str) -> Option<u64> {
    let rest = &head[head.find("\"node_count\":")? + "\"node_count\":".len()..];
    let digits = rest.trim_start();
    let end = digits.find(|c: char| !c.is_ascii_digit()).unwrap_or(digits.len());
    digits[..end].parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> Profile {
        serde_json::from_value(json!({
            "nodes": [
                {"id": 1, "callFrame": {"functionName": "(root)", "url": "", "lineNumber": -1}},
                {"id": 2, "callFrame": {"functionName": "(idle)", "url": "", "lineNumber": -1}},
                {"id": 3, "callFrame": {"functionName": "fib", "url": "http://127.0.0.1:8765/app.js?v=1", "lineNumber": 9}},
                {"id": 4, "callFrame": {"functionName": "", "url": "http://127.0.0.1:8765/app.js", "lineNumber": 0}},
                {"id": 5, "callFrame": {"functionName": "(garbage collector)", "url": "", "lineNumber": -1}},
                {"id": 6, "callFrame": {"functionName": "fib", "url": "http://127.0.0.1:8765/app.js", "lineNumber": 9}}
            ],
            "startTime": 1000.0,
            "endTime": 11000.0,
            "samples": [2, 3, 6, 4, 5, 3],
            "timeDeltas": [0.0, 1000.0, 3000.0, 2000.0, 1000.0, 1000.0]
        }))
        .expect("profile")
    }

    #[test]
    fn self_time_follows_samples_and_merges_call_sites() {
        let summary = Summary::of(&profile());
        assert_eq!(summary.total_ms, 10.0);
        assert_eq!(summary.idle_ms, 1.0);
        assert_eq!(summary.gc_ms, 1.0);
        assert_eq!(
            summary.functions,
            [("fib".to_owned(), "app.js:10".to_owned(), 7.0), ("(anonymous)".to_owned(), "app.js:1".to_owned(), 1.0)]
        );
        let lines = summary.lines();
        assert_eq!(lines[0], "busy 9 ms: js 8 ms, gc 1 ms, browser 0.0 ms; idle 1 ms");
        assert_eq!(lines[2], "  7 ms 78% fib (app.js:10)");
    }

    #[test]
    fn heap_header_gives_the_node_count() {
        let head = r#"{"snapshot":{"meta":{"node_fields":["type","name"]},"node_count":45123,"edge_count":9}"#;
        assert_eq!(node_count(head), Some(45_123));
        assert_eq!(node_count("{\"snapshot\":{"), None);
    }

    fn readings(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
        pairs.iter().map(|(name, value)| ((*name).to_owned(), *value)).collect()
    }

    #[test]
    fn metrics_read_in_human_units_with_time_since_the_last_reading() {
        let before = readings(&[
            ("LayoutCount", 1.0),
            ("LayoutDuration", 0.0002),
            ("ScriptDuration", 0.001),
            ("TaskDuration", 0.011),
        ]);
        let now = readings(&[
            ("JSHeapUsedSize", 1_572_864.0),
            ("JSHeapTotalSize", 2_097_152.0),
            ("Nodes", 41.0),
            ("Documents", 1.0),
            ("Frames", 1.0),
            ("JSEventListeners", 4.0),
            ("LayoutCount", 3.0),
            ("LayoutDuration", 0.0014),
            ("RecalcStyleCount", 3.0),
            ("RecalcStyleDuration", 0.0004),
            ("ScriptDuration", 0.301),
            ("TaskDuration", 0.311),
        ]);
        assert_eq!(
            metric_lines(&now, Some((&before, Duration::from_millis(3200)))),
            [
                "js heap 1.5 MB used of 2.0 MB (renderer process)",
                "dom nodes 41, documents 1, frames 1, event listeners 4 (renderer process, all its tabs)",
                "since the last reading 3.20 s ago: layouts 2 (1 ms), style recalcs 3 (0.4 ms), script 300 ms, tasks 300 ms",
            ]
        );
        let first = metric_lines(&now, None);
        assert!(first[2].starts_with("layout, style, script and task time count from now"), "{first:?}");
    }
}
