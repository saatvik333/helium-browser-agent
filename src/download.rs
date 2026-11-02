//! Downloads started from a tab, observed without changing the browser's download settings.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::cdp::Event;
use crate::cli::{DownloadArgs, MouseButton};
use crate::daemon::Cx;
use crate::page::{Page, Signal};

/// One download a tab started.
#[derive(Debug, Clone)]
pub struct Download {
    pub seq: u64,
    pub guid: String,
    pub url: String,
    pub filename: String,
    /// `inProgress`, `completed` or `canceled`.
    pub state: String,
    pub path: Option<String>,
    pub received: u64,
    pub total: u64,
    pub started: Instant,
}

/// The tab's recent downloads, numbered so actions can report the ones they caused.
#[derive(Debug, Default)]
pub struct Log {
    pub items: Vec<Download>,
    seq: u64,
}

impl Log {
    /// Number of the latest download, for [`Log::since`].
    pub fn seq(&self) -> u64 {
        self.seq
    }

    /// Downloads numbered after `seq`.
    pub fn since(&self, seq: u64) -> impl Iterator<Item = &Download> {
        self.items.iter().filter(move |item| item.seq > seq)
    }

    /// Appends one `+ download` line per download after `seq`.
    pub fn report_since(&self, seq: u64, out: &mut String) {
        for item in self.since(seq) {
            let where_ = item.path.as_deref().unwrap_or(&item.filename);
            let _ = writeln!(out, "+ download {} ({})", where_, item.state);
        }
    }
}

/// Records `Browser.download*` events that belong to this tab's frames.
pub fn on_event(page: &Page, event: &Event) {
    match event.method.as_str() {
        "Browser.downloadWillBegin" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Begin {
                frame_id: String,
                guid: String,
                url: String,
                suggested_filename: String,
            }
            let Ok(begin) = event.parse::<Begin>() else { return };
            if page.frame_session(&begin.frame_id).is_none() {
                return;
            }
            let mut log = page.downloads.lock().expect("downloads lock");
            log.seq += 1;
            let seq = log.seq;
            log.items.push(Download {
                seq,
                guid: begin.guid,
                url: begin.url,
                filename: begin.suggested_filename,
                state: "inProgress".to_owned(),
                path: None,
                received: 0,
                total: 0,
                started: Instant::now(),
            });
            if log.items.len() > 100 {
                log.items.remove(0);
            }
            drop(log);
            page.signal(Signal::Download);
        }
        "Browser.downloadProgress" => {
            #[derive(Deserialize)]
            #[serde(rename_all = "camelCase")]
            struct Progress {
                guid: String,
                state: String,
                #[serde(default)]
                received_bytes: f64,
                #[serde(default)]
                total_bytes: f64,
                #[serde(default)]
                file_path: Option<String>,
            }
            let Ok(progress) = event.parse::<Progress>() else { return };
            let mut log = page.downloads.lock().expect("downloads lock");
            let Some(item) = log.items.iter_mut().find(|item| item.guid == progress.guid) else { return };
            let finished = item.state != progress.state && progress.state != "inProgress";
            item.state = progress.state;
            item.received = progress.received_bytes as u64;
            item.total = progress.total_bytes as u64;
            if progress.file_path.is_some() {
                item.path = progress.file_path;
            }
            drop(log);
            if finished {
                page.signal(Signal::Download);
            }
        }
        _ => {}
    }
}

/// `hba download TARGET [-o PATH]`: clicks the element and waits for the download it starts.
pub async fn command(cx: &mut Cx<'_>, args: DownloadArgs) -> Result<()> {
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let patience = cx.timeout.min(Duration::from_secs(10));
    let element = page.resolve(&args.target, patience).await?;
    let point = crate::actions::actionable_point(&page, &element, None, false, patience).await?;
    let watch = cx.watch(&browser).await;
    let mut mark = page.mark();
    let mut signals = page.subscribe();
    let human = page.human();
    crate::input::move_to(&page, point, human).await?;
    crate::input::click(&page, MouseButton::Left, 1, 0, human).await?;
    let mut finished = None;
    let waited = page
        .wait_until(&mut signals, cx.timeout, || {
            let log = page.downloads.lock().expect("downloads lock");
            finished = log.since(mark.download_seq).find(|item| item.state != "inProgress").cloned();
            finished.is_some()
        })
        .await;
    let name = browser.config().browser.name();
    let download = match (waited, finished) {
        (Ok(()), Some(download)) => download,
        (result, _) => {
            let started = page.downloads.lock().expect("downloads lock").since(mark.download_seq).next().cloned();
            return Err(match started {
                Some(item) => anyhow::anyhow!(
                    "the download of {} from {} did not finish within {:.0}s ({} of {} bytes); if {name} asks where to save files, answer its dialog",
                    item.filename,
                    crate::output::short_url(&item.url, 80),
                    cx.timeout.as_secs_f32(),
                    item.received,
                    item.total
                ),
                None => result.err().map_or_else(
                    || anyhow::anyhow!("clicking {} started no download", element.label),
                    |error| error.context(format!("clicking {} started no download", element.label)),
                ),
            });
        }
    };
    if download.state != "completed" {
        bail!("the download of {} was {}", download.filename, download.state);
    }
    let saved = PathBuf::from(download.path.as_deref().with_context(|| {
        format!(
            "{} finished, but {name} did not say where it saved it; look in its downloads folder",
            download.filename
        )
    })?);
    let path = match &args.output {
        Some(output) => {
            let output = cx.path(output);
            let name = saved.file_name().context("the downloaded file has no name")?;
            let target = destination(&output, output.is_dir(), name);
            move_file(&saved, &target).await?;
            target
        }
        None => saved,
    };
    let size = std::fs::metadata(&path).with_context(|| format!("reading {}", path.display()))?.len();
    cx.say(format!("saved {} ({})", path.display(), crate::output::bytes(size)));
    // This download is the command's result; the report covers anything else the click did.
    mark.download_seq = download.seq;
    crate::actions::after(cx, &browser, &page, &mark, watch, false).await
}

/// Where `-o` puts a file: inside `output` when it is a directory (or ends with `/`), else `output`.
fn destination(output: &Path, is_dir: bool, name: &std::ffi::OsStr) -> PathBuf {
    if is_dir || output.as_os_str().to_string_lossy().ends_with('/') { output.join(name) } else { output.to_owned() }
}

/// Moves a file, copying across filesystems, without blocking the daemon's thread.
async fn move_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        tokio::fs::create_dir_all(parent).await.with_context(|| format!("creating {}", parent.display()))?;
    }
    match tokio::fs::rename(from, to).await {
        Err(error) if error.kind() == std::io::ErrorKind::CrossesDevices => {
            tokio::fs::copy(from, to)
                .await
                .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
            tokio::fs::remove_file(from).await.with_context(|| format!("removing {}", from.display()))
        }
        result => result.with_context(|| format!("moving {} to {}", from.display(), to.display())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outputs_into_directories_keep_the_name() {
        let name = std::ffi::OsStr::new("report.pdf");
        assert_eq!(destination(Path::new("/tmp/out"), true, name), Path::new("/tmp/out/report.pdf"));
        assert_eq!(destination(Path::new("/tmp/new/"), false, name), Path::new("/tmp/new/report.pdf"));
        assert_eq!(destination(Path::new("/tmp/x.pdf"), false, name), Path::new("/tmp/x.pdf"));
    }

    #[tokio::test]
    async fn moving_replaces_and_creates_directories() {
        let dir = std::env::temp_dir().join(format!("hba-move-{}", std::process::id()));
        let from = dir.join("a.txt");
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(&from, "data").expect("write");
        let to = dir.join("sub/b.txt");
        move_file(&from, &to).await.expect("moved");
        assert_eq!(std::fs::read_to_string(&to).expect("read"), "data");
        assert!(!from.exists());
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
