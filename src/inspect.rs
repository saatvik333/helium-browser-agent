//! Reading the page: snapshots, search, text and element properties.

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use crate::cli::{ElementState, FindArgs, GetArgs, GetWhat, IsArgs, SnapArgs, TextArgs};
use crate::daemon::Cx;
use crate::page::{Arg, Page};
use crate::snapshot::{self, Options};

pub async fn snap(cx: &mut Cx<'_>, args: SnapArgs) -> Result<()> {
    let page = cx.page().await?;
    let options = Options {
        interactive: args.interactive,
        viewport: args.viewport,
        boxes: args.boxes,
        raw: args.raw,
        depth: args.depth,
    };
    let mut tree = snapshot::capture(&page, &options).await?;
    if let Some(scope) = &args.scope {
        let reference = match crate::page::parse_ref(scope) {
            Some((_, reference)) => reference,
            None => ref_of(&page, scope, cx.timeout).await?,
        };
        tree = snapshot::subtree(&tree, &reference).with_context(|| format!("{scope} is not in the snapshot"))?;
    }
    let lines = snapshot::render(&tree, &options);
    let previous = page.last_snapshot();
    if args.scope.is_none() {
        page.remember_snapshot(lines.clone());
    }
    let text = if args.diff {
        match previous {
            Some((loader, old)) if loader == page.loader() => match snapshot::diff(&old, &lines) {
                Some(diff) if diff.is_empty() => "(no changes)".to_owned(),
                Some(diff) => diff,
                None => format!("(most of the page changed)\n{}", lines.join("\n")),
            },
            _ => format!("(new page)\n{}", lines.join("\n")),
        }
    } else {
        lines.join("\n")
    };
    if let Some(path) = &args.output {
        let path = cx.path(path);
        std::fs::write(&path, &text).with_context(|| format!("writing {}", path.display()))?;
        cx.say(format!("{} ({} lines)", path.display(), text.lines().count()));
        return Ok(());
    }
    emit_budgeted(cx, &text, args.max, "snapshot", "yml", "use -i, -v or -S REF to narrow it")
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
    let element = page.resolve(selector, timeout).await?;
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
    let page = cx.page().await?;
    let options = Options::default();
    let tree = snapshot::capture(&page, &options).await?;
    let lines = snapshot::render(&tree, &options);
    page.remember_snapshot(lines.clone());
    let query = args.query.to_lowercase();
    let role = args.role.as_deref().map(str::to_lowercase);
    let hits: Vec<&str> = lines
        .iter()
        .map(|line| line.trim_start())
        .filter(|line| line.to_lowercase().contains(&query))
        .filter(|line| {
            role.as_deref()
                .is_none_or(|role| line.strip_prefix("- ").is_some_and(|rest| rest.to_lowercase().starts_with(role)))
        })
        .collect();
    if hits.is_empty() {
        bail!("nothing on the page matches {:?}", args.query);
    }
    for hit in hits.iter().take(50) {
        cx.say(*hit);
    }
    if hits.len() > 50 {
        cx.say(format!("… {} more; refine the query or add --role", hits.len() - 50));
    }
    Ok(())
}

pub async fn text(cx: &mut Cx<'_>, args: TextArgs) -> Result<()> {
    let page = cx.page().await?;
    let function = "function(e) { const t = e ? (e.innerText ?? e.textContent) : \
        (this.document.body ? this.document.body.innerText : this.document.documentElement.textContent); \
        return (t || '').replace(/[ \\t]+\\n/g, '\\n').replace(/\\n{3,}/g, '\\n\\n').trim(); }";
    let value = if let Some(target) = &args.target {
        let element = page.resolve(target, cx.timeout).await?;
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
    let element = page.resolve(target, cx.timeout).await?;
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
    let element = match page.resolve(&args.target, Duration::ZERO).await {
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
    let message = error.to_string();
    message.starts_with("no element matches ") || message.contains(" is not on the page any more")
}

/// Whether an error only says the page was between documents, so asking again will answer.
pub fn retryable(error: &anyhow::Error) -> bool {
    error.to_string().ends_with("; retry")
}
