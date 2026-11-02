//! Screenshots (viewport, full page, element, annotated with refs) and PDFs of the page.
//!
//! Screenshots are taken in CSS pixels whatever the screen's pixel ratio, so images stay small
//! and coordinates in them match the page's. Ref labels are drawn onto the captured image, not
//! into the page.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt as _;

use crate::cdp::Cdp;
use crate::cli::{PdfArgs, ShotArgs};
use crate::daemon::Cx;
use crate::page::{Arg, Element, Page, World};
use crate::snapshot::{self, Node};

/// Longest wait for the page to paint before a capture. Background tabs may paint about once a
/// second; the capture itself asks for a frame, so this only makes it show the current DOM.
const FRAME_WAIT: Duration = Duration::from_millis(1000);

/// A view grab takes up to about a second while Helium's window is hidden; one unanswered for
/// longer is asked for again.
const GRAB_AGAIN: Duration = Duration::from_millis(1500);

/// A viewport capture unanswered this long is asked for again.
const ASK_AGAIN: Duration = Duration::from_secs(1);

/// A viewport capture unanswered this long falls back to grabbing the view.
const SURFACE_WAIT: Duration = Duration::from_secs(4);

/// A capture beyond the viewport unanswered this long is asked for again; large pages take time.
const BEYOND_AGAIN: Duration = Duration::from_secs(3);

/// Bytes asked for per `IO.read` of a protocol stream (PDFs, traces).
const STREAM_CHUNK: u32 = 4 << 20;

/// Image format of a screenshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Format {
    Jpeg,
    Png,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Format::Jpeg => "jpeg",
            Format::Png => "png",
        }
    }

    fn extension(self) -> &'static str {
        match self {
            Format::Jpeg => "jpg",
            Format::Png => "png",
        }
    }
}

/// PNG for `--png` or a `.png` path, JPEG otherwise; `--png` with a JPEG path is a mistake.
fn format_for(path: Option<&Path>, png: bool) -> Result<Format> {
    let extension = path.and_then(Path::extension).and_then(|ext| ext.to_str()).map(str::to_ascii_lowercase);
    let jpeg_path = matches!(extension.as_deref(), Some("jpg" | "jpeg"));
    if png && jpeg_path {
        bail!("--png conflicts with the .jpg path; name the file .png");
    }
    Ok(if png || extension.as_deref() == Some("png") { Format::Png } else { Format::Jpeg })
}

/// Pixel size of a PNG or JPEG image, read from its header.
fn image_size(bytes: &[u8]) -> Option<(u32, u32)> {
    let be16 = |at: usize| Some(u32::from(u16::from_be_bytes(bytes.get(at..at + 2)?.try_into().ok()?)));
    let be32 = |at: usize| Some(u32::from_be_bytes(bytes.get(at..at + 4)?.try_into().ok()?));
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Some((be32(16)?, be32(20)?));
    }
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    // Walk the JPEG segments to the first start-of-frame marker.
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            return None;
        }
        let marker = bytes[at + 1];
        let length = be16(at + 2)? as usize;
        let start_of_frame = matches!(marker, 0xC0..=0xCF) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if start_of_frame {
            return Some((be16(at + 7)?, be16(at + 5)?));
        }
        at += 2 + length;
    }
    None
}

/// Paper name and size in inches (Playwright's sizes).
fn paper(format: &str) -> Result<(&'static str, f64, f64)> {
    Ok(match format.to_ascii_lowercase().as_str() {
        "a4" => ("A4", 8.27, 11.7),
        "a3" => ("A3", 11.7, 16.54),
        "letter" => ("Letter", 8.5, 11.0),
        "legal" => ("Legal", 8.5, 14.0),
        "tabloid" => ("Tabloid", 11.0, 17.0),
        _ => bail!("unknown paper format {format:?}; use A4, Letter, Legal, A3 or Tabloid"),
    })
}

#[derive(Debug, Deserialize)]
struct LayoutMetrics {
    #[serde(rename = "cssLayoutViewport")]
    layout: Viewport,
    #[serde(rename = "cssVisualViewport")]
    visual: Viewport,
    #[serde(rename = "cssContentSize")]
    content: Size,
}

/// `Page.captureScreenshot` result.
#[derive(Deserialize)]
struct Captured {
    data: String,
}

/// `Page.printToPDF` result with `ReturnAsStream`.
#[derive(Deserialize)]
struct Printed {
    stream: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Viewport {
    page_x: f64,
    page_y: f64,
    client_width: f64,
    client_height: f64,
    #[serde(default = "unit_scale")]
    scale: f64,
}

fn unit_scale() -> f64 {
    1.0
}

#[derive(Debug, Deserialize)]
struct Size {
    width: f64,
    height: f64,
}

/// A capture region in document CSS pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Clip {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    /// Output pixels per CSS pixel times the device pixel ratio (Chromium multiplies them).
    scale: f64,
}

impl Clip {
    /// The smallest whole-pixel region around `left, top, right, bottom`, kept on the document.
    fn enclosing(left: f64, top: f64, right: f64, bottom: f64, scale: f64) -> Self {
        let (x, y) = (left.floor().max(0.0), top.floor().max(0.0));
        Clip { x, y, width: right.ceil() - x, height: bottom.ceil() - y, scale }
    }

    fn inside(&self, viewport: &Viewport) -> bool {
        self.x >= viewport.page_x
            && self.y >= viewport.page_y
            && self.x + self.width <= viewport.page_x + viewport.client_width
            && self.y + self.height <= viewport.page_y + viewport.client_height
    }
}

/// A rectangle in CSS or image pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

/// An interactive element's box in CSS pixels of the main frame's viewport, and its ref.
#[derive(Debug, Clone, PartialEq)]
struct Label {
    reference: String,
    rect: Rect,
}

/// How a captured image becomes the screenshot.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Crop {
    /// The part of the image to keep, in its pixels.
    source: Rect,
    /// The part of the requested clip it shows, in CSS pixels relative to the visual viewport.
    shown: Rect,
    /// Pixel size of the screenshot.
    width: u32,
    height: u32,
}

/// Cuts `clip` (CSS pixels relative to the visual viewport) out of a `grab` of the whole view
/// (its pixel size), one pixel per CSS pixel. The grab includes the vertical scrollbar, so the
/// scale comes from `view_width`, `innerWidth`, which includes it too.
fn crop(grab: (u32, u32), view_width: f64, clip: Rect) -> Crop {
    let scale = f64::from(grab.0) / view_width;
    let (left, top) = (clip.x.max(0.0), clip.y.max(0.0));
    let width = (clip.width - (left - clip.x)).min(view_width - left).max(1.0);
    let height = (clip.height - (top - clip.y)).min(f64::from(grab.1) / scale - top).max(1.0);
    Crop {
        source: Rect { x: left * scale, y: top * scale, width: width * scale, height: height * scale },
        shown: Rect { x: left, y: top, width, height },
        width: width.round() as u32,
        height: height.round() as u32,
    }
}

/// `labels` on an image `size` pixels large whose top-left corner is `origin` (viewport CSS
/// pixels), at `scale` pixels per CSS pixel: `[ref, x, y, width, height]` in its pixels. Labels
/// of elements outside the image are left out.
fn place(labels: &[Label], origin: (f64, f64), scale: f64, size: (u32, u32)) -> Vec<Value> {
    labels
        .iter()
        .filter_map(|label| {
            let (x, y) = ((label.rect.x - origin.0) * scale, (label.rect.y - origin.1) * scale);
            let (width, height) = (label.rect.width * scale, label.rect.height * scale);
            let inside = x < f64::from(size.0) && y < f64::from(size.1) && x + width > 0.0 && y + height > 0.0;
            inside.then(|| json!([label.reference, x, y, width, height]))
        })
        .collect()
}

/// What a screenshot shows and how it is encoded.
struct Request<'a> {
    element: Option<&'a Element>,
    full: bool,
    format: Format,
    quality: u8,
    timeout: Duration,
    /// Helium's process, whose window niri reports as shown or not.
    browser_pid: Option<u32>,
    /// Ref labels to draw onto the image.
    labels: &'a [Label],
}

pub async fn shot(cx: &mut Cx<'_>, args: ShotArgs) -> Result<()> {
    if args.full && args.element.is_some() {
        bail!("--full and --element exclude each other; pick one");
    }
    let format = format_for(args.path.as_deref(), args.png)?;
    let path = match &args.path {
        Some(path) => cx.path(path),
        None => cx.artifact("shot", format.extension())?,
    };
    let page = cx.page().await?;
    let element = match &args.element {
        Some(target) => {
            let element = page.resolve(target, cx.timeout.min(Duration::from_secs(10))).await?;
            page.guarded(page.cdp.send(
                "DOM.scrollIntoViewIfNeeded",
                json!({"objectId": element.object}),
                Some(&element.world.session),
            ))
            .await
            .ok();
            Some(element)
        }
        None => None,
    };
    let labels = if args.annotate { labels(&page).await? } else { Vec::new() };
    let browser_pid = cx.browser().await?.endpoint.pid;
    let request = Request {
        element: element.as_ref(),
        full: args.full,
        format,
        quality: args.quality,
        timeout: cx.timeout,
        browser_pid,
        labels: &labels,
    };
    let (bytes, labelled) = take(&page, &request).await?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, &bytes).with_context(|| format!("writing {}", path.display()))?;
    let (width, height) = image_size(&bytes).unwrap_or_default();
    cx.say(format!("{} ({width}x{height}, {})", path.display(), crate::output::bytes(bytes.len() as u64)));
    if args.annotate {
        cx.say(format!("{labelled} elements labelled with their refs"));
    }
    Ok(())
}

/// Captures the viewport, the full page or one element with the request's labels drawn on;
/// returns the image and how many labels it shows.
async fn take(page: &Page, request: &Request<'_>) -> Result<(Vec<u8>, usize)> {
    fresh_frame(page).await?;
    let session = page.session()?;
    let world = page.main_world().await?;
    let (metrics, view) = tokio::join!(
        page.cdp.call::<LayoutMetrics>("Page.getLayoutMetrics", json!({}), Some(&session)),
        page.call(
            &world,
            "function() { const w = this.window; return [w.innerWidth, w.innerHeight, w.devicePixelRatio]; }",
            &[]
        ),
    );
    let (metrics, view) = (metrics?, view?);
    // The view's size as `VIEW_SIZE` reports it, to wait for after a capture that resizes it.
    let size = json!([view[0], view[1]]);
    let ratio = view[2].as_f64().filter(|r| *r > 0.0).unwrap_or(1.0);
    let visual = &metrics.visual;
    let view_width = view[0].as_f64().filter(|w| *w > 0.0).unwrap_or(visual.client_width);
    let clip = match request.element {
        Some(element) => element_clip(page, element, &metrics.layout, ratio).await?,
        None if request.full => {
            Clip { x: 0.0, y: 0.0, width: metrics.content.width, height: metrics.content.height, scale: 1.0 / ratio }
        }
        None => Clip {
            x: visual.page_x,
            y: visual.page_y,
            width: visual.client_width,
            height: visual.client_height,
            scale: visual.scale / ratio,
        },
    };
    let beyond = !clip.inside(visual);
    // Captures come from the compositor surface, which Helium draws only while its window is on
    // screen. Hidden (another workspace, minimized), only the view can be grabbed.
    let shown = match request.browser_pid {
        Some(pid) => crate::niri::shows_process(pid).await,
        None => None,
    };
    let hidden = shown == Some(false);
    if hidden && beyond {
        bail!(NOT_SHOWN);
    }
    if beyond && shown.is_none() {
        // Whether this tab's window is drawn is not known (several Helium windows, or no niri),
        // and a capture beyond the viewport would wait the whole timeout for a surface that is
        // not drawn, so a viewport capture checks first.
        surface(page, &session, &glance(), ASK_AGAIN, SURFACE_WAIT).await.context(NOT_SHOWN)?;
    }
    let annotate = !request.labels.is_empty();
    let wanted = Rect { x: clip.x - visual.page_x, y: clip.y - visual.page_y, width: clip.width, height: clip.height };
    let (image, cut) = if beyond {
        // Only a clipped capture renders beyond the viewport. Labels go onto a lossless one,
        // which is then encoded once as requested.
        let format = if annotate { Format::Png } else { request.format };
        let mut params = json!({
            "format": format.name(),
            "clip": {"x": clip.x, "y": clip.y, "width": clip.width, "height": clip.height, "scale": clip.scale},
            "captureBeyondViewport": true,
        });
        if format == Format::Jpeg {
            params["quality"] = json!(request.quality);
        }
        let raw = surface(page, &session, &params, BEYOND_AGAIN, request.timeout).await;
        // The view is put back to its size before it is laid out again at that size.
        if raw.is_err() {
            restore_size(page, &world, &session, &size).await;
        }
        relayout(page, &world, &size).await;
        let data = captured(&raw.context("capturing beyond the viewport")?)?;
        if !annotate {
            return Ok((decode(&data)?, 0));
        }
        let size = png_size(&data).context("the capture is not a PNG")?;
        let whole = Rect { x: 0.0, y: 0.0, width: f64::from(size.0), height: f64::from(size.1) };
        (data, Crop { source: whole, shown: wanted, width: size.0, height: size.1 })
    } else {
        // The whole view, cut out here: for a clipped capture Chromium resizes the view to the
        // clip and back, which can leave the page without its scrollbar.
        let surfaced = if hidden {
            None
        } else {
            let plain = json!({"format": "png", "optimizeForSpeed": true});
            let raw = surface(page, &session, &plain, ASK_AGAIN, SURFACE_WAIT).await.ok();
            // With device metrics emulated, Chromium still resizes the view for any capture.
            if page.emulation.lock().expect("emulation lock").emulates_metrics() {
                relayout(page, &world, &size).await;
            }
            raw.as_deref().map(captured).transpose()?
        };
        let view = if let Some(view) = surfaced {
            view
        } else {
            // Hidden in a way niri did not report, or no niri.
            let device = (visual.client_width * ratio, visual.client_height * ratio);
            grab_view(page, &session, device, request.timeout).await?
        };
        let size = png_size(&view).context("the capture is not a PNG")?;
        (view, crop(size, view_width, wanted))
    };
    // Labels are measured from the layout viewport, the clip from the visual one.
    let origin =
        (cut.shown.x + visual.page_x - metrics.layout.page_x, cut.shown.y + visual.page_y - metrics.layout.page_y);
    let boxes = place(request.labels, origin, f64::from(cut.width) / cut.shown.width, (cut.width, cut.height));
    let labelled = boxes.len();
    let rendered = render(page, &world, image, &cut, boxes, request).await?;
    Ok((decode(&rendered)?, labelled))
}

/// Lays the page's root out again once the view is back to `size` (`[innerWidth, innerHeight]`).
/// After a capture that resized the view, Chromium can leave the root without the scrollbar it
/// needs until something changes its layout. A generated element added by a style sheet and
/// dropped again within one task does that without touching the DOM. The sheets are copied
/// first: `adoptedStyleSheets` is a live array, so putting it back as read would keep the sheet.
async fn relayout(page: &Page, world: &World, size: &Value) {
    page.call(
        world,
        "async function(width, height) { \
           const w = this.window, d = this.document; \
           for (let i = 0; i < 40 && (w.innerWidth !== width || w.innerHeight !== height); i++) \
             await new Promise(resolve => w.setTimeout(resolve, 25)); \
           await new Promise(resolve => { w.requestAnimationFrame(resolve); w.setTimeout(resolve, 100); }); \
           const root = d.documentElement; \
           if (!root) return; \
           const sheet = new CSSStyleSheet(); \
           sheet.replaceSync(':root::after { content: \"\"; display: block; position: absolute }'); \
           const sheets = [...d.adoptedStyleSheets]; \
           d.adoptedStyleSheets = [...sheets, sheet]; \
           root.offsetWidth; \
           d.adoptedStyleSheets = sheets; }",
        &[Arg::Value(size[0].clone()), Arg::Value(size[1].clone())],
    )
    .await
    .ok();
}

/// What [`take`] says when only the viewport can be captured.
const NOT_SHOWN: &str = "Helium's window is not on screen (minimized or on another workspace), so only what fits \
                         in the viewport can be captured; show the window, or capture the viewport (`hba scroll` \
                         moves it)";

fn decode(data: &str) -> Result<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(data).context("decoding the screenshot")
}

/// The base64 image of a `Page.captureScreenshot` answer.
fn captured(raw: &RawValue) -> Result<String> {
    Ok(serde_json::from_str::<Captured>(raw.get()).context("decoding the screenshot")?.data)
}

/// The cheapest capture of the view: a JPEG at quality 1, whose image is not wanted.
fn glance() -> Value {
    json!({"format": "jpeg", "quality": 1, "optimizeForSpeed": true})
}

/// Pixel size of a base64 PNG, from the header in its first 32 characters.
fn png_size(data: &str) -> Option<(u32, u32)> {
    let head = base64::engine::general_purpose::STANDARD.decode(data.get(..32)?).ok()?;
    image_size(&head).filter(|_| head.starts_with(b"\x89PNG"))
}

/// `[innerWidth, innerHeight]` of the page.
const VIEW_SIZE: &str = "function() { return [this.window.innerWidth, this.window.innerHeight]; }";

/// A surface capture. A tab that was not drawing may hold the request until another capture
/// arrives, so while it is unanswered a plain capture of the view goes out every `again`. Asking
/// for the same capture again would not do: Chromium restores each clipped capture's temporary
/// settings when it ends, so the later one would put back the earlier one's (hidden scrollbars,
/// an emulated view size). A capture without clip changes and restores nothing.
async fn surface(page: &Page, session: &str, params: &Value, again: Duration, wait: Duration) -> Result<Box<RawValue>> {
    let capture = page.cdp.raw_within("Page.captureScreenshot", params, Some(session), wait);
    tokio::pin!(capture);
    let wake = glance();
    loop {
        tokio::select! {
            answer = &mut capture => return answer,
            () = tokio::time::sleep(again) => {
                // Only sent; its image is not wanted.
                drop(page.cdp.start("Page.captureScreenshot", &wake, Some(session), wait));
            }
        }
    }
}

/// A capture beyond the viewport resizes the page while it runs; one that never answered leaves
/// it resized. Puts the page back to `size` (and the tab's own emulation back on).
async fn restore_size(page: &Page, world: &World, session: &str, size: &Value) {
    page.cdp.send("Emulation.clearDeviceMetricsOverride", json!({}), Some(session)).await.ok();
    crate::emulate::on_session(page, &Arc::from(session), true).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline && page.call(world, VIEW_SIZE, &[]).await.ok().as_ref() != Some(size) {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Cuts `cut` out of a base64 PNG, draws `boxes` (from [`place`]) and encodes the result as the
/// request's format, in hba's isolated world. Returns the base64 screenshot.
async fn render(
    page: &Page,
    world: &World,
    image: String,
    cut: &Crop,
    boxes: Vec<Value>,
    request: &Request<'_>,
) -> Result<String> {
    let Rect { x, y, width, height } = cut.source;
    let options = json!({
        "source": [x, y, width, height],
        "width": cut.width,
        "height": cut.height,
        "boxes": boxes,
        "scale": f64::from(cut.width) / cut.shown.width,
        "type": format!("image/{}", request.format.name()),
        "quality": f64::from(request.quality) / 100.0,
    });
    let rendered = page
        .call(world, RENDER, &[Arg::Value(Value::String(image)), Arg::Value(options)])
        .await
        .context("drawing the screenshot")?;
    rendered.as_str().map(str::to_owned).context("drawing the screenshot")
}

/// Cuts `o.source` (`[x, y, width, height]` in pixels) out of a base64 PNG, scales it to
/// `o.width`×`o.height`, draws a box and a ref label for each of `o.boxes` (`[ref, x, y, width,
/// height]` in the result's pixels, `o.scale` of them per CSS pixel) and encodes it as `o.type`.
const RENDER: &str = "async function(image, o) { \
    const bytes = Uint8Array.from(atob(image), c => c.charCodeAt(0)); \
    const bitmap = await createImageBitmap(new Blob([bytes], {type: 'image/png'})); \
    const canvas = new OffscreenCanvas(o.width, o.height), g = canvas.getContext('2d'); \
    const [sx, sy, sw, sh] = o.source; \
    g.drawImage(bitmap, sx, sy, sw, sh, 0, 0, o.width, o.height); \
    const k = o.scale, color = '#c8104f'; \
    g.lineWidth = 2 * k; \
    g.strokeStyle = color; \
    for (const [, x, y, w, h] of o.boxes) g.strokeRect(x + k, y + k, Math.max(0, w - 2 * k), Math.max(0, h - 2 * k)); \
    g.font = `bold ${11 * k}px monospace`; \
    g.textBaseline = 'middle'; \
    for (const [ref, x, y] of o.boxes) { \
      const w = g.measureText(ref).width + 6 * k, h = 14 * k, top = y >= 15 * k ? y - h : y; \
      g.fillStyle = color; \
      g.fillRect(x, top, w, h); \
      g.fillStyle = '#fff'; \
      g.fillText(ref, x + 3 * k, top + h / 2); \
    } \
    const blob = await canvas.convertToBlob({type: o.type, quality: o.quality}); \
    const out = new Uint8Array(await blob.arrayBuffer()); \
    let binary = ''; \
    for (let i = 0; i < out.length; i += 0x8000) binary += String.fromCharCode(...out.subarray(i, i + 0x8000)); \
    return btoa(binary); }";

/// A base64 PNG of the whole view, grabbed without the compositor surface. A grab can go
/// unanswered (every other one while the window is hidden), so another follows after the time a
/// grab takes, until one answers. Helium may answer with the image of an earlier, abandoned
/// request, so only a PNG covering `device`, the visible viewport in device pixels, is taken. A
/// grab that fails while no other is pending ends the wait with its error.
async fn grab_view(page: &Page, session: &str, device: (f64, f64), timeout: Duration) -> Result<String> {
    let params = json!({"format": "png", "fromSurface": false});
    let deadline = tokio::time::Instant::now() + timeout;
    let mut grabs = futures_util::stream::FuturesUnordered::new();
    let mut failure: Option<anyhow::Error> = None;
    loop {
        grabs.push(page.cdp.raw_within("Page.captureScreenshot", &params, Some(session), timeout));
        let again = tokio::time::sleep_until((tokio::time::Instant::now() + GRAB_AGAIN).min(deadline));
        tokio::pin!(again);
        loop {
            tokio::select! {
                Some(answer) = futures_util::StreamExt::next(&mut grabs) => {
                    match answer {
                        Ok(raw) => {
                            let data = captured(&raw)?;
                            if png_size(&data).is_some_and(|grab| covers(grab, device)) {
                                return Ok(data);
                            }
                        }
                        Err(error) => failure = Some(error),
                    }
                    if grabs.is_empty()
                        && let Some(error) = failure.take()
                    {
                        return Err(error.context("grabbing the tab's view"));
                    }
                }
                () = &mut again => break,
            }
        }
        if tokio::time::Instant::now() >= deadline {
            let error = anyhow!("the tab produced no image within {:.0}s", timeout.as_secs_f32());
            return Err(match failure {
                Some(failure) => error.context(format!("{failure:#}")),
                None => error,
            });
        }
    }
}

/// Whether a grab of `grab` pixels covers `device` (pixels of the visible viewport, which may be
/// fractional). The view grows by the scrollbar, and a pixel of rounding is allowed.
fn covers(grab: (u32, u32), device: (f64, f64)) -> bool {
    f64::from(grab.0) >= (device.0 - 1.0).floor() && f64::from(grab.1) >= (device.1 - 1.0).floor()
}

/// The element's border box in document coordinates, across frames.
async fn element_clip(page: &Page, element: &Element, layout: &Viewport, ratio: f64) -> Result<Clip> {
    #[derive(Deserialize)]
    struct Quads {
        quads: Vec<Vec<f64>>,
    }
    let quads: Quads = page
        .renderer_call("DOM.getContentQuads", json!({"objectId": element.object}), &element.world.session)
        .await
        .with_context(|| format!("{} is not rendered", element.label))?;
    let shift = crate::actions::frame_offset(page, &element.world.frame, &element.world.session).await?;
    let points = quads
        .quads
        .iter()
        .filter(|quad| quad.len() == 8)
        .flat_map(|quad| quad.as_chunks::<2>().0.iter().map(|[x, y]| (*x, *y)));
    let (left, top, right, bottom) = points.fold((f64::MAX, f64::MAX, f64::MIN, f64::MIN), |(l, t, r, b), (x, y)| {
        (l.min(x), t.min(y), r.max(x), b.max(y))
    });
    if right - left < 1.0 || bottom - top < 1.0 {
        bail!("{} has no visible area to capture", element.label);
    }
    let (dx, dy) = (shift.0 + layout.page_x, shift.1 + layout.page_y);
    Ok(Clip::enclosing(left + dx, top + dy, right + dx, bottom + dy, 1.0 / ratio))
}

/// Waits up to [`FRAME_WAIT`] for the page to paint, so the capture shows the current DOM.
async fn fresh_frame(page: &Page) -> Result<()> {
    let world = page.main_world().await?;
    // The page-side timer ends the call even when no frame comes; the outer timeout covers
    // timers that a background tab throttles.
    let wait = [Arg::Value(json!(FRAME_WAIT.as_millis()))];
    let painted = page.call(
        &world,
        "function(wait) { const w = this.window; \
           return new Promise(resolve => { w.requestAnimationFrame(() => resolve(true)); w.setTimeout(() => resolve(false), wait); }); }",
        &wait,
    );
    match tokio::time::timeout(FRAME_WAIT + Duration::from_millis(200), painted).await {
        Ok(result) => result.map(drop),
        Err(_) => Ok(()),
    }
}

/// Boxes of the interactive elements of a fresh snapshot and their refs, measured in each
/// frame from `hba`'s world and placed in the main frame's viewport.
async fn labels(page: &Arc<Page>) -> Result<Vec<Label>> {
    let tree = snapshot::capture(page, &snapshot::Options::default()).await?;
    let mut by_frame: BTreeMap<u32, Vec<String>> = BTreeMap::new();
    collect_refs(&tree.nodes, &mut by_frame);
    let mut labels = Vec::new();
    for (seq, refs) in by_frame {
        let Some(frame) = page.frame_by_seq(seq) else { continue };
        let world = page.world(&frame).await?;
        let refs = [Arg::Value(json!(refs))];
        let (boxes, origin) = tokio::join!(page.call(&world, BOXES, &refs), viewport_origin(page, &frame));
        let boxes: Vec<(String, f64, f64, f64, f64)> =
            serde_json::from_value(boxes?).context("reading the boxes of the elements")?;
        let origin = origin?;
        labels.extend(boxes.into_iter().map(|(reference, x, y, width, height)| Label {
            reference,
            rect: Rect { x: x + origin.0, y: y + origin.1, width, height },
        }));
    }
    Ok(labels)
}

/// Interactive refs of the tree, grouped by the frame sequence in their prefix.
fn collect_refs(nodes: &[Node], by_frame: &mut BTreeMap<u32, Vec<String>>) {
    for node in nodes {
        let Node::Element(element) = node else { continue };
        if snapshot::is_interactive(element)
            && let Some((seq, reference)) = element.reference.as_deref().and_then(crate::page::parse_ref)
        {
            by_frame.entry(seq).or_default().push(reference);
        }
        collect_refs(&element.children, by_frame);
    }
}

/// Boxes of the elements of `refs` (from the engine's last snapshot) in the frame's viewport, as
/// `[ref, x, y, width, height]`; in a child frame, cut to what the frame shows.
const BOXES: &str = "function(refs) { \
    const w = this.window, snap = this._lastAriaSnapshotForQuery; \
    if (!snap) return []; \
    const child = w.parent !== w, root = w.document.documentElement; \
    const low = child ? 0 : -Infinity; \
    const right = child && root ? root.clientWidth : Infinity, bottom = child && root ? root.clientHeight : Infinity; \
    const boxes = []; \
    for (const ref of refs) { \
      const entry = snap.info.get(ref), element = entry && entry.element; \
      if (!element || !element.isConnected) continue; \
      const r = element.getBoundingClientRect(); \
      const x = Math.max(r.left, low), y = Math.max(r.top, low); \
      const width = Math.min(r.right, right) - x, height = Math.min(r.bottom, bottom) - y; \
      if (width >= 1 && height >= 1) boxes.push([ref, x, y, width, height]); \
    } \
    return boxes; }";

/// Where a frame's viewport lies in the main frame's viewport: the content box of its owner
/// element, which the parent's process measures from its own viewport, plus where that lies.
async fn viewport_origin(page: &Page, frame: &str) -> Result<(f64, f64)> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Owner {
        backend_node_id: i64,
    }
    #[derive(Deserialize)]
    struct Model {
        model: BoxModel,
    }
    #[derive(Deserialize)]
    struct BoxModel {
        content: Vec<f64>,
    }
    let Some(parent) = page.frame_parent(frame) else { return Ok((0.0, 0.0)) };
    let session = page.frame_session(&parent).context("the frame's parent is gone")?;
    let owner: Owner = page.renderer_call("DOM.getFrameOwner", json!({"frameId": frame}), &session).await?;
    let (model, above) = tokio::join!(
        page.renderer_call::<Model>("DOM.getBoxModel", json!({"backendNodeId": owner.backend_node_id}), &session),
        crate::actions::frame_offset(page, &parent, &session)
    );
    let (content, above) = (model?.model.content, above?);
    let (x, y) = (content.first().copied().unwrap_or(0.0), content.get(1).copied().unwrap_or(0.0));
    Ok((x + above.0, y + above.1))
}

pub async fn pdf(cx: &mut Cx<'_>, args: PdfArgs) -> Result<()> {
    let (paper_name, width, height) = paper(&args.format)?;
    let path = match &args.path {
        Some(path) => cx.path(path),
        None => cx.artifact("page", "pdf")?,
    };
    let page = cx.page().await?;
    let session = page.session()?;
    let file = crate::output::create(&path)?;
    let params = json!({"landscape": args.landscape, "printBackground": args.background, "paperWidth": width,
                        "paperHeight": height, "transferMode": "ReturnAsStream"});
    let printed =
        page.cdp.raw_within("Page.printToPDF", &params, Some(&session), cx.timeout).await.context("printing to PDF")?;
    let printed: Printed = serde_json::from_str(printed.get()).context("Helium returned no PDF stream")?;
    let written = save_stream(&page.cdp, &session, &printed.stream, file, &path).await?;
    let orientation = if args.landscape { " landscape" } else { "" };
    cx.say(format!("{} ({paper_name}{orientation}, {})", path.display(), crate::output::bytes(written)));
    Ok(())
}

/// Copies a protocol stream (a PDF, a trace) into `file`, which is `path`, and closes the stream;
/// returns the bytes written.
pub async fn save_stream(cdp: &Cdp, session: &str, handle: &str, file: std::fs::File, path: &Path) -> Result<u64> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Chunk {
        data: String,
        #[serde(default)]
        eof: bool,
        #[serde(default)]
        base64_encoded: bool,
    }
    let copied = async {
        let mut file = tokio::fs::File::from_std(file);
        let mut written = 0;
        loop {
            let chunk: Chunk =
                cdp.call("IO.read", json!({"handle": handle, "size": STREAM_CHUNK}), Some(session)).await?;
            let bytes = if chunk.base64_encoded {
                base64::engine::general_purpose::STANDARD
                    .decode(chunk.data.as_bytes())
                    .context("decoding the stream")?
            } else {
                chunk.data.into_bytes()
            };
            file.write_all(&bytes).await.with_context(|| format!("writing {}", path.display()))?;
            written += bytes.len() as u64;
            if chunk.eof {
                break;
            }
        }
        file.flush().await.with_context(|| format!("writing {}", path.display()))?;
        Ok(written)
    }
    .await;
    cdp.send("IO.close", json!({"handle": handle}), Some(session)).await.ok();
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_follows_flag_and_extension() {
        assert_eq!(format_for(None, false).expect("ok"), Format::Jpeg);
        assert_eq!(format_for(None, true).expect("ok"), Format::Png);
        assert_eq!(format_for(Some(Path::new("a/b.PNG")), false).expect("ok"), Format::Png);
        assert_eq!(format_for(Some(Path::new("b.jpeg")), false).expect("ok"), Format::Jpeg);
        assert_eq!(format_for(Some(Path::new("b")), true).expect("ok"), Format::Png);
        assert!(format_for(Some(Path::new("b.jpg")), true).is_err());
    }

    #[test]
    fn image_headers_give_dimensions() {
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend(1280u32.to_be_bytes());
        png.extend(720u32.to_be_bytes());
        assert_eq!(image_size(&png), Some((1280, 720)));
        // SOI, an APP0 segment of 16 bytes, then SOF0 with height 600 and width 800.
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        jpeg.extend([0u8; 14]);
        jpeg.extend([0xFF, 0xC0, 0x00, 0x11, 0x08, 0x02, 0x58, 0x03, 0x20]);
        assert_eq!(image_size(&jpeg), Some((800, 600)));
        assert_eq!(image_size(b"GIF89a"), None);
    }

    #[test]
    fn paper_formats_ignore_case() {
        assert_eq!(paper("a4").expect("known"), ("A4", 8.27, 11.7));
        assert_eq!(paper("LETTER").expect("known"), ("Letter", 8.5, 11.0));
        assert!(paper("A5").is_err());
    }

    #[test]
    fn element_clips_cover_whole_pixels() {
        let clip = Clip::enclosing(10.4, -3.0, 50.2, 20.1, 0.5);
        assert_eq!(clip, Clip { x: 10.0, y: 0.0, width: 41.0, height: 21.0, scale: 0.5 });
        let viewport = Viewport { page_x: 0.0, page_y: 0.0, client_width: 100.0, client_height: 30.0, scale: 1.0 };
        assert!(clip.inside(&viewport));
        assert!(!Clip { y: 20.0, ..clip }.inside(&viewport));
    }

    #[test]
    fn view_grabs_are_cut_at_the_grab_scale_scrollbar_included() {
        // A 1280 px wide window with a 15 px scrollbar, grabbed at device pixel ratio 2.
        let grab = (2560, 1440);
        let viewport = crop(grab, 1280.0, Rect { x: 0.0, y: 0.0, width: 1265.0, height: 720.0 });
        assert_eq!(viewport.source, Rect { x: 0.0, y: 0.0, width: 2530.0, height: 1440.0 });
        assert_eq!((viewport.width, viewport.height), (1265, 720));
        let card = crop(grab, 1280.0, Rect { x: 1000.0, y: 100.0, width: 50.0, height: 20.0 });
        assert_eq!(card.source, Rect { x: 2000.0, y: 200.0, width: 100.0, height: 40.0 }, "no drift with x");
        assert_eq!((card.width, card.height), (50, 20));
        let edge = crop(grab, 1280.0, Rect { x: -10.0, y: 700.0, width: 40.0, height: 50.0 });
        assert_eq!(edge.shown, Rect { x: 0.0, y: 700.0, width: 30.0, height: 20.0 }, "kept on the view");
        assert_eq!(edge.source, Rect { x: 0.0, y: 1400.0, width: 60.0, height: 40.0 });
    }

    #[test]
    fn grabs_fit_in_device_pixels() {
        // A 1265x720 CSS viewport (scrollbar not counted) of a 1280 px wide view.
        let device = |ratio: f64| (1265.0 * ratio, 720.0 * ratio);
        // Zoomed out to 90% on a 1x screen, the grab has fewer pixels than the CSS viewport.
        assert!(covers((1152, 648), device(0.9)));
        assert!(covers((1280, 720), device(1.0)));
        assert!(covers((2560, 1440), device(2.0)));
        // A stale CSS-sized image from an abandoned capture is no grab of a 2x screen.
        assert!(!covers((1280, 720), device(2.0)));
        assert!(!covers((1265, 300), device(1.0)));
    }

    #[test]
    fn labels_land_where_the_image_shows_them() {
        let label = |reference: &str, x: f64, y: f64| Label {
            reference: reference.to_owned(),
            rect: Rect { x, y, width: 20.0, height: 10.0 },
        };
        let labels = [label("e1", 100.0, 50.0), label("e2", 900.0, 50.0), label("f1e3", -400.0, -300.0)];
        // The viewport at 1:1.
        assert_eq!(place(&labels, (0.0, 0.0), 1.0, (800, 600)), [json!(["e1", 100.0, 50.0, 20.0, 10.0])]);
        // A full page scrolled down 300 px: the image starts 300 px above the viewport.
        assert_eq!(
            place(&labels, (-500.0, -300.0), 1.0, (2000, 3000)),
            [
                json!(["e1", 600.0, 350.0, 20.0, 10.0]),
                json!(["e2", 1400.0, 350.0, 20.0, 10.0]),
                json!(["f1e3", 100.0, 0.0, 20.0, 10.0]),
            ]
        );
        // An element clip at twice the pixels.
        assert_eq!(place(&labels, (95.0, 45.0), 2.0, (60, 40)), [json!(["e1", 10.0, 10.0, 40.0, 20.0])]);
    }
}
