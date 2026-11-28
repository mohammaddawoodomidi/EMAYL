//! SMTP replies: parsing, classification and plain-language explanations.
//!
//! Reply codes: 2xx success, 3xx "go on", 4xx temporary failure (try again
//! later), 5xx permanent failure. Enhanced status codes (RFC 3463, as recalled)
//! such as `5.1.1` say more precisely what went wrong.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EnhancedStatus {
    pub class: u8,
    pub subject: u16,
    pub detail: u16,
}

impl fmt::Display for EnhancedStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.class, self.subject, self.detail)
    }
}

impl EnhancedStatus {
    /// Parses `5.1.1` at the start of `text` (followed by space or end).
    pub fn parse_prefix(text: &str) -> Option<Self> {
        let token = text.split_whitespace().next()?;
        Self::parse(token)
    }

    pub fn parse(token: &str) -> Option<Self> {
        let mut it = token.split('.');
        let class: u8 = it.next()?.parse().ok()?;
        let subject: u16 = it.next()?.parse().ok()?;
        let detail: u16 = it.next()?.parse().ok()?;
        if it.next().is_some() || !matches!(class, 2 | 4 | 5) || subject > 999 || detail > 999 {
            return None;
        }
        Some(Self {
            class,
            subject,
            detail,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    /// Text of each line, without the code.
    pub lines: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyClass {
    Positive,
    Intermediate,
    Transient,
    Permanent,
}

/// One physical reply line: `250-text` (more follows) or `250 text` (last).
pub fn parse_line(line: &str) -> Option<(u16, bool, &str)> {
    let b = line.as_bytes();
    if b.len() < 3 || !b[..3].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let code: u16 = line[..3].parse().ok()?;
    if !(200..=599).contains(&code) {
        return None;
    }
    match b.get(3) {
        None => Some((code, false, "")),
        Some(b' ') => Some((code, false, &line[4..])),
        Some(b'-') => Some((code, true, &line[4..])),
        _ => None,
    }
}

impl Reply {
    pub fn class(&self) -> ReplyClass {
        match self.code / 100 {
            2 => ReplyClass::Positive,
            3 => ReplyClass::Intermediate,
            4 => ReplyClass::Transient,
            _ => ReplyClass::Permanent,
        }
    }

    pub fn is_positive(&self) -> bool {
        self.class() == ReplyClass::Positive
    }

    pub fn enhanced(&self) -> Option<EnhancedStatus> {
        self.lines
            .first()
            .and_then(|l| EnhancedStatus::parse_prefix(l))
    }

    /// Reply text, cleaned for display and logs, with the enhanced code removed.
    pub fn text(&self) -> String {
        let joined = self.lines.join(" ");
        let mut t = joined.trim();
        if let Some(e) = self.enhanced() {
            t = t.strip_prefix(&e.to_string()).unwrap_or(t).trim();
        }
        t.chars().filter(|c| !c.is_control()).take(300).collect()
    }
}

/// What a failure means for a non-technical person, and what to do about it.
pub fn explain(code: u16, enhanced: Option<EnhancedStatus>) -> &'static str {
    if let Some(e) = enhanced {
        let hit = match (e.class, e.subject, e.detail) {
            (5, 1, 1) | (5, 1, 10) => {
                Some("One of the addresses does not exist. Check it for typos.")
            }
            (5, 1, 2) => {
                Some("The part of the address after the @ does not exist. Check it for typos.")
            }
            (5, 1, 3) => Some("An address is not written correctly. Check it for typos."),
            (5, 2, 1) => Some("The recipient's mailbox is disabled or not accepting mail."),
            (5, 2, 2) | (4, 2, 2) => Some(
                "The recipient's mailbox is full. Try again later or contact them another way.",
            ),
            (5, 2, 3) | (5, 3, 4) => Some(
                "The message is too large for the recipient's or your provider's limit. Remove or shrink attachments.",
            ),
            (4, 2, 1) => {
                Some("The recipient's mailbox is temporarily unavailable. We will try again.")
            }
            (5, 7, 1) | (5, 7, 0) => Some(
                "The server refused the message for policy reasons (for example spam rules or a blocked sender or recipient).",
            ),
            (5, 7, 8) | (5, 7, 9) | (5, 7, 30) => Some(
                "Your provider requires a different sign-in. Use an app password or sign in with your browser.",
            ),
            (4, 7, _) => Some(
                "The receiving server asked us to wait or is applying temporary spam protection. We will try again.",
            ),
            (5, 7, _) => Some("The message was blocked by security or policy rules."),
            (5, 4, _) => Some("The message could not be routed to the recipient's server."),
            (4, 4, _) => {
                Some("The recipient's server could not be reached right now. We will try again.")
            }
            _ => None,
        };
        if let Some(t) = hit {
            return t;
        }
    }
    match code {
        421 => "The server is closing the connection (busy or shutting down). We will try again.",
        450..=452 => "The server could not accept the message right now. We will try again.",
        550 => {
            "The server refused the message or an address. Check the addresses; the reply text below explains more."
        }
        552 => "The message is too large for the server's limit. Remove or shrink attachments.",
        553 => "An address was refused as not allowed or not valid. Check it for typos.",
        554 => "The server refused the message (often spam or policy rules).",
        535 | 534 => {
            "The server rejected the sign-in. Check your password; some providers need an app password."
        }
        c if (400..500).contains(&c) => {
            "The server reported a temporary problem. We will try again."
        }
        _ => "The server refused the message. The reply text below explains more.",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_reply_lines() {
        assert_eq!(
            parse_line("250-SIZE 35882577"),
            Some((250, true, "SIZE 35882577"))
        );
        assert_eq!(parse_line("250 OK"), Some((250, false, "OK")));
        assert_eq!(parse_line("354"), Some((354, false, "")));
        for bad in [
            "", "25", "abc OK", "250OK", "999 x", "199 x", "600 x", "2500 x", "250_x",
        ] {
            assert_eq!(parse_line(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn enhanced_codes() {
        assert_eq!(EnhancedStatus::parse("5.1.1").unwrap().to_string(), "5.1.1");
        for bad in ["5.1", "1.1.1", "5.1.1.1", "5.x.1", "5.1.9999", "", "3.0.0"] {
            assert!(EnhancedStatus::parse(bad).is_none(), "{bad:?}");
        }
        let r = Reply {
            code: 550,
            lines: vec![
                "5.1.1 <x@y>: Recipient address rejected".into(),
                "second".into(),
            ],
        };
        assert_eq!(r.enhanced().unwrap().to_string(), "5.1.1");
        assert_eq!(r.text(), "<x@y>: Recipient address rejected second");
        assert_eq!(r.class(), ReplyClass::Permanent);
    }

    #[test]
    fn text_is_sanitised_and_bounded() {
        let r = Reply {
            code: 250,
            lines: vec![format!("ok\u{7}{}", "x".repeat(1000))],
        };
        assert!(r.text().len() <= 300 && !r.text().contains('\u{7}'));
    }

    #[test]
    fn explanations_prefer_enhanced_codes_then_basic_codes() {
        let e = EnhancedStatus::parse("5.2.2");
        assert!(explain(550, e).contains("full"));
        assert!(explain(550, None).contains("refused"));
        assert!(explain(451, None).contains("try again"));
        assert!(explain(552, None).contains("too large"));
        assert!(explain(450, EnhancedStatus::parse("4.7.1")).contains("wait"));
        assert!(explain(599, None).contains("refused"));
    }
}
