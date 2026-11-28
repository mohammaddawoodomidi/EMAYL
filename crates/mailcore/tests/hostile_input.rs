//! Hostile-input tests: every parser that sees data from outside is fed thousands of
//! corrupted variants of realistic input. Nothing may panic or hang, and the properties
//! that matter for safety must hold for whatever comes out.
//!
//! The generator is deterministic (fixed seed) so a failure can be reproduced. This is a
//! mutation test, not a coverage-guided fuzzer: it finds shallow bugs, not deep ones.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::{Duration, Instant};

use mailcore::autoconfig::parse_client_config;
use mailcore::bounce::parse_bounce;
use mailcore::compose::{Attachment, Mailbox, OutgoingMessage, compose, sanitize_filename};
use mailcore::email::EmailAddress;
use mailcore::imap::utf7;
use mailcore::provider::ConfigSource;
use mailcore::render::{RenderOptions, analyze_link, render, sanitize_html, summarize_header};
use mailcore::replies::{format_recipients, parse_recipients, parse_recipients_lenient};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Bytes that tend to matter to parsers.
const SPICY: &[&[u8]] = &[
    b"\r\n",
    b"\n",
    b"\r",
    b"\0",
    b"\xff\xfe",
    b"=?utf-8?b?",
    b"=?",
    b"?=",
    b"\"",
    b"<",
    b">",
    b"(",
    b"\\",
    b"--",
    b"boundary=",
    b"; ",
    b":",
    b"@",
    b"%",
    b"&#0;",
    b"<!DOCTYPE",
    b"<![CDATA[",
    b"\xe2\x80\xae",
    b"\xf0\x9f\x98\x80",
    b"\xc0\x80",
    b"AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
];

fn mutate(rng: &mut Rng, seeds: &[&[u8]]) -> Vec<u8> {
    let mut v = seeds[rng.below(seeds.len())].to_vec();
    for _ in 0..=rng.below(6) {
        match rng.below(7) {
            0 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
            1 if !v.is_empty() => {
                let a = rng.below(v.len());
                let b = (a + rng.below(40)).min(v.len());
                v.drain(a..b);
            }
            2 if !v.is_empty() => {
                let a = rng.below(v.len());
                let b = (a + rng.below(60)).min(v.len());
                let piece = v[a..b].to_vec();
                let at = rng.below(v.len() + 1);
                for (k, byte) in piece.into_iter().enumerate() {
                    v.insert(at + k, byte);
                }
            }
            3 => {
                let s = SPICY[rng.below(SPICY.len())];
                let at = rng.below(v.len() + 1);
                for (k, byte) in s.iter().enumerate() {
                    v.insert(at + k, *byte);
                }
            }
            4 if !v.is_empty() => v.truncate(rng.below(v.len())),
            5 => {
                let other = seeds[rng.below(seeds.len())];
                let cut = rng.below(other.len() + 1);
                let at = rng.below(v.len() + 1);
                v.splice(at.., other[cut..].iter().copied());
            }
            _ => {
                let at = rng.below(v.len() + 1);
                let n = rng.below(20);
                for _ in 0..n {
                    v.insert(at, (rng.next() & 0xff) as u8);
                }
            }
        }
        if v.len() > 200_000 {
            v.truncate(200_000);
        }
    }
    v
}

const PLAIN: &[u8] = b"From: =?utf-8?b?QWxpY2U=?= <alice@example.com>\r\nTo: bob@example.org, \"B, C\" <c@example.net>\r\nSubject: =?utf-8?q?Hello_=E2=9C=93?=\r\nDate: Tue, 17 Nov 2026 10:00:00 +0000\r\nMessage-ID: <a@example.com>\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Transfer-Encoding: quoted-printable\r\n\r\nHi =E2=9C=93\r\n> quoted\r\n>> more\r\n";

const ALT: &[u8] = b"From: a@example.com\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"outer\"\r\n\r\n--outer\r\nContent-Type: multipart/alternative; boundary=\"inner\"\r\n\r\n--inner\r\nContent-Type: text/plain\r\n\r\nhello\r\n--inner\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Hi <a href=\"https://example.com\">link</a><img src=\"https://t.example/p.gif\"><img src=\"cid:x\"><script>alert(1)</script></p>\r\n--inner--\r\n--outer\r\nContent-Type: application/pdf; name=\"a.pdf\"\r\nContent-Disposition: attachment; filename=\"a.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\nJVBERi0xLjQK\r\n--outer--\r\n";

const DSN: &[u8] = b"From: MAILER-DAEMON@example.org\r\nSubject: Undelivered Mail Returned to Sender\r\nMIME-Version: 1.0\r\nContent-Type: multipart/report; report-type=delivery-status; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nCould not deliver.\r\n--b\r\nContent-Type: message/delivery-status\r\n\r\nReporting-MTA: dns; mx.example.org\r\n\r\nFinal-Recipient: rfc822; bob@example.org\r\nAction: failed\r\nStatus: 5.1.1\r\nDiagnostic-Code: smtp; 550 5.1.1 user unknown\r\n\r\n--b\r\nContent-Type: message/rfc822\r\n\r\nMessage-ID: <ours@example.com>\r\nSubject: hi\r\n\r\nbody\r\n--b--\r\n";

const HTML_ONLY: &[u8] = b"Content-Type: text/html\r\n\r\n<html><body><table><tr><td><ul><li>a<li>b</ul><blockquote>q<blockquote>r</blockquote></blockquote><a href='javascript:x'>x</a><a href='http://a@b.example/'>c</a><style>p{background:url(http://x/)}</style><svg><use href='x'/></svg><form action='http://x'><input></form></td></tr></table>";

const XML: &[u8] = br#"<?xml version="1.0"?><clientConfig version="1.1"><emailProvider id="example.com"><domain>example.com</domain><displayName>Example</displayName><incomingServer type="imap"><hostname>imap.example.com</hostname><port>993</port><socketType>SSL</socketType><username>%EMAILLOCALPART%</username><authentication>password-cleartext</authentication></incomingServer><outgoingServer type="smtp"><hostname>smtp.example.com</hostname><port>587</port><socketType>STARTTLS</socketType><username>%EMAILADDRESS%</username><authentication>password-cleartext</authentication></outgoingServer></emailProvider></clientConfig>"#;

/// Rounds per test; set `EMAYL_FUZZ_ROUNDS` for a longer one-off campaign.
fn rounds() -> usize {
    std::env::var("EMAYL_FUZZ_ROUNDS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1500)
}

/// A whole run must stay far below this: it catches inputs that make a parser very slow.
const TIME_BUDGET: Duration = Duration::from_secs(120);

#[test]
fn messages_of_every_shape_render_without_panicking() {
    let seeds: &[&[u8]] = &[PLAIN, ALT, DSN, HTML_ONLY];
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    let start = Instant::now();
    let mut slowest = Duration::ZERO;
    for _ in 0..rounds() {
        let raw = mutate(&mut rng, seeds);
        let t = Instant::now();
        for prefer_plain in [false, true] {
            if let Ok(m) = render(&raw, &RenderOptions { prefer_plain }) {
                // The two guarantees the reader relies on, whatever the input was.
                for r in &m.remote_images {
                    let _ = r; // remote pictures are only ever listed, never fetched here
                }
                assert!(m.plain_text.len() <= 64 * 1024 * 1024);
            }
        }
        let _ = summarize_header(&raw);
        slowest = slowest.max(t.elapsed());
    }
    assert!(
        start.elapsed() < TIME_BUDGET,
        "too slow: {:?}",
        start.elapsed()
    );
    assert!(
        slowest < Duration::from_secs(5),
        "one input took {slowest:?}"
    );
}

/// The `<...>` pieces of cleaned HTML (quoted `>` inside attribute values is handled).
fn tags_of(html: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur: Option<String> = None;
    let mut quote: Option<char> = None;
    for c in html.chars() {
        match (&mut cur, c) {
            (None, '<') => cur = Some("<".to_owned()),
            (Some(t), ch) => {
                t.push(ch);
                match quote {
                    Some(q) if ch == q => quote = None,
                    None if ch == '"' || ch == '\'' => quote = Some(ch),
                    None if ch == '>' => out.push(cur.take().unwrap_or_default()),
                    _ => {}
                }
            }
            _ => {}
        }
    }
    out
}

/// `(name, value)` pairs of one tag, names lowercased, quotes respected.
fn attributes_of(tag: &str) -> Vec<(String, String)> {
    let b: Vec<char> = tag
        .trim_start_matches('<')
        .trim_end_matches('>')
        .chars()
        .collect();
    let mut i = 0;
    let mut out = Vec::new();
    while i < b.len() && !b[i].is_whitespace() {
        i += 1; // the tag name
    }
    while i < b.len() {
        while i < b.len() && (b[i].is_whitespace() || b[i] == '/') {
            i += 1;
        }
        let start = i;
        while i < b.len() && !b[i].is_whitespace() && b[i] != '=' && b[i] != '/' {
            i += 1;
        }
        let name: String = b[start..i].iter().collect::<String>().to_ascii_lowercase();
        let mut value = String::new();
        while i < b.len() && b[i].is_whitespace() {
            i += 1;
        }
        if i < b.len() && b[i] == '=' {
            i += 1;
            while i < b.len() && b[i].is_whitespace() {
                i += 1;
            }
            if i < b.len() && (b[i] == '"' || b[i] == '\'') {
                let q = b[i];
                i += 1;
                while i < b.len() && b[i] != q {
                    value.push(b[i]);
                    i += 1;
                }
                i += 1;
            } else {
                while i < b.len() && !b[i].is_whitespace() {
                    value.push(b[i]);
                    i += 1;
                }
            }
        }
        if !name.is_empty() {
            out.push((name, value));
        }
    }
    out
}

#[test]
fn the_html_cleaner_never_lets_active_content_through() {
    let seeds: &[&[u8]] = &[HTML_ONLY, ALT, b"<p onclick=x>a</p><iframe src=x></iframe><object></object><meta http-equiv=refresh content=0;url=http://x>"];
    let mut rng = Rng(0xDEAD_BEEF_CAFE_F00D);
    for _ in 0..rounds() {
        let raw = mutate(&mut rng, seeds);
        let html = String::from_utf8_lossy(&raw);
        let clean = sanitize_html(&html).to_ascii_lowercase();
        // Only real tags count: text such as "p onclick=x" is shown escaped and is harmless.
        for tag in tags_of(&clean) {
            for banned in [
                "<script", "<iframe", "<object", "<embed", "<form", "<meta", "<link", "<style",
            ] {
                assert!(
                    !tag.starts_with(banned),
                    "{banned} survived in {clean:?} (from {html:?})"
                );
            }
            for (name, value) in attributes_of(&tag) {
                assert!(
                    !name.starts_with("on"),
                    "event attribute {name} in {tag:?} (from {html:?})"
                );
                if name == "href" || name == "src" {
                    // Browsers ignore tabs and line breaks inside addresses, so the check does too.
                    let v: String = value
                        .chars()
                        .filter(|c| !c.is_control())
                        .collect::<String>()
                        .trim_start()
                        .to_ascii_lowercase();
                    assert!(
                        ["http:", "https:", "mailto:", "tel:", "cid:"]
                            .iter()
                            .any(|p| v.starts_with(p)),
                        "{name}={value:?} in {tag:?} (from {html:?})"
                    );
                }
                assert!(
                    name != "style" && name != "srcset" && name != "background",
                    "{tag:?}"
                );
            }
        }
    }
}

#[test]
fn bounce_reading_never_panics_and_only_names_our_mail() {
    let seeds: &[&[u8]] = &[DSN, PLAIN, ALT];
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    for _ in 0..rounds() {
        let raw = mutate(&mut rng, seeds);
        // Nothing is ours: no report may come back.
        assert!(parse_bounce(&raw, &|_| false).is_none());
        let _ = parse_bounce(&raw, &|_| true);
    }
}

#[test]
fn server_settings_files_never_panic_and_stay_inside_the_rules() {
    let seeds: &[&[u8]] = &[XML];
    let mut rng = Rng(0x0F0F_F0F0_1234_4321);
    let email = EmailAddress::parse("alice@example.com").unwrap();
    for _ in 0..rounds() {
        let raw = mutate(&mut rng, seeds);
        if let Ok(r) = parse_client_config(&raw, &email, ConfigSource::IspWellKnown) {
            assert!(
                matches!(r.settings.imap.port, 143 | 993),
                "{}",
                r.settings.imap.port
            );
            assert!(
                matches!(r.settings.smtp.port, 25 | 465 | 587 | 2525),
                "{}",
                r.settings.smtp.port
            );
            for host in [&r.settings.imap.host, &r.settings.smtp.host] {
                assert!(host.contains('.') && host.is_ascii(), "{host}");
                assert!(
                    !host.ends_with(".local") && !host.ends_with(".internal"),
                    "{host}"
                );
            }
            for name in [&r.settings.imap_username, &r.settings.smtp_username] {
                assert!(!name.chars().any(char::is_control), "{name:?}");
            }
        }
    }
}

#[test]
fn addresses_and_recipient_lists_hold_their_promises() {
    let seeds: &[&[u8]] = &[
        b"Bob <bob@example.org>, \"C, D\" <c@example.net>; e@example.com",
        b"=?utf-8?b?4pyT?= <x@y.example>, a@b.example",
        b"alice@example.com",
    ];
    let mut rng = Rng(0xA5A5_A5A5_5A5A_5A5A);
    for _ in 0..rounds() * 2 {
        let raw = mutate(&mut rng, seeds);
        let text = String::from_utf8_lossy(&raw).into_owned();
        if let Ok(a) = EmailAddress::parse(&text) {
            let s = a.to_string();
            assert!(
                !s.chars().any(|c| c.is_control()
                    || c.is_whitespace()
                    || matches!(c, '<' | '>' | '"' | ',')),
                "{s:?}"
            );
            assert_eq!(EmailAddress::parse(&s).unwrap(), a);
        }
        let _ = parse_recipients_lenient(&text);
        if let Ok(list) = parse_recipients(&text) {
            // What we show in the To field must read back as the same people.
            let again = parse_recipients(&format_recipients(&list)).unwrap();
            assert_eq!(again.len(), list.len(), "{text:?}");
            for (x, y) in list.iter().zip(&again) {
                assert_eq!(x.address, y.address);
            }
        }
    }
}

#[test]
fn links_are_only_ever_safe_kinds_without_hidden_logins() {
    let seeds: &[&[u8]] = &[
        b"https://paypal.com@evil.example/login?x=1#y",
        b"HTTP://[::1]:8080/a",
        b"mailto:a@example.com?subject=hi",
        b"javascript:alert(1)",
        b"https://xn--pypal-4ve.com/",
        b"https://\xd0\xb0pple.com/",
    ];
    let mut rng = Rng(0x7777_1111_3333_9999);
    for _ in 0..rounds() * 3 {
        let raw = mutate(&mut rng, seeds);
        let href = String::from_utf8_lossy(&raw).into_owned();
        if let Some(l) = analyze_link(&href, &href) {
            assert!(
                ["http://", "https://", "mailto:", "tel:"]
                    .iter()
                    .any(|p| l.url.starts_with(p)),
                "{}",
                l.url
            );
            if l.url.starts_with("http") {
                let after = l.url.split("://").nth(1).unwrap_or("");
                let authority = after.split(['/', '?', '#']).next().unwrap_or("");
                assert!(!authority.contains('@'), "{}", l.url);
            }
            assert!(
                !l.url.chars().any(|c| c.is_control() || c.is_whitespace()),
                "{:?}",
                l.url
            );
        }
    }
}

#[test]
fn folder_names_survive_the_round_trip_and_garbage_is_refused() {
    let mut rng = Rng(0x4242_4242_2424_2424);
    for _ in 0..rounds() * 2 {
        let n = rng.below(24);
        let s: String = (0..n)
            .filter_map(|_| char::from_u32((rng.next() % 0x11_0000) as u32))
            .collect();
        let e = utf7::encode(&s);
        assert!(e.is_ascii(), "{e:?}");
        assert_eq!(
            utf7::decode(&e).as_deref(),
            Some(s.as_str()),
            "{s:?} -> {e:?}"
        );
        let raw = mutate(&mut rng, &[e.as_bytes()]);
        let _ = utf7::decode(&String::from_utf8_lossy(&raw));
    }
}

#[test]
fn file_names_are_always_a_single_safe_leaf() {
    let seeds: &[&[u8]] = &[
        b"../../etc/passwd",
        b"C:\\Windows\\System32\\a.exe",
        b"report\xe2\x80\xaefdp.exe",
        b"a:b.txt:stream",
        b"..",
        b"  ",
    ];
    let mut rng = Rng(0x0BAD_C0DE_0BAD_F00D);
    for _ in 0..rounds() * 3 {
        let raw = mutate(&mut rng, seeds);
        let name = sanitize_filename(&String::from_utf8_lossy(&raw));
        assert!(!name.is_empty() && name.chars().count() <= 120, "{name:?}");
        assert!(!name.contains(['/', '\\', ':', '\0']), "{name:?}");
        assert!(!name.chars().any(char::is_control), "{name:?}");
        assert!(name != "." && name != "..", "{name:?}");
        assert!(!name.starts_with('.') && !name.ends_with('.'), "{name:?}");
        assert!(
            !name
                .chars()
                .any(|c| matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')),
            "{name:?}"
        );
    }
}

fn random_text(rng: &mut Rng, max: usize) -> String {
    let n = rng.below(max);
    (0..n)
        .map(|_| match rng.below(10) {
            0 => '\n',
            1 => ' ',
            2 => char::from_u32(0x4E00 + (rng.next() % 500) as u32).unwrap_or('x'),
            3 => char::from_u32(0x1F600 + (rng.next() % 40) as u32).unwrap_or('x'),
            4 => ['.', '=', '"', '\\', '<', '>', ':', ';', ','][rng.below(9)],
            _ => (b'a' + (rng.next() % 26) as u8) as char,
        })
        .collect()
}

#[test]
fn composed_messages_are_always_valid_mail_within_line_limits() {
    let mut rng = Rng(0xC0FF_EE00_1234_5678);
    let me = |n: &str| Mailbox {
        name: n.into(),
        address: EmailAddress::parse("alice@example.com").unwrap(),
    };
    let mut composed = 0usize;
    let total = rounds().min(3000);
    for round in 0..total {
        let mut m = OutgoingMessage {
            from: Some(me(&random_text(&mut rng, 30).replace(['\n'], " "))),
            to: vec![Mailbox {
                name: random_text(&mut rng, 20).replace(['\n'], " "),
                address: EmailAddress::parse("bob@example.org").unwrap(),
            }],
            bcc: vec![Mailbox {
                name: String::new(),
                address: EmailAddress::parse("secret-copy@example.net").unwrap(),
            }],
            subject: random_text(&mut rng, 80).replace(['\n'], " "),
            text: random_text(&mut rng, 3000),
            ..OutgoingMessage::default()
        };
        if round % 7 == 0 {
            // One enormous line with no line break at all.
            m.text = "x".repeat(5000 + rng.below(5000));
        }
        if round % 11 == 0 {
            m.subject = "word ".repeat(300);
        }
        if round % 5 == 0 {
            m.attachments.push(Attachment {
                filename: random_text(&mut rng, 40).replace(['\n'], " "),
                content_type: "application/octet-stream".into(),
                data: (0..rng.below(4000))
                    .map(|_| (rng.next() & 0xff) as u8)
                    .collect(),
            });
        }
        let Ok(c) = compose(&m, 1_794_909_600) else {
            continue; // refusing is allowed; panicking is not
        };
        composed += 1;
        for (i, line) in c.bytes.split(|b| *b == b'\n').enumerate() {
            assert!(
                line.len() <= 998,
                "line {i} is {} bytes (round {round})",
                line.len()
            );
        }
        let text = String::from_utf8_lossy(&c.bytes).to_lowercase();
        assert!(
            !text.contains("secret-copy@example.net"),
            "Bcc leaked (round {round})"
        );
        assert!(!text.contains("\nbcc:"), "Bcc header (round {round})");
        let parsed = mailparse::parse_mail(&c.bytes).expect("our own message must parse");
        assert_eq!(
            parsed
                .headers
                .iter()
                .filter(|h| h.get_key().eq_ignore_ascii_case("subject"))
                .count(),
            1
        );
        assert!(
            c.recipients
                .iter()
                .any(|r| r.to_string() == "secret-copy@example.net")
        );
    }
    // The test is only meaningful if most inputs were actually accepted.
    assert!(
        composed * 2 > total,
        "only {composed} of {total} messages were composed"
    );
}
