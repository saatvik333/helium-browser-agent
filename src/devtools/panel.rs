//! `hba devtools [PANEL]`: opens the browser's developer tools on the tab, for a human.

use anyhow::{Context, Result, bail};
use serde_json::json;

use crate::cli::DevtoolsArgs;
use crate::daemon::Cx;

/// Panel names accepted on the command line and the developer tools' ids for them.
const PANELS: [(&str, &str); 13] = [
    ("elements", "elements"),
    ("console", "console"),
    ("network", "network"),
    ("sources", "sources"),
    ("resources", "resources"),
    ("application", "resources"),
    ("timeline", "timeline"),
    ("performance", "timeline"),
    ("memory", "heap-profiler"),
    ("heap-profiler", "heap-profiler"),
    ("lighthouse", "lighthouse"),
    ("security", "security"),
    ("recorder", "chrome-recorder"),
];

/// Opens the developer tools window docked to the tab, optionally at a panel.
pub async fn open_devtools(cx: &mut Cx<'_>, args: DevtoolsArgs) -> Result<()> {
    let panel = args.panel.as_deref().map(panel_id).transpose()?;
    let page = cx.page().await?;
    let target = page.page_target().context("the tab has no page yet")?;
    let mut params = json!({"targetId": target});
    if let Some(panel) = panel {
        params["panelId"] = json!(panel);
    }
    page.cdp.send("Target.openDevTools", params, None).await.context("opening DevTools")?;
    let tab = cx.tab_number(&page.tab);
    match args.panel {
        Some(name) => cx.say(format!("opened DevTools on {tab} at the {name} panel")),
        None => cx.say(format!("opened DevTools on {tab}")),
    }
    Ok(())
}

fn panel_id(name: &str) -> Result<&'static str> {
    let lower = name.to_ascii_lowercase();
    if let Some((_, id)) = PANELS.iter().find(|(alias, _)| *alias == lower) {
        return Ok(id);
    }
    let names: Vec<&str> = PANELS.iter().map(|(alias, _)| *alias).collect();
    bail!("unknown panel {name}; use one of: {}", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_panel_names_map_to_devtools_ids() {
        assert_eq!(panel_id("Performance").expect("known"), "timeline");
        assert_eq!(panel_id("application").expect("known"), "resources");
        assert_eq!(panel_id("network").expect("known"), "network");
        assert!(panel_id("profiles").is_err());
    }
}
