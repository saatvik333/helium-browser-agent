//! Navigation: open, history and reload.

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Result, bail};
use serde::Deserialize;
use serde_json::json;

use crate::cli::{LoadState, OpenArgs, ReloadArgs};
use crate::daemon::Cx;

pub async fn open(cx: &mut Cx<'_>, args: OpenArgs) -> Result<()> {
    let url = normalize_url(&args.url, &cx.cwd);
    let browser = cx.browser().await?;
    let reuse = !args.new && cx.current_tab(&browser).await.is_ok();
    let page = if reuse { cx.page().await? } else { cx.new_tab(args.front).await? };
    if reuse && args.front {
        browser.cdp.send("Target.activateTarget", json!({"targetId": page.tab}), None).await?;
    }
    let mark = page.mark();
    page.goto(&url, args.wait, cx.timeout).await?;
    let tab = cx.tab_number(&page.tab);
    let title = browser.tab_info(&page.tab).await?.map(|info| info.title).unwrap_or_default();
    if cx.json {
        let value = json!({"tab": tab, "id": page.tab, "page": page.page_target(), "url": page.url(), "title": title});
        cx.say(value.to_string());
        return Ok(());
    }
    cx.say(format!("{tab} {} {}", crate::output::short_url(&page.url(), 120), quote_title(&title)));
    let mut report = String::new();
    page.report(&mark, &mut report);
    for line in report.lines().filter(|line| !line.starts_with(['→', '↻'])) {
        cx.say(line);
    }
    Ok(())
}

pub async fn history(cx: &mut Cx<'_>, delta: i64) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct History {
        current_index: i64,
        entries: Vec<Entry>,
    }
    #[derive(Deserialize)]
    struct Entry {
        id: i64,
        url: String,
    }
    let page = cx.page().await?;
    let session = page.session()?;
    let history: History = page.cdp.call("Page.getNavigationHistory", json!({}), Some(&session)).await?;
    let index = history.current_index + delta;
    let Some(entry) = usize::try_from(index).ok().and_then(|i| history.entries.get(i)) else {
        bail!("no {} history entry", if delta < 0 { "previous" } else { "next" });
    };
    let mark = page.mark();
    let go = page.cdp.send("Page.navigateToHistoryEntry", json!({"entryId": entry.id}), Some(&session));
    page.heading_to(&entry.url, go).await?;
    page.settle(&mark, cx.timeout).await?;
    finish(cx, &page, &mark).await
}

pub async fn reload(cx: &mut Cx<'_>, args: ReloadArgs) -> Result<()> {
    let page = cx.page().await?;
    let session = page.session()?;
    let mark = page.mark();
    let mut signals = page.subscribe();
    page.cdp.send("Page.reload", json!({"ignoreCache": args.hard}), Some(&session)).await?;
    page.wait_until(&mut signals, cx.timeout, || page.navigations() != mark.navigations).await?;
    page.wait_load(LoadState::Dom, cx.timeout).await?;
    finish(cx, &page, &mark).await
}

async fn finish(cx: &mut Cx<'_>, page: &crate::page::Page, mark: &crate::page::Mark) -> Result<()> {
    let browser = cx.browser().await?;
    let title = browser.tab_info(&page.tab).await?.map(|info| info.title).unwrap_or_default();
    cx.say(format!("{} {}", crate::output::short_url(&page.url(), 120), quote_title(&title)));
    let mut report = String::new();
    page.report(mark, &mut report);
    for line in report.lines().filter(|line| !line.starts_with(['→', '↻'])) {
        cx.say(line);
    }
    Ok(())
}

fn quote_title(title: &str) -> String {
    if title.is_empty() { String::new() } else { format!("{:?}", crate::output::truncate(title, 80)) }
}

/// Adds a scheme where one is missing; existing local files become `file://` URLs.
pub fn normalize_url(input: &str, cwd: &Path) -> String {
    let input = input.trim();
    let has_scheme = input.split_once(':').is_some_and(|(scheme, rest)| {
        !scheme.is_empty()
            && scheme.chars().all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            && !rest.chars().next().is_some_and(|c| c.is_ascii_digit())
    });
    if has_scheme {
        return input.to_owned();
    }
    let path = crate::client::absolute(cwd, Path::new(input));
    if (input.starts_with('.') || input.starts_with('/'))
        && let Ok(real) = path.canonicalize()
    {
        return file_url(&real);
    }
    if crate::console::is_local_url(&format!("http://{input}")) {
        format!("http://{input}")
    } else {
        format!("https://{input}")
    }
}

/// A `file://` URL for an absolute path, percent-encoded so that `#`, `?`, `%` and spaces stay
/// part of the file name.
fn file_url(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        // RFC 3986 path characters (`pchar` and `/`) stay; everything else is encoded.
        if byte.is_ascii_alphanumeric() || b"/-._~!$&'()*+,;=:@".contains(&byte) {
            url.push(char::from(byte));
        } else {
            let _ = write!(url, "%{byte:02X}");
        }
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_get_sensible_schemes() {
        let cwd = Path::new("/nonexistent");
        assert_eq!(normalize_url("example.com", cwd), "https://example.com");
        assert_eq!(normalize_url("localhost:3000/x", cwd), "http://localhost:3000/x");
        assert_eq!(normalize_url("127.0.0.1:8080", cwd), "http://127.0.0.1:8080");
        assert_eq!(normalize_url("127.0.0.1.attacker.example", cwd), "https://127.0.0.1.attacker.example");
        assert_eq!(
            normalize_url("127.0.0.1.attacker.example:8080/x", cwd),
            "https://127.0.0.1.attacker.example:8080/x"
        );
        assert_eq!(normalize_url("https://a.b/c", cwd), "https://a.b/c");
        assert_eq!(normalize_url("about:blank", cwd), "about:blank");
        assert_eq!(normalize_url("chrome://extensions", cwd), "chrome://extensions");
    }

    #[test]
    fn existing_files_become_file_urls() {
        let dir = std::env::temp_dir().join(format!("hba-nav-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("report #1?100% done.html");
        std::fs::write(&file, "<p>x</p>").unwrap();
        let real = dir.canonicalize().unwrap();
        let expected = format!("{}/report%20%231%3F100%25%20done.html", file_url(&real));
        assert_eq!(normalize_url("./report #1?100% done.html", &dir), expected);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn file_urls_encode_what_a_url_would_read() {
        assert_eq!(file_url(Path::new("/tmp/a b/c#d?e%f.html")), "file:///tmp/a%20b/c%23d%3Fe%25f.html");
        assert_eq!(file_url(Path::new("/srv/(1)+x,y=z@v.txt")), "file:///srv/(1)+x,y=z@v.txt");
        assert_eq!(file_url(Path::new("/tmp/ä\\\"<>.html")), "file:///tmp/%C3%A4%5C%22%3C%3E.html");
    }
}
