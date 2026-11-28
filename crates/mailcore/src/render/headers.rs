//! Reading the headers a person sees, and spotting sender tricks.

use mailparse::{MailAddr, MailHeaderMap, ParsedMail, SingleInfo, addrparse_header, dateparse};

use super::links::{analyze_link, same_site};
use super::{DisplayHeaders, Notice, NoticeKind, Person, Severity, Unsubscribe};
use crate::textsafe::{clean_line, has_bidi_control};

const MAX_LIST: usize = 200;

fn person(info: &SingleInfo) -> Person {
    Person {
        name: clean_line(info.display_name.as_deref().unwrap_or(""), 200),
        address: clean_line(&info.addr, 254),
    }
}

fn people(root: &ParsedMail<'_>, header: &str) -> Vec<Person> {
    let mut out = Vec::new();
    for h in root.headers.get_all_headers(header) {
        let Ok(list) = addrparse_header(h) else {
            continue;
        };
        for a in list.iter() {
            match a {
                MailAddr::Single(s) => out.push(person(s)),
                MailAddr::Group(g) => out.extend(g.addrs.iter().map(person)),
            }
            if out.len() >= MAX_LIST {
                return out;
            }
        }
    }
    out.retain(|p| !p.address.is_empty());
    out
}

fn domain_of(address: &str) -> String {
    address
        .rsplit_once('@')
        .map(|(_, d)| d.to_ascii_lowercase())
        .unwrap_or_default()
}

fn strip_brackets(s: &str) -> String {
    clean_line(s.trim().trim_start_matches('<').trim_end_matches('>'), 998)
}

/// Pulls `<...>` items out of a header like `<a>, <b>`.
fn angle_items(v: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = v;
    while let Some(s) = rest.find('<') {
        let Some(e) = rest[s..].find('>') else { break };
        out.push(rest[s + 1..s + e].trim().to_owned());
        rest = &rest[s + e + 1..];
        if out.len() >= 20 {
            break;
        }
    }
    out
}

fn unsubscribe(root: &ParsedMail<'_>) -> Option<Unsubscribe> {
    let v = root.headers.get_first_value("List-Unsubscribe")?;
    let (mut web, mut mailto) = (None, None);
    for item in angle_items(&v) {
        if let Some(l) = analyze_link(&item, "") {
            if l.url.starts_with("mailto:") && mailto.is_none() {
                mailto = Some(l.url);
            } else if l.url.starts_with("http") && web.is_none() {
                web = Some(l.url);
            }
        }
    }
    if web.is_none() && mailto.is_none() {
        return None;
    }
    let post = root
        .headers
        .get_first_value("List-Unsubscribe-Post")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let one_click = post.contains("list-unsubscribe=one-click")
        && web.as_deref().is_some_and(|w| w.starts_with("https:"));
    Some(Unsubscribe {
        web,
        mailto,
        one_click,
    })
}

pub(super) fn parse(root: &ParsedMail<'_>, notices: &mut Vec<Notice>) -> DisplayHeaders {
    let raw_subject = root.headers.get_first_value("Subject").unwrap_or_default();
    let mut from_list = people(root, "From");
    let from = if from_list.is_empty() {
        None
    } else {
        Some(from_list.remove(0))
    };
    let reply_to = people(root, "Reply-To");
    let to = people(root, "To");
    let cc = people(root, "Cc");

    // Direction-control characters anywhere a person reads are removed (done by
    // `clean_line`); say so, since they have no honest use in these places.
    let hidden = has_bidi_control(&raw_subject)
        || root
            .headers
            .get_all_values("From")
            .iter()
            .any(|v| has_bidi_control(v));
    if hidden {
        notices.push(Notice {
            kind: NoticeKind::HiddenTextDirection,
            severity: Severity::Warning,
            text: "This message uses hidden text-direction characters in its subject or sender, which can be used to disguise names. They were removed.".into(),
        });
    }

    if let Some(f) = &from {
        let dom = domain_of(&f.address);
        // "boss@company.com <attacker@evil.net>": the name claims another address.
        if let Some(claimed) = f.name.split_whitespace().find(|w| w.contains('@')) {
            let claimed = claimed.trim_matches(|c: char| !c.is_alphanumeric());
            let cd = domain_of(claimed);
            if !cd.is_empty() && cd != dom {
                notices.push(Notice {
                    kind: NoticeKind::NameLooksLikeOtherAddress,
                    severity: Severity::Warning,
                    text: format!("The sender's name shows \"{claimed}\", but the message really comes from {}.", f.address),
                });
            }
        }
        if !dom.is_ascii() || dom.split('.').any(|l| l.starts_with("xn--")) {
            notices.push(Notice {
                kind: NoticeKind::SenderHasSpecialCharacters,
                severity: Severity::Caution,
                text: format!("The sender's address ({}) uses special characters, which can imitate a well-known name.", f.address),
            });
        }
        if let Some(r) = reply_to.first() {
            let rd = domain_of(&r.address);
            if !rd.is_empty() && !dom.is_empty() && !same_site(&rd, &dom) {
                notices.push(Notice {
                    kind: NoticeKind::ReplyGoesElsewhere,
                    severity: Severity::Caution,
                    text: format!(
                        "Replies will go to {}, which is a different site from the sender ({}).",
                        r.address, f.address
                    ),
                });
            }
        }
    }

    let refs = root
        .headers
        .get_first_value("References")
        .unwrap_or_default();
    DisplayHeaders {
        from,
        reply_to,
        to,
        cc,
        subject: clean_line(&raw_subject, 998),
        date: root
            .headers
            .get_first_value("Date")
            .and_then(|d| dateparse(&d).ok())
            .filter(|t| (0..=32_503_680_000).contains(t)),
        message_id: root
            .headers
            .get_first_value("Message-ID")
            .map(|v| strip_brackets(&v))
            .filter(|v| !v.is_empty()),
        in_reply_to: root
            .headers
            .get_first_value("In-Reply-To")
            .and_then(|v| {
                angle_items(&v)
                    .into_iter()
                    .next()
                    .map(|s| strip_brackets(&s))
            })
            .filter(|v| !v.is_empty()),
        references: angle_items(&refs)
            .into_iter()
            .map(|s| strip_brackets(&s))
            .filter(|s| !s.is_empty())
            .take(50)
            .collect(),
        unsubscribe: unsubscribe(root),
    }
}
