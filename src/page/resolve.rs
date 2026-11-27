//! Finding elements: refs from the last snapshot and Playwright selectors.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use super::{Arg, Element, NotFound, Page, World};

/// The element of an aria ref in the engine's last snapshot, or `null`.
const REF_ELEMENT: &str =
    "function(ref) { return this.querySelector(this.parseSelector('aria-ref=' + ref), this.document, false) || null; }";

/// The one element a selector matches, or `null`; several matches are an error naming a few.
const SELECTOR_ELEMENT: &str = "function(selector) { \
    const parsed = this.parseSelector(selector); \
    const all = this.querySelectorAll(parsed, this.document); \
    if (all.length > 1) throw new Error(`${selector} matched ${all.length} elements: ` + \
      all.slice(0, 3).map(e => this.previewNode(e)).join(', ') + \
      (all.length > 3 ? ', …' : '') + '; use a ref from `hba snap` or a narrower selector'); \
    return all[0] || null; }";

impl Page {
    /// Resolves a ref (`e5`, `f1e3`) or Playwright selector, waiting up to `timeout` for a
    /// selector to match, and describes the element for messages.
    pub async fn resolve(&self, target: &str, timeout: Duration) -> Result<Element> {
        let (world, object) = self.find_element(target, timeout).await?;
        let label = self.describe(&world, &object).await.unwrap_or_default();
        Ok(Element { world, object, label })
    }

    /// Like [`Page::resolve`], but labels the element with `target` itself, sparing the call
    /// that describes it: for reads whose output does not name the element.
    pub async fn locate(&self, target: &str, timeout: Duration) -> Result<Element> {
        let (world, object) = self.find_element(target, timeout).await?;
        Ok(Element { world, object, label: target.to_owned() })
    }

    async fn find_element(&self, target: &str, timeout: Duration) -> Result<(World, String)> {
        if let Some((seq, reference)) = parse_ref(target) {
            let frame = self.frame_by_seq(seq).ok_or_else(|| {
                NotFound(format!("ref {reference} is not on the page any more (frame f{seq} is gone); run `hba snap`"))
            })?;
            let world = self
                .snapshot_world(&frame)
                .with_context(|| format!("ref {reference} is from an earlier page; run `hba snap` for fresh refs"))?;
            let object = self.ref_object(&world, &reference).await?.ok_or_else(|| {
                NotFound(format!("ref {reference} is not on the page any more; run `hba snap` for fresh refs"))
            })?;
            return Ok((world, object));
        }
        let deadline = Instant::now() + timeout;
        loop {
            match self.query(target).await {
                Ok(Some(found)) => return Ok(found),
                Ok(None) => {}
                // Between two documents (a redirect after a login) the element is not there yet.
                Err(error) if crate::inspect::retryable(&error) => {}
                Err(error) => return Err(error),
            }
            if Instant::now() >= deadline {
                return Err(NotFound(format!("no element matches {target:?}")).into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// The element `selector` matches in the main frame now, if any.
    async fn query(&self, selector: &str) -> Result<Option<(World, String)>> {
        let world = self.main_world().await?;
        let object = self.call_handle(&world, SELECTOR_ELEMENT, &[Arg::Value(json!(selector))]).await?;
        Ok(object.map(|object| (world, object)))
    }

    /// The element of ref `reference` in `world`'s last snapshot, if it is still on the page.
    pub(crate) async fn ref_object(&self, world: &World, reference: &str) -> Result<Option<String>> {
        self.call_handle(world, REF_ELEMENT, &[Arg::Value(json!(reference))]).await
    }

    /// The id of the frame an iframe element `object` (in `world`) shows, if it is an iframe.
    pub(crate) async fn content_frame(&self, world: &World, object: &str) -> Result<Option<String>> {
        let described: Value =
            self.renderer_call("DOM.describeNode", json!({"objectId": object}), &world.session).await?;
        let node = &described["node"];
        let frame = node["frameId"].as_str().or_else(|| node["contentDocument"]["frameId"].as_str());
        Ok(frame.map(str::to_owned))
    }

    /// `role "name"` of an element, for messages.
    async fn describe(&self, world: &World, object: &str) -> Result<String> {
        let value = self
            .call(
                world,
                "function(e) { \
                   const role = this.utils.getAriaRole(e); \
                   const name = this.utils.normalizeWhiteSpace(this.utils.getElementAccessibleNameText(e, false) || ''); \
                   if (!role) return this.previewNode(e); \
                   return name ? `${role} ${JSON.stringify(name.length > 60 ? name.slice(0, 59) + '…' : name)}` : role; }",
                &[Arg::Object(object)],
            )
            .await?;
        Ok(value.as_str().unwrap_or_default().chars().take(100).collect())
    }
}

/// Parses `e12`, `@e12` or `f2e5` into (frame sequence, ref).
pub fn parse_ref(target: &str) -> Option<(u32, String)> {
    let reference = target.strip_prefix('@').unwrap_or(target);
    let reference = reference.strip_prefix("ref=").unwrap_or(reference);
    let (seq, rest) = match reference.strip_prefix('f') {
        Some(rest) => {
            let digits = rest.find('e')?;
            (rest[..digits].parse().ok()?, &rest[digits + 1..])
        }
        None => (0, reference.strip_prefix('e')?),
    };
    (!rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit())).then(|| (seq, reference.to_owned()))
}
