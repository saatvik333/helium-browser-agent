//! Playwright's in-page engine (vendored, pinned) running in `hba`'s private isolated world.
//!
//! The world is invisible to page scripts. Playwright's few page-visible event names are
//! randomized per daemon so no fixed `__playwright_` string ever reaches a page.

use std::fmt;

use serde::{Serialize, Serializer};

/// Vendored by `scripts/vendor-playwright.mjs`: `function(options) { ... return injectedScript }`.
const FACTORY: &str = include_str!("../assets/playwright/injected.js");

/// Per-daemon world name and engine expression, split around the frame number.
pub struct Source {
    pub world_name: String,
    head: String,
    tail: &'static str,
}

impl Source {
    pub fn new() -> Self {
        let token = format!("_{:012x}", fastrand::u64(..) & 0xffff_ffff_ffff);
        let factory = FACTORY.trim().replace("__playwright_", &token);
        Self {
            world_name: format!("w{:08x}", fastrand::u32(..)),
            head: format!(
                "(() => {{ const pw = ({factory})({{isUnderTest: false, sdkLanguage: 'javascript', frameSeq: "
            ),
            tail: ", testIdAttributeName: 'data-testid', stableRafCount: 1, browserName: 'chromium', \
                   shouldPrependErrorPrefix: false, isUtilityWorld: true, customEngines: []}); \
                   pw.hbaMutations = 0; \
                   new MutationObserver(records => { pw.hbaMutations += records.length; }) \
                     .observe(document, {subtree: true, childList: true, attributes: true, characterData: true}); \
                   return pw; })()",
        }
    }

    /// Expression creating the engine for one frame. Refs get an `f<seq>` prefix in child frames.
    /// The engine also counts DOM mutations so actions can wait for the page to settle.
    pub fn bootstrap(&self, frame_seq: u32) -> Bootstrap<'_> {
        Bootstrap { source: self, frame_seq }
    }
}

impl Default for Source {
    fn default() -> Self {
        Self::new()
    }
}

/// The engine expression for one frame. It serializes as a JSON string written straight into
/// the request, so the large engine source is not copied per frame.
pub struct Bootstrap<'a> {
    source: &'a Source,
    frame_seq: u32,
}

impl fmt::Display for Bootstrap<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}{}", self.source.head, self.frame_seq, self.source.tail)
    }
}

impl Serialize for Bootstrap<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_visible_names_are_randomized() {
        let source = Source::new();
        let script = source.bootstrap(0).to_string();
        assert!(!script.contains("__playwright_"));
        assert!(script.contains("frameSeq: 0, testIdAttributeName"));
        assert_ne!(Source::new().world_name, source.world_name);
    }

    #[test]
    fn bootstrap_serializes_as_the_expression_string() {
        let source = Source::new();
        let json = serde_json::to_string(&source.bootstrap(7)).expect("serializes");
        let parsed: String = serde_json::from_str(&json).expect("a JSON string");
        assert_eq!(parsed, source.bootstrap(7).to_string());
        assert!(parsed.contains("frameSeq: 7,"));
    }
}
