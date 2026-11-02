//! Helium's extension manager API (`chrome.developerPrivate`), used from a helper
//! `chrome://extensions` page: manifest and runtime errors, developer mode, and reloading.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::browser::Browser;

use super::debugger::{Debugger, Helper};

/// Reads (and optionally deletes) an extension's errors. Helium records runtime errors only in
/// developer mode, so it is turned on when off.
const ERRORS: &str = r"async (id, clear) => {
  const dp = chrome.developerPrivate;
  const { inDeveloperMode } = await dp.getProfileConfiguration();
  if (!inDeveloperMode) await dp.updateProfileConfiguration({ inDeveloperMode: true });
  const info = await dp.getExtensionInfo(id);
  const manifest = (info.manifestErrors ?? []).map((e) => ({ message: e.message }));
  const runtime = (info.runtimeErrors ?? []).map((e) => {
    const top = e.stackTrace?.[0] ?? {};
    return { severity: e.severity, message: e.message, url: top.url || e.source || e.contextUrl || '',
             line: top.lineNumber ?? 0, occurrences: e.occurrences ?? 1 };
  });
  if (clear && (manifest.length || runtime.length)) await dp.deleteExtensionErrors({ extensionId: id });
  return { enabledDeveloperMode: !inDeveloperMode, manifest, runtime };
}";

/// Reloads like the reload button; resolves to the load error of a broken unpacked extension.
const RELOAD: &str = r"async (id) =>
  (await chrome.developerPrivate.reload(id, { failQuietly: true, populateErrorForUnpacked: true })) ?? null";

/// An extension's recorded problems.
#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Errors {
    /// Developer mode was off; hba turned it on so Helium records runtime errors.
    pub enabled_developer_mode: bool,
    pub manifest: Vec<ManifestError>,
    pub runtime: Vec<RuntimeError>,
}

/// A manifest warning, e.g. an unknown key or permission.
#[derive(Debug, Deserialize, Serialize)]
pub struct ManifestError {
    pub message: String,
}

/// An error or `console.error` recorded from the extension's own pages and worker.
#[derive(Debug, Deserialize, Serialize)]
pub struct RuntimeError {
    /// `ERROR`, `WARN` or `LOG`.
    pub severity: String,
    pub message: String,
    /// Script of the top stack frame, else where the error came from.
    pub url: String,
    /// One-based line in `url`.
    pub line: i64,
    pub occurrences: i64,
}

impl RuntimeError {
    /// `error sw.js:30 Uncaught Error: boom (×3)`.
    pub fn summary(&self) -> String {
        let mut text = self.severity.to_lowercase();
        let path = self.url.split(['?', '#']).next().unwrap_or_default();
        if let Some(file) = path.rsplit('/').find(|part| !part.is_empty()) {
            let _ = write!(text, " {file}");
            if self.line > 0 {
                let _ = write!(text, ":{}", self.line);
            }
        }
        let _ = write!(text, " {}", self.message);
        if self.occurrences > 1 {
            let _ = write!(text, " (×{})", self.occurrences);
        }
        text
    }
}

/// Why a reload failed, as `chrome://extensions` reports it.
#[derive(Debug, Deserialize)]
pub struct LoadError {
    pub error: String,
    #[serde(default)]
    source: Option<Source>,
}

/// The manifest text around a load error.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Source {
    #[serde(default)]
    before_highlight: String,
    #[serde(default)]
    highlight: String,
}

impl LoadError {
    /// The message, plus the manifest line it points at.
    pub fn describe(&self) -> String {
        let mut text = self.error.clone();
        if let Some(source) = self.source.as_ref().filter(|source| !source.highlight.trim().is_empty()) {
            let line = source.before_highlight.matches('\n').count() + 1;
            let _ = write!(text, "\nmanifest.json:{line}: {}", source.highlight.trim());
        }
        text
    }
}

/// An open helper `chrome://extensions` page.
pub struct Manager<'a> {
    browser: &'a Browser,
    helper: Helper,
}

impl<'a> Manager<'a> {
    pub async fn open(debugger: &Debugger<'a>) -> Result<Self> {
        let helper = Helper::open(debugger, "chrome://extensions/").await.context("opening chrome://extensions")?;
        Ok(Self { browser: debugger.browser, helper })
    }

    /// Calls `function` with `args` in the page and decodes what it resolves to.
    async fn run<T: DeserializeOwned>(&self, function: &str, args: &[Value]) -> Result<T> {
        let expression = format!("({function})(...{})", Value::from(args.to_vec()));
        let session = &self.helper.session;
        let value =
            crate::page::evaluate_on(&self.browser.cdp, session, None, &expression, false, crate::cdp::CALL_TIMEOUT)
                .await
                .context("chrome.developerPrivate")?;
        serde_json::from_value(value).context("decoding chrome.developerPrivate's answer")
    }

    /// The extension's errors; `clear` deletes them after reading.
    pub async fn errors(&self, id: &str, clear: bool) -> Result<Errors> {
        self.run(ERRORS, &[json!(id), json!(clear)]).await
    }

    /// Reloads the extension; `Some` describes why it failed to load.
    pub async fn reload(&self, id: &str) -> Result<Option<LoadError>> {
        self.run(RELOAD, &[json!(id)]).await
    }

    pub async fn close(self) {
        self.helper.close().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_error_summaries() {
        let error = |url: &str, line: i64, occurrences: i64| RuntimeError {
            severity: "ERROR".to_owned(),
            message: "Uncaught Error: boom".to_owned(),
            url: url.to_owned(),
            line,
            occurrences,
        };
        assert_eq!(error("chrome-extension://abc/js/sw.js", 30, 1).summary(), "error sw.js:30 Uncaught Error: boom");
        assert_eq!(
            error("chrome-extension://abc/popup.html", 0, 3).summary(),
            "error popup.html Uncaught Error: boom (×3)"
        );
        assert_eq!(error("", 1, 1).summary(), "error Uncaught Error: boom");
    }

    #[test]
    fn load_errors_point_at_the_manifest_line() {
        let error: LoadError = serde_json::from_value(json!({
            "error": "Required value 'version' is missing or invalid.",
            "path": "~/ext",
            "source": {"beforeHighlight": "{\n  \"name\": \"x\",\n  ", "highlight": "\"version\": 5,", "afterHighlight": "\n}"}
        }))
        .expect("load error");
        assert_eq!(
            error.describe(),
            "Required value 'version' is missing or invalid.\nmanifest.json:3: \"version\": 5,"
        );
        let bare: LoadError =
            serde_json::from_value(json!({"error": "Could not load background script 'sw.js'."})).expect("load error");
        assert_eq!(bare.describe(), "Could not load background script 'sw.js'.");
    }
}
