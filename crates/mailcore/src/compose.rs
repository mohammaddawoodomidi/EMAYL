//! Building an outgoing message: validation first, then MIME encoding.
//!
//! Everything a user (or a pasted web page) can type is untrusted here: control
//! characters are refused so nobody can smuggle extra headers or SMTP commands,
//! names and file names are cleaned, and size limits stop a runaway message
//! before it is ever queued.

use mail_builder::MessageBuilder;
use serde::{Deserialize, Serialize};

use crate::email::EmailAddress;

pub const MAX_RECIPIENTS: usize = 500;
pub const MAX_ATTACHMENTS: usize = 50;
/// Sum of attachment sizes before encoding. Most providers cap the *encoded*
/// message near 25 MB; 18 MiB raw stays under that after base64 growth (4/3).
pub const MAX_ATTACHMENT_BYTES: usize = 18 * 1024 * 1024;
pub const MAX_BODY_BYTES: usize = 5 * 1024 * 1024;
const MAX_SUBJECT_CHARS: usize = 998;
const MAX_NAME_CHARS: usize = 200;
const MAX_REFERENCES: usize = 20;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ComposeError {
    #[error("the message has no recipients")]
    NoRecipients,
    #[error("too many recipients (limit {MAX_RECIPIENTS})")]
    TooManyRecipients,
    #[error("too many attachments (limit {MAX_ATTACHMENTS})")]
    TooManyAttachments,
    #[error("attachments are too large (limit {} MB in total)", MAX_ATTACHMENT_BYTES / (1024 * 1024))]
    AttachmentsTooLarge,
    #[error("the message text is too large")]
    BodyTooLarge,
    #[error("invalid {field}: {reason}")]
    Invalid {
        field: &'static str,
        reason: &'static str,
    },
    #[error("could not encode the message")]
    Encoding,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Mailbox {
    /// Display name, e.g. "Bob Smith". May be empty.
    pub name: String,
    pub address: EmailAddress,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("filename", &self.filename)
            .field("content_type", &self.content_type)
            .field("bytes", &self.data.len())
            .finish()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingMessage {
    pub from: Option<Mailbox>,
    pub to: Vec<Mailbox>,
    pub cc: Vec<Mailbox>,
    /// Blind copies: delivered to, never written into the headers.
    pub bcc: Vec<Mailbox>,
    pub subject: String,
    pub text: String,
    pub html: Option<String>,
    pub attachments: Vec<Attachment>,
    /// Message-ID of the message being answered, with angle brackets.
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComposedMessage {
    /// Without angle brackets, e.g. `3f2a...@example.com`.
    pub message_id: String,
    /// Complete RFC 5322 message (CRLF line endings).
    pub bytes: Vec<u8>,
    pub from: EmailAddress,
    /// Envelope recipients: To + Cc + Bcc, de-duplicated.
    pub recipients: Vec<EmailAddress>,
    pub subject: String,
}

fn bad(field: &'static str, reason: &'static str) -> ComposeError {
    ComposeError::Invalid { field, reason }
}

fn clean_text(value: &str, field: &'static str, max: usize) -> Result<(), ComposeError> {
    if value.chars().count() > max {
        return Err(bad(field, "is too long"));
    }
    if value.chars().any(char::is_control) {
        return Err(bad(field, "contains control characters"));
    }
    Ok(())
}

/// `<id>` where id has no whitespace, control characters or angle brackets inside.
pub(crate) fn valid_msgid(s: &str) -> Option<&str> {
    let inner = s.strip_prefix('<')?.strip_suffix('>')?;
    let ok = !inner.is_empty()
        && inner.len() <= 255
        && inner.contains('@')
        && inner.is_ascii()
        && !inner
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '<' | '>'));
    ok.then_some(inner)
}

/// File names: no path parts, no control characters, bounded.
pub fn sanitize_filename(name: &str) -> String {
    let leaf = name.rsplit(['/', '\\']).next().unwrap_or("");
    let cleaned: String = leaf
        .chars()
        .filter(|c| {
            !c.is_control()
                && !crate::textsafe::is_invisible(*c)
                && !matches!(c, '"' | '<' | '>' | ':' | '|' | '?' | '*')
        })
        .take(120)
        .collect();
    let cleaned = cleaned.trim().trim_matches('.').to_owned();
    if cleaned.is_empty() {
        "attachment".to_owned()
    } else {
        cleaned
    }
}

fn valid_content_type(ct: &str) -> bool {
    let Some((t, s)) = ct.split_once('/') else {
        return false;
    };
    let token = |x: &str| {
        !x.is_empty()
            && x.len() <= 64
            && x.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'+' | b'_'))
    };
    token(t) && token(s)
}

fn check_mailbox(m: &Mailbox, field: &'static str) -> Result<(), ComposeError> {
    clean_text(&m.name, field, MAX_NAME_CHARS)
}

/// Encodes a message. `now` is the Unix time for the Date header.
pub fn compose(msg: &OutgoingMessage, now: i64) -> Result<ComposedMessage, ComposeError> {
    let from = msg
        .from
        .as_ref()
        .ok_or_else(|| bad("sender", "is missing"))?;
    check_mailbox(from, "sender name")?;
    for m in &msg.to {
        check_mailbox(m, "recipient name")?;
    }
    for m in &msg.cc {
        check_mailbox(m, "recipient name")?;
    }
    for m in &msg.bcc {
        check_mailbox(m, "recipient name")?;
    }
    clean_text(&msg.subject, "subject", MAX_SUBJECT_CHARS)?;

    // Envelope recipients, de-duplicated case-insensitively.
    let mut recipients: Vec<EmailAddress> = Vec::new();
    for m in msg.to.iter().chain(&msg.cc).chain(&msg.bcc) {
        if !recipients
            .iter()
            .any(|r| r.to_string().eq_ignore_ascii_case(&m.address.to_string()))
        {
            recipients.push(m.address.clone());
        }
    }
    if recipients.is_empty() {
        return Err(ComposeError::NoRecipients);
    }
    if recipients.len() > MAX_RECIPIENTS {
        return Err(ComposeError::TooManyRecipients);
    }

    if msg.text.len() > MAX_BODY_BYTES
        || msg.html.as_ref().is_some_and(|h| h.len() > MAX_BODY_BYTES)
    {
        return Err(ComposeError::BodyTooLarge);
    }
    if msg.attachments.len() > MAX_ATTACHMENTS {
        return Err(ComposeError::TooManyAttachments);
    }
    let total: usize = msg.attachments.iter().map(|a| a.data.len()).sum();
    if total > MAX_ATTACHMENT_BYTES {
        return Err(ComposeError::AttachmentsTooLarge);
    }

    let reply_to = match msg.in_reply_to.as_deref() {
        Some(r) => {
            Some(valid_msgid(r).ok_or_else(|| bad("in-reply-to", "is not a valid message id"))?)
        }
        None => None,
    };
    if msg.references.len() > MAX_REFERENCES {
        return Err(bad("references", "has too many entries"));
    }
    let references: Vec<&str> = msg
        .references
        .iter()
        .map(|r| valid_msgid(r).ok_or_else(|| bad("references", "contains an invalid message id")))
        .collect::<Result<_, _>>()?;

    let id = {
        let mut raw = [0u8; 16];
        getrandom::fill(&mut raw).map_err(|_| ComposeError::Encoding)?;
        let hex: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        format!("{hex}@{}", from.address.domain())
    };

    let addr = |m: &Mailbox| (m.name.clone(), m.address.to_string());
    let mut b = MessageBuilder::new()
        .message_id(id.clone())
        .date(now)
        .from(addr(from))
        .subject(msg.subject.clone())
        .text_body(msg.text.clone());
    if !msg.to.is_empty() {
        b = b.to(msg.to.iter().map(addr).collect::<Vec<_>>());
    }
    if !msg.cc.is_empty() {
        b = b.cc(msg.cc.iter().map(addr).collect::<Vec<_>>());
    }
    if let Some(html) = &msg.html {
        b = b.html_body(html.clone());
    }
    if let Some(r) = reply_to {
        b = b.in_reply_to(r.to_owned());
    }
    if !references.is_empty() {
        b = b.references(
            references
                .iter()
                .map(|r| (*r).to_owned())
                .collect::<Vec<_>>(),
        );
    }
    for a in &msg.attachments {
        let ct = if valid_content_type(&a.content_type) {
            a.content_type.to_ascii_lowercase()
        } else {
            "application/octet-stream".to_owned()
        };
        b = b.attachment(ct, sanitize_filename(&a.filename), a.data.clone());
    }
    let bytes = b.write_to_vec().map_err(|_| ComposeError::Encoding)?;

    Ok(ComposedMessage {
        message_id: id,
        bytes: crlf(&bytes),
        from: from.address.clone(),
        recipients,
        subject: msg.subject.clone(),
    })
}

/// Converts every line break (bare LF, bare CR, CRLF) to CRLF.
///
/// Bare LF/CR in an SMTP body is what "SMTP smuggling" attacks rely on, so the
/// wire format never contains them.
pub fn crlf(input: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(input.len() + input.len() / 40);
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'\r' => {
                out.extend_from_slice(b"\r\n");
                if input.get(i + 1) == Some(&b'\n') {
                    i += 1;
                }
            }
            b'\n' => out.extend_from_slice(b"\r\n"),
            b => out.push(b),
        }
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use mailparse::MailHeaderMap;

    use super::*;

    fn mb(name: &str, addr: &str) -> Mailbox {
        Mailbox {
            name: name.into(),
            address: EmailAddress::parse(addr).unwrap(),
        }
    }

    fn base() -> OutgoingMessage {
        OutgoingMessage {
            from: Some(mb("Alice Example", "alice@example.com")),
            to: vec![mb("Bob", "bob@example.org")],
            subject: "Hello".into(),
            text: "Hi Bob,\nsee you.\n".into(),
            ..OutgoingMessage::default()
        }
    }

    fn parsed(c: &ComposedMessage) -> mailparse::ParsedMail<'_> {
        mailparse::parse_mail(&c.bytes).unwrap()
    }

    #[test]
    fn composes_a_readable_message() {
        let c = compose(&base(), 1_794_909_600).unwrap();
        let p = parsed(&c);
        let h = |n: &str| p.headers.get_first_value(n).unwrap();
        assert_eq!(h("Subject"), "Hello");
        assert!(h("From").contains("alice@example.com") && h("From").contains("Alice Example"));
        assert!(h("To").contains("bob@example.org"));
        assert_eq!(h("Message-ID"), format!("<{}>", c.message_id));
        assert!(c.message_id.ends_with("@example.com"));
        assert!(h("Date").contains("2026"));
        assert_eq!(
            p.get_body().unwrap().replace("\r\n", "\n"),
            "Hi Bob,\nsee you.\n"
        );
    }

    #[test]
    fn output_has_only_crlf_line_endings() {
        let mut m = base();
        m.text = "a\nb\rc\r\nd\n.\nend".into();
        let c = compose(&m, 0).unwrap();
        for (i, b) in c.bytes.iter().enumerate() {
            if *b == b'\n' {
                assert!(i > 0 && c.bytes[i - 1] == b'\r', "bare LF at {i}");
            }
            if *b == b'\r' {
                assert_eq!(c.bytes.get(i + 1), Some(&b'\n'), "bare CR at {i}");
            }
        }
    }

    #[test]
    fn outgoing_headers_reveal_nothing_beyond_the_message_itself() {
        // Every header name in a composed message must be on this list: no program name,
        // no computer name, no address of this computer, no read-receipt request.
        let mut m = base();
        m.cc = vec![mb("", "carol@example.net")];
        m.in_reply_to = Some("<a@example.org>".into());
        m.references = vec!["<a@example.org>".into()];
        m.attachments = vec![Attachment {
            filename: "a.txt".into(),
            content_type: "text/plain".into(),
            data: b"x".to_vec(),
        }];
        let c = compose(&m, 1_794_909_600).unwrap();
        let text = String::from_utf8_lossy(&c.bytes).to_lowercase();
        let p = parsed(&c);
        let allowed = [
            "message-id",
            "date",
            "from",
            "to",
            "cc",
            "subject",
            "in-reply-to",
            "references",
            "mime-version",
            "content-type",
            "content-transfer-encoding",
            "content-disposition",
        ];
        let mut seen = Vec::new();
        collect_header_names(&p, &mut seen);
        for name in &seen {
            assert!(allowed.contains(&name.as_str()), "unexpected header {name}");
        }
        for banned in [
            "x-mailer",
            "user-agent",
            "x-originating-ip",
            "received:",
            "disposition-notification-to",
            "return-receipt-to",
            "x-confirm-reading-to",
            "emayl",
        ] {
            assert!(!text.contains(banned), "found {banned}");
        }
    }

    fn collect_header_names(p: &mailparse::ParsedMail<'_>, out: &mut Vec<String>) {
        for h in &p.headers {
            out.push(h.get_key().to_ascii_lowercase());
        }
        for s in &p.subparts {
            collect_header_names(s, out);
        }
    }

    #[test]
    fn bcc_is_delivered_but_never_written_to_headers() {
        let mut m = base();
        m.bcc = vec![mb("", "secret@example.net")];
        let c = compose(&m, 0).unwrap();
        assert!(
            c.recipients
                .iter()
                .any(|r| r.to_string() == "secret@example.net")
        );
        assert!(!String::from_utf8_lossy(&c.bytes).contains("secret@example.net"));
        assert!(
            !String::from_utf8_lossy(&c.bytes)
                .to_lowercase()
                .contains("bcc:")
        );
    }

    #[test]
    fn recipients_are_deduplicated_across_fields() {
        let mut m = base();
        m.cc = vec![mb("", "BOB@example.org")];
        m.bcc = vec![mb("", "bob@example.org")];
        assert_eq!(compose(&m, 0).unwrap().recipients.len(), 1);
    }

    #[test]
    fn header_injection_attempts_are_refused() {
        let mut m = base();
        m.subject = "Hi\r\nBcc: evil@example.net".into();
        assert!(matches!(
            compose(&m, 0),
            Err(ComposeError::Invalid {
                field: "subject",
                ..
            })
        ));
        let mut m = base();
        m.to[0].name = "Bob\r\nX-Evil: 1".into();
        assert!(compose(&m, 0).is_err());
        let mut m = base();
        m.in_reply_to = Some("<a@b>\r\nX: y".into());
        assert!(compose(&m, 0).is_err());
        let mut m = base();
        m.references = vec!["<ok@example.com>".into(), "not-an-id".into()];
        assert!(compose(&m, 0).is_err());
    }

    #[test]
    fn non_ascii_subject_and_names_survive() {
        let mut m = base();
        m.subject = "Grüße 🌍 日本語".into();
        m.to[0].name = "Jörg Müller".into();
        let c = compose(&m, 0).unwrap();
        assert!(c.bytes.is_ascii(), "wire format must be 7-bit");
        let p = parsed(&c);
        assert_eq!(
            p.headers.get_first_value("Subject").unwrap(),
            "Grüße 🌍 日本語"
        );
        assert!(
            p.headers
                .get_first_value("To")
                .unwrap()
                .contains("Jörg Müller")
        );
    }

    #[test]
    fn display_name_with_specials_is_quoted_not_injected() {
        let mut m = base();
        m.to[0].name = "Smith, Bob \"The Builder\" <fake@evil.example>".into();
        let c = compose(&m, 0).unwrap();
        let p = parsed(&c);
        let to = p.headers.get_first_value("To").unwrap();
        let list = mailparse::addrparse(&to).unwrap();
        assert_eq!(list.len(), 1, "{to}");
        assert!(
            matches!(&list[0], mailparse::MailAddr::Single(s) if s.addr == "bob@example.org"),
            "{to}"
        );
    }

    #[test]
    fn attachments_are_encoded_and_names_sanitised() {
        let mut m = base();
        m.attachments = vec![
            Attachment {
                filename: "../../etc/pass\"wd.txt".into(),
                content_type: "text/plain".into(),
                data: b"hello attachment".to_vec(),
            },
            Attachment {
                filename: "".into(),
                content_type: "not a type; boundary=x".into(),
                data: vec![0, 1, 2, 255],
            },
        ];
        let c = compose(&m, 0).unwrap();
        let p = parsed(&c);
        let atts: Vec<_> = p
            .subparts
            .iter()
            .filter(|s| {
                s.get_content_disposition().disposition == mailparse::DispositionType::Attachment
            })
            .collect();
        assert_eq!(atts.len(), 2);
        assert_eq!(
            atts[0]
                .get_content_disposition()
                .params
                .get("filename")
                .unwrap(),
            "passwd.txt"
        );
        assert_eq!(atts[0].get_body_raw().unwrap(), b"hello attachment");
        assert_eq!(atts[1].ctype.mimetype, "application/octet-stream");
        assert_eq!(atts[1].get_body_raw().unwrap(), vec![0, 1, 2, 255]);
    }

    #[test]
    fn html_gets_a_plain_text_alternative() {
        let mut m = base();
        m.html = Some("<p>Hi <b>Bob</b></p>".into());
        let c = compose(&m, 0).unwrap();
        let p = parsed(&c);
        assert_eq!(p.ctype.mimetype, "multipart/alternative");
        assert_eq!(p.subparts.len(), 2);
    }

    #[test]
    fn reply_headers_are_written() {
        let mut m = base();
        m.in_reply_to = Some("<orig@example.org>".into());
        m.references = vec!["<root@example.org>".into(), "<orig@example.org>".into()];
        let c = compose(&m, 0).unwrap();
        let text = String::from_utf8_lossy(&c.bytes).into_owned();
        assert!(text.contains("In-Reply-To: <orig@example.org>"), "{text}");
        assert!(text.contains("<root@example.org>"));
    }

    #[test]
    fn limits_are_enforced() {
        assert_eq!(
            compose(
                &OutgoingMessage {
                    to: vec![],
                    ..base()
                },
                0
            )
            .unwrap_err(),
            ComposeError::NoRecipients
        );
        let mut m = base();
        m.to = (0..=MAX_RECIPIENTS)
            .map(|i| mb("", &format!("u{i}@example.org")))
            .collect();
        assert_eq!(compose(&m, 0).unwrap_err(), ComposeError::TooManyRecipients);
        let mut m = base();
        m.attachments = vec![Attachment {
            filename: "a".into(),
            content_type: "x/y".into(),
            data: vec![0; MAX_ATTACHMENT_BYTES + 1],
        }];
        assert_eq!(
            compose(&m, 0).unwrap_err(),
            ComposeError::AttachmentsTooLarge
        );
        let mut m = base();
        m.from = None;
        assert!(compose(&m, 0).is_err());
        let mut m = base();
        m.text = "x".repeat(MAX_BODY_BYTES + 1);
        assert_eq!(compose(&m, 0).unwrap_err(), ComposeError::BodyTooLarge);
    }

    #[test]
    fn message_ids_are_unique() {
        let a = compose(&base(), 0).unwrap().message_id;
        let b = compose(&base(), 0).unwrap().message_id;
        assert_ne!(a, b);
    }

    #[test]
    fn crlf_normaliser_cases() {
        assert_eq!(crlf(b"a\nb"), b"a\r\nb");
        assert_eq!(crlf(b"a\rb"), b"a\r\nb");
        assert_eq!(crlf(b"a\r\nb"), b"a\r\nb");
        assert_eq!(crlf(b"\n\n"), b"\r\n\r\n");
        assert_eq!(crlf(b"a\r\r\nb"), b"a\r\n\r\nb");
        assert_eq!(crlf(b""), b"");
    }

    #[test]
    fn filename_sanitiser_cases() {
        assert_eq!(sanitize_filename("C:\\Users\\me\\report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename(".."), "attachment");
        assert_eq!(sanitize_filename("  "), "attachment");
        assert_eq!(sanitize_filename("a\u{0}b.txt"), "ab.txt");
        assert!(sanitize_filename(&"x".repeat(500)).len() <= 120);
    }
}
