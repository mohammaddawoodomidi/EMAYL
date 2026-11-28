//! Small pure helpers: dates, names and colours for the list.

use chrono::{DateTime, Datelike, FixedOffset, Local, Offset, TimeZone, Utc};
use iced::Color;

/// Local time zone offset in seconds east of UTC (0 if unknown).
pub fn local_offset_secs() -> i32 {
    Local::now().offset().fix().local_minus_utc()
}

fn at(ts: i64, offset: i32) -> Option<DateTime<FixedOffset>> {
    let tz = FixedOffset::east_opt(offset)?;
    Utc.timestamp_opt(ts, 0)
        .single()
        .map(|d| d.with_timezone(&tz))
}

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Short date for the message list: "14:05" today, "Nov 13" this year, "13 Nov 2025" before.
pub fn short_when(ts: i64, now: i64, offset: i32) -> String {
    let (Some(d), Some(n)) = (at(ts, offset), at(now, offset)) else {
        return String::new();
    };
    if ts <= 0 {
        return String::new();
    }
    if d.date_naive() == n.date_naive() {
        return d.format("%H:%M").to_string();
    }
    let month = MONTHS[d.month0() as usize];
    if d.year() == n.year() {
        format!("{month} {}", d.day())
    } else {
        format!("{} {month} {}", d.day(), d.year())
    }
}

/// Full date for the reader header: "Tue 17 Nov 2026, 10:00".
pub fn long_when(ts: i64, offset: i32) -> String {
    match at(ts, offset) {
        Some(d) if ts > 0 => format!(
            "{} {} {} {}, {}",
            d.format("%a"),
            d.day(),
            MONTHS[d.month0() as usize],
            d.year(),
            d.format("%H:%M")
        ),
        _ => String::new(),
    }
}

/// The name to show for a sender: the display name, else the address.
pub fn display_sender(name: &str, addr: &str) -> String {
    let n = name.trim();
    if n.is_empty() {
        addr.trim().to_owned()
    } else {
        n.to_owned()
    }
}

/// First letter (or digit) for the round avatar; "?" if there is none.
pub fn initial(name: &str, addr: &str) -> String {
    display_sender(name, addr)
        .chars()
        .find(|c| c.is_alphanumeric())
        .map_or_else(|| "?".to_owned(), |c| c.to_uppercase().collect())
}

const AVATARS: [(u8, u8, u8); 8] = [
    (0x3B, 0x6C, 0xF6),
    (0x8B, 0x5C, 0xF6),
    (0xE0, 0x6C, 0x9F),
    (0xF0, 0x8A, 0x3C),
    (0x1F, 0x9D, 0x6B),
    (0x0E, 0x9F, 0xB5),
    (0xD6, 0x45, 0x45),
    (0x64, 0x74, 0x8B),
];

/// A stable colour per sender (same address, same colour).
pub fn avatar_color(addr: &str) -> Color {
    let mut h: u32 = 2_166_136_261;
    for b in addr.trim().to_ascii_lowercase().bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(16_777_619);
    }
    let (r, g, b) = AVATARS[(h as usize) % AVATARS.len()];
    Color::from_rgb8(r, g, b)
}

/// The colour that stands for the account at this place in the account list. Unlike
/// `avatar_color` this never gives two accounts the same colour (up to eight accounts; after
/// that colours repeat), so the side bar and the list can tell them apart at a glance.
pub fn account_color(index: usize) -> Color {
    // Ordered so that neighbours differ as much as possible.
    const ORDER: [usize; 8] = [0, 3, 4, 1, 5, 2, 6, 7];
    let (r, g, b) = AVATARS[ORDER[index % ORDER.len()]];
    Color::from_rgb8(r, g, b)
}

/// Human-readable size: "12 KB", "3.4 MB".
pub fn size_text(bytes: usize) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{:.1} MB", b / (KB * KB))
    }
}

/// Cuts text to `max` characters, adding an ellipsis.
pub fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_owned();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Longest web address handed to the operating system.
const MAX_OPEN_URL: usize = 16 * 1024;

/// The last check before an address is given to the operating system: it must be a plain
/// web address with no control characters or spaces (some openers split on those or pass
/// them on to other programs). The link check upstream already guarantees this for links
/// from messages; this keeps the promise even if a caller is changed later.
pub(crate) fn checked_web_url(url: &str) -> Result<&str, String> {
    let lower = url.to_ascii_lowercase();
    if !(lower.starts_with("https://") || lower.starts_with("http://")) {
        return Err("Only web addresses can be opened.".into());
    }
    if url.len() > MAX_OPEN_URL || url.chars().any(|c| c.is_control() || c.is_whitespace()) {
        return Err("This web address is not safe to open.".into());
    }
    Ok(url)
}

/// Opens an address in the default browser. Only web addresses ever get here
/// (the link check refuses everything else).
pub fn open_in_browser(url: &str) -> Result<(), String> {
    let url = checked_web_url(url)?;
    open::that_detached(url).map_err(|_| "Could not open your web browser.".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    const NOW: i64 = 1_794_909_600; // 2026-11-17 10:00:00 UTC

    #[test]
    fn dates_in_the_list() {
        assert_eq!(short_when(NOW - 3600, NOW, 0), "09:00");
        assert_eq!(short_when(NOW - 86_400 * 4, NOW, 0), "Nov 13");
        assert_eq!(short_when(NOW - 86_400 * 400, NOW, 0), "13 Oct 2025");
        assert_eq!(short_when(0, NOW, 0), "");
    }

    #[test]
    fn the_time_zone_decides_what_today_is() {
        // 23:30 UTC the day before is already "today" for someone at UTC+2 at 01:00.
        let now = 1_794_909_600 - 9 * 3600; // 01:00 UTC on 17 Nov
        let ts = now - 3600; // 00:00 UTC
        assert_eq!(short_when(ts, now, 2 * 3600), "02:00");
        assert_eq!(short_when(ts - 7200, now, 0), "Nov 16");
    }

    #[test]
    fn long_date() {
        assert_eq!(long_when(NOW, 0), "Tue 17 Nov 2026, 10:00");
        assert_eq!(long_when(NOW, 3600), "Tue 17 Nov 2026, 11:00");
        assert_eq!(long_when(0, 0), "");
    }

    #[test]
    fn names_and_initials() {
        assert_eq!(display_sender(" ", "a@b.c"), "a@b.c");
        assert_eq!(initial("élodie", "x@y.z"), "É");
        assert_eq!(initial("", "bob@example.com"), "B");
        assert_eq!(initial("😀 ", "?@?"), "?");
        assert_eq!(
            avatar_color("Bob@Example.com"),
            avatar_color(" bob@example.com")
        );
    }

    #[test]
    fn only_plain_web_addresses_reach_the_operating_system() {
        assert!(checked_web_url("https://example.com/a?b=1#c").is_ok());
        assert!(checked_web_url("HTTP://example.com/").is_ok());
        for bad in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "mailto:a@example.com",
            "-https://example.com",
            "ms-msdt:/id PCWDiagnostic",
            "https://example.com/a b",
            "https://example.com/\n--flag",
            "https://example.com/\u{0}",
            "https://example.com/\u{85}",
            "",
        ] {
            assert!(checked_web_url(bad).is_err(), "{bad:?}");
        }
        let long = format!("https://example.com/{}", "a".repeat(MAX_OPEN_URL));
        assert!(checked_web_url(&long).is_err());
    }

    #[test]
    fn sizes_and_clipping() {
        assert_eq!(size_text(10), "10 B");
        assert_eq!(size_text(2048), "2 KB");
        assert_eq!(size_text(3_500_000), "3.3 MB");
        assert_eq!(clip("hello", 10), "hello");
        assert_eq!(clip("hello world", 6), "hello…");
        assert_eq!(clip("ééé", 2), "é…");
    }
}
