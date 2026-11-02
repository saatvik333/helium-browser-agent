//! Trusted mouse and keyboard input through `Input.dispatch*Event`, optionally human-like.
//!
//! Human mode moves the pointer along a curve with jittered timing, clicks off-center and types
//! with per-key delays; it costs ~0.1 s per click. Local pages get instant input.

use std::pin::Pin;
use std::time::Duration;

use anyhow::{Result, bail};
use serde_json::json;

use crate::cli::MouseButton;
use crate::keyboard::{self, Key};
use crate::page::Page;

impl MouseButton {
    pub fn cdp(self) -> &'static str {
        match self {
            MouseButton::Left => "left",
            MouseButton::Right => "right",
            MouseButton::Middle => "middle",
        }
    }

    fn mask(self) -> u8 {
        match self {
            MouseButton::Left => 1,
            MouseButton::Right => 2,
            MouseButton::Middle => 4,
        }
    }
}

/// The CDP `button` of a pointer move: the held button, as in a drag, else `none`.
fn held_button(buttons: u8) -> &'static str {
    [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
        .into_iter()
        .find(|button| buttons & button.mask() != 0)
        .map_or("none", MouseButton::cdp)
}

async fn pause(min_ms: u64, max_ms: u64) {
    tokio::time::sleep(Duration::from_millis(fastrand::u64(min_ms..=max_ms))).await;
}

/// The page's acknowledgement of the last pointer move, for callers that send nothing after it.
pub struct Moved<'a>(Option<Pin<Box<dyn Future<Output = Result<()>> + Send + 'a>>>);

impl Moved<'_> {
    /// Waits until the page has taken the move.
    pub async fn taken(self) -> Result<()> {
        match self.0 {
            Some(ack) => ack.await,
            None => Ok(()),
        }
    }
}

/// Moves the pointer to `to`, along a curved path when `human`. The moves are sent without
/// waiting for the page to take them: a page whose window is not on screen takes a pointer move
/// only with the next frame or event, up to a second later. Callers that send nothing after
/// the move await [`Moved::taken`].
pub async fn move_to(page: &Page, to: (f64, f64), human: bool) -> Result<Moved<'_>> {
    let from = page.mouse();
    let buttons = page.buttons();
    let distance = (to.0 - from.0).hypot(to.1 - from.1);
    let steps = if human { (distance / 60.0).clamp(4.0, 14.0) as u32 } else { 1 };
    // Bézier control point pushed sideways from the straight line by up to 15% of the distance.
    let bend = (fastrand::f64() - 0.5) * 0.3 * distance;
    let (dx, dy) = if distance > 0.0 { ((to.1 - from.1) / distance, -(to.0 - from.0) / distance) } else { (0.0, 0.0) };
    let control = (f64::midpoint(from.0, to.0) + dx * bend, f64::midpoint(from.1, to.1) + dy * bend);
    let mut last = None;
    for step in 1..=steps {
        let t = f64::from(step) / f64::from(steps);
        let eased = t * t * (3.0 - 2.0 * t);
        let point = if step == steps {
            to
        } else {
            let (a, b, c) = ((1.0 - eased) * (1.0 - eased), 2.0 * (1.0 - eased) * eased, eased * eased);
            (a * from.0 + b * control.0 + c * to.0, a * from.1 + b * control.1 + c * to.1)
        };
        let move_event = json!({"type": "mouseMoved", "x": point.0, "y": point.1, "button": held_button(buttons),
                                "buttons": buttons, "pointerType": "mouse"});
        // Earlier moves are taken at the latest with the next one; only the last one's answer counts.
        last = Some(page.send_input("Input.dispatchMouseEvent", &move_event)?);
        if human && step < steps {
            pause(4, 12).await;
        }
    }
    page.set_mouse(to);
    Ok(Moved(last.map(|ack| Box::pin(ack) as Pin<Box<dyn Future<Output = Result<()>> + Send + '_>>)))
}

/// Presses and releases a mouse button `count` times at the current pointer position.
pub async fn click(page: &Page, button: MouseButton, count: u8, modifiers: i64, human: bool) -> Result<()> {
    let (x, y) = page.mouse();
    let held = page.buttons();
    for click_count in 1..=count {
        page.input(
            "Input.dispatchMouseEvent",
            json!({"type": "mousePressed", "x": x, "y": y, "button": button.cdp(), "buttons": held | button.mask(),
                   "clickCount": click_count, "modifiers": modifiers, "force": 0.5, "pointerType": "mouse"}),
        )
        .await?;
        if human {
            pause(35, 95).await;
        }
        if page.dialog().is_some() {
            return Ok(()); // The press itself opened a dialog; the page is blocked until it is handled.
        }
        page.input(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseReleased", "x": x, "y": y, "button": button.cdp(), "buttons": held,
                   "clickCount": click_count, "modifiers": modifiers, "pointerType": "mouse"}),
        )
        .await?;
        if page.dialog().is_some() {
            return Ok(());
        }
        if human && click_count < count {
            pause(60, 110).await;
        }
    }
    Ok(())
}

/// One raw mouse event at the current pointer position (`mouse down` / `mouse up`). Held
/// buttons are remembered, so pointer moves in between are drags.
pub async fn button(page: &Page, button: MouseButton, down: bool) -> Result<()> {
    let (x, y) = page.mouse();
    let buttons = if down { page.buttons() | button.mask() } else { page.buttons() & !button.mask() };
    page.input(
        "Input.dispatchMouseEvent",
        json!({"type": if down { "mousePressed" } else { "mouseReleased" }, "x": x, "y": y, "button": button.cdp(),
               "buttons": buttons, "clickCount": 1, "pointerType": "mouse"}),
    )
    .await?;
    page.set_buttons(buttons);
    Ok(())
}

/// Wheel scroll at `at` by (dx, dy) pixels, in a few steps when `human`.
pub async fn wheel(page: &Page, at: (f64, f64), delta: (f64, f64), human: bool) -> Result<()> {
    let steps = if human { ((delta.0.abs() + delta.1.abs()) / 120.0).clamp(1.0, 8.0) as u32 } else { 1 };
    for _ in 0..steps {
        page.input(
            "Input.dispatchMouseEvent",
            json!({"type": "mouseWheel", "x": at.0, "y": at.1, "deltaX": delta.0 / f64::from(steps),
                   "deltaY": delta.1 / f64::from(steps), "pointerType": "mouse"}),
        )
        .await?;
        if human {
            pause(16, 40).await;
        }
    }
    Ok(())
}

async fn key_event(page: &Page, key: &Key, down: bool, modifiers: i64) -> Result<()> {
    let mut event = json!({
        "type": if !down { "keyUp" } else if key.text.is_empty() { "rawKeyDown" } else { "keyDown" },
        "modifiers": modifiers,
        "windowsVirtualKeyCode": key.key_code,
        "code": key.code,
        "key": key.key,
        "location": key.location,
        "isKeypad": key.is_keypad(),
    });
    if down && !key.text.is_empty() {
        event["text"] = json!(key.text);
        event["unmodifiedText"] = json!(key.text);
    }
    page.input("Input.dispatchKeyEvent", event).await
}

/// A chord such as `Control+Shift+K`, checked whole before any key goes down: a typo after a
/// modifier must not leave the page with that modifier held.
pub struct Chord {
    /// Modifiers in the order they go down.
    held: Vec<Key>,
    key: Key,
}

impl Chord {
    pub fn parse(chord: &str) -> Result<Self> {
        let names = keyboard::split_chord(chord);
        let (last, names) = names.split_last().expect("split_chord returns at least one key");
        let mut held = Vec::with_capacity(names.len());
        let mut shift = false;
        for name in names {
            let key = keyboard::key(name, shift)?;
            if !keyboard::MODIFIERS.contains(&key.key.as_str()) {
                bail!("{name:?} is not a modifier in {chord:?}");
            }
            shift |= key.key == "Shift";
            held.push(key);
        }
        let mut key = keyboard::key(last, shift)?;
        // With Control/Alt/Meta held the key produces no text, as in a real keyboard shortcut.
        if held.iter().any(|modifier| modifier.key != "Shift") {
            key.text.clear();
        }
        Ok(Self { held, key })
    }
}

/// Presses a chord: modifiers down, key down/up, modifiers up.
pub async fn press(page: &Page, chord: &Chord, human: bool) -> Result<()> {
    let mut modifiers = 0;
    for key in &chord.held {
        modifiers |= keyboard::modifier_bit(&key.key);
        key_event(page, key, true, modifiers).await?;
    }
    key_event(page, &chord.key, true, modifiers).await?;
    if human {
        pause(30, 80).await;
    }
    if page.dialog().is_none() {
        key_event(page, &chord.key, false, modifiers).await?;
    }
    for key in chord.held.iter().rev() {
        if page.dialog().is_some() {
            break;
        }
        modifiers &= !keyboard::modifier_bit(&key.key);
        key_event(page, key, false, modifiers).await?;
    }
    Ok(())
}

/// Types text with key events; characters outside the layout are inserted as text.
pub async fn type_text(page: &Page, text: &str, delay: Option<u64>, human: bool) -> Result<()> {
    let mut buffer = [0; 4];
    let total = text.chars().count();
    for (typed, c) in text.chars().enumerate() {
        if let Some(dialog) = page.dialog() {
            bail!(
                "a {} dialog opened while typing; {} of {total} characters were not typed",
                dialog.kind,
                total - typed
            );
        }
        if keyboard::typeable(c) {
            let key = keyboard::key(c.encode_utf8(&mut buffer), false)?;
            key_event(page, &key, true, 0).await?;
            if human {
                pause(15, 45).await;
            }
            // A dialog the key opened blocks the page until it is handled.
            if page.dialog().is_none() {
                key_event(page, &key, false, 0).await?;
            }
        } else {
            page.input("Input.insertText", json!({"text": c.to_string()})).await?;
        }
        match delay {
            Some(ms) if ms > 0 => tokio::time::sleep(Duration::from_millis(ms)).await,
            None if human => pause(30, 110).await,
            _ => {}
        }
    }
    Ok(())
}

/// Inserts text at the caret like an IME commit or paste (one input event).
pub async fn insert_text(page: &Page, text: &str) -> Result<()> {
    page.input("Input.insertText", json!({"text": text})).await
}

/// Parses `--modifiers` names into a CDP mask.
pub fn modifier_mask(names: &[String]) -> Result<i64> {
    names.iter().try_fold(0, |mask, name| {
        let name = if name == "ControlOrMeta" { "Control" } else { name.as_str() };
        match keyboard::modifier_bit(name) {
            0 => bail!("unknown modifier {name:?}; use Alt, Control, Meta or Shift"),
            bit => Ok(mask | bit),
        }
    })
}

#[cfg(test)]
mod tests {
    use super::Chord;

    #[test]
    fn chords_are_checked_whole_before_pressing() {
        assert!(Chord::parse("Control+Typo").is_err());
        assert!(Chord::parse("Control+a+b").is_err());
        let shortcut = Chord::parse("Control+Shift+KeyA").unwrap();
        assert_eq!(shortcut.held.len(), 2);
        assert_eq!((shortcut.key.key.as_str(), shortcut.key.text.as_str()), ("A", ""));
        assert_eq!(Chord::parse("Shift+KeyA").unwrap().key.text, "A");
    }
}
