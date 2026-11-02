//! Escape hatches: page JavaScript, raw DevTools protocol, and batch scripts.

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::cli::{CdpArgs, JsArgs, RunArgs};
use crate::daemon::Cx;

pub async fn js(cx: &mut Cx<'_>, args: JsArgs) -> Result<()> {
    let page = cx.page().await?;
    let value = if let Some(extension) = &args.world {
        crate::extensions::world_eval(cx, &page, extension, &args.code).await?
    } else if let Some(frame) = &args.frame {
        let element = page.resolve(frame, cx.timeout).await?;
        let described: Value =
            page.renderer_call("DOM.describeNode", json!({"objectId": element.object}), &element.world.session).await?;
        let frame_id = described["node"]["frameId"]
            .as_str()
            .or_else(|| described["node"]["contentDocument"]["frameId"].as_str())
            .with_context(|| format!("{frame} is not an iframe"))?
            .to_owned();
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
    crate::inspect::emit_budgeted(cx, result.get(), 32_000, "cdp", "json", "the full result is in the file")
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
