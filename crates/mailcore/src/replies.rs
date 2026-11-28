//! Preparing replies and forwards, and reading recipient lists typed by a person.
//!
//! Everything here works on text that came from other people's mail (names,
//! subjects, message bodies), so it is cleaned before it can end up in a new
//! message: no control characters, bounded lengths, no unbounded quoting.

use crate::Error;
use crate::compose::{MAX_RECIPIENTS, Mailbox, OutgoingMessage, valid_msgid};
use crate::email::EmailAddress;
use crate::render::{DisplayHeaders, Person};

const MAX_QUOTED_LINES: usize = 300;
const MAX_QUOTED_CHARS: usize = 30_000;
const MAX_NAME_CHARS: usize = 200;
const MAX_SUBJECT_CHARS: usize = 200;
/// Same limit as [`crate::compose`]; the first and the newest are kept.
const MAX_REFERENCES: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyKind {
    Reply,
    ReplyAll,
    Forward,
}

/// The message being answered.
#[derive(Debug, Clone, Copy)]
pub struct Original<'a> {
    pub headers: &'a DisplayHeaders,
    /// The readable text of the message.
    pub text: &'a str,
    /// The date as it should be written in the introduction line.
    pub date_text: &'a str,
}

// ---------------------------------------------------------------------------
// Typed recipient lists
// ---------------------------------------------------------------------------

/// Splits on commas, semicolons and line breaks that are not inside quotes or
/// angle brackets.
fn split_list(input: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let (mut quoted, mut angle, mut escaped) = (false, false, false);
    for c in input.chars() {
        if escaped {
            cur.push(c);
            escaped = false;
            continue;
        }
        match c {
            '\\' if quoted => {
                cur.push(c);
                escaped = true;
            }
            '"' => {
                quoted = !quoted;
                cur.push(c);
            }
            '<' if !quoted => {
                angle = true;
                cur.push(c);
            }
            '>' if !quoted => {
                angle = false;
                cur.push(c);
            }
            ',' | ';' | '\n' | '\r' if !quoted && !angle => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

fn clean_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.chars().take(MAX_NAME_CHARS).collect()
}

fn unquote(name: &str) -> String {
    let n = name.trim();
    let Some(inner) = n.strip_prefix('"').and_then(|r| r.strip_suffix('"')) else {
        return n.to_owned();
    };
    let mut out = String::new();
    let mut esc = false;
    for c in inner.chars() {
        if esc {
            out.push(c);
            esc = false;
        } else if c == '\\' {
            esc = true;
        } else {
            out.push(c);
        }
    }
    out
}

fn same_address(a: &EmailAddress, b: &EmailAddress) -> bool {
    a.to_string().eq_ignore_ascii_case(&b.to_string())
}

fn parse_one(part: &str) -> Result<Mailbox, String> {
    let p = part.trim();
    let (name, addr) = match (p.rfind('<'), p.ends_with('>')) {
        (Some(open), true) => (unquote(&p[..open]), p[open + 1..p.len() - 1].trim()),
        (Some(_), false) => {
            return Err(format!("“{p}” has a “<” without a closing “>”."));
        }
        (None, _) => (String::new(), p),
    };
    if addr.chars().any(char::is_whitespace) {
        return Err(format!(
            "“{p}” is not a single email address. Separate addresses with commas."
        ));
    }
    match EmailAddress::parse(addr) {
        Ok(a) => Ok(Mailbox {
            name: clean_name(&name),
            address: a,
        }),
        Err(Error::InvalidEmail(reason)) => {
            Err(format!("“{addr}” can't be used as an address: {reason}."))
        }
        Err(_) => Err(format!("“{addr}” can't be used as an address.")),
    }
}

/// Reads "Ann <ann@example.com>, bob@example.org" and similar. Repeated addresses
/// are kept once. The first problem found is returned in plain words.
pub fn parse_recipients(input: &str) -> Result<Vec<Mailbox>, String> {
    let mut out: Vec<Mailbox> = Vec::new();
    for part in split_list(input) {
        if part.trim().is_empty() {
            continue;
        }
        let m = parse_one(&part)?;
        if !out.iter().any(|o| same_address(&o.address, &m.address)) {
            out.push(m);
        }
    }
    if out.len() > MAX_RECIPIENTS {
        return Err(format!(
            "There are too many recipients (the limit is {MAX_RECIPIENTS})."
        ));
    }
    Ok(out)
}

/// Like [`parse_recipients`], but skips what cannot be read instead of failing.
/// Used to save drafts while a person is still typing an address.
pub fn parse_recipients_lenient(input: &str) -> Vec<Mailbox> {
    let mut out: Vec<Mailbox> = Vec::new();
    for part in split_list(input) {
        if let Ok(m) = parse_one(&part)
            && !out.iter().any(|o| same_address(&o.address, &m.address))
            && out.len() < MAX_RECIPIENTS
        {
            out.push(m);
        }
    }
    out
}

fn display_name(name: &str) -> String {
    if name.chars().any(|c| "\",;<>()[]:@\\".contains(c)) {
        let mut s = String::from('"');
        for c in name.chars() {
            if c == '"' || c == '\\' {
                s.push('\\');
            }
            s.push(c);
        }
        s.push('"');
        s
    } else {
        name.to_owned()
    }
}

/// The text form of one recipient, as shown in an address field.
pub fn format_mailbox(m: &Mailbox) -> String {
    if m.name.is_empty() {
        m.address.to_string()
    } else {
        format!("{} <{}>", display_name(&m.name), m.address)
    }
}

/// Recipients as they appear in an address field. Reading this text back with
/// [`parse_recipients`] gives the same recipients.
pub fn format_recipients(list: &[Mailbox]) -> String {
    list.iter()
        .map(format_mailbox)
        .collect::<Vec<_>>()
        .join(", ")
}

// ---------------------------------------------------------------------------
// Replies and forwards
// ---------------------------------------------------------------------------

fn person_to_mailbox(p: &Person) -> Option<Mailbox> {
    EmailAddress::parse(&p.address).ok().map(|address| Mailbox {
        name: clean_name(&p.name),
        address,
    })
}

fn is_mine(m: &Mailbox, mine: &[EmailAddress]) -> bool {
    mine.iter().any(|a| same_address(a, &m.address))
}

fn push_unique(list: &mut Vec<Mailbox>, m: Mailbox) {
    if !list.iter().any(|o| same_address(&o.address, &m.address)) {
        list.push(m);
    }
}

fn clean_subject(s: &str) -> String {
    let s: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_SUBJECT_CHARS)
        .collect()
}

fn subject_for(kind: ReplyKind, original: &str) -> String {
    let s = clean_subject(original);
    let lower = s.to_lowercase();
    let (known, add): (&[&str], &str) = match kind {
        ReplyKind::Reply | ReplyKind::ReplyAll => (&["re:", "aw:"], "Re: "),
        ReplyKind::Forward => (&["fwd:", "fw:", "wg:"], "Fwd: "),
    };
    if known.iter().any(|k| lower.starts_with(k)) {
        s
    } else {
        format!("{add}{s}").trim_end().to_owned()
    }
}

fn clean_body(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .chars()
        .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
        .collect()
}

/// "> " in front of every line (">>" for what was already quoted), bounded.
fn quote(text: &str) -> String {
    let text = clean_body(text);
    let mut out = String::new();
    let mut cut = false;
    for (lines, line) in text.trim_end().split('\n').enumerate() {
        if lines >= MAX_QUOTED_LINES || out.len() >= MAX_QUOTED_CHARS {
            cut = true;
            break;
        }
        let line = line.trim_end();
        if line.is_empty() {
            out.push_str(">\n");
        } else if line.starts_with('>') {
            out.push('>');
            out.push_str(line);
            out.push('\n');
        } else {
            out.push_str("> ");
            out.push_str(line);
            out.push('\n');
        }
    }
    if cut {
        out.push_str("> […]\n");
    }
    out
}

fn who(headers: &DisplayHeaders) -> String {
    match &headers.from {
        Some(p) if !clean_name(&p.name).is_empty() => clean_name(&p.name),
        Some(p) => clean_name(&p.address),
        None => "someone".to_owned(),
    }
}

fn list_text(people: &[Person]) -> String {
    people
        .iter()
        .map(|p| match person_to_mailbox(p) {
            Some(m) => format_mailbox(&m),
            None => clean_name(&p.address),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Builds the starting point of a reply or forward. `mine` are the user's own
/// addresses (they are never added as recipients). The result has no sender and
/// no attachments; the caller sets those.
pub fn prepare(kind: ReplyKind, orig: &Original<'_>, mine: &[EmailAddress]) -> OutgoingMessage {
    let h = orig.headers;
    let mut msg = OutgoingMessage {
        subject: subject_for(kind, &h.subject),
        ..OutgoingMessage::default()
    };

    if kind == ReplyKind::Forward {
        let mut t = String::from("\n\n---------- Forwarded message ----------\n");
        t.push_str(&format!("From: {}\n", list_text(h.from.as_slice())));
        if !orig.date_text.is_empty() {
            t.push_str(&format!("Date: {}\n", clean_name(orig.date_text)));
        }
        t.push_str(&format!("Subject: {}\n", clean_subject(&h.subject)));
        t.push_str(&format!("To: {}\n", list_text(&h.to)));
        if !h.cc.is_empty() {
            t.push_str(&format!("Cc: {}\n", list_text(&h.cc)));
        }
        t.push('\n');
        t.push_str(clean_body(orig.text).trim_end());
        t.push('\n');
        msg.text = t;
        return msg;
    }

    // Who gets the answer: the Reply-To address if there is one, else the sender.
    let from = h.from.as_ref().and_then(person_to_mailbox);
    let reply_to: Vec<Mailbox> = h.reply_to.iter().filter_map(person_to_mailbox).collect();
    let mut targets: Vec<Mailbox> = if reply_to.is_empty() {
        from.into_iter().collect()
    } else {
        reply_to
    };
    let own_mail = !targets.is_empty() && targets.iter().all(|m| is_mine(m, mine));
    if own_mail {
        // Answering a message we wrote ourselves: write to the people it went to.
        targets = h.to.iter().filter_map(person_to_mailbox).collect();
    }

    let mut to: Vec<Mailbox> = Vec::new();
    for m in targets {
        if !is_mine(&m, mine) || own_mail {
            push_unique(&mut to, m);
        }
    }
    let mut cc: Vec<Mailbox> = Vec::new();
    if kind == ReplyKind::ReplyAll {
        for m in h.to.iter().filter_map(person_to_mailbox) {
            if !is_mine(&m, mine) {
                push_unique(&mut to, m);
            }
        }
        for m in h.cc.iter().filter_map(person_to_mailbox) {
            if !is_mine(&m, mine) && !to.iter().any(|t| same_address(&t.address, &m.address)) {
                push_unique(&mut cc, m);
            }
        }
    }
    msg.to = to;
    msg.cc = cc;

    // Threading headers.
    if let Some(id) = h.message_id.as_deref() {
        let wrapped = format!("<{id}>");
        if valid_msgid(&wrapped).is_some() {
            let mut refs: Vec<String> = h
                .references
                .iter()
                .map(|r| format!("<{}>", r.trim_matches(|c| c == '<' || c == '>')))
                .filter(|r| valid_msgid(r).is_some())
                .collect();
            if !refs.contains(&wrapped) {
                refs.push(wrapped.clone());
            }
            if refs.len() > MAX_REFERENCES {
                let first = refs[0].clone();
                let tail = refs.split_off(refs.len() - (MAX_REFERENCES - 1));
                refs = std::iter::once(first).chain(tail).collect();
            }
            msg.references = refs;
            msg.in_reply_to = Some(wrapped);
        }
    }

    let intro = if orig.date_text.is_empty() {
        format!("{} wrote:", who(h))
    } else {
        format!("On {}, {} wrote:", clean_name(orig.date_text), who(h))
    };
    let quoted = quote(orig.text);
    msg.text = if quoted.is_empty() {
        "\n".to_owned()
    } else {
        format!("\n\n{intro}\n{quoted}")
    };
    msg
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn person(name: &str, addr: &str) -> Person {
        Person {
            name: name.into(),
            address: addr.into(),
        }
    }

    fn me() -> Vec<EmailAddress> {
        vec![EmailAddress::parse("me@example.com").unwrap()]
    }

    fn headers() -> DisplayHeaders {
        DisplayHeaders {
            from: Some(person("Maria Lopez", "maria@acme.io")),
            to: vec![
                person("Me", "me@example.com"),
                person("Jonas", "jonas@acme.io"),
            ],
            cc: vec![person("", "ops@acme.io"), person("", "ME@example.com")],
            subject: "Launch plan".into(),
            message_id: Some("m1@acme.io".into()),
            ..DisplayHeaders::default()
        }
    }

    fn orig<'a>(h: &'a DisplayHeaders, text: &'a str) -> Original<'a> {
        Original {
            headers: h,
            text,
            date_text: "17 Nov 2025 at 09:30",
        }
    }

    #[test]
    fn parses_names_quotes_and_separators() {
        let l = parse_recipients(
            "Ann <ann@example.com>, \"Smith, Bob\" <bob@example.org>; carol@example.net\n <dave@example.net>",
        )
        .unwrap();
        let names: Vec<_> = l
            .iter()
            .map(|m| (m.name.as_str(), m.address.to_string()))
            .collect();
        assert_eq!(
            names,
            vec![
                ("Ann", "ann@example.com".to_owned()),
                ("Smith, Bob", "bob@example.org".to_owned()),
                ("", "carol@example.net".to_owned()),
                ("", "dave@example.net".to_owned()),
            ]
        );
    }

    #[test]
    fn lenient_reading_skips_unfinished_addresses() {
        let l =
            parse_recipients_lenient("ann@example.com, bo, carol@exa mple.com, dan@example.org");
        let a: Vec<_> = l.iter().map(|m| m.address.to_string()).collect();
        assert_eq!(a, vec!["ann@example.com", "dan@example.org"]);
    }

    #[test]
    fn repeated_addresses_are_kept_once_ignoring_case() {
        let l = parse_recipients("a@example.com, A@Example.com, <a@example.com>").unwrap();
        assert_eq!(l.len(), 1);
    }

    #[test]
    fn problems_are_explained() {
        assert!(
            parse_recipients("bob smith")
                .unwrap_err()
                .contains("Separate addresses")
        );
        assert!(
            parse_recipients("nobody")
                .unwrap_err()
                .contains("can't be used")
        );
        assert!(
            parse_recipients("Ann <ann@example.com")
                .unwrap_err()
                .contains("closing")
        );
        assert!(parse_recipients("a@@b").is_err());
        assert_eq!(parse_recipients("  ,; ").unwrap(), vec![]);
    }

    #[test]
    fn no_way_to_smuggle_a_header_through_a_name() {
        let l = parse_recipients("\"Eve\r\nBcc: x@evil.example\" <eve@example.com>").unwrap();
        assert_eq!(l.len(), 1, "{l:?}");
        assert!(!l[0].name.contains('\n') && !l[0].name.contains('\r'));
    }

    #[test]
    fn formatting_round_trips() {
        let l = parse_recipients(
            "\"Smith, Bob\" <bob@example.org>, \"Q \\\"X\\\"\" <q@example.org>, c@example.net",
        )
        .unwrap();
        let again = parse_recipients(&format_recipients(&l)).unwrap();
        assert_eq!(l, again);
    }

    #[test]
    fn reply_goes_to_the_sender_and_threads() {
        let h = headers();
        let m = prepare(ReplyKind::Reply, &orig(&h, "Hi\n\nBye"), &me());
        assert_eq!(m.subject, "Re: Launch plan");
        assert_eq!(m.to.len(), 1);
        assert_eq!(m.to[0].address.to_string(), "maria@acme.io");
        assert!(m.cc.is_empty());
        assert_eq!(m.in_reply_to.as_deref(), Some("<m1@acme.io>"));
        assert_eq!(m.references, vec!["<m1@acme.io>".to_owned()]);
        assert!(
            m.text
                .starts_with("\n\nOn 17 Nov 2025 at 09:30, Maria Lopez wrote:\n> Hi\n>\n> Bye\n")
        );
    }

    #[test]
    fn reply_follows_reply_to() {
        let mut h = headers();
        h.reply_to = vec![person("List", "list@acme.io")];
        let m = prepare(ReplyKind::Reply, &orig(&h, "x"), &me());
        assert_eq!(m.to[0].address.to_string(), "list@acme.io");
    }

    #[test]
    fn reply_all_never_includes_me_and_has_no_duplicates() {
        let h = headers();
        let m = prepare(ReplyKind::ReplyAll, &orig(&h, "x"), &me());
        let to: Vec<_> = m.to.iter().map(|x| x.address.to_string()).collect();
        let cc: Vec<_> = m.cc.iter().map(|x| x.address.to_string()).collect();
        assert_eq!(to, vec!["maria@acme.io", "jonas@acme.io"]);
        assert_eq!(cc, vec!["ops@acme.io"]);
    }

    #[test]
    fn answering_my_own_message_writes_to_its_recipients() {
        let mut h = headers();
        h.from = Some(person("Me", "me@example.com"));
        h.to = vec![person("Jonas", "jonas@acme.io")];
        h.cc = vec![];
        let m = prepare(ReplyKind::Reply, &orig(&h, "x"), &me());
        assert_eq!(m.to.len(), 1);
        assert_eq!(m.to[0].address.to_string(), "jonas@acme.io");
    }

    #[test]
    fn subject_prefixes_are_not_stacked() {
        assert_eq!(subject_for(ReplyKind::Reply, "Re: hi"), "Re: hi");
        assert_eq!(subject_for(ReplyKind::Reply, "RE: hi"), "RE: hi");
        assert_eq!(subject_for(ReplyKind::Reply, "AW: hi"), "AW: hi");
        assert_eq!(subject_for(ReplyKind::Forward, "Fwd: hi"), "Fwd: hi");
        assert_eq!(subject_for(ReplyKind::Forward, "hi"), "Fwd: hi");
        assert_eq!(subject_for(ReplyKind::Reply, ""), "Re:");
        assert_eq!(
            subject_for(ReplyKind::Reply, "a\r\nBcc: x@y.z"),
            "Re: a Bcc: x@y.z"
        );
    }

    #[test]
    fn long_reference_chains_keep_first_and_newest() {
        let mut h = headers();
        h.references = (0..40).map(|i| format!("r{i}@x.io")).collect();
        let m = prepare(ReplyKind::Reply, &orig(&h, "x"), &me());
        assert_eq!(m.references.len(), MAX_REFERENCES);
        assert_eq!(m.references[0], "<r0@x.io>");
        assert_eq!(m.references.last().unwrap(), "<m1@acme.io>");
        assert!(
            crate::compose::compose(
                &OutgoingMessage {
                    from: Some(Mailbox {
                        name: String::new(),
                        address: me()[0].clone()
                    }),
                    ..m
                },
                0
            )
            .is_ok()
        );
    }

    #[test]
    fn invalid_message_ids_are_not_copied_into_headers() {
        let mut h = headers();
        h.message_id = Some("bad id@x".into());
        let m = prepare(ReplyKind::Reply, &orig(&h, "x"), &me());
        assert!(m.in_reply_to.is_none() && m.references.is_empty());
    }

    #[test]
    fn quoting_is_nested_bounded_and_clean() {
        let q = quote("one\r\n> two\r\n\r\nthree\u{0}\u{7}");
        assert_eq!(q, "> one\n>> two\n>\n> three\n");
        let long: String = (0..1000).map(|i| format!("line {i}\n")).collect();
        let q = quote(&long);
        assert!(q.lines().count() <= MAX_QUOTED_LINES + 1);
        assert!(q.ends_with("> […]\n"));
    }

    #[test]
    fn forward_has_a_header_block_and_no_recipients() {
        let h = headers();
        let m = prepare(ReplyKind::Forward, &orig(&h, "Body text"), &me());
        assert_eq!(m.subject, "Fwd: Launch plan");
        assert!(m.to.is_empty() && m.in_reply_to.is_none());
        assert!(m.text.contains(
            "---------- Forwarded message ----------\nFrom: Maria Lopez <maria@acme.io>\n"
        ));
        assert!(m.text.contains("Subject: Launch plan\nTo: Me <me@example.com>, Jonas <jonas@acme.io>\nCc: ops@acme.io, ME@example.com\n\nBody text\n"));
    }
}
