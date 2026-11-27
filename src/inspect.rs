//! Reading the page: snapshots, search, text and element properties.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cli::{ElementState, FindArgs, GetArgs, GetWhat, IsArgs, SnapArgs, TextArgs};
use crate::daemon::Cx;
use crate::page::{Arg, Page};
use crate::snapshot::{self, Baseline, Options};

/// Characters of a snapshot printed before the rest spills to a file.
pub const SNAP_BUDGET: usize = 24_000;

pub async fn snap(cx: &mut Cx<'_>, args: SnapArgs) -> Result<()> {
    let page = cx.page().await?;
    let options = Options {
        interactive: args.interactive,
        viewport: args.viewport,
        boxes: args.boxes,
        raw: args.raw,
        depth: args.depth,
    };
    let text = snapshot_text(&page, options, args.scope.as_deref(), args.diff, cx.timeout).await?;
    if let Some(path) = &args.output {
        let path = cx.path(path);
        std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
        cx.say(format!("{} ({} lines)", path.display(), text.lines().count()));
        return Ok(());
    }
    emit_budgeted(cx, &text, args.max, "snapshot", "yml", "use -i, -v or -S REF to narrow it")
}

/// After an action, what changed since the tab's last snapshot, taken the same way again.
pub async fn snap_diff(cx: &mut Cx<'_>, page: &Arc<Page>) -> Result<()> {
    let (options, scope) = page.last_snapshot().map(|seen| (seen.options, seen.scope)).unwrap_or_default();
    let text = match snapshot_text(page, options.clone(), scope.as_deref(), true, cx.timeout).await {
        Ok(text) => text,
        // The action removed the subtree the last snapshot showed.
        Err(_) if scope.is_some() => snapshot_text(page, options, None, true, cx.timeout).await?,
        Err(error) => return Err(error),
    };
    emit_budgeted(cx, &text, SNAP_BUDGET, "snapshot", "yml", "use -i, -v or -S REF to narrow it")
}

/// A fresh snapshot taken with `options`, of the subtree of `scope` (a ref or selector) if
/// given, as text: the tree, or with `diff` what changed since the tab's last snapshot. It
/// becomes the tab's last snapshot.
async fn snapshot_text(
    page: &Arc<Page>,
    options: Options,
    scope: Option<&str>,
    diff: bool,
    timeout: Duration,
) -> Result<String> {
    let mut tree = snapshot::capture(page, &options).await?;
    let scope = match scope {
        Some(scope) => {
            let reference = match crate::page::parse_ref(scope) {
                Some((_, reference)) => reference,
                None => ref_of(page, scope, timeout).await?,
            };
            tree = snapshot::subtree(tree, &reference).with_context(|| format!("{scope} is not in the snapshot"))?;
            Some(reference)
        }
        None => None,
    };
    let lines = snapshot::render(tree, &options);
    let text = if diff {
        let loader = page.loader();
        diff_text(page.last_snapshot().as_ref(), loader.as_deref(), &options, scope.as_deref(), &lines)
    } else {
        lines.join("\n")
    };
    page.remember_snapshot(options, scope, lines);
    Ok(text)
}

/// What `snap -d` prints for `lines`, a snapshot of the document of `loader` taken with
/// `options` (of the subtree of `scope`), given the tab's last snapshot.
fn diff_text(
    previous: Option<&Baseline>,
    loader: Option<&str>,
    options: &Options,
    scope: Option<&str>,
    lines: &[String],
) -> String {
    let whole = || lines.join("\n");
    match previous {
        Some(seen) if seen.loader.as_deref() == loader => {
            if seen.options != *options || seen.scope.as_deref() != scope {
                return format!("(the last snapshot was taken with other options; all of this one)\n{}", whole());
            }
            match snapshot::diff(&seen.lines, lines) {
                Some(diff) if diff.is_empty() => "(no changes)".to_owned(),
                Some(diff) => diff,
                None => format!("(most of the page changed)\n{}", whole()),
            }
        }
        _ => format!("(new page)\n{}", whole()),
    }
}

/// Prints `text`, or its head plus the path of a file holding all of it.
pub fn emit_budgeted(cx: &mut Cx<'_>, text: &str, max: usize, stem: &str, extension: &str, hint: &str) -> Result<()> {
    let (head, rest) = crate::output::head_lines(text, max);
    if rest == 0 {
        if !text.is_empty() {
            cx.say(text);
        }
        return Ok(());
    }
    let path = cx.artifact(stem, extension)?;
    std::fs::write(&path, text)?;
    cx.say(head);
    cx.say(format!("… {rest} more lines in {} ({hint})", path.display()));
    Ok(())
}

/// The snapshot ref of the element a selector matches.
async fn ref_of(page: &Page, selector: &str, timeout: Duration) -> Result<String> {
    let element = page.locate(selector, timeout).await?;
    let value = page
        .call(
            &element.world,
            "function(e) { const s = this._lastAriaSnapshotForQuery; return (s && s.refs.get(e)) || null; }",
            &[Arg::Object(&element.object)],
        )
        .await?;
    value.as_str().map(str::to_owned).with_context(|| format!("{selector} has no ref (not visible to the snapshot)"))
}

pub async fn find(cx: &mut Cx<'_>, args: FindArgs) -> Result<()> {
    const SHOWN: usize = 50;
    let page = cx.page().await?;
    let options = Options::default();
    let tree = snapshot::capture(&page, &options).await?;
    let lines = snapshot::render(tree, &options);
    let query = args.query.to_lowercase();
    let hits: Vec<&str> = lines
        .iter()
        .map(|line| line.trim_start())
        .filter(|line| line_matches(line, &query, args.role.as_deref()))
        .collect();
    let total = hits.len();
    let shown: Vec<String> = hits.into_iter().take(SHOWN).map(str::to_owned).collect();
    // Its refs are the tab's current ones, so it is the snapshot `snap -d` compares against.
    page.remember_snapshot(options, None, lines);
    if total == 0 {
        bail!("nothing on the page matches {:?}", args.query);
    }
    for hit in shown {
        cx.say(hit);
    }
    if total > SHOWN {
        cx.say(format!("… {} more; refine the query or add --role", total - SHOWN));
    }
    Ok(())
}

/// Whether a snapshot line (without its indent) mentions `query`, in lowercase, and is an
/// element of exactly `role` when one is given: `tab` is not `tablist`.
fn line_matches(line: &str, query: &str, role: Option<&str>) -> bool {
    let role_matches = || {
        let Some(rest) = line.strip_prefix("- ") else { return false };
        let key = rest.strip_prefix('\'').unwrap_or(rest);
        key.split([' ', ':']).next().is_some_and(|found| role.is_some_and(|role| found.eq_ignore_ascii_case(role)))
    };
    line.to_lowercase().contains(query) && (role.is_none() || role_matches())
}

pub async fn text(cx: &mut Cx<'_>, args: TextArgs) -> Result<()> {
    let page = cx.page().await?;
    let function = "function(e) { const t = e ? (e.innerText ?? e.textContent) : \
        (this.document.body ? this.document.body.innerText : this.document.documentElement.textContent); \
        return (t || '').replace(/[ \\t]+\\n/g, '\\n').replace(/\\n{3,}/g, '\\n\\n').trim(); }";
    let value = if let Some(target) = &args.target {
        let element = page.locate(target, cx.timeout).await?;
        page.call(&element.world, function, &[Arg::Object(&element.object)]).await?
    } else {
        let world = page.main_world().await?;
        page.call(&world, function, &[Arg::Value(Value::Null)]).await?
    };
    let text = value.as_str().unwrap_or_default();
    if text.is_empty() {
        cx.say("(no text)");
        return Ok(());
    }
    emit_budgeted(cx, text, args.max, "text", "txt", "use `hba text TARGET` for one part")
}

pub async fn get(cx: &mut Cx<'_>, args: GetArgs) -> Result<()> {
    let page = cx.page().await?;
    if args.what == GetWhat::Url {
        cx.say(page.url());
        return Ok(());
    }
    if args.what == GetWhat::Title {
        let world = page.main_world().await?;
        let title = page.call(&world, "function() { return this.document.title; }", &[]).await?;
        cx.say(title.as_str().unwrap_or_default());
        return Ok(());
    }
    let target = args.target.as_deref().context("give a TARGET (ref or selector)")?;
    if args.what == GetWhat::Count {
        let world = page.main_world().await?;
        let count = page
            .call(
                &world,
                "function(s) { return this.querySelectorAll(this.parseSelector(s), this.document).length; }",
                &[Arg::Value(json!(target))],
            )
            .await?;
        cx.say(count.to_string());
        return Ok(());
    }
    let element = page.locate(target, cx.timeout).await?;
    let (function, arg) = match args.what {
        GetWhat::Html => ("function(e) { return e.outerHTML; }", Value::Null),
        GetWhat::Inner => ("function(e) { return e.innerHTML; }", Value::Null),
        GetWhat::Value => {
            ("function(e) { return 'value' in e && !e.isContentEditable ? e.value : e.innerText; }", Value::Null)
        }
        GetWhat::Attr => {
            let name = args.name.as_deref().context("give the attribute name: `hba get attr TARGET href`")?;
            ("function(e, n) { return e.getAttribute(n); }", json!(name))
        }
        GetWhat::Box => (
            "function(e) { const r = e.getBoundingClientRect(); \
               return `x=${Math.round(r.x)} y=${Math.round(r.y)} w=${Math.round(r.width)} h=${Math.round(r.height)}`; }",
            Value::Null,
        ),
        GetWhat::Style => (
            "function(e, n) { const s = getComputedStyle(e); \
               if (n) return s.getPropertyValue(n); \
               return ['display','visibility','opacity','position','color','background-color','font-size','cursor','pointer-events','z-index'] \
                 .map(p => `${p}: ${s.getPropertyValue(p)}`).join('\\n'); }",
            args.name.as_deref().map_or(Value::Null, |n| json!(n)),
        ),
        GetWhat::Locator => {
            ("function(e) { return this.utils.asLocator('javascript', this.generateSelectorSimple(e)); }", Value::Null)
        }
        GetWhat::Url | GetWhat::Title | GetWhat::Count => unreachable!("handled above"),
    };
    let value = page.call(&element.world, function, &[Arg::Object(&element.object), Arg::Value(arg)]).await?;
    match value {
        Value::Null => cx.say("(none)"),
        Value::String(text) => {
            let text = crate::output::render_value(&Value::String(text), args.max);
            cx.say(text);
        }
        other => cx.say(other.to_string()),
    }
    Ok(())
}

pub async fn is(cx: &mut Cx<'_>, args: IsArgs) -> Result<()> {
    let page = cx.page().await?;
    let element = match page.locate(&args.target, Duration::ZERO).await {
        Ok(element) => element,
        // An element that is not on the page is hidden; any other failure answers nothing.
        Err(error) if not_found(&error) && matches!(args.state, ElementState::Hidden | ElementState::Visible) => {
            cx.say((args.state == ElementState::Hidden).to_string());
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let state = match args.state {
        ElementState::Visible => "visible",
        ElementState::Hidden => "hidden",
        ElementState::Enabled => "enabled",
        ElementState::Disabled => "disabled",
        ElementState::Checked => "checked",
        ElementState::Unchecked => "unchecked",
        ElementState::Editable => "editable",
        ElementState::Focused => "focused",
    };
    let value = page
        .call(
            &element.world,
            "function(e, state) { \
               if (state === 'focused') return e === e.getRootNode().activeElement; \
               return this.elementState(e, state, true).matches; }",
            &[Arg::Object(&element.object), Arg::Value(json!(state))],
        )
        .await?;
    cx.say(value.as_bool().unwrap_or(false).to_string());
    Ok(())
}

/// Whether an error of [`Page::resolve`] means the element is not on the page, unlike an
/// ambiguous or invalid selector, a dialog or a closed tab.
pub fn not_found(error: &anyhow::Error) -> bool {
    error.downcast_ref::<crate::page::NotFound>().is_some()
}

/// Whether an error only says the page was between documents, so asking again will answer.
pub fn retryable(error: &anyhow::Error) -> bool {
    error.downcast_ref::<crate::page::Retry>().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn baseline(options: Options, scope: Option<&str>) -> Baseline {
        let lines = ["- button \"Save\" [ref=e1]", "- button \"Undo\" [ref=e2]"].map(String::from).to_vec();
        Baseline { loader: Some("L".into()), options, scope: scope.map(str::to_owned), lines }
    }

    #[test]
    fn diffs_compare_only_snapshots_taken_the_same_way() {
        let lines = ["- button \"Saved\" [ref=e1]", "- button \"Undo\" [ref=e2]"].map(String::from).to_vec();
        let full = Options::default();
        let same = diff_text(Some(&baseline(full.clone(), None)), Some("L"), &full, None, &lines);
        assert!(same.starts_with("- - button \"Save\""), "{same}");
        // `snap -i` before `click -d`: the interactive lines are no baseline for a full render.
        let interactive = Options { interactive: true, ..Options::default() };
        let other = diff_text(Some(&baseline(interactive, None)), Some("L"), &full, None, &lines);
        assert!(other.starts_with("(the last snapshot was taken with other options"), "{other}");
        let subtree = diff_text(Some(&baseline(full.clone(), Some("e7"))), Some("L"), &full, None, &lines);
        assert!(subtree.starts_with("(the last snapshot was taken with other options"), "{subtree}");
        let navigated = diff_text(Some(&baseline(full.clone(), None)), Some("M"), &full, None, &lines);
        assert!(navigated.starts_with("(new page)"), "{navigated}");
    }

    #[test]
    fn find_roles_match_whole_words() {
        let line = "- tablist \"Sections\" [ref=e3]:";
        assert!(line_matches(line, "sections", Some("tablist")));
        assert!(!line_matches(line, "sections", Some("tab")), "tab is not tablist");
        assert!(line_matches("- tab \"Sections\" [ref=e4]", "sections", Some("TAB")));
        assert!(!line_matches("- listitem: Sections", "sections", Some("list")));
        assert!(line_matches("- 'button \"a: b\" [ref=e5]'", "a: b", Some("button")));
        assert!(line_matches("- text: Sections", "sections", None));
        assert!(!line_matches("- text: Sections", "nowhere", None));
    }
}
