//! Acting on the page: pointer and keyboard actions, form controls, scrolling, waiting, dialogs.
//!
//! Pointer actions follow Playwright's actionability rules: the element must be visible,
//! enabled and stable, is scrolled into view, and must be the hit target at the click point.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde::de::IgnoredAny;
use serde_json::{Value, json};

use crate::browser::Browser;
use crate::cli::{
    ClickArgs, DialogAction, DialogArgs, FillArgs, MAX_TIMEOUT, MouseAction, MouseArgs, PressArgs, ScrollArgs,
    ScrollDirection, SelectArgs, TargetArg, TypeArgs, WaitArgs,
};
use crate::daemon::Cx;
use crate::input;
use crate::page::{Arg, Element, Mark, Page};

/// How long actions wait for an element to become actionable.
fn action_timeout(cx: &Cx<'_>) -> Duration {
    cx.timeout.min(Duration::from_secs(10))
}

/// The point to click in page (main viewport) coordinates, after actionability checks.
pub async fn actionable_point(
    page: &Page,
    element: &Element,
    offset: Option<(f64, f64)>,
    force: bool,
    timeout: Duration,
) -> Result<(f64, f64)> {
    let mut scroll = json!({"objectId": element.object});
    if let Some((x, y)) = offset {
        // The offset point must come into view, not the element's center.
        scroll["rect"] = json!({"x": x, "y": y, "width": 0, "height": 0});
    }
    // Why the element is not actionable yet, reported when time runs out.
    let mut problem = None;
    let attempts = async {
        loop {
            let (reason, pause) = 'attempt: {
                if !force {
                    // Visible and enabled, then stable: the same box twice, 40 ms apart. Timing comes
                    // from the daemon because background tabs may paint at a few frames per second.
                    const STATE: &str = "async function(e) { \
                        const r = await this.checkElementStates(e, ['visible', 'enabled'], true); \
                        if (r === 'error:notconnected') return 'detached'; \
                        if (r && r.missingState) return `not ${r.missingState}`; \
                        const b = e.getBoundingClientRect(); return [b.x, b.y, b.width, b.height].join(); }";
                    let first = page.call(&element.world, STATE, &[Arg::Object(&element.object)]).await?;
                    tokio::time::sleep(Duration::from_millis(40)).await;
                    let second = page.call(&element.world, STATE, &[Arg::Object(&element.object)]).await?;
                    match (first.as_str().unwrap_or_default(), second.as_str().unwrap_or_default()) {
                        (_, "detached") | ("detached", _) => {
                            bail!("{} was removed from the page; run `hba snap` for fresh refs", element.label)
                        }
                        (_, state) if state.starts_with("not ") => {
                            break 'attempt (format!("{} is {state}", element.label), 100);
                        }
                        (a, b) if a != b => break 'attempt (format!("{} is still moving", element.label), 60),
                        _ => {}
                    }
                }
                page.renderer_call::<IgnoredAny>("DOM.scrollIntoViewIfNeeded", &scroll, &element.world.session)
                    .await
                    .ok();
                let point = match click_point(page, element, offset).await {
                    Ok(point) => point,
                    Err(error) if page.dialog().is_some() => return Err(error),
                    Err(error) => break 'attempt (format!("{} cannot be clicked: {error:#}", element.label), 100),
                };
                if force {
                    return Ok(point);
                }
                let local = to_frame(page, element, point).await?;
                let hit = page
                    .call(
                        &element.world,
                        "function(e, p) { const r = this.expectHitTarget(p, e); return r === 'done' ? 'done' : r.hitTargetDescription; }",
                        &[Arg::Object(&element.object), Arg::Value(json!({"x": local.0, "y": local.1}))],
                    )
                    .await?;
                match hit.as_str() {
                    Some("done") => return Ok(point),
                    blocker => (
                        format!(
                            "{} is covered: {} intercepts pointer events; close the overlay first or pass --force",
                            element.label,
                            blocker.unwrap_or("another element")
                        ),
                        150,
                    ),
                }
            };
            crate::trace!("actionable: {reason}");
            problem = Some(reason);
            tokio::time::sleep(Duration::from_millis(pause)).await;
        }
    };
    // A busy renderer holds one protocol call for half a minute; the action's deadline comes first.
    let outcome = tokio::time::timeout_at((Instant::now() + timeout).into(), attempts).await;
    outcome.unwrap_or_else(|_| {
        Err(anyhow!(problem.unwrap_or_else(|| {
            format!("{} did not answer within {:.0}s; the page is busy", element.label, timeout.as_secs_f32())
        })))
    })
}

#[derive(Deserialize)]
struct Quads {
    quads: Vec<Vec<f64>>,
}

/// A point inside the element's first visible content quad: its center, an explicit offset
/// from the quad's top-left corner (wherever the viewport cuts it), or a random spot in the
/// central half when input is human-like.
pub async fn click_point(page: &Page, element: &Element, offset: Option<(f64, f64)>) -> Result<(f64, f64)> {
    let quads: Quads =
        page.renderer_call("DOM.getContentQuads", json!({"objectId": element.object}), &element.world.session).await?;
    let shift = frame_offset(page, &element.world.frame, &element.world.session).await?;
    let viewport = viewport(page).await?;
    let (origin, (left, top, right, bottom)) = quads
        .quads
        .iter()
        .filter(|q| q.len() == 8)
        .map(|q| {
            let xs = [q[0], q[2], q[4], q[6]];
            let ys = [q[1], q[3], q[5], q[7]];
            let fold = |v: [f64; 4], f: fn(f64, f64) -> f64, init: f64| v.into_iter().fold(init, f);
            let (left, right) = (fold(xs, f64::min, f64::MAX) + shift.0, fold(xs, f64::max, f64::MIN) + shift.0);
            let (top, bottom) = (fold(ys, f64::min, f64::MAX) + shift.1, fold(ys, f64::max, f64::MIN) + shift.1);
            ((left, top), (left.max(0.0), top.max(0.0), right.min(viewport.0), bottom.min(viewport.1)))
        })
        .find(|(_, (l, t, r, b))| r - l >= 1.0 && b - t >= 1.0)
        .context("it has no visible area inside the viewport")?;
    if let Some((x, y)) = offset {
        return Ok((origin.0 + x, origin.1 + y));
    }
    let (width, height) = (right - left, bottom - top);
    if page.human() && width > 6.0 && height > 6.0 {
        let jitter = |size: f64| (fastrand::f64() - 0.5) * size * 0.5;
        return Ok((left + width / 2.0 + jitter(width), top + height / 2.0 + jitter(height)));
    }
    Ok((left + width / 2.0, top + height / 2.0))
}

/// CSS viewport size of the page.
pub async fn viewport(page: &Page) -> Result<(f64, f64)> {
    let session = page.session()?;
    let metrics: Value = page.renderer_call("Page.getLayoutMetrics", json!({}), &session).await?;
    let visual = &metrics["cssVisualViewport"];
    Ok((visual["clientWidth"].as_f64().unwrap_or(1e9), visual["clientHeight"].as_f64().unwrap_or(1e9)))
}

/// Offset of an out-of-process frame's viewport within the page; zero for same-process frames,
/// whose quads Chromium already reports in page coordinates.
pub async fn frame_offset(page: &Page, frame: &str, session: &str) -> Result<(f64, f64)> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Owner {
        backend_node_id: i64,
    }
    let primary = page.session()?;
    if *primary == *session {
        return Ok((0.0, 0.0));
    }
    let Some(parent) = page.frame_parent(frame) else { return Ok((0.0, 0.0)) };
    let parent_session = page.frame_session(&parent).context("the frame's parent is gone")?;
    let owner: Owner = page.renderer_call("DOM.getFrameOwner", json!({"frameId": frame}), &parent_session).await?;
    let model: Value =
        page.renderer_call("DOM.getBoxModel", json!({"backendNodeId": owner.backend_node_id}), &parent_session).await?;
    let content = model["model"]["content"].as_array().context("the frame has no box")?;
    let (x, y) = (content[0].as_f64().unwrap_or(0.0), content[1].as_f64().unwrap_or(0.0));
    let above = Box::pin(frame_offset(page, &parent, &parent_session)).await?;
    Ok((x + above.0, y + above.1))
}

/// Converts a page point into the element frame's own viewport coordinates.
async fn to_frame(page: &Page, element: &Element, point: (f64, f64)) -> Result<(f64, f64)> {
    let main = page.main_frame()?;
    if element.world.frame == main {
        return Ok(point);
    }
    let origin = page
        .call(
            &element.world,
            "function() { let x = 0, y = 0, w = this.window; \
               while (w.frameElement) { const r = w.frameElement.getBoundingClientRect(); \
                 const s = getComputedStyle(w.frameElement); \
                 x += r.left + parseFloat(s.borderLeftWidth) + parseFloat(s.paddingLeft); \
                 y += r.top + parseFloat(s.borderTopWidth) + parseFloat(s.paddingTop); w = w.parent; } \
               return [x, y]; }",
            &[],
        )
        .await?;
    let shift = frame_offset(page, &element.world.frame, &element.world.session).await?;
    let same_process = (origin[0].as_f64().unwrap_or(0.0), origin[1].as_f64().unwrap_or(0.0));
    Ok((point.0 - same_process.0 - shift.0, point.1 - same_process.1 - shift.1))
}

/// Settles after an action and reports navigation, popups, dialogs, errors and downloads.
pub async fn after(
    cx: &mut Cx<'_>,
    browser: &Browser,
    page: &Arc<Page>,
    mark: &Mark,
    watch: crate::daemon::PopupWatch,
    diff: bool,
) -> Result<()> {
    page.settle(mark, cx.timeout).await?;
    cx.adopt_popups(browser, page, watch).await?;
    let mut report = String::new();
    page.report(mark, &mut report);
    if !report.is_empty() {
        cx.say(report.trim_end());
    }
    if diff && page.is_live() && page.dialog().is_none() {
        crate::inspect::snap(
            cx,
            crate::cli::SnapArgs {
                interactive: false,
                viewport: false,
                diff: true,
                scope: None,
                depth: None,
                boxes: false,
                raw: false,
                max: 24_000,
                output: None,
            },
        )
        .await?;
    }
    Ok(())
}

pub async fn click(cx: &mut Cx<'_>, args: ClickArgs) -> Result<()> {
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let modifiers = input::modifier_mask(&args.modifiers)?;
    let element = page.resolve(&args.target, action_timeout(cx)).await?;
    let point = actionable_point(&page, &element, args.at, args.force, action_timeout(cx)).await?;
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    let human = page.human();
    input::move_to(&page, point, human).await?;
    input::click(&page, args.button, args.count, modifiers, human).await?;
    let verb = match (args.count, args.button) {
        (2, _) => "double-clicked",
        (3, _) => "triple-clicked",
        (_, crate::cli::MouseButton::Right) => "right-clicked",
        _ => "clicked",
    };
    cx.say(format!("{verb} {}", element.label));
    after(cx, &browser, &page, &mark, watch, args.diff).await
}

pub async fn hover(cx: &mut Cx<'_>, args: TargetArg) -> Result<()> {
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let element = page.resolve(&args.target, action_timeout(cx)).await?;
    let point = actionable_point(&page, &element, None, false, action_timeout(cx)).await?;
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    input::move_to(&page, point, page.human()).await?.taken().await?;
    cx.say(format!("hovering {}", element.label));
    after(cx, &browser, &page, &mark, watch, false).await
}

pub async fn focus(cx: &mut Cx<'_>, args: TargetArg) -> Result<()> {
    let page = cx.page().await?;
    let element = focus_target(cx, &page, &args.target, false).await?;
    cx.say(format!("focused {}", element.label));
    Ok(())
}

/// Resolves `target` and focuses it as Playwright does. `before_keys` also puts the caret at
/// the start of an input that was not focused yet, as Playwright's `type` and `press` do.
async fn focus_target(cx: &Cx<'_>, page: &Page, target: &str, before_keys: bool) -> Result<Element> {
    let element = page.resolve(target, action_timeout(cx)).await?;
    let result = page
        .call(
            &element.world,
            "function(e, reset) { return this.focusNode(e, reset); }",
            &[Arg::Object(&element.object), Arg::Value(json!(before_keys))],
        )
        .await?;
    if result.as_str() != Some("done") {
        bail!("could not focus {}: {result}", element.label);
    }
    Ok(element)
}

/// Reads an editable element's text the way a user sees it.
const READ_VALUE: &str = "function(e) { \
    if (e.isContentEditable) return e.innerText.replace(/\\n$/, ''); \
    return 'value' in e ? String(e.value) : e.textContent; }";

pub async fn fill(cx: &mut Cx<'_>, args: FillArgs) -> Result<()> {
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let text = args.text.replace("\r\n", "\n").replace('\r', "\n");
    let element = page.resolve(&args.target, action_timeout(cx)).await?;
    let checks = page
        .call(
            &element.world,
            "async function(e) { \
               const r = await this.checkElementStates(e, ['visible', 'enabled', 'editable'], true); \
               if (r === 'error:notconnected') return 'is detached from the page'; \
               if (r && r.missingState) return `is not ${r.missingState}`; \
               return e instanceof HTMLInputElement ? 'single' : 'multi'; }",
            &[Arg::Object(&element.object)],
        )
        .await?;
    match checks.as_str() {
        Some("single") if text.contains('\n') => {
            bail!("{} is a single-line field; it cannot hold newlines", element.label)
        }
        Some("single" | "multi") => {}
        other => bail!("{} {}", element.label, other.unwrap_or("cannot be filled")),
    }
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    let outcome = page
        .call(
            &element.world,
            "function(e, v) { return this.fill(e, v); }",
            &[Arg::Object(&element.object), Arg::Value(json!(text))],
        )
        .await?;
    match outcome.as_str() {
        Some("done") => {}
        Some("needsinput") if text.is_empty() => input::press(&page, &input::Chord::parse("Delete")?, false).await?,
        Some("needsinput") => input::insert_text(&page, &text).await?,
        other => bail!("{} could not be filled: {}", element.label, other.unwrap_or("unknown error")),
    }
    let value = page.call(&element.world, READ_VALUE, &[Arg::Object(&element.object)]).await?;
    let value = value.as_str().unwrap_or_default();
    if value == text {
        cx.say(format!("filled {}", element.label));
    } else {
        // Masks and formatters legitimately rewrite input; show the result instead of failing.
        cx.say(format!("filled {}; the field now reads {:?}", element.label, crate::output::truncate(value, 200)));
    }
    if args.submit {
        input::press(&page, &input::Chord::parse("Enter")?, page.human()).await?;
    }
    after(cx, &browser, &page, &mark, watch, args.diff).await
}

pub async fn type_text(cx: &mut Cx<'_>, args: TypeArgs) -> Result<()> {
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    if let Some(target) = &args.into {
        focus_target(cx, &page, target, true).await?;
    }
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    let human = page.human();
    input::type_text(&page, &args.text, args.delay, human).await?;
    if args.submit {
        input::press(&page, &input::Chord::parse("Enter")?, human).await?;
    }
    cx.say(format!("typed {}", crate::output::count(args.text.chars().count(), "character")));
    after(cx, &browser, &page, &mark, watch, false).await
}

pub async fn press(cx: &mut Cx<'_>, args: PressArgs) -> Result<()> {
    // Every chord is checked before the first key goes down.
    let chords = args.keys.iter().map(|chord| input::Chord::parse(chord)).collect::<Result<Vec<_>>>()?;
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    if let Some(target) = &args.into {
        focus_target(cx, &page, target, true).await?;
    }
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    let human = page.human();
    for chord in &chords {
        input::press(&page, chord, human).await?;
    }
    cx.say(format!("pressed {}", args.keys.join(" ")));
    after(cx, &browser, &page, &mark, watch, args.diff).await
}

pub async fn set_checked(cx: &mut Cx<'_>, args: TargetArg, checked: bool) -> Result<()> {
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let element = page.resolve(&args.target, action_timeout(cx)).await?;
    let state = |page: Arc<Page>, element: Element| async move {
        let value = page
            .call(
                &element.world,
                "function(e) { return this.elementState(e, 'checked', true); }",
                &[Arg::Object(&element.object)],
            )
            .await?;
        if value["received"] == "error:notconnected" {
            bail!("{} was removed from the page", element.label);
        }
        Ok::<bool, anyhow::Error>(value["matches"].as_bool().unwrap_or(false))
    };
    let verb = if checked { "checked" } else { "unchecked" };
    if state(Arc::clone(&page), element.clone()).await? == checked {
        cx.say(format!("{} was already {verb}", element.label));
        return Ok(());
    }
    let point = actionable_point(&page, &element, None, false, action_timeout(cx)).await?;
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    let human = page.human();
    input::move_to(&page, point, human).await?;
    input::click(&page, crate::cli::MouseButton::Left, 1, 0, human).await?;
    if state(Arc::clone(&page), element.clone()).await? != checked {
        bail!("clicking {} did not make it {verb}", element.label);
    }
    cx.say(format!("{verb} {}", element.label));
    after(cx, &browser, &page, &mark, watch, false).await
}

pub async fn select(cx: &mut Cx<'_>, args: SelectArgs) -> Result<()> {
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let element = page.resolve(&args.target, action_timeout(cx)).await?;
    let options: Vec<Value> = args.values.iter().map(|value| json!({"valueOrLabel": value})).collect();
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    let selected = page
        .call(
            &element.world,
            "function(e, o) { return this.selectOptions(e, o); }",
            &[Arg::Object(&element.object), Arg::Value(Value::Array(options))],
        )
        .await?;
    match &selected {
        Value::Array(values) => {
            let values: Vec<String> = values.iter().map(|v| v.as_str().unwrap_or_default().to_owned()).collect();
            cx.say(format!("selected {} in {}", values.join(", "), element.label));
        }
        Value::String(error) if error == "error:optionsnotfound" => {
            bail!("{} has no option matching {}", element.label, args.values.join(", "))
        }
        Value::String(error) if error == "error:optionnotenabled" => {
            bail!("that option of {} is disabled", element.label)
        }
        other => bail!("could not select in {}: {other}", element.label),
    }
    after(cx, &browser, &page, &mark, watch, false).await
}

pub async fn scroll(cx: &mut Cx<'_>, args: ScrollArgs) -> Result<()> {
    let page = cx.page().await?;
    let element = match &args.element {
        Some(target) => Some(page.resolve(target, action_timeout(cx)).await?),
        None => None,
    };
    if args.into_view {
        let element = element.context("--into-view needs --element")?;
        page.renderer_call::<IgnoredAny>(
            "DOM.scrollIntoViewIfNeeded",
            json!({"objectId": element.object}),
            &element.world.session,
        )
        .await?;
        cx.say(format!("scrolled {} into view", element.label));
        return Ok(());
    }
    let world = match &element {
        Some(element) => element.world.clone(),
        None => page.main_world().await?,
    };
    let object = element.as_ref().map(|e| e.object.as_str());
    let started = Instant::now();
    match args.direction {
        ScrollDirection::Top | ScrollDirection::Bottom => {
            let top = args.direction == ScrollDirection::Top;
            page.call(
                &world,
                "function(e, top) { const s = e || this.document.scrollingElement || this.document.documentElement; \
                   s.scrollTo({top: top ? 0 : s.scrollHeight, behavior: 'instant'}); }",
                &[scroll_target(object), Arg::Value(json!(top))],
            )
            .await?;
        }
        direction => {
            let (width, height) = viewport(&page).await?;
            let amount = args.amount.unwrap_or(match direction {
                ScrollDirection::Left | ScrollDirection::Right => width * 0.8,
                _ => height * 0.8,
            });
            let delta = match direction {
                ScrollDirection::Up => (0.0, -amount),
                ScrollDirection::Down => (0.0, amount),
                ScrollDirection::Left => (-amount, 0.0),
                _ => (amount, 0.0),
            };
            let before = page.call(&world, SCROLL_POSITION, &[scroll_target(object)]).await?;
            // The wheel is real input, but it only applies when the page paints and goes to the
            // innermost scroller under the pointer. Otherwise the target scrolls directly.
            let at = if let Some(element) = &element {
                click_point(&page, element, None).await.ok()
            } else {
                let point = page
                    .call(&world, PAGE_WHEEL_POINT, &[Arg::Value(json!(delta.0)), Arg::Value(json!(delta.1))])
                    .await?;
                serde_json::from_value::<(f64, f64)>(point).ok()
            };
            let mut moved = false;
            // A window that is not on screen scrolls nothing by wheel and takes seconds per event.
            let shown = match cx.browser().await?.endpoint.pid {
                Some(pid) => crate::niri::shows_process(pid).await != Some(false),
                None => true,
            };
            if let Some(at) = at
                && shown
                && painting(&page, &world).await
            {
                let human = page.human();
                input::move_to(&page, at, human).await?;
                input::wheel(&page, at, delta, human).await?;
                let deadline = Instant::now() + Duration::from_millis(500);
                while !moved && Instant::now() < deadline {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    moved = page.call(&world, SCROLL_POSITION, &[scroll_target(object)]).await? != before;
                }
            }
            // A painting page applies a wheel within a few frames; one that didn't (covered or
            // off-screen element, non-scrolling target) gets a direct scroll.
            if !moved {
                page.call(
                    &world,
                    "function(e, dx, dy) { const s = e || this.document.scrollingElement || this.document.documentElement; \
                       s.scrollBy({left: dx, top: dy, behavior: 'instant'}); }",
                    &[scroll_target(object), Arg::Value(json!(delta.0)), Arg::Value(json!(delta.1))],
                )
                .await?;
            }
        }
    }
    page.quiet(started, Duration::from_secs(1)).await;
    let now = page.call(&world, SCROLL_POSITION, &[scroll_target(object)]).await?;
    let n = |i: usize| now[i].as_i64().unwrap_or(0);
    cx.say(format!("scroll x={}/{} y={}/{}", n(0), n(2).max(0), n(1), n(3).max(0)));
    Ok(())
}

/// `[left, top, max left, max top]` of an element, or of the page for `null`.
const SCROLL_POSITION: &str = "function(e) { const s = e || this.document.scrollingElement || this.document.documentElement; \
    return [Math.round(s.scrollLeft), Math.round(s.scrollTop), s.scrollWidth - s.clientWidth, s.scrollHeight - s.clientHeight]; }";

/// The scroll target argument: an element, or `null` for the page.
fn scroll_target(object: Option<&str>) -> Arg<'_> {
    object.map_or(Arg::Value(Value::Null), Arg::Object)
}

/// A viewport point, as `[x, y]`, where a wheel scrolls the page itself: no inner scroller that
/// can move in the wheel's direction and no iframe under it. `null` when there is none.
const PAGE_WHEEL_POINT: &str = "function(dx, dy) { \
    const doc = this.document, win = doc.defaultView, root = doc.scrollingElement || doc.documentElement; \
    const deep = (x, y) => { let e = doc.elementFromPoint(x, y); \
      while (e && e.shadowRoot) { const inner = e.shadowRoot.elementFromPoint(x, y); if (!inner || inner === e) break; e = inner; } \
      return e; }; \
    const up = e => e.assignedSlot || e.parentElement || (e.getRootNode() instanceof ShadowRoot ? e.getRootNode().host : null); \
    const scrolls = e => { const s = win.getComputedStyle(e); \
      if (!/(auto|scroll|overlay)/.test(dy ? s.overflowY : s.overflowX)) return false; \
      if (dy) return dy > 0 ? e.scrollTop + e.clientHeight < e.scrollHeight - 1 : e.scrollTop > 0; \
      return dx > 0 ? e.scrollLeft + e.clientWidth < e.scrollWidth - 1 : e.scrollLeft > 0; }; \
    for (const [fx, fy] of [[.5, .5], [.5, .25], [.5, .75], [.25, .5], [.75, .5], [.1, .5], [.9, .5], [.5, .1], [.5, .9]]) { \
      const x = Math.round(win.innerWidth * fx), y = Math.round(win.innerHeight * fy); \
      let e = deep(x, y); \
      if (!e || e.tagName === 'IFRAME' || e.tagName === 'FRAME' || e.tagName === 'EMBED' || e.tagName === 'OBJECT') continue; \
      while (e && e !== root && e !== doc.body && !scrolls(e)) e = up(e); \
      if (!e || e === root || e === doc.body) return [x, y]; } \
    return null; }";

/// Whether the page is painting frames: input that waits for a frame (wheel scrolling, mouse
/// moves) is slow or lost otherwise, e.g. in a minimized window.
async fn painting(page: &Page, world: &crate::page::World) -> bool {
    let frame = page.call(world, "function() { return new Promise(r => requestAnimationFrame(() => r(true))); }", &[]);
    matches!(tokio::time::timeout(Duration::from_millis(150), frame).await, Ok(Ok(Value::Bool(true))))
}

pub async fn mouse(cx: &mut Cx<'_>, args: MouseArgs) -> Result<()> {
    let page = cx.page().await?;
    let point = (args.x, args.y);
    let human = page.human();
    match args.action {
        MouseAction::Move => input::move_to(&page, point, human).await?.taken().await?,
        MouseAction::Down => {
            input::move_to(&page, point, human).await?;
            input::button(&page, args.button, true).await?;
        }
        MouseAction::Up => {
            input::move_to(&page, point, human).await?;
            input::button(&page, args.button, false).await?;
        }
        MouseAction::Click => {
            input::move_to(&page, point, human).await?;
            input::click(&page, args.button, 1, 0, human).await?;
        }
        MouseAction::Wheel => input::wheel(&page, point, (0.0, args.delta), human).await?,
    }
    cx.say(format!("mouse {:?} at {},{}", args.action, args.x, args.y).to_lowercase());
    Ok(())
}

pub async fn wait(cx: &mut Cx<'_>, args: WaitArgs) -> Result<()> {
    if let Some(ms) = args.what.as_deref().and_then(|what| what.parse::<u64>().ok()) {
        if u128::from(ms) > MAX_TIMEOUT.as_millis() {
            bail!("wait at most {} ms (a day)", MAX_TIMEOUT.as_millis());
        }
        tokio::time::sleep(Duration::from_millis(ms)).await;
        cx.say(format!("waited {ms} ms"));
        return Ok(());
    }
    let page = cx.page().await?;
    let deadline = Instant::now() + cx.timeout;
    let started = Instant::now();
    if let Some(state) = args.load {
        page.wait_load(state, cx.timeout).await?;
    } else if let Some(target) = &args.what {
        let gone = args.gone;
        loop {
            // A poll that outlasts the deadline (a busy renderer) counts as not yet.
            match tokio::time::timeout_at(deadline.into(), visible(&page, target)).await {
                Ok(Ok(present)) if present != gone => break,
                Ok(Ok(_)) | Err(_) => {}
                // Between two documents the element is neither there nor gone yet.
                Ok(Err(error)) if crate::inspect::retryable(&error) => {}
                Ok(Err(error)) => return Err(error),
            }
            if Instant::now() >= deadline {
                bail!(
                    "{target} did not {} within {:.0}s",
                    if gone { "go away" } else { "appear" },
                    cx.timeout.as_secs_f32()
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else if args.text.is_some() || args.url.is_some() || args.until.is_some() {
        loop {
            match tokio::time::timeout_at(deadline.into(), holds(&page, &args)).await {
                Ok(Ok(true)) => break,
                Ok(Ok(false)) | Err(_) => {}
                // A navigation during the poll: the next one asks the new document.
                Ok(Err(error)) if crate::inspect::retryable(&error) => {}
                Ok(Err(error)) => return Err(error),
            }
            if Instant::now() >= deadline {
                bail!("the condition did not hold within {:.0}s", cx.timeout.as_secs_f32());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    } else {
        bail!("say what to wait for: milliseconds, an element, --text, --url, --load or --until");
    }
    cx.say(format!("ok after {} ms", started.elapsed().as_millis()));
    Ok(())
}

/// Whether the `--text`, `--url` or `--until` condition of a wait holds now.
async fn holds(page: &Page, args: &WaitArgs) -> Result<bool> {
    if let Some(text) = &args.text {
        let world = page.main_world().await?;
        let shown = page
            .call(
                &world,
                "function(t) { return !!this.document.body && this.document.body.innerText.includes(t); }",
                &[Arg::Value(json!(text))],
            )
            .await?;
        Ok(shown.as_bool().unwrap_or(false))
    } else if let Some(url) = &args.url {
        Ok(url_matches(&page.url(), url))
    } else {
        let expression = args.until.as_deref().unwrap_or("true");
        Ok(truthy(&page.evaluate(expression, false).await?))
    }
}

/// Whether `target` is on the page and visible.
async fn visible(page: &Page, target: &str) -> Result<bool> {
    let element = match page.resolve(target, Duration::ZERO).await {
        Ok(element) => element,
        Err(error) if crate::inspect::not_found(&error) => return Ok(false),
        Err(error) => return Err(error),
    };
    let visible = page
        .call(
            &element.world,
            "function(e) { return this.elementState(e, 'visible', true).matches; }",
            &[Arg::Object(&element.object)],
        )
        .await?;
    Ok(visible.as_bool().unwrap_or(false))
}

/// Substring match, or a whole-URL glob when the pattern contains `*`.
fn url_matches(url: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return url.contains(pattern);
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    let Some(mut rest) = url.strip_prefix(first) else { return false };
    for middle in &parts[1..parts.len() - 1] {
        match rest.find(middle) {
            Some(at) => rest = &rest[at + middle.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}

pub async fn dialog(cx: &mut Cx<'_>, args: DialogArgs) -> Result<()> {
    let page = cx.page_with_dialog().await?;
    let Some(action) = args.action else {
        match page.dialog() {
            Some(dialog) => {
                let prompt = if dialog.kind == "prompt" {
                    format!(" (default {:?})", dialog.default_prompt)
                } else {
                    String::new()
                };
                cx.say(format!("{} {:?}{prompt}", dialog.kind, dialog.message));
            }
            None => cx.say("no dialog"),
        }
        return Ok(());
    };
    let browser = cx.browser().await?;
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    let accept = action == DialogAction::Accept;
    let dialog = page.handle_dialog(accept, args.text.as_deref()).await?;
    cx.say(format!("{} {} {:?}", if accept { "accepted" } else { "dismissed" }, dialog.kind, dialog.message));
    after(cx, &browser, &page, &mark, watch, false).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_patterns_match() {
        assert!(url_matches("https://a.com/checkout/done", "checkout"));
        assert!(!url_matches("https://a.com/cart", "checkout"));
        assert!(url_matches("https://a.com/x/done", "https://a.com/*/done"));
        assert!(url_matches("https://a.com/x/y/done", "*/done"));
        assert!(!url_matches("https://a.com/x/done?q", "*/done"));
        assert!(!url_matches("https://b.com/x/done", "https://a.com/*"));
    }

    #[test]
    fn truthiness_follows_javascript() {
        assert!(!truthy(&Value::Null) && !truthy(&json!(0)) && !truthy(&json!("")) && !truthy(&json!(false)));
        assert!(truthy(&json!(1)) && truthy(&json!("x")) && truthy(&json!([])) && truthy(&json!({})));
    }
}
