//! Compact text helpers shared by commands, dates, and files only their owner may read.

use std::borrow::Cow;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::Context as _;
use serde_json::Value;

/// Truncates to `max` characters, marking the cut with `…`.
pub fn truncate(text: &str, max: usize) -> Cow<'_, str> {
    match text.char_indices().nth(max) {
        None => Cow::Borrowed(text),
        Some((end, _)) => {
            let cut = text[..end].char_indices().last().map_or(0, |(i, _)| i);
            Cow::Owned(format!("{}…", &text[..cut]))
        }
    }
}

/// `512 B`, `1.2 KB`, `84 KB`, `3.4 MB` (binary multiples).
pub fn bytes(count: u64) -> String {
    const UNITS: [&str; 3] = ["KB", "MB", "GB"];
    if count < 1024 {
        return format!("{count} B");
    }
    let mut value = count as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 { format!("{value:.1} {}", UNITS[unit]) } else { format!("{value:.0} {}", UNITS[unit]) }
}

/// `0.4 ms`, `120 ms`, `1.24 s`, `12.5 s`.
pub fn millis(ms: f64) -> String {
    if ms < 1.0 {
        format!("{ms:.1} ms")
    } else if ms < 1000.0 {
        format!("{ms:.0} ms")
    } else if ms < 10_000.0 {
        format!("{:.2} s", ms / 1000.0)
    } else {
        format!("{:.1} s", ms / 1000.0)
    }
}

/// `1 tab`, `3 tabs`.
pub fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// URL without `https://` and a trailing `/`, cut in the middle when longer than `max`.
pub fn short_url(url: &str, max: usize) -> String {
    let bare = url.strip_prefix("https://").unwrap_or(url);
    let bare = bare.strip_suffix('/').unwrap_or(bare);
    let chars = bare.chars().count();
    if chars <= max || max < 8 {
        return bare.to_owned();
    }
    let head: String = bare.chars().take(max * 2 / 3).collect();
    let tail: String = bare.chars().skip(chars - (max - max * 2 / 3 - 1)).collect();
    format!("{head}…{tail}")
}

/// A JavaScript result for the terminal: strings raw, everything else as compact JSON.
pub fn render_value(value: &Value, max: usize) -> String {
    let text = match value {
        Value::String(text) => text.clone(),
        Value::Null => "undefined".to_owned(),
        other => serde_json::to_string(other).unwrap_or_default(),
    };
    let total = text.chars().count();
    if total <= max {
        return text;
    }
    let mut cut: String = text.chars().take(max).collect();
    let _ = write!(cut, "… ({total} chars; narrow the expression or raise --max)");
    cut
}

/// Cuts a multi-line text at a line boundary within `max` characters.
pub fn head_lines(text: &str, max: usize) -> (&str, usize) {
    if text.chars().count() <= max {
        return (text, 0);
    }
    let byte_limit = text.char_indices().nth(max).map_or(text.len(), |(i, _)| i);
    let end = text[..byte_limit].rfind('\n').unwrap_or(byte_limit);
    let rest = text[end..].lines().filter(|line| !line.is_empty()).count();
    (&text[..end], rest)
}

/// Seconds since the Unix epoch as ISO 8601 UTC with milliseconds: `2026-11-01T13:01:01.500Z`.
pub fn iso8601(seconds: f64) -> String {
    let millis = (seconds * 1000.0).round() as i64;
    let (days, ms) = (millis.div_euclid(86_400_000), millis.rem_euclid(86_400_000));
    format!("{}T{:02}:{:02}:{:02}.{:03}Z", date(days), ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Seconds since the Unix epoch as `YYYY-MM-DDTHH:MMZ`, the seconds cut off.
pub fn utc_minute(seconds: f64) -> String {
    let seconds = seconds.floor() as i64;
    let (days, rest) = (seconds.div_euclid(86_400), seconds.rem_euclid(86_400));
    format!("{}T{:02}:{:02}Z", date(days), rest / 3600, rest % 3600 / 60)
}

/// `YYYY-MM-DD` of a day counted from 1970-01-01, by Howard Hinnant's days-to-civil algorithm
/// (proleptic Gregorian calendar).
fn date(days: i64) -> String {
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era = (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 { month_index + 3 } else { month_index - 9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Creates or empties `path`, and the directories above it.
pub fn create(path: &Path) -> anyhow::Result<std::fs::File> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::File::create(path).with_context(|| format!("creating {}", path.display()))
}

/// Creates or empties `path`, and the directories above it, as a file only its owner can read
/// and write, for files holding secrets: saved logins, HAR files, response bodies, heap snapshots.
pub fn create_private(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
    // `mode` only applies to new files; an existing one may have been readable by others.
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(file)
}

/// Writes `bytes` to a file only its owner can read, replacing any earlier one.
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write as _;
    create_private(path)
        .and_then(|mut file| file.write_all(bytes))
        .with_context(|| format!("writing {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn truncation_respects_characters() {
        assert_eq!(truncate("héllo", 10), "héllo");
        assert_eq!(truncate("héllo wörld", 5), "héll…");
    }

    #[test]
    fn sizes_keep_two_or_three_significant_digits() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1023), "1023 B");
        assert_eq!(bytes(1234), "1.2 KB");
        assert_eq!(bytes(86_016), "84 KB");
        assert_eq!(bytes(3_565_158), "3.4 MB");
        assert_eq!(bytes(5 << 40), "5120 GB");
    }

    #[test]
    fn durations_switch_to_seconds() {
        assert_eq!(millis(0.42), "0.4 ms");
        assert_eq!(millis(119.6), "120 ms");
        assert_eq!(millis(1240.0), "1.24 s");
        assert_eq!(millis(12_500.0), "12.5 s");
    }

    #[test]
    fn urls_shorten_in_the_middle() {
        assert_eq!(short_url("https://example.com/", 50), "example.com");
        assert_eq!(short_url("http://localhost:3000/a", 50), "http://localhost:3000/a");
        let long = short_url(&format!("https://example.com/{}", "a".repeat(200)), 40);
        assert_eq!(long.chars().count(), 40);
        assert!(long.contains('…'));
    }

    #[test]
    fn values_render_compactly() {
        assert_eq!(render_value(&json!("text"), 100), "text");
        assert_eq!(render_value(&json!({"a": [1, 2]}), 100), r#"{"a":[1,2]}"#);
        assert_eq!(render_value(&Value::Null, 100), "undefined");
        assert!(render_value(&json!("x".repeat(50)), 10).starts_with("xxxxxxxxxx… (50 chars"));
    }

    #[test]
    fn head_lines_cuts_on_line_boundaries() {
        let text = "one\ntwo\nthree\nfour";
        assert_eq!(head_lines(text, 100), (text, 0));
        assert_eq!(head_lines(text, 9), ("one\ntwo", 2));
    }

    #[test]
    fn dates_are_utc() {
        assert_eq!(iso8601(0.0), "1970-01-01T00:00:00.000Z");
        assert_eq!(iso8601(951_782_400.0), "2000-02-29T00:00:00.000Z");
        assert_eq!(iso8601(1_798_761_599.999), "2026-12-31T23:59:59.999Z");
        assert_eq!(utc_minute(0.0), "1970-01-01T00:00Z");
        assert_eq!(utc_minute(1_793_538_061.5), "2026-11-01T13:01Z");
        assert_eq!(utc_minute(-1.0), "1969-12-31T23:59Z");
    }

    #[test]
    fn private_files_are_owner_only_even_when_replacing() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("hba-private-{}", std::process::id()));
        let path = dir.join("new/state.json");
        write_private(&path, b"first").expect("written with its directories");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        write_private(&path, b"secret").expect("replaced");
        let mode = std::fs::metadata(&path).expect("metadata").permissions().mode() & 0o777;
        assert_eq!((std::fs::read_to_string(&path).expect("read"), mode), ("secret".to_owned(), 0o600));
        std::fs::remove_dir_all(&dir).expect("cleanup");
    }
}
