//! The connected browser: target registry, profile contexts, tab creation and attached pages.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{broadcast, mpsc};

use crate::cdp::{Cdp, Event};
use crate::config::Config;
use crate::endpoint::{self, Endpoint};
use crate::page::Page;
use crate::profiles::{Profile, Profiles};
use crate::undo::Undo;

/// A target as reported by `Target.getTargets` and target events.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target_id: String,
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub opener_id: Option<String>,
    #[serde(default)]
    pub browser_context_id: Option<String>,
    #[serde(default)]
    pub subtype: Option<String>,
    #[serde(default)]
    pub embedder_data: Option<TabStrip>,
}

/// Tab-strip placement reported for `tab` targets (Chromium 150+) as `embedderData`.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct TabStrip {
    #[serde(default, rename = "tabStripIndex")]
    pub index: i64,
    #[serde(default, rename = "tabActive")]
    pub active: bool,
    #[serde(default, rename = "tabPinned")]
    pub pinned: bool,
    #[serde(default, rename = "tabGroupId")]
    pub group: Option<String>,
}

/// Browser-level target changes, for popups and closed tabs.
#[derive(Debug, Clone)]
pub enum TargetEvent {
    Created(Arc<TargetInfo>),
    Destroyed(String),
}

pub struct Browser {
    pub cdp: Cdp,
    pub endpoint: Endpoint,
    /// e.g. `Chrome/156.0.8078.73`.
    pub product: String,
    config: Arc<Config>,
    /// Browser context id → profile directory, learned from tabs this daemon identified.
    contexts: Mutex<HashMap<String, String>>,
    pages: Mutex<HashMap<String, Arc<Page>>>,
    events: broadcast::Sender<TargetEvent>,
    /// `hba`'s isolated world name and injected script source.
    pub world: Arc<crate::injected::Source>,
    /// Extension service-worker sessions and their logs.
    pub extensions: Mutex<crate::extensions::State>,
}

#[derive(Deserialize)]
struct Version {
    product: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Targets {
    target_infos: Vec<TargetInfo>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Created {
    target_id: String,
}

impl Browser {
    pub async fn connect(
        config: Arc<Config>,
        world: Arc<crate::injected::Source>,
        profile: Option<&str>,
    ) -> Result<Arc<Self>> {
        let endpoint = endpoint::ensure_running(&config, profile).await?;
        let (cdp, events_rx) = Cdp::connect(&endpoint.ws_url).await?;
        let version: Version = cdp.call("Browser.getVersion", json!({}), None).await?;
        let (events, _) = broadcast::channel(256);
        let browser = Arc::new(Self {
            cdp,
            endpoint,
            product: version.product,
            config,
            contexts: Mutex::new(HashMap::new()),
            pages: Mutex::new(HashMap::new()),
            events,
            world,
            extensions: Mutex::default(),
        });
        tokio::spawn(Self::pump(Arc::downgrade(&browser), events_rx));
        browser
            .cdp
            .send(
                "Target.setDiscoverTargets",
                json!({"discover": true, "filter": [{"type": "page"}, {"type": "tab"}, {"exclude": true}]}),
                None,
            )
            .await?;
        // Downloads keep Helium's own policy; events report where files land.
        browser
            .cdp
            .send("Browser.setDownloadBehavior", json!({"behavior": "default", "eventsEnabled": true}), None)
            .await?;
        Ok(browser)
    }

    pub fn is_connected(&self) -> bool {
        !self.cdp.is_closed()
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TargetEvent> {
        self.events.subscribe()
    }

    async fn pump(browser: std::sync::Weak<Self>, mut events: mpsc::UnboundedReceiver<Event>) {
        while let Some(event) = events.recv().await {
            let Some(browser) = browser.upgrade() else { return };
            browser.on_event(&event);
        }
        if let Some(browser) = browser.upgrade() {
            for page in browser.pages.lock().expect("pages lock").drain() {
                page.1.mark_detached();
            }
        }
    }

    fn on_event(self: &Arc<Self>, event: &Event) {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Info {
            target_info: TargetInfo,
        }
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Gone {
            target_id: String,
        }
        match event.method.as_str() {
            "Target.targetCreated" => {
                if let Ok(Info { target_info }) = event.parse() {
                    let _ = self.events.send(TargetEvent::Created(Arc::new(target_info)));
                }
            }
            "Target.targetDestroyed" => {
                if let Ok(Gone { target_id }) = event.parse() {
                    let page = self.pages.lock().expect("pages lock").remove(&target_id);
                    if let Some(page) = page {
                        page.mark_detached();
                        let browser = Arc::clone(self);
                        tokio::spawn(async move { crate::storage::on_release(&browser, &page).await });
                    }
                    let _ = self.events.send(TargetEvent::Destroyed(target_id));
                }
            }
            "Target.targetInfoChanged" => {
                if let Ok(Info { target_info }) = event.parse()
                    && target_info.kind == "page"
                {
                    for page in self.pages() {
                        page.on_target_changed(&target_info);
                    }
                }
            }
            "Browser.downloadWillBegin" | "Browser.downloadProgress" => {
                for page in self.pages() {
                    crate::download::on_event(&page, event);
                }
            }
            _ => {}
        }
    }

    /// Every target except tabs and the browser: pages, frames, workers, background pages. The
    /// filter is explicit because without one `Target.getTargets` applies the discovery filter,
    /// which leaves out workers.
    pub async fn targets(&self) -> Result<Vec<TargetInfo>> {
        let filter = json!({"filter": [{"type": "browser", "exclude": true}, {"type": "tab", "exclude": true}, {}]});
        let targets: Targets = self.cdp.call("Target.getTargets", filter, None).await?;
        Ok(targets.target_infos)
    }

    /// Tab-strip tabs (those with placement data), in the order `Target.getTargets` reports them.
    pub async fn tabs(&self) -> Result<Vec<TargetInfo>> {
        let targets: Targets =
            self.cdp.call("Target.getTargets", json!({"filter": [{"type": "tab"}, {"exclude": true}]}), None).await?;
        Ok(targets.target_infos.into_iter().filter(|tab| tab.embedder_data.is_some()).collect())
    }

    pub async fn tab_info(&self, tab: &str) -> Result<Option<TargetInfo>> {
        Ok(self.tabs().await?.into_iter().find(|info| info.target_id == tab))
    }

    pub fn profiles(&self) -> Result<Profiles> {
        Profiles::load(&self.config.user_data_dir)
    }

    pub fn profile_of_context(&self, context: &str) -> Option<String> {
        self.contexts.lock().expect("contexts lock").get(context).cloned()
    }

    pub fn contexts(&self) -> HashMap<String, String> {
        self.contexts.lock().expect("contexts lock").clone()
    }

    pub fn learn_context(&self, context: &str, profile_dir: &str) {
        self.contexts.lock().expect("contexts lock").insert(context.to_owned(), profile_dir.to_owned());
    }

    pub fn restore_contexts(&self, known: HashMap<String, String>) {
        self.contexts.lock().expect("contexts lock").extend(known);
    }

    /// Maps the only live context to the only open profile, without opening anything.
    pub async fn infer_contexts(&self) -> Result<()> {
        let tabs = self.tabs().await?;
        let mut live: Vec<&str> = tabs.iter().filter_map(|tab| tab.browser_context_id.as_deref()).collect();
        live.sort_unstable();
        live.dedup();
        let profiles = self.profiles()?;
        let mut contexts = self.contexts.lock().expect("contexts lock");
        contexts.retain(|context, _| live.contains(&context.as_str()));
        if let ([context], [dir]) = (live.as_slice(), profiles.open.as_slice()) {
            contexts.entry((*context).to_owned()).or_insert_with(|| dir.clone());
        }
        Ok(())
    }

    /// Opens a new tab in `profile` and returns its tab target id.
    pub async fn create_tab(&self, profile: &Profile, background: bool) -> Result<String> {
        let known = {
            let contexts = self.contexts.lock().expect("contexts lock");
            contexts.iter().find(|(_, dir)| **dir == profile.dir).map(|(context, _)| context.clone())
        };
        let context = match known {
            Some(context) => Some(context),
            None => self
                .identify_default_context()
                .await?
                .filter(|(_, dir)| *dir == profile.dir)
                .map(|(context, _)| context),
        };
        if let Some(context) = context {
            let created = self
                .cdp
                .call::<Created>(
                    "Target.createTarget",
                    json!({"url": "about:blank", "browserContextId": context, "background": background, "forTab": true}),
                    None,
                )
                .await;
            match created {
                Ok(created) => return Ok(created.target_id),
                // Only the last-used profile accepts new targets over CDP; others go through the launcher.
                Err(error) if format!("{error:#}").contains("Failed to find browser context") => {}
                Err(error) => return Err(error),
            }
        }
        self.launch_tab(profile).await
    }

    /// Learns which profile the default (last-used) context is, with a short-lived probe tab
    /// that reads `chrome://version`. Returns (context, profile directory).
    async fn identify_default_context(&self) -> Result<Option<(String, String)>> {
        let created: Created = self
            .cdp
            .call("Target.createTarget", json!({"url": "about:blank", "background": true, "forTab": true}), None)
            .await?;
        let probe = created.target_id;
        // Closed also when the command is cancelled meanwhile.
        let close = Undo::new({
            let (cdp, probe) = (self.cdp.clone(), probe.clone());
            async move {
                cdp.send("Target.closeTarget", json!({"targetId": probe}), None).await.ok();
            }
        });
        let outcome = self.identify(&probe).await;
        self.close_target(&probe).await.ok();
        close.disarm();
        outcome.map(Some)
    }

    /// Reads a tab's profile directory from `chrome://version` and records its context.
    async fn identify(&self, tab: &str) -> Result<(String, String)> {
        let info = self.tab_info(tab).await?.context("the probe tab disappeared")?;
        let context = info.browser_context_id.context("the probe tab has no browser context")?;
        let page = self.attach(tab).await?;
        let dir = page.profile_directory().await?;
        self.learn_context(&context, &dir);
        Ok((context, dir))
    }

    /// Opens a marked tab through the launcher so the profile is chosen by directory.
    async fn launch_tab(&self, profile: &Profile) -> Result<String> {
        let nonce = format!("{:016x}", fastrand::u64(..));
        let marker = format!("data:text/html,<title>hba {nonce}</title>");
        let focus = crate::niri::focused_window().await;
        endpoint::launch(&self.config, &[format!("--profile-directory={}", profile.dir), marker.clone()])?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let tab = loop {
            if let Some(tab) = self.tabs().await?.into_iter().find(|tab| tab.url == marker) {
                break tab;
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("Helium did not open a tab in profile {:?}", profile.name);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        };
        crate::niri::restore_focus(focus).await;
        if let Some(context) = &tab.browser_context_id {
            self.learn_context(context, &profile.dir);
        }
        Ok(tab.target_id)
    }

    pub async fn close_target(&self, target: &str) -> Result<()> {
        let page = self.pages.lock().expect("pages lock").remove(target);
        if let Some(page) = page {
            page.mark_detached();
            crate::storage::on_release(self, &page).await;
        }
        self.cdp.send("Target.closeTarget", json!({"targetId": target}), None).await
    }

    /// The attached page for a tab target, attaching on first use.
    pub async fn attach(&self, tab: &str) -> Result<Arc<Page>> {
        if let Some(page) = self.pages.lock().expect("pages lock").get(tab).filter(|page| page.is_live()).cloned() {
            return Ok(page);
        }
        let page = Page::attach(self.cdp.clone(), Arc::clone(&self.world), self.config.humanize, tab).await?;
        self.pages.lock().expect("pages lock").insert(tab.to_owned(), Arc::clone(&page));
        Ok(page)
    }

    /// Detaches from a tab without closing it.
    pub async fn detach(&self, tab: &str) {
        let page = self.pages.lock().expect("pages lock").remove(tab);
        if let Some(page) = page {
            page.detach().await;
            crate::storage::on_release(self, &page).await;
        }
    }

    pub fn attached(&self, tab: &str) -> Option<Arc<Page>> {
        self.pages.lock().expect("pages lock").get(tab).cloned()
    }

    /// The attached pages.
    pub fn pages(&self) -> Vec<Arc<Page>> {
        self.pages.lock().expect("pages lock").values().cloned().collect()
    }

    /// The tab target that hosts `page_target`, found by asking candidate tabs for their page.
    pub async fn tab_of_page(&self, page_target: &str, url_hint: &str) -> Result<Option<String>> {
        let attached: Vec<String> = self.pages.lock().expect("pages lock").keys().cloned().collect();
        let mut candidates: Vec<TargetInfo> =
            self.tabs().await?.into_iter().filter(|tab| !attached.contains(&tab.target_id)).collect();
        candidates.sort_by_key(|tab| tab.url != url_hint);
        for tab in candidates.into_iter().take(8) {
            if Page::page_target_of(&self.cdp, &tab.target_id).await?.as_deref() == Some(page_target) {
                return Ok(Some(tab.target_id));
            }
        }
        Ok(None)
    }
}
