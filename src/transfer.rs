//! File uploads and drag and drop.
//!
//! Uploads set the files of a file input directly, or answer the file chooser an element opens.
//! Drags press at the source and move to the destination in steps, which drives mouse-based
//! drag libraries; when the page starts an HTML5 drag, Helium hands the drag data over and the
//! drop happens through `Input.dispatchDragEvent`, as in Playwright.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use tokio::sync::broadcast;

use crate::actions;
use crate::cli::{DragArgs, UploadArgs};
use crate::daemon::Cx;
use crate::page::{Arg, Element, Page, Signal, World};
use crate::undo::Undo;

/// Pause between drag steps: about one frame, so libraries that update on animation frames see
/// each step.
const STEP_PAUSE: Duration = Duration::from_millis(16);

/// How long Helium may take to hand over a drag the page started.
const DRAG_HANDOVER: Duration = Duration::from_secs(2);

/// How long actions wait for an element to become actionable (as `actions` does).
fn patience(cx: &Cx<'_>) -> Duration {
    cx.timeout.min(Duration::from_secs(10))
}

/// The next signal `pick` accepts within `timeout`, if any.
async fn next_signal<T>(
    signals: &mut broadcast::Receiver<Signal>,
    timeout: Duration,
    mut pick: impl FnMut(Signal) -> Option<T>,
) -> Option<T> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match tokio::time::timeout_at(deadline, signals.recv()).await {
            Ok(Ok(signal)) => {
                if let Some(found) = pick(signal) {
                    return Some(found);
                }
            }
            Ok(Err(broadcast::error::RecvError::Lagged(_))) => {}
            Ok(Err(broadcast::error::RecvError::Closed)) | Err(_) => return None,
        }
    }
}

/// Absolute paths of existing files, as the protocol wants them.
fn file_paths(cx: &Cx<'_>, files: &[impl AsRef<Path>]) -> Result<Vec<String>> {
    files
        .iter()
        .map(|file| {
            let path = cx.path(file.as_ref());
            if !path.is_file() {
                bail!("{} is not a file", path.display());
            }
            path.to_str().map(str::to_owned).with_context(|| format!("{} is not a UTF-8 path", path.display()))
        })
        .collect()
}

/// File names for messages.
fn names(paths: &[String]) -> String {
    let names: Vec<&str> = paths.iter().map(|path| path.rsplit('/').next().unwrap_or(path)).collect();
    names.join(", ")
}

pub async fn upload(cx: &mut Cx<'_>, args: UploadArgs) -> Result<()> {
    let paths = file_paths(cx, &args.files)?;
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let element = page.resolve(&args.target, patience(cx)).await?;
    // A file input, or the input a label points at.
    let input = page
        .call_handle(
            &element.world,
            "function(e) { const t = this.retarget(e, 'follow-label'); \
               return t && t.nodeName === 'INPUT' && t.type === 'file' ? t : null; }",
            &[Arg::Object(&element.object)],
        )
        .await?;
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    match input {
        Some(input) => {
            let multiple =
                page.call(&element.world, "function(e) { return e.multiple; }", &[Arg::Object(&input)]).await?;
            if paths.len() > 1 && multiple != Value::Bool(true) {
                bail!("{} takes one file, not {}", element.label, paths.len());
            }
            page.cdp
                .send("DOM.setFileInputFiles", json!({"files": paths, "objectId": input}), Some(&element.world.session))
                .await?;
        }
        None => choose_files(&page, &element, &paths, patience(cx)).await?,
    }
    cx.say(format!("uploaded {} to {}", names(&paths), element.label));
    actions::after(cx, &browser, &page, &mark, watch, false).await
}

/// Clicks an element that opens a file chooser and answers the chooser with `paths`.
async fn choose_files(page: &Page, element: &Element, paths: &[String], patience: Duration) -> Result<()> {
    let point = actions::actionable_point(page, element, None, false, patience).await?;
    let session = &element.world.session;
    let mut signals = page.subscribe();
    // Armed first: a tab left intercepting would never show a file chooser again.
    let intercepting = {
        let (cdp, session) = (page.cdp.clone(), Arc::clone(session));
        Undo::new(async move {
            cdp.send("Page.setInterceptFileChooserDialog", json!({"enabled": false}), Some(&session)).await.ok();
        })
    };
    page.cdp.send("Page.setInterceptFileChooserDialog", json!({"enabled": true}), Some(session)).await?;
    let answered = async {
        let human = page.human();
        crate::input::move_to(page, point, human).await?;
        crate::input::click(page, crate::cli::MouseButton::Left, 1, 0, human).await?;
        let chooser = next_signal(&mut signals, patience, |signal| match signal {
            Signal::FileChooser { backend_node_id, multiple } => Some((backend_node_id, multiple)),
            _ => None,
        })
        .await;
        let Some((node, multiple)) = chooser else {
            bail!("clicking {} opened no file chooser; target the file input itself (`hba find file`)", element.label);
        };
        let files = if paths.len() > 1 && !multiple { Vec::new() } else { paths.to_vec() };
        page.cdp.send("DOM.setFileInputFiles", json!({"files": files, "backendNodeId": node}), Some(session)).await?;
        if files.is_empty() {
            bail!("the file chooser of {} takes one file, not {}", element.label, paths.len());
        }
        Ok(())
    }
    .await;
    intercepting.finish().await;
    answered
}

/// Remembers whether the page starts an HTML5 drag (an uncancelled `dragstart`), from `hba`'s
/// world so the page cannot see the listener.
const WATCH_DRAG: &str = "function() { \
    const w = this.window; \
    if (this.hbaDrag) this.hbaDrag.stop(); \
    let event = null; \
    const listener = e => { event = e; }; \
    w.addEventListener('dragstart', listener, true); \
    this.hbaDrag = { started: () => !!event && !event.defaultPrevented, \
                     stop: () => w.removeEventListener('dragstart', listener, true) }; }";

const UNWATCH_DRAG: &str = "function() { if (this.hbaDrag) { this.hbaDrag.stop(); this.hbaDrag = null; } }";

/// Where a drag stands: the pointer is down, or the page's HTML5 drag carries `data`.
enum Drag {
    Pressed,
    Html5(Value),
}

pub async fn drag(cx: &mut Cx<'_>, args: DragArgs) -> Result<()> {
    let page = cx.page().await?;
    let browser = cx.browser().await?;
    let patience = patience(cx);
    let source = page.resolve(&args.source, patience).await?;
    let destination = page.resolve(&args.destination, patience).await?;
    let from = actions::actionable_point(&page, &source, None, false, patience).await?;
    let watch = cx.watch(&browser).await;
    let mark = page.mark();
    let session = page.session()?;
    // Its own task, so a client that hangs up does not leave the tab intercepting drags or the
    // page seeing a held button.
    let (dragging, world, target) = (Arc::clone(&page), source.world.clone(), destination.clone());
    let state = tokio::spawn(async move {
        let page = dragging;
        page.call(&world, WATCH_DRAG, &[]).await?;
        page.cdp.send("Input.setInterceptDrags", json!({"enabled": true}), Some(&session)).await?;
        let mut state = Drag::Pressed;
        let outcome = drag_to(&page, &world, &target, from, patience, &mut state).await;
        if outcome.is_err() {
            // Let go so the page is not left mid-drag.
            let (x, y) = page.mouse();
            let release = match &state {
                Drag::Html5(data) => {
                    page.input("Input.dispatchDragEvent", json!({"type": "dragCancel", "x": x, "y": y, "data": data}))
                        .await
                }
                Drag::Pressed => mouse_event(&page, "mouseReleased", (x, y), 0).await,
            };
            release.ok();
        }
        page.cdp.send("Input.setInterceptDrags", json!({"enabled": false}), Some(&session)).await.ok();
        page.call(&world, UNWATCH_DRAG, &[]).await.ok();
        outcome.map(|()| state)
    })
    .await??;
    let how = match state {
        Drag::Html5(_) => " (HTML5 drag and drop)",
        Drag::Pressed => "",
    };
    cx.say(format!("dragged {} to {}{how}", source.label, destination.label));
    actions::after(cx, &browser, &page, &mark, watch, false).await
}

/// Presses at `from`, moves to the destination in steps and drops there. Mouse events carry the
/// pressed button, so pointer-driven drag libraries follow; once the page starts an HTML5 drag,
/// the remaining steps are drag events with the data Helium handed over.
async fn drag_to(
    page: &Page,
    source: &World,
    destination: &Element,
    from: (f64, f64),
    patience: Duration,
    state: &mut Drag,
) -> Result<()> {
    let human = page.human();
    let mut signals = page.subscribe();
    crate::input::move_to(page, from, human).await?;
    mouse_event(page, "mousePressed", from, 1).await?;
    let to = actions::actionable_point(page, destination, None, false, patience).await?;
    let steps = step_count(from, to);
    for step in 1..=steps {
        let t = f64::from(step) / f64::from(steps);
        let point = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
        match state {
            Drag::Html5(data) => {
                page.input(
                    "Input.dispatchDragEvent",
                    json!({"type": "dragOver", "x": point.0, "y": point.1, "data": data}),
                )
                .await?;
            }
            Drag::Pressed if step == 1 => {
                // The page decides with the first move whether this becomes an HTML5 drag.
                mouse_event(page, "mouseMoved", point, 1).await?;
                if let Some(data) = handed_over(page, source, &mut signals).await? {
                    page.input(
                        "Input.dispatchDragEvent",
                        json!({"type": "dragEnter", "x": point.0, "y": point.1, "data": data}),
                    )
                    .await?;
                    *state = Drag::Html5(data);
                }
            }
            Drag::Pressed => {
                // Later moves go out without waiting: a page that is not drawing takes a lone
                // move only after a second, and the release delivers them in order.
                let moved = json!({"type": "mouseMoved", "x": point.0, "y": point.1, "button": "left", "buttons": 1,
                                   "clickCount": 1, "pointerType": "mouse"});
                drop(page.send_input("Input.dispatchMouseEvent", &moved)?);
            }
        }
        page.set_mouse(point);
        let pause = if human { STEP_PAUSE + Duration::from_millis(fastrand::u64(0..=12)) } else { STEP_PAUSE };
        tokio::time::sleep(pause).await;
    }
    match state {
        Drag::Html5(data) => {
            page.input("Input.dispatchDragEvent", json!({"type": "drop", "x": to.0, "y": to.1, "data": data})).await
        }
        Drag::Pressed => mouse_event(page, "mouseReleased", to, 0).await,
    }
}

/// The drag data of an HTML5 drag the page just started, if it started one.
async fn handed_over(page: &Page, source: &World, signals: &mut broadcast::Receiver<Signal>) -> Result<Option<Value>> {
    let intercepted = |signal: Signal| match signal {
        Signal::DragIntercepted(data) => Some(data),
        _ => None,
    };
    while let Ok(signal) = signals.try_recv() {
        if let Some(data) = intercepted(signal) {
            return Ok(Some(data));
        }
    }
    let started = page.call(source, "function() { return !!this.hbaDrag && this.hbaDrag.started(); }", &[]).await?;
    if started != Value::Bool(true) {
        return Ok(None);
    }
    let data = next_signal(signals, DRAG_HANDOVER, intercepted).await;
    data.map(Some).context("the page started a drag, but Helium did not hand it over")
}

/// A left-button mouse event at `point`; `buttons` is 1 while the button is held.
async fn mouse_event(page: &Page, kind: &str, point: (f64, f64), buttons: u8) -> Result<()> {
    page.input(
        "Input.dispatchMouseEvent",
        json!({"type": kind, "x": point.0, "y": point.1, "button": "left", "buttons": buttons, "clickCount": 1,
               "pointerType": "mouse"}),
    )
    .await
}

/// Steps for a drag over `from`→`to`: about one per 50 px, at least 4 so libraries with a start
/// threshold see movement before the drop, at most 10 because pages that redraw on every move
/// take a frame per step.
fn step_count(from: (f64, f64), to: (f64, f64)) -> u32 {
    let distance = (to.0 - from.0).hypot(to.1 - from.1);
    ((distance / 50.0).round() as u32).clamp(4, 10)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drag_steps_scale_with_distance() {
        assert_eq!(step_count((0.0, 0.0), (10.0, 0.0)), 4);
        assert_eq!(step_count((0.0, 0.0), (210.0, 280.0)), 7);
        assert_eq!(step_count((0.0, 0.0), (3000.0, 0.0)), 10);
    }

    #[test]
    fn file_names_drop_directories() {
        assert_eq!(names(&["/tmp/a.txt".to_owned(), "/x/y/b.png".to_owned()]), "a.txt, b.png");
    }
}
