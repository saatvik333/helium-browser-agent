//! `hba perf coverage`: how much of each JavaScript and CSS file the page used.
//!
//! JavaScript comes from V8's precise block coverage, CSS from rule usage tracking; style sheet
//! URLs and sizes arrive as `CSS.styleSheetAdded` events while coverage runs.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};

use crate::cdp::Event;
use crate::daemon::Cx;
use crate::output;
use crate::page::Page;

/// Files listed by `hba perf coverage stop`.
const SHOWN: usize = 20;

/// Coverage being collected on a tab.
#[derive(Debug)]
pub(super) struct Run {
    session: Arc<str>,
    /// The page's own style sheets seen since coverage started, by id.
    sheets: HashMap<String, Sheet>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Sheet {
    url: String,
    length: u64,
}

/// Remembers a style sheet's URL and length while coverage runs on its session.
pub(super) fn on_style_sheet(page: &Page, event: &Event) {
    #[derive(Deserialize)]
    struct Added {
        header: Header,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Header {
        style_sheet_id: String,
        #[serde(rename = "sourceURL", default)]
        source_url: String,
        #[serde(default)]
        length: f64,
        #[serde(default)]
        origin: String,
    }
    let Ok(Added { header }) = event.parse::<Added>() else {
        return;
    };
    // Extension, DevTools and user-agent sheets are not the page's.
    if header.origin != "regular" || header.source_url.is_empty() {
        return;
    }
    let mut state = page.devtools.lock().expect("devtools lock");
    if let Some(run) = state.coverage.as_mut()
        && event.session.as_deref() == Some(&*run.session)
    {
        run.sheets.insert(header.style_sheet_id, Sheet { url: header.source_url, length: header.length as u64 });
    }
}

/// `hba perf coverage start`: precise JS coverage and CSS rule tracking on the page session.
pub(super) async fn start(cx: &mut Cx<'_>, page: &Arc<Page>) -> Result<()> {
    let session = page.session()?;
    {
        let mut state = page.devtools.lock().expect("devtools lock");
        if state.coverage.is_some() {
            bail!("coverage is already running on this tab; `hba perf coverage stop` reports it");
        }
        // Before CSS.enable, which announces the existing style sheets.
        state.coverage = Some(Run { session: Arc::clone(&session), sheets: HashMap::new() });
    }
    // Its own task, so a client that hangs up leaves coverage either running and recorded or off.
    let page = Arc::clone(page);
    let tab = page.tab.clone();
    tokio::spawn(async move {
        let s = Some(&*session);
        let js = async {
            page.cdp.send("Profiler.enable", json!({}), s).await?;
            page.cdp.send("Profiler.startPreciseCoverage", json!({"callCount": true, "detailed": true}), s).await
        };
        let css = async {
            page.cdp.send("DOM.enable", json!({}), s).await?;
            page.cdp.send("CSS.enable", json!({}), s).await?;
            page.cdp.send("CSS.startRuleUsageTracking", json!({}), s).await
        };
        let (js, css) = tokio::join!(js, css);
        let started = js.and(css);
        if started.is_err() {
            page.devtools.lock().expect("devtools lock").coverage = None;
            finish(&page, &session).await;
        }
        started
    })
    .await?
    .context("starting coverage")?;
    cx.say(format!(
        "collecting JS and CSS coverage on {}; `hba reload` covers page load, `hba perf coverage stop` reports",
        cx.tab_number(&tab)
    ));
    Ok(())
}

/// Turns off what coverage turned on; the Profiler stays on while a CPU profile records.
async fn finish(page: &Page, session: &str) {
    let profiling = page.devtools.lock().expect("devtools lock").profiling();
    let s = Some(session);
    let js = async {
        page.cdp.send("Profiler.stopPreciseCoverage", json!({}), s).await.ok();
        if !profiling {
            page.cdp.send("Profiler.disable", json!({}), s).await.ok();
        }
    };
    let css = async {
        page.cdp.send("CSS.disable", json!({}), s).await.ok();
        page.cdp.send("DOM.disable", json!({}), s).await.ok();
    };
    tokio::join!(js, css);
}

/// `hba perf coverage stop [-o FILE]`: per-file used share, most unused first.
pub(super) async fn stop(cx: &mut Cx<'_>, page: &Arc<Page>, output: Option<&Path>) -> Result<()> {
    if page.devtools.lock().expect("devtools lock").coverage.is_none() {
        bail!("no coverage is running on this tab; `hba perf coverage start` begins it");
    }
    // Taking the coverage ends it, so the file is opened first: a path that cannot be written
    // leaves coverage running.
    let file = match output {
        Some(output) => {
            let path = cx.path(output);
            let file = output::create_private(&path).with_context(|| format!("creating {}", path.display()))?;
            Some((path, file))
        }
        None => None,
    };
    let saved = file.as_ref().map(|(path, _)| path.clone());
    // Its own task, so a client that hangs up still gets the coverage saved and Profiler, CSS and
    // DOM turned off.
    let page = Arc::clone(page);
    let (sheets, js, css) = tokio::spawn(async move {
        let run = page.devtools.lock().expect("devtools lock").coverage.take().context("coverage was stopped")?;
        let s = Some(&*run.session);
        let (js, css) = tokio::join!(
            page.cdp.raw("Profiler.takePreciseCoverage", json!({}), s),
            page.cdp.raw("CSS.stopRuleUsageTracking", json!({}), s)
        );
        finish(&page, &run.session).await;
        let (js, css) = (js.context("reading JavaScript coverage")?, css.context("reading CSS coverage")?);
        if let Some((path, file)) = file {
            save(file, &js, &css, &run.sheets).with_context(|| format!("writing {}", path.display()))?;
        }
        anyhow::Ok((run.sheets, js, css))
    })
    .await??;
    let scripts: JsCoverage = serde_json::from_str(js.get()).context("decoding JavaScript coverage")?;
    let rules: CssCoverage = serde_json::from_str(css.get()).context("decoding CSS coverage")?;
    let rows = summarize(&scripts.result, &rules.rule_usage, &sheets);
    if let Some(path) = saved {
        cx.say(format!("saved {}", path.display()));
    }
    if cx.json {
        let json: Vec<Value> = rows
            .iter()
            .map(|row| json!({"type": row.kind.label(), "url": row.url, "used": row.used, "total": row.total}))
            .collect();
        cx.say(serde_json::to_string(&json)?);
        return Ok(());
    }
    cx.say(table(&rows));
    Ok(())
}

/// Writes `{"js": …, "css": …, "styleSheets": …}` piece by piece, the protocol's answers as they came.
fn save(file: std::fs::File, js: &RawValue, css: &RawValue, sheets: &HashMap<String, Sheet>) -> std::io::Result<()> {
    use std::io::Write as _;
    let sheets: BTreeMap<&str, Value> =
        sheets.iter().map(|(id, sheet)| (id.as_str(), json!({"url": sheet.url, "length": sheet.length}))).collect();
    let mut out = std::io::BufWriter::new(file);
    out.write_all(b"{\"js\":")?;
    out.write_all(js.get().as_bytes())?;
    out.write_all(b",\"css\":")?;
    out.write_all(css.get().as_bytes())?;
    out.write_all(b",\"styleSheets\":")?;
    serde_json::to_writer(&mut out, &sheets)?;
    out.write_all(b"}")?;
    out.flush()
}

#[derive(Deserialize)]
struct JsCoverage {
    result: Vec<Script>,
}

#[derive(Deserialize)]
struct Script {
    url: String,
    functions: Vec<Function>,
}

#[derive(Deserialize)]
struct Function {
    ranges: Vec<Range>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Range {
    start_offset: u64,
    end_offset: u64,
    count: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CssCoverage {
    rule_usage: Vec<RuleUsage>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuleUsage {
    style_sheet_id: String,
    start_offset: f64,
    end_offset: f64,
    used: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Js,
    Css,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::Js => "js",
            Kind::Css => "css",
        }
    }
}

/// Usage of one file, in characters.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Row {
    kind: Kind,
    url: String,
    used: u64,
    total: u64,
}

/// One source file: its kind, URL and length.
type Source<'a> = (Kind, &'a str, u64);

/// Per-file usage, most unused first. Scripts without a URL (evaluated code, `hba`'s own) and
/// extension scripts are left out. Copies of one source (same URL and length, as before and
/// after a reload) count once, with what either copy used.
fn summarize(scripts: &[Script], rules: &[RuleUsage], sheets: &HashMap<String, Sheet>) -> Vec<Row> {
    let mut sources: BTreeMap<Source<'_>, Vec<(u64, u64)>> = BTreeMap::new();
    for script in
        scripts.iter().filter(|script| !script.url.is_empty() && !script.url.starts_with("chrome-extension://"))
    {
        let ranges = script
            .functions
            .iter()
            .flat_map(|function| &function.ranges)
            .map(|r| (r.start_offset, r.end_offset, r.count));
        let (used, total) = js_used(ranges.collect());
        sources.entry((Kind::Js, script.url.as_str(), total)).or_default().extend(used);
    }
    for sheet in sheets.values() {
        sources.entry((Kind::Css, sheet.url.as_str(), sheet.length)).or_default();
    }
    for rule in rules.iter().filter(|rule| rule.used) {
        if let Some(sheet) = sheets.get(&rule.style_sheet_id) {
            let used = (rule.start_offset as u64, rule.end_offset as u64);
            sources.entry((Kind::Css, sheet.url.as_str(), sheet.length)).or_default().push(used);
        }
    }
    let mut files: BTreeMap<(Kind, &str), (u64, u64)> = BTreeMap::new();
    for ((kind, url, total), used) in sources {
        let file = files.entry((kind, url)).or_default();
        file.0 += union_length(used).min(total);
        file.1 += total;
    }
    let mut rows: Vec<Row> =
        files.into_iter().map(|((kind, url), (used, total))| Row { kind, url: url.to_owned(), used, total }).collect();
    rows.sort_by(|a, b| (b.total - b.used).cmp(&(a.total - a.used)).then_with(|| a.url.cmp(&b.url)));
    rows
}

/// The character ranges of a script that ran, and its length, from V8 block coverage: nested
/// ranges where the innermost one containing a character gives its execution count.
fn js_used(mut ranges: Vec<(u64, u64, u64)>) -> (Vec<(u64, u64)>, u64) {
    // Outer ranges before the ranges they contain.
    ranges.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let total = ranges.iter().map(|range| range.1).max().unwrap_or(0);
    let mut open: Vec<(u64, u64)> = Vec::new();
    let mut cursor = 0;
    let mut used = Vec::new();
    let mut advance = |to: u64, count: u64, cursor: &mut u64| {
        if to > *cursor {
            if count > 0 {
                used.push((*cursor, to));
            }
            *cursor = to;
        }
    };
    for (start, end, count) in ranges {
        while let Some(&(open_end, open_count)) = open.last() {
            if open_end > start {
                break;
            }
            advance(open_end, open_count, &mut cursor);
            open.pop();
        }
        if let Some(&(_, open_count)) = open.last() {
            advance(start, open_count, &mut cursor);
        }
        cursor = cursor.max(start);
        open.push((end, count));
    }
    while let Some((open_end, open_count)) = open.pop() {
        advance(open_end, open_count, &mut cursor);
    }
    (used, total)
}

/// Characters covered by the union of `ranges`.
fn union_length(mut ranges: Vec<(u64, u64)>) -> u64 {
    ranges.sort_unstable();
    let (mut covered, mut reach) = (0, 0);
    for (start, end) in ranges {
        let start = start.max(reach);
        if end > start {
            covered += end - start;
        }
        reach = reach.max(end);
    }
    covered
}

fn percent(used: u64, total: u64) -> u64 {
    (used * 100 + total / 2).checked_div(total).unwrap_or(0)
}

/// Totals, then the most unused files with their share used.
fn table(rows: &[Row]) -> String {
    if rows.is_empty() {
        return "no coverage: no page scripts or style sheets were seen; run `hba reload` while coverage runs"
            .to_owned();
    }
    let used: u64 = rows.iter().map(|row| row.used).sum();
    let total: u64 = rows.iter().map(|row| row.total).sum();
    let mut out = format!(
        "{} of {} used ({}%) in {}, most unused first:\n",
        output::bytes(used),
        output::bytes(total),
        percent(used, total),
        output::count(rows.len(), "file")
    );
    for row in rows.iter().take(SHOWN) {
        let _ = writeln!(
            out,
            "{:>3}% {:<3} {} of {}  {}",
            percent(row.used, row.total),
            row.kind.label(),
            output::bytes(row.used),
            output::bytes(row.total),
            output::short_url(&row.url, 100)
        );
    }
    if rows.len() > SHOWN {
        let _ = writeln!(out, "… {} more (`-o FILE` keeps everything)", output::count(rows.len() - SHOWN, "file"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn innermost_ranges_decide_what_ran() {
        // A 100-character script whose function body (10..60) ran but whose branch (20..30) did not.
        let (used, total) = js_used(vec![(0, 100, 1), (10, 60, 3), (20, 30, 0), (70, 90, 0)]);
        assert_eq!(total, 100);
        assert_eq!(union_length(used), 70);
        // Script never ran beyond its top level.
        assert_eq!(js_used(vec![(0, 50, 1), (5, 45, 0)]), (vec![(0, 5), (45, 50)], 50));
        assert_eq!(js_used(Vec::new()), (Vec::new(), 0));
    }

    #[test]
    fn css_usage_merges_overlapping_rules() {
        assert_eq!(union_length(vec![(10, 20), (0, 5), (15, 30)]), 25);
        assert_eq!(union_length(vec![(0, 10), (2, 4)]), 10);
    }

    #[test]
    fn rows_group_by_file_merge_reload_copies_and_put_waste_first() {
        let script = |url: &str, ranges: &[(u64, u64, u64)]| Script {
            url: url.into(),
            functions: vec![Function {
                ranges: ranges
                    .iter()
                    .map(|&(start_offset, end_offset, count)| Range { start_offset, end_offset, count })
                    .collect(),
            }],
        };
        let scripts = vec![
            script("http://x.test/app.js", &[(0, 100, 1)]),
            script("http://x.test/app.js", &[(0, 100, 1), (20, 80, 0)]),
            script("http://x.test/", &[(0, 40, 1), (10, 40, 0)]),
            script("", &[(0, 999, 1)]),
        ];
        let sheets: HashMap<String, Sheet> = [
            ("s1".to_owned(), Sheet { url: "http://x.test/site.css".into(), length: 200 }),
            ("s1-reloaded".to_owned(), Sheet { url: "http://x.test/site.css".into(), length: 200 }),
            ("s2".to_owned(), Sheet { url: "http://x.test/".into(), length: 50 }),
        ]
        .into_iter()
        .collect();
        let rules = vec![
            RuleUsage { style_sheet_id: "s1".into(), start_offset: 0.0, end_offset: 50.0, used: true },
            RuleUsage { style_sheet_id: "s1".into(), start_offset: 50.0, end_offset: 200.0, used: false },
            RuleUsage { style_sheet_id: "s1-reloaded".into(), start_offset: 100.0, end_offset: 150.0, used: true },
            RuleUsage { style_sheet_id: "s2".into(), start_offset: 0.0, end_offset: 50.0, used: true },
            RuleUsage { style_sheet_id: "gone".into(), start_offset: 0.0, end_offset: 10.0, used: true },
        ];
        let rows = summarize(&scripts, &rules, &sheets);
        let brief: Vec<(&str, &str, u64, u64)> =
            rows.iter().map(|r| (r.kind.label(), r.url.as_str(), r.used, r.total)).collect();
        assert_eq!(
            brief,
            [
                ("css", "http://x.test/site.css", 100, 200),
                ("js", "http://x.test/", 10, 40),
                ("css", "http://x.test/", 50, 50),
                ("js", "http://x.test/app.js", 100, 100),
            ]
        );
        let text = table(&rows);
        assert!(text.starts_with("260 B of 390 B used (67%) in 4 files, most unused first:\n"), "{text}");
        assert!(text.contains("\n 50% css 100 B of 200 B  http://x.test/site.css\n"), "{text}");
    }
}
