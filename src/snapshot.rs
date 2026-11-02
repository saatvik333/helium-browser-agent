//! Accessibility snapshots: Playwright's AI-mode tree, stitched across frames, rendered as YAML.
//!
//! The renderer mirrors Playwright's `renderAriaSnapshotAsYaml` byte for byte in raw mode. The
//! default compact mode only removes what carries no information for an agent.

use std::fmt::Write as _;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::page::{Arg, Page};

/// One node of Playwright's JSON tree.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Node {
    Text(String),
    Element(Box<Element>),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Element {
    pub role: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub checked: Option<Value>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub expanded: bool,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub invalid: Option<Value>,
    #[serde(default)]
    pub level: Option<u32>,
    #[serde(default)]
    pub pressed: Option<Value>,
    #[serde(default)]
    pub selected: bool,
    #[serde(default)]
    pub aria_hidden: bool,
    #[serde(rename = "ref", default)]
    pub reference: Option<String>,
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(rename = "box", default)]
    pub bounds: Option<Bounds>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub placeholder: Option<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub children: Vec<Node>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Bounds {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, Default)]
pub struct Options {
    pub interactive: bool,
    pub viewport: bool,
    pub boxes: bool,
    pub raw: bool,
    pub depth: Option<u32>,
}

/// A captured tree plus the information the renderer needs.
pub struct Tree {
    pub nodes: Vec<Node>,
    pub page_url: String,
    pub viewport: (f64, f64),
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Captured {
    json: Vec<Node>,
    #[serde(default)]
    iframe_depths: std::collections::HashMap<String, u32>,
    #[serde(default)]
    width: f64,
    #[serde(default)]
    height: f64,
}

/// Captures the whole page, recursing into iframes (same- and out-of-process).
pub async fn capture(page: &Arc<Page>, options: &Options) -> Result<Tree> {
    let frame = page.main_frame()?;
    let (nodes, size) = capture_frame(page, &frame, options, options.depth, 0).await?;
    Ok(Tree { nodes, page_url: page.url(), viewport: size })
}

async fn capture_frame(
    page: &Arc<Page>,
    frame: &str,
    options: &Options,
    depth: Option<u32>,
    nesting: u32,
) -> Result<(Vec<Node>, (f64, f64))> {
    let world = page.world(frame).await?;
    let wanted = json!({"mode": "ai", "depth": depth, "boxes": options.boxes || options.viewport});
    let value = page
        .call(
            &world,
            "function(options) { \
               const root = this.document.body || this.document.documentElement; \
               if (!root) return {json: [], iframeDepths: {}, width: 0, height: 0}; \
               const r = this.ariaSnapshotJSON(root, options); \
               return {json: r.json, iframeDepths: r.iframeDepths, \
                       width: this.window.innerWidth, height: this.window.innerHeight}; }",
            &[Arg::Value(wanted)],
        )
        .await?;
    let captured: Captured = serde_json::from_value(value).context("decoding the snapshot")?;
    page.note_snapshot(&world);
    let mut nodes = captured.json;
    if nesting < 4 && !captured.iframe_depths.is_empty() {
        let mut frames = Vec::new();
        for (reference, at_depth) in &captured.iframe_depths {
            let child_depth = depth.map(|d| d.saturating_sub(at_depth + 1));
            if depth.is_some() && child_depth == Some(0) {
                continue;
            }
            if let Some(child) = child_frame(page, &world, reference).await {
                frames.push((reference.clone(), child, child_depth));
            }
        }
        for (reference, child, child_depth) in frames {
            let captured = Box::pin(capture_frame(page, &child, options, child_depth, nesting + 1)).await;
            if let (Ok((children, _)), Some(iframe)) = (captured, iframe_mut(&mut nodes, &reference)) {
                iframe.children = children;
            }
        }
    }
    Ok((nodes, (captured.width, captured.height)))
}

/// The frame id inside the iframe element with `reference`.
async fn child_frame(page: &Page, world: &crate::page::World, reference: &str) -> Option<String> {
    let object = page
        .call_handle(
            world,
            "function(ref) { return this.querySelector(this.parseSelector('aria-ref=' + ref), this.document, false) || null; }",
            &[Arg::Value(json!(reference))],
        )
        .await
        .ok()??;
    let described: Value =
        page.renderer_call("DOM.describeNode", json!({"objectId": object}), &world.session).await.ok()?;
    let node = described.get("node")?;
    node.get("frameId")
        .or_else(|| node.get("contentDocument").and_then(|doc| doc.get("frameId")))
        .and_then(Value::as_str)
        .map(str::to_owned)
}

/// The iframe node with `reference`.
fn iframe_mut<'a>(nodes: &'a mut [Node], reference: &str) -> Option<&'a mut Element> {
    for node in nodes {
        if let Node::Element(element) = node {
            if element.role == "iframe" && element.reference.as_deref() == Some(reference) {
                return Some(element);
            }
            if let Some(iframe) = iframe_mut(&mut element.children, reference) {
                return Some(iframe);
            }
        }
    }
    None
}

/// Renders the tree to YAML lines, applying compaction and filters.
pub fn render(tree: &Tree, options: &Options) -> Vec<String> {
    let mut nodes = tree.nodes.clone();
    if options.viewport {
        nodes = in_viewport(nodes, tree.viewport);
    }
    if !options.raw {
        let origin = origin_of(&tree.page_url);
        nodes = compact(nodes, origin.as_deref());
    }
    if options.interactive {
        nodes = interactive_only(nodes);
    }
    if !options.boxes {
        strip_boxes(&mut nodes);
    }
    let mut lines = Vec::new();
    for node in &nodes {
        match node {
            Node::Text(text) => visit_text(&mut lines, text, 0),
            Node::Element(element) if element.role == "text" => {
                visit_text(&mut lines, element.text.as_deref().unwrap_or(""), 0);
            }
            Node::Element(element) => visit(&mut lines, element, 0),
        }
    }
    lines
}

/// Like [`render`] but for the subtree of the element whose ref is `reference`.
pub fn subtree(tree: &Tree, reference: &str) -> Option<Tree> {
    fn find(nodes: &[Node], reference: &str) -> Option<Node> {
        nodes.iter().find_map(|node| match node {
            Node::Element(element) if element.reference.as_deref() == Some(reference) => Some(node.clone()),
            Node::Element(element) => find(&element.children, reference),
            Node::Text(_) => None,
        })
    }
    find(&tree.nodes, reference).map(|node| Tree {
        nodes: vec![node],
        page_url: tree.page_url.clone(),
        viewport: tree.viewport,
    })
}

fn visit_text(lines: &mut Vec<String>, text: &str, depth: usize) {
    let escaped = yaml_value(text);
    if !escaped.is_empty() {
        lines.push(format!("{}- text: {escaped}", indent(depth)));
    }
}

fn visit(lines: &mut Vec<String>, node: &Element, depth: usize) {
    let key = format!("{}- {}", indent(depth), yaml_key(&create_key(node)));
    let mut props: Vec<(&str, &str)> = Vec::new();
    if let Some(url) = &node.url {
        props.push(("url", url));
    }
    if let Some(placeholder) = &node.placeholder {
        props.push(("placeholder", placeholder));
    }
    match (&node.text, props.is_empty(), node.children.is_empty()) {
        (None, true, true) => lines.push(key),
        (Some(text), true, _) => lines.push(format!("{key}: {}", yaml_value(text))),
        _ => {
            lines.push(format!("{key}:"));
            for (name, value) in props {
                lines.push(format!("{}- /{name}: {}", indent(depth + 1), yaml_value(value)));
            }
            if let Some(text) = &node.text {
                visit_text(lines, text, depth + 1);
            } else {
                for child in &node.children {
                    match child {
                        Node::Text(text) => visit_text(lines, text, depth + 1),
                        Node::Element(element) => visit(lines, element, depth + 1),
                    }
                }
            }
        }
    }
}

fn create_key(node: &Element) -> String {
    let mut key = node.role.clone();
    if let Some(name) = node.name.as_deref().filter(|name| !name.is_empty() && name.chars().count() <= 900) {
        key.push(' ');
        key.push_str(&serde_json::to_string(name).unwrap_or_default());
    }
    match &node.checked {
        Some(Value::String(mixed)) if mixed == "mixed" => key.push_str(" [checked=mixed]"),
        Some(Value::Bool(true)) => key.push_str(" [checked]"),
        _ => {}
    }
    if node.disabled {
        key.push_str(" [disabled]");
    }
    if node.expanded {
        key.push_str(" [expanded]");
    }
    if node.active {
        key.push_str(" [active]");
    }
    match &node.invalid {
        Some(Value::String(kind)) if kind == "grammar" || kind == "spelling" => {
            let _ = write!(key, " [invalid={kind}]");
        }
        Some(Value::Bool(true)) => key.push_str(" [invalid]"),
        _ => {}
    }
    if let Some(level) = node.level.filter(|level| *level > 0) {
        let _ = write!(key, " [level={level}]");
    }
    match &node.pressed {
        Some(Value::String(mixed)) if mixed == "mixed" => key.push_str(" [pressed=mixed]"),
        Some(Value::Bool(true)) => key.push_str(" [pressed]"),
        _ => {}
    }
    if node.selected {
        key.push_str(" [selected]");
    }
    if node.aria_hidden {
        key.push_str(" [aria-hidden]");
    }
    if let Some(reference) = &node.reference {
        let _ = write!(key, " [ref={reference}]");
        if node.cursor.as_deref() == Some("pointer") {
            key.push_str(" [cursor=pointer]");
        }
    }
    if let Some(b) = node.bounds {
        let _ = write!(key, " [box={},{},{},{}]", b.x.round(), b.y.round(), b.width.round(), b.height.round());
    }
    key
}

fn indent(depth: usize) -> String {
    "  ".repeat(depth)
}

/// Playwright's `yamlEscapeKeyIfNeeded`.
fn yaml_key(text: &str) -> String {
    if needs_quotes(text) { format!("'{}'", text.replace('\'', "''")) } else { text.to_owned() }
}

/// Playwright's `yamlEscapeValueIfNeeded`.
fn yaml_value(text: &str) -> String {
    if !needs_quotes(text) {
        return text.to_owned();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c <= '\u{1f}' || ('\u{7f}'..='\u{9f}').contains(&c) => {
                let _ = write!(out, "\\x{:02x}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// JavaScript's `\s`.
fn js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200a}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}'
    )
}

/// Playwright's `yamlStringNeedsQuotes`.
fn needs_quotes(text: &str) -> bool {
    let (Some(first), Some(last)) = (text.chars().next(), text.chars().last()) else { return true };
    if js_space(first) || js_space(last) {
        return true;
    }
    let mut previous = None;
    for c in text.chars() {
        if matches!(c, '\u{0}'..='\u{8}' | '\u{b}' | '\u{c}' | '\u{e}'..='\u{1f}' | '\u{7f}'..='\u{9f}')
            || matches!(c, '{' | '}' | '`' | '\n' | '\r')
            || (c == '#' && previous.is_some_and(js_space))
        {
            return true;
        }
        previous = Some(c);
    }
    let chars: Vec<char> = text.chars().collect();
    if chars.windows(2).any(|pair| matches!(pair[0], '\n' | ':') && js_space(pair[1])) || last == ':' {
        return true;
    }
    if first == '-' || first == '[' || "&*],?!>|@\"'#%".contains(first) {
        return true;
    }
    js_numeric(text)
        || matches!(text.to_lowercase().as_str(), "y" | "n" | "yes" | "no" | "true" | "false" | "on" | "off" | "null")
}

/// True when JavaScript's `Number(text)` is not NaN.
fn js_numeric(text: &str) -> bool {
    let trimmed = text.trim_matches(|c: char| js_space(c));
    if trimmed.is_empty() {
        return true;
    }
    let lower = trimmed.to_ascii_lowercase();
    for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
        if let Some(digits) = lower.strip_prefix(prefix) {
            return !digits.is_empty() && digits.chars().all(|c| c.is_digit(radix));
        }
    }
    let unsigned = trimmed.strip_prefix(['+', '-']).unwrap_or(trimmed);
    if unsigned == "Infinity" {
        return true;
    }
    let (mantissa, exponent) = match unsigned.find(['e', 'E']) {
        Some(at) => (&unsigned[..at], Some(&unsigned[at + 1..])),
        None => (unsigned, None),
    };
    let (whole, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let digits = |part: &str| part.chars().all(|c| c.is_ascii_digit());
    let mantissa_ok = (!whole.is_empty() || !fraction.is_empty()) && digits(whole) && digits(fraction);
    let exponent_ok = exponent.is_none_or(|exp| {
        let exp = exp.strip_prefix(['+', '-']).unwrap_or(exp);
        !exp.is_empty() && digits(exp)
    });
    mantissa_ok && exponent_ok
}

/// Roles whose pointer cursor says nothing new.
fn intrinsically_clickable(role: &str) -> bool {
    matches!(
        role,
        "link"
            | "button"
            | "checkbox"
            | "radio"
            | "switch"
            | "tab"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "option"
            | "treeitem"
            | "combobox"
            | "slider"
            | "spinbutton"
    )
}

/// Elements an agent acts on: controls, inputs, frames and anything with a pointer cursor.
pub(crate) fn is_interactive(element: &Element) -> bool {
    intrinsically_clickable(&element.role)
        || matches!(element.role.as_str(), "textbox" | "searchbox" | "iframe" | "scrollbar")
        || (element.reference.is_some() && element.cursor.as_deref() == Some("pointer"))
}

fn origin_of(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    Some(format!("{scheme}://{host}"))
}

/// Drops nameless layout wrappers and redundant cursor marks; shortens same-origin links.
fn compact(nodes: Vec<Node>, origin: Option<&str>) -> Vec<Node> {
    let mut out = Vec::with_capacity(nodes.len());
    for node in nodes {
        match node {
            Node::Text(text) => out.push(Node::Text(text)),
            Node::Element(mut element) => {
                element.children = compact(std::mem::take(&mut element.children), origin);
                if intrinsically_clickable(&element.role) {
                    element.cursor = None;
                }
                if let Some(url) = element.url.take() {
                    element.url = compact_url(&url, origin);
                }
                let wrapper = element.role == "generic"
                    && element.name.as_deref().is_none_or(str::is_empty)
                    && element.cursor.as_deref() != Some("pointer");
                if wrapper {
                    match element.text.take() {
                        Some(text) => out.push(Node::Text(text)),
                        None => out.extend(element.children),
                    }
                } else {
                    out.push(Node::Element(element));
                }
            }
        }
    }
    out
}

fn compact_url(url: &str, origin: Option<&str>) -> Option<String> {
    if url.is_empty() || url == "#" || url.starts_with("javascript:") {
        return None;
    }
    let local = origin.and_then(|origin| url.strip_prefix(origin)).filter(|rest| rest.starts_with('/'));
    Some(crate::output::truncate(local.unwrap_or(url), 120).into_owned())
}

fn in_viewport(nodes: Vec<Node>, (width, height): (f64, f64)) -> Vec<Node> {
    let visible = |b: &Bounds| b.x < width && b.y < height && b.x + b.width > 0.0 && b.y + b.height > 0.0;
    let mut out = Vec::new();
    for node in nodes {
        match node {
            Node::Text(text) => out.push(Node::Text(text)),
            Node::Element(mut element) => {
                let inside = element.bounds.as_ref().is_some_and(visible);
                if element.role == "iframe" {
                    if inside {
                        out.push(Node::Element(element));
                    }
                    continue;
                }
                element.children = in_viewport(std::mem::take(&mut element.children), (width, height));
                let has_elements = element.children.iter().any(|child| matches!(child, Node::Element(_)));
                if inside || has_elements {
                    if !inside {
                        element.children.retain(|child| matches!(child, Node::Element(_)));
                        element.text = None;
                    }
                    out.push(Node::Element(element));
                }
            }
        }
    }
    out
}

/// Keeps actionable elements plus the headings and named containers that locate them.
fn interactive_only(nodes: Vec<Node>) -> Vec<Node> {
    let mut out = Vec::new();
    for node in nodes {
        let Node::Element(mut element) = node else { continue };
        let children = interactive_only(std::mem::take(&mut element.children));
        if is_interactive(&element) {
            element.children = children;
            if !intrinsically_clickable(&element.role) && element.role != "iframe" {
                element.text = element.text.map(|text| crate::output::truncate(&text, 80).into_owned());
            }
            out.push(Node::Element(element));
        } else if element.role == "heading" {
            element.children.clear();
            out.push(Node::Element(element));
            out.extend(children);
        } else if !children.is_empty() {
            let landmark = matches!(
                element.role.as_str(),
                "dialog"
                    | "alertdialog"
                    | "form"
                    | "navigation"
                    | "main"
                    | "banner"
                    | "contentinfo"
                    | "complementary"
                    | "region"
                    | "search"
                    | "menu"
                    | "menubar"
                    | "tablist"
                    | "toolbar"
                    | "listbox"
                    | "tree"
                    | "grid"
                    | "table"
                    | "row"
                    | "radiogroup"
            );
            if landmark {
                element.children = children;
                element.text = None;
                element.url = None;
                out.push(Node::Element(element));
            } else {
                out.extend(children);
            }
        }
    }
    out
}

fn strip_boxes(nodes: &mut [Node]) {
    for node in nodes {
        if let Node::Element(element) = node {
            element.bounds = None;
            strip_boxes(&mut element.children);
        }
    }
}

/// Line diff against the previous snapshot; `None` when most of it changed.
pub fn diff(old: &[String], new: &[String]) -> Option<String> {
    use similar::{ChangeTag, TextDiff};
    let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
    let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();
    let diff = TextDiff::from_slices(&old_refs, &new_refs);
    if diff.ratio() < 0.4 {
        return None;
    }
    let mut out = String::new();
    for group in diff.grouped_ops(1) {
        if !out.is_empty() {
            out.push_str("  ⋯\n");
        }
        for op in group {
            for change in diff.iter_changes(&op) {
                let sign = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                let _ = writeln!(out, "{sign} {}", change.value());
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(json: Value) -> Tree {
        Tree {
            nodes: serde_json::from_value(json).unwrap(),
            page_url: "https://example.com/start".into(),
            viewport: (800.0, 600.0),
        }
    }

    fn raw() -> Options {
        Options { raw: true, ..Options::default() }
    }

    #[test]
    fn raw_rendering_matches_playwright() {
        let t = tree(json!([{"role": "generic", "active": true, "ref": "e1", "children": [
            {"role": "heading", "name": "Sign up", "level": 1, "ref": "e2"},
            {"role": "link", "name": "Home", "ref": "e4", "cursor": "pointer", "url": "/home"},
            {"role": "textbox", "name": "Email", "ref": "e8", "placeholder": "you@x.com"},
            {"role": "checkbox", "name": "I agree", "ref": "e10", "checked": true},
            {"role": "combobox", "ref": "e11", "children": [
                {"role": "option", "name": "Free", "selected": true}, {"role": "option", "name": "Pro"}]},
            {"role": "generic", "ref": "e14", "cursor": "pointer", "text": "Clickable div"},
            {"role": "paragraph", "ref": "e15", "text": "Text: with colon"},
            "loose text"]}]));
        let expected = [
            "- generic [active] [ref=e1]:",
            "  - heading \"Sign up\" [level=1] [ref=e2]",
            "  - link \"Home\" [ref=e4] [cursor=pointer]:",
            "    - /url: /home",
            "  - textbox \"Email\" [ref=e8]:",
            "    - /placeholder: you@x.com",
            "  - checkbox \"I agree\" [checked] [ref=e10]",
            "  - combobox [ref=e11]:",
            "    - option \"Free\" [selected]",
            "    - option \"Pro\"",
            "  - generic [ref=e14] [cursor=pointer]: Clickable div",
            "  - paragraph [ref=e15]: \"Text: with colon\"",
            "  - text: loose text",
        ];
        assert_eq!(render(&t, &raw()), expected);
    }

    /// Cases computed by Playwright's own `yamlStringNeedsQuotes` and escapers (1.64.1).
    #[test]
    fn yaml_quoting_matches_playwright_oracle() {
        let cases: Vec<(String, bool, String, String)> =
            serde_json::from_str(include_str!("../tests/fixtures/playwright-yaml-quoting.json")).unwrap();
        for (text, quoted, value, key) in cases {
            assert_eq!(needs_quotes(&text), quoted, "needs_quotes({text:?})");
            assert_eq!(yaml_value(&text), value, "yaml_value({text:?})");
            assert_eq!(yaml_key(&text), key, "yaml_key({text:?})");
        }
    }

    #[test]
    fn compaction_unwraps_wrappers_and_shortens_links() {
        let t = tree(json!([{"role": "generic", "ref": "e1", "children": [
            {"role": "generic", "ref": "e2", "children": [
                {"role": "link", "name": "Docs", "ref": "e3", "cursor": "pointer", "url": "https://example.com/docs?a=1"},
                {"role": "link", "name": "Ext", "ref": "e4", "cursor": "pointer", "url": "https://other.org/x"},
                {"role": "link", "name": "JS", "ref": "e5", "cursor": "pointer", "url": "javascript:void(0)"}]},
            {"role": "generic", "ref": "e6", "text": "plain"},
            {"role": "generic", "ref": "e7", "cursor": "pointer", "text": "Click me"}]}]));
        let lines = render(&t, &Options::default());
        assert_eq!(
            lines,
            [
                "- link \"Docs\" [ref=e3]:",
                "  - /url: /docs?a=1",
                "- link \"Ext\" [ref=e4]:",
                "  - /url: https://other.org/x",
                "- link \"JS\" [ref=e5]",
                "- text: plain",
                "- generic [ref=e7] [cursor=pointer]: Click me",
            ]
        );
    }

    #[test]
    fn interactive_mode_keeps_actions_and_context() {
        let t = tree(json!([{"role": "main", "ref": "e1", "children": [
            {"role": "heading", "name": "Title", "level": 1, "ref": "e2"},
            {"role": "paragraph", "ref": "e3", "text": "long prose"},
            {"role": "list", "ref": "e4", "children": [{"role": "listitem", "ref": "e5", "children": [
                {"role": "button", "name": "Go", "ref": "e6"}]}]},
            {"role": "dialog", "name": "Confirm", "ref": "e7", "children": [{"role": "button", "name": "OK", "ref": "e8"}]}]}]));
        let lines = render(&t, &Options { interactive: true, ..Options::default() });
        assert_eq!(
            lines,
            [
                "- main [ref=e1]:",
                "  - heading \"Title\" [level=1] [ref=e2]",
                "  - button \"Go\" [ref=e6]",
                "  - dialog \"Confirm\" [ref=e7]:",
                "    - button \"OK\" [ref=e8]",
            ]
        );
    }

    #[test]
    fn viewport_mode_drops_offscreen_nodes() {
        let t = tree(json!([
            {"role": "button", "name": "Top", "ref": "e1", "box": {"x": 10, "y": 10, "width": 50, "height": 20}},
            {"role": "button", "name": "Below", "ref": "e2", "box": {"x": 10, "y": 900, "width": 50, "height": 20}}]));
        let lines = render(&t, &Options { viewport: true, ..Options::default() });
        assert_eq!(lines, ["- button \"Top\" [ref=e1]"]);
    }

    #[test]
    fn diffs_show_changes_with_context() {
        let old: Vec<String> = ["- a", "- b", "- c", "- d"].map(String::from).to_vec();
        let new: Vec<String> = ["- a", "- b", "- C", "- d"].map(String::from).to_vec();
        let diff = diff(&old, &new).unwrap();
        assert_eq!(diff, "  - b\n- - c\n+ - C\n  - d\n");
        let unrelated: Vec<String> = ["- x", "- y", "- z", "- w"].map(String::from).to_vec();
        assert!(super::diff(&old, &unrelated).is_none());
    }
}
