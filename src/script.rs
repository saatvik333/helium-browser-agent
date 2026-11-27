//! Escape hatches: page JavaScript, raw DevTools protocol, and batch scripts.

use std::borrow::Cow;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::cli::{CdpArgs, JsArgs, RunArgs};
use crate::daemon::Cx;

/// Characters of a `cdp` result printed before the rest spills to a file.
const CDP_BUDGET: usize = 32_000;

pub async fn js(cx: &mut Cx<'_>, args: JsArgs) -> Result<()> {
    let page = cx.page().await?;
    let value = if let Some(extension) = &args.world {
        crate::extensions::world_eval(cx, &page, extension, &args.code).await?
    } else if let Some(frame) = &args.frame {
        let element = page.locate(frame, cx.timeout).await?;
        let frame_id = page
            .content_frame(&element.world, &element.object)
            .await?
            .with_context(|| format!("{frame} is not an iframe"))?;
        page.evaluate_in_frame(&frame_id, &args.code).await?
    } else {
        page.evaluate(&args.code, true).await?
    };
    if cx.json {
        let text = serde_json::to_string(&value)?;
        cx.say(text);
    } else {
        cx.say(crate::output::render_value(&value, args.max));
    }
    Ok(())
}

pub async fn cdp(cx: &mut Cx<'_>, args: CdpArgs) -> Result<()> {
    let params: Value = match args.params.as_deref() {
        Some(text) if !text.trim().is_empty() => serde_json::from_str(text).context("PARAMS must be a JSON object")?,
        _ => json!({}),
    };
    let result = if args.browser {
        let browser = cx.browser().await?;
        browser.cdp.raw(&args.method, &params, None).await?
    } else {
        let page = cx.page_with_dialog().await?;
        let session = page.session()?;
        page.cdp.raw(&args.method, &params, Some(&session)).await?
    };
    let text = cdp_text(result.get())?;
    crate::inspect::emit_budgeted(cx, &text, CDP_BUDGET, "cdp", "json", "the full result is in the file")
}

/// A `cdp` result as printed: compact JSON while it fits the budget; beyond it, one line per
/// field, so the printed head ends between fields rather than in the middle of a value.
fn cdp_text(raw: &str) -> Result<Cow<'_, str>> {
    if raw.chars().count() <= CDP_BUDGET {
        return Ok(Cow::Borrowed(raw));
    }
    let value: Value = serde_json::from_str(raw).context("decoding the result")?;
    Ok(Cow::Owned(serde_json::to_string_pretty(&value)?))
}

pub async fn run(cx: &mut Cx<'_>, args: RunArgs) -> Result<()> {
    let total = args.commands.len();
    let mut failures = 0;
    for (index, (line, command)) in args.commands.into_iter().enumerate() {
        cx.say(format!("> {line}"));
        if let Err(error) = Box::pin(crate::commands::run(cx, command)).await {
            failures += 1;
            cx.say(format!("error: {error:#}"));
            if !args.keep_going {
                anyhow::bail!("stopped at line {} of {total}", index + 1);
            }
        }
    }
    if failures > 0 {
        anyhow::bail!("{failures} of {total} commands failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_cdp_results_are_cut_between_fields() {
        assert_eq!(cdp_text(r#"{"a":1}"#).unwrap(), r#"{"a":1}"#);
        let entries: Vec<Value> = (0..2000).map(|i| json!({"name": format!("cookie-{i}"), "value": "x"})).collect();
        let raw = json!({"cookies": entries}).to_string();
        let text = cdp_text(&raw).unwrap();
        // Short lines, so the budget cuts the result between fields, never inside a value.
        assert!(text.lines().count() > 2000 && text.lines().all(|line| line.len() < 80), "{text:.300}");
        let (head, rest) = crate::output::head_lines(&text, CDP_BUDGET);
        assert!(rest > 0 && head.ends_with(['{', '}', ',', '"', '[']), "{:?}", &head[head.len() - 40..]);
    }
}
