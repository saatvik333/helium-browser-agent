//! `hba perf vitals`: Core Web Vitals from the page's buffered performance entries, read in
//! `hba`'s isolated world so page scripts see no observer.

use std::fmt::Write as _;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::daemon::Cx;
use crate::output;
use crate::page::Page;

/// Collects the entries and computes the vitals the way the `web-vitals` library does: CLS as
/// the largest session window, INP as the worst interaction ignoring one per fifty. Buffered
/// event timing entries arrive in an observer task, not through `takeRecords`, hence the wait.
const MEASURE: &str = r"async function() {
  const take = (type, options) => new Promise((resolve) => {
    if (!PerformanceObserver.supportedEntryTypes.includes(type)) return resolve([]);
    const entries = [];
    const observer = new PerformanceObserver((list) => entries.push(...list.getEntries()));
    observer.observe({type, buffered: true, ...options});
    setTimeout(() => {
      entries.push(...observer.takeRecords());
      observer.disconnect();
      resolve(entries);
    }, 100);
  });
  const [paints, lcps, layoutShifts, events, firstInput] = await Promise.all([
    take('paint'),
    take('largest-contentful-paint'),
    take('layout-shift'),
    take('event', {durationThreshold: 16}),
    take('first-input'),
  ]);
  const nav = performance.getEntriesByType('navigation')[0];
  const activation = (nav && nav.activationStart) || 0;
  const since = (time) => Math.max(time - activation, 0);
  const fcp = paints.find((entry) => entry.name === 'first-contentful-paint');
  const lcp = lcps[lcps.length - 1];
  let element = '';
  if (lcp) {
    const node = lcp.element;
    if (node) element = node.localName + (node.id ? '#' + node.id : '');
    if (lcp.url) element = (element ? element + ' ' : '') + lcp.url;
  }
  let cls = 0, session = 0, first = 0, previous = 0, shifts = 0;
  for (const shift of layoutShifts) {
    if (shift.hadRecentInput) continue;
    shifts++;
    if (session && (shift.startTime - previous >= 1000 || shift.startTime - first >= 5000)) session = 0;
    if (!session) first = shift.startTime;
    session += shift.value;
    previous = shift.startTime;
    cls = Math.max(cls, session);
  }
  const latency = new Map();
  for (const entry of [...events, ...firstInput]) {
    if (entry.interactionId) latency.set(entry.interactionId, Math.max(latency.get(entry.interactionId) || 0, entry.duration));
  }
  const worst = [...latency.values()].sort((a, b) => b - a);
  // As web-vitals: one slowest interaction is skipped per 50 the page had, fast ones included.
  const count = performance.interactionCount ?? worst.length;
  return {
    ttfb: nav ? since(nav.responseStart) : null,
    fcp: fcp ? since(fcp.startTime) : null,
    lcp: lcp ? since(lcp.startTime) : null,
    lcpElement: element,
    cls,
    shifts,
    inp: worst.length ? worst[Math.min(worst.length - 1, Math.floor(count / 50))] : null,
    interacted: firstInput.length > 0,
  };
}";

/// Chrome buffers only interactions at least this slow (ms); faster ones are never observable
/// after the fact.
const BUFFERED_INTERACTION_MS: f64 = 104.0;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Measured {
    ttfb: Option<f64>,
    fcp: Option<f64>,
    lcp: Option<f64>,
    #[serde(default)]
    lcp_element: String,
    #[serde(default)]
    cls: f64,
    #[serde(default)]
    shifts: u32,
    inp: Option<f64>,
    #[serde(default)]
    interacted: bool,
}

/// web.dev thresholds: good up to `good`, poor above `poor`.
#[derive(Debug, Clone, Copy)]
struct Threshold {
    good: f64,
    poor: f64,
}

const TTFB: Threshold = Threshold { good: 800.0, poor: 1800.0 };
const FCP: Threshold = Threshold { good: 1800.0, poor: 3000.0 };
const LCP: Threshold = Threshold { good: 2500.0, poor: 4000.0 };
const CLS: Threshold = Threshold { good: 0.1, poor: 0.25 };
const INP: Threshold = Threshold { good: 200.0, poor: 500.0 };

impl Threshold {
    fn rate(self, value: f64) -> &'static str {
        if value <= self.good {
            "good"
        } else if value <= self.poor {
            "needs improvement"
        } else {
            "poor"
        }
    }
}

/// `hba perf vitals`: one line per vital with its web.dev rating.
pub(super) async fn report(cx: &mut Cx<'_>, page: &Page) -> Result<()> {
    let world = page.main_world().await?;
    let value = page.call(&world, MEASURE, &[]).await?;
    let measured: Measured = serde_json::from_value(value).context("reading the performance entries")?;
    if cx.json {
        cx.say(serde_json::to_string(&json_of(&measured))?);
        return Ok(());
    }
    for line in lines(&measured) {
        cx.say(line);
    }
    Ok(())
}

fn lines(m: &Measured) -> [String; 5] {
    let timed = |name: &str, value: Option<f64>, threshold: Threshold, missing: &str| match value {
        Some(ms) => format!("{name} {} {}", output::millis(ms), threshold.rate(ms)),
        None => format!("{name} n/a ({missing})"),
    };
    let mut lcp = timed("LCP", m.lcp, LCP, "no candidate painted yet");
    if m.lcp.is_some() && !m.lcp_element.is_empty() {
        let _ = write!(lcp, " ({})", output::short_url(&m.lcp_element, 80));
    }
    let shifts = if m.shifts == 0 { "no layout shifts".to_owned() } else { output::count(m.shifts as usize, "shift") };
    let inp = match m.inp {
        Some(ms) if ms >= BUFFERED_INTERACTION_MS => {
            format!("INP {} {}", output::millis(ms), INP.rate(ms))
        }
        Some(_) => "INP < 104 ms good (no slow interactions)".to_owned(),
        None if m.interacted => "INP < 104 ms good (no slow interactions)".to_owned(),
        None => "INP n/a (no interactions yet)".to_owned(),
    };
    [
        timed("TTFB", m.ttfb, TTFB, "no navigation timing"),
        timed("FCP", m.fcp, FCP, "nothing painted yet"),
        lcp,
        format!("CLS {:.3} {} ({shifts})", m.cls, CLS.rate(m.cls)),
        inp,
    ]
}

fn json_of(m: &Measured) -> Value {
    let rated = |value: Option<f64>, threshold: Threshold| match value {
        Some(value) => json!({"value": value, "rating": threshold.rate(value)}),
        None => Value::Null,
    };
    let inp = match m.inp {
        Some(ms) if ms >= BUFFERED_INTERACTION_MS => rated(Some(ms), INP),
        _ if m.interacted || m.inp.is_some() => {
            json!({"below": BUFFERED_INTERACTION_MS, "rating": "good"})
        }
        _ => Value::Null,
    };
    json!({
        "ttfb": rated(m.ttfb, TTFB),
        "fcp": rated(m.fcp, FCP),
        "lcp": rated(m.lcp, LCP),
        "lcpElement": m.lcp_element,
        "cls": rated(Some(m.cls), CLS),
        "inp": inp,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratings_follow_web_dev_thresholds() {
        assert_eq!(LCP.rate(2500.0), "good");
        assert_eq!(LCP.rate(2501.0), "needs improvement");
        assert_eq!(LCP.rate(4001.0), "poor");
        assert_eq!(CLS.rate(0.1), "good");
        assert_eq!(CLS.rate(0.3), "poor");
        assert_eq!(INP.rate(350.0), "needs improvement");
    }

    #[test]
    fn vitals_print_one_per_line() {
        let measured = Measured {
            ttfb: Some(4.2),
            fcp: Some(48.0),
            lcp: Some(3100.0),
            lcp_element: "img#hero http://127.0.0.1:8765/hero.png".into(),
            cls: 0.153,
            shifts: 2,
            inp: Some(240.0),
            interacted: true,
        };
        assert_eq!(
            lines(&measured),
            [
                "TTFB 4 ms good",
                "FCP 48 ms good",
                "LCP 3.10 s needs improvement (img#hero http://127.0.0.1:8765/hero.png)",
                "CLS 0.153 needs improvement (2 shifts)",
                "INP 240 ms needs improvement",
            ]
        );
    }

    #[test]
    fn missing_vitals_say_why() {
        let quiet = lines(&Measured::default());
        assert_eq!(quiet[1], "FCP n/a (nothing painted yet)");
        assert_eq!(quiet[3], "CLS 0.000 good (no layout shifts)");
        assert_eq!(quiet[4], "INP n/a (no interactions yet)");
        let fast = lines(&Measured { interacted: true, inp: Some(24.0), ..Measured::default() });
        assert_eq!(fast[4], "INP < 104 ms good (no slow interactions)");
    }
}
