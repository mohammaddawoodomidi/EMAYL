//! Reading delivery-failure messages ("bounces") and linking them back to the
//! message that caused them.
//!
//! Standard delivery status notifications (RFC 3464, as recalled: a
//! `multipart/report; report-type=delivery-status` message holding a
//! `message/delivery-status` part and the original message or its headers) are
//! parsed exactly. Older or non-standard bounces are recognised heuristically and
//! reported with `confidence: Heuristic` and no per-recipient detail.
//!
//! A bounce is just an e-mail anyone can send, so it is only ever applied to an
//! outbox item of the account that received it, and only via the original's
//! Message-ID (128 random bits, not guessable by outsiders).

use mailparse::{MailHeaderMap, ParsedMail};

use crate::smtp::EnhancedStatus;

/// Bytes of a bounce examined; the original message is often attached in full.
const MAX_INPUT: usize = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BounceAction {
    Failed,
    Delayed,
    Delivered,
    Relayed,
    Expanded,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BounceKind {
    /// Permanent: the mail will not be delivered.
    Hard,
    /// Temporary or a delay warning: the sending server is still trying.
    Soft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    Standard,
    Heuristic,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BounceRecipient {
    pub address: String,
    pub action: BounceAction,
    /// Enhanced status such as `5.1.1`.
    pub status: Option<String>,
    pub diagnostic: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BounceReport {
    /// Message-ID of the original, without angle brackets.
    pub original_message_id: String,
    pub recipients: Vec<BounceRecipient>,
    pub kind: BounceKind,
    pub confidence: Confidence,
}

impl BounceRecipient {
    /// Plain-language reason.
    pub fn explanation(&self) -> &'static str {
        let enhanced = self.status.as_deref().and_then(EnhancedStatus::parse);
        let code = self
            .diagnostic
            .as_deref()
            .and_then(|d| {
                d.split_whitespace()
                    .find_map(|w| w.parse::<u16>().ok().filter(|c| (400..600).contains(c)))
            })
            .unwrap_or(match enhanced.map(|e| e.class) {
                Some(4) => 450,
                _ => 550,
            });
        crate::smtp::explain(code, enhanced)
    }
}

fn clean(s: &str, max: usize) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(max)
        .collect::<String>()
        .trim()
        .to_owned()
}

fn strip_brackets(id: &str) -> Option<String> {
    let id = id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    let ok = !id.is_empty()
        && id.len() <= 255
        && id.contains('@')
        && !id
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>'));
    ok.then(|| id.to_owned())
}

fn first_message_id(headers: &[mailparse::MailHeader<'_>]) -> Option<String> {
    headers
        .get_first_value("Message-ID")
        .and_then(|v| strip_brackets(&v))
}

fn address_of(value: &str) -> Option<String> {
    let v = value.split_once(';').map_or(value, |(_, rest)| rest);
    let v = v
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    let ok = v.len() <= 254
        && v.contains('@')
        && !v.chars().any(|c| c.is_control() || c.is_whitespace());
    ok.then(|| v.to_ascii_lowercase())
}

fn parse_action(v: &str) -> BounceAction {
    match v.trim().to_ascii_lowercase().as_str() {
        "failed" => BounceAction::Failed,
        "delayed" => BounceAction::Delayed,
        "delivered" => BounceAction::Delivered,
        "relayed" => BounceAction::Relayed,
        "expanded" => BounceAction::Expanded,
        _ => BounceAction::Unknown,
    }
}

fn walk<'a>(part: &'a ParsedMail<'a>, out: &mut Vec<&'a ParsedMail<'a>>) {
    out.push(part);
    for p in &part.subparts {
        walk(p, out);
    }
}

fn recipients_from_status(body: &[u8]) -> Vec<BounceRecipient> {
    let text = String::from_utf8_lossy(body).replace("\r\n", "\n");
    let mut out = Vec::new();
    for block in text.split("\n\n").skip(1) {
        if out.len() >= 200 {
            break;
        }
        let Ok((headers, _)) = mailparse::parse_headers(block.trim_start_matches('\n').as_bytes())
        else {
            continue;
        };
        let Some(address) = headers
            .get_first_value("Final-Recipient")
            .as_deref()
            .and_then(address_of)
        else {
            continue;
        };
        out.push(BounceRecipient {
            address,
            action: headers
                .get_first_value("Action")
                .map_or(BounceAction::Unknown, |a| parse_action(&a)),
            status: headers
                .get_first_value("Status")
                .and_then(|s| EnhancedStatus::parse(s.trim()))
                .map(|e| e.to_string()),
            diagnostic: headers
                .get_first_value("Diagnostic-Code")
                .map(|d| clean(&d, 300))
                .filter(|d| !d.is_empty()),
        });
    }
    out
}

fn kind_of(recipients: &[BounceRecipient]) -> BounceKind {
    let hard = recipients.iter().any(|r| {
        r.action == BounceAction::Failed || r.status.as_deref().is_some_and(|s| s.starts_with('5'))
    });
    if hard {
        BounceKind::Hard
    } else {
        BounceKind::Soft
    }
}

/// Subjects and senders typical of automatic failure notices.
fn looks_like_failure_notice(p: &ParsedMail<'_>) -> bool {
    let from = p
        .headers
        .get_first_value("From")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let subject = p
        .headers
        .get_first_value("Subject")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let auto = p.headers.get_first_value("Auto-Submitted").is_some();
    let from_daemon = from.contains("mailer-daemon") || from.contains("postmaster");
    let subj = [
        "undeliverable",
        "undelivered",
        "delivery status notification",
        "delivery failure",
        "delivery has failed",
        "failure notice",
        "returned mail",
        "mail delivery failed",
        "could not be delivered",
        "not delivered",
    ]
    .iter()
    .any(|s| subject.contains(s));
    (from_daemon || auto) && subj
}

/// Looks for a `Message-ID: <...>` line anywhere in the bounce's parts: bounces
/// without a machine-readable report usually quote the original's headers.
fn scan_for_message_id(parts: &[&ParsedMail<'_>], ours: &dyn Fn(&str) -> bool) -> Option<String> {
    for p in parts {
        let Ok(body) = p.get_body() else { continue };
        for line in body.lines().take(5000) {
            let l = line.trim_start_matches(['>', ' ', '\t']);
            // `get` (not slicing) so a multi-byte character at byte 11 cannot panic.
            if let Some(head) = l.get(..11)
                && head.eq_ignore_ascii_case("message-id:")
                && let Some(id) = l.get(11..).and_then(strip_brackets)
                && ours(&id)
            {
                return Some(id);
            }
        }
    }
    None
}

/// Examines a received message. `is_ours` says whether a Message-ID belongs to
/// something this app sent (so a bounce-shaped message that quotes only other
/// people's mail is ignored). Returns `None` if it is not a recognisable bounce
/// of one of our messages.
pub fn parse_bounce(raw: &[u8], is_ours: &dyn Fn(&str) -> bool) -> Option<BounceReport> {
    let raw = &raw[..raw.len().min(MAX_INPUT)];
    let msg = mailparse::parse_mail(raw).ok()?;
    let mut parts = Vec::new();
    walk(&msg, &mut parts);

    let is_dsn = msg.ctype.mimetype.eq_ignore_ascii_case("multipart/report")
        && msg
            .ctype
            .params
            .get("report-type")
            .is_some_and(|t| t.eq_ignore_ascii_case("delivery-status"));

    if is_dsn {
        let status_part = parts.iter().find(|p| {
            p.ctype
                .mimetype
                .eq_ignore_ascii_case("message/delivery-status")
        })?;
        let recipients = recipients_from_status(&status_part.get_body_raw().ok()?);

        // The original may come back whole (message/rfc822) or as headers only.
        let mut original = None;
        for p in &parts {
            let mt = p.ctype.mimetype.to_ascii_lowercase();
            if mt == "text/rfc822-headers" {
                if let Ok(raw) = p.get_body_raw()
                    && let Ok((h, _)) = mailparse::parse_headers(&raw)
                {
                    original = original.or_else(|| first_message_id(&h));
                }
            } else if mt == "message/rfc822" {
                if let Some(inner) = p.subparts.first() {
                    original = original.or_else(|| first_message_id(&inner.headers));
                } else if let Ok(raw) = p.get_body_raw()
                    && let Ok((h, _)) = mailparse::parse_headers(&raw)
                {
                    original = original.or_else(|| first_message_id(&h));
                }
            }
        }
        let original = original.filter(|id| is_ours(id))?;
        return Some(BounceReport {
            original_message_id: original,
            kind: kind_of(&recipients),
            recipients,
            confidence: Confidence::Standard,
        });
    }

    if looks_like_failure_notice(&msg) {
        let id = scan_for_message_id(&parts, is_ours)?;
        return Some(BounceReport {
            original_message_id: id,
            recipients: Vec::new(),
            kind: BounceKind::Hard,
            confidence: Confidence::Heuristic,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crlf(s: &str) -> Vec<u8> {
        s.replace('\n', "\r\n").into_bytes()
    }

    const OURS: &str = "abc123@example.com";
    fn ours(id: &str) -> bool {
        id == OURS
    }

    fn postfix_dsn(status_block: &str, original: &str) -> Vec<u8> {
        crlf(&format!(
            "From: MAILER-DAEMON@mx.example.org (Mail Delivery System)\n\
             To: alice@example.com\n\
             Subject: Undelivered Mail Returned to Sender\n\
             Auto-Submitted: auto-replied\n\
             MIME-Version: 1.0\n\
             Content-Type: multipart/report; report-type=delivery-status;\n\
             \tboundary=\"BND\"\n\
             \n\
             --BND\n\
             Content-Description: Notification\n\
             Content-Type: text/plain; charset=utf-8\n\
             \n\
             This is the mail system at host mx.example.org.\n\
             \n\
             --BND\n\
             Content-Description: Delivery report\n\
             Content-Type: message/delivery-status\n\
             \n\
             Reporting-MTA: dns; mx.example.org\n\
             \n\
             {status_block}\n\
             --BND\n\
             {original}\n\
             --BND--\n"
        ))
    }

    const HARD_BLOCK: &str = "Final-Recipient: rfc822; Gone@Example.ORG\n\
         Original-Recipient: rfc822;gone@example.org\n\
         Action: failed\n\
         Status: 5.1.1\n\
         Diagnostic-Code: smtp; 550 5.1.1 <gone@example.org>: Recipient address\n \
         rejected: User unknown in virtual mailbox table\n";

    const RFC822_PART: &str = "Content-Description: Undelivered Message\n\
         Content-Type: message/rfc822\n\
         \n\
         From: alice@example.com\n\
         Message-ID: <abc123@example.com>\n\
         Subject: Hello\n\
         \n\
         body text\n";

    const HEADERS_PART: &str = "Content-Description: Undelivered Message Headers\n\
         Content-Type: text/rfc822-headers\n\
         \n\
         From: alice@example.com\n\
         Message-ID: <abc123@example.com>\n\
         Subject: Hello\n";

    #[test]
    fn parses_a_standard_hard_bounce_with_the_full_original() {
        let r = parse_bounce(&postfix_dsn(HARD_BLOCK, RFC822_PART), &ours).unwrap();
        assert_eq!(r.original_message_id, OURS);
        assert_eq!(
            (r.kind, r.confidence),
            (BounceKind::Hard, Confidence::Standard)
        );
        assert_eq!(r.recipients.len(), 1);
        let rc = &r.recipients[0];
        assert_eq!(rc.address, "gone@example.org");
        assert_eq!(rc.action, BounceAction::Failed);
        assert_eq!(rc.status.as_deref(), Some("5.1.1"));
        assert!(rc.diagnostic.as_deref().unwrap().contains("User unknown"));
        assert!(rc.explanation().contains("does not exist"));
    }

    #[test]
    fn original_may_come_back_as_headers_only() {
        let r = parse_bounce(&postfix_dsn(HARD_BLOCK, HEADERS_PART), &ours).unwrap();
        assert_eq!(r.original_message_id, OURS);
    }

    #[test]
    fn several_recipients_are_reported_separately() {
        let two = format!(
            "{HARD_BLOCK}\nFinal-Recipient: rfc822; full@example.org\nAction: failed\nStatus: 5.2.2\n\
             Diagnostic-Code: smtp; 552 mailbox full\n"
        );
        let r = parse_bounce(&postfix_dsn(&two, RFC822_PART), &ours).unwrap();
        assert_eq!(r.recipients.len(), 2);
        assert!(r.recipients[1].explanation().contains("full"));
    }

    #[test]
    fn delay_warnings_are_soft() {
        let block = "Final-Recipient: rfc822; slow@example.org\nAction: delayed\nStatus: 4.4.1\n\
                     Diagnostic-Code: smtp; 451 connection timed out\n";
        let r = parse_bounce(&postfix_dsn(block, HEADERS_PART), &ours).unwrap();
        assert_eq!(r.kind, BounceKind::Soft);
        assert_eq!(r.recipients[0].action, BounceAction::Delayed);
        assert!(r.recipients[0].explanation().contains("try again"));
    }

    #[test]
    fn bounce_of_someone_elses_message_is_ignored() {
        assert!(parse_bounce(&postfix_dsn(HARD_BLOCK, RFC822_PART), &|_| false).is_none());
    }

    #[test]
    fn plain_text_failure_notice_is_found_heuristically() {
        let raw = crlf(
            "From: Mail Delivery Subsystem <mailer-daemon@old.example.net>\n\
             To: alice@example.com\n\
             Subject: Mail delivery failed: returning message to sender\n\
             \n\
             This message was created automatically by mail delivery software.\n\
             A message that you sent could not be delivered.\n\
             \n\
             ------ This is a copy of the message, including all the headers. ------\n\
             Message-ID: <abc123@example.com>\n\
             Subject: Hello\n",
        );
        let r = parse_bounce(&raw, &ours).unwrap();
        assert_eq!(r.confidence, Confidence::Heuristic);
        assert!(r.recipients.is_empty());
        assert_eq!(r.original_message_id, OURS);
    }

    #[test]
    fn ordinary_mail_is_not_a_bounce() {
        let raw = crlf(
            "From: bob@example.org\nTo: alice@example.com\nSubject: Re: Hello\n\n\
             Message-ID: <abc123@example.com>\nthanks!\n",
        );
        assert!(parse_bounce(&raw, &ours).is_none());
        // Even a human writing "undelivered" in the subject is not enough without a daemon sender.
        let raw = crlf(
            "From: bob@example.org\nSubject: undelivered parcel\n\nMessage-ID: <abc123@example.com>\n",
        );
        assert!(parse_bounce(&raw, &ours).is_none());
    }

    #[test]
    fn dsn_without_a_status_part_or_original_is_rejected() {
        let no_orig = postfix_dsn(HARD_BLOCK, "Content-Type: text/plain\n\nnothing here\n");
        assert!(parse_bounce(&no_orig, &ours).is_none());
    }

    #[test]
    fn hostile_inputs_do_not_panic_or_hang() {
        assert!(parse_bounce(b"", &ours).is_none());
        assert!(parse_bounce(&[0xff; 4096], &ours).is_none());
        // Deeply nested multiparts hit the parser's recursion limit and fail cleanly.
        let mut deep = String::new();
        for i in 0..300 {
            deep.push_str(&format!(
                "Content-Type: multipart/mixed; boundary=\"b{i}\"\n\n--b{i}\n"
            ));
        }
        let _ = parse_bounce(&crlf(&deep), &ours);
        // Huge input is truncated, not rejected by running out of memory.
        let mut big = postfix_dsn(HARD_BLOCK, RFC822_PART);
        big.extend(std::iter::repeat_n(b'x', 6 * 1024 * 1024));
        let _ = parse_bounce(&big, &ours);
        // A recipient field stuffed with junk is dropped.
        let junk = "Final-Recipient: rfc822; not an address with spaces\nAction: failed\n";
        let r = parse_bounce(&postfix_dsn(junk, RFC822_PART), &ours).unwrap();
        assert!(r.recipients.is_empty());
    }

    #[test]
    fn multibyte_text_around_the_message_id_prefix_cannot_panic() {
        // Byte 11 falls inside a multi-byte character on these lines.
        let raw = crlf(
            "From: Mail Delivery Subsystem <mailer-daemon@old.example.net>\n\
             To: alice@example.com\n\
             Subject: Mail delivery failed: returning message to sender\n\
             \n\
             A message that you sent could not be delivered.\n\
             Message-é\u{20ac}€€€ID: x\n\
             ééééé€ééé\n\
             \u{1F600}\u{1F600}\u{1F600}\n\
             Message-ID: <abc123@example.com>\n",
        );
        let r = parse_bounce(&raw, &ours).unwrap();
        assert_eq!(r.original_message_id, OURS);
    }

    #[test]
    fn control_characters_in_server_text_are_removed() {
        let block = "Final-Recipient: rfc822; a@example.org\nAction: failed\nStatus: 5.7.1\n\
                     Diagnostic-Code: smtp; 554 spam\u{7} blocked\n";
        let r = parse_bounce(&postfix_dsn(block, RFC822_PART), &ours).unwrap();
        assert!(
            !r.recipients[0]
                .diagnostic
                .as_deref()
                .unwrap()
                .contains('\u{7}')
        );
    }
}
