//! Checking links before they are shown or opened.
//!
//! Only `http`, `https`, `mailto` and `tel` links survive. For web links we show
//! where the link really goes and flag the usual tricks: link text that names a
//! different site, `user@host` tricks, look-alike (internationalised) domains,
//! raw IP addresses and plain `http`.

use url::{Host, Url};

use crate::textsafe::clean_line;

const MAX_URL_CHARS: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkFlag {
    /// The visible text looks like an address of a different site.
    TextShowsOtherSite { shown: String },
    /// The web address contains non-ASCII characters (can imitate a known name).
    SpecialCharacters,
    /// One part of the name mixes alphabets (a strong sign of imitation).
    MixedAlphabets,
    /// The address is a number, not a name.
    NumericAddress,
    /// The link contained `name:password@`, which is often used to disguise the real site.
    HadCredentials,
    /// Not encrypted.
    InsecureHttp,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    /// What the message shows as the link text (cleaned).
    pub text: String,
    /// Normalised destination, credentials removed.
    pub url: String,
    /// The real site name as a person should read it (Unicode form); empty for mailto/tel.
    pub host: String,
    /// The same name in ASCII (punycode) form.
    pub host_ascii: String,
    pub flags: Vec<LinkFlag>,
}

impl Link {
    /// Whether to show a warning next to this link.
    pub fn is_suspicious(&self) -> bool {
        self.flags
            .iter()
            .any(|f| !matches!(f, LinkFlag::InsecureHttp))
    }
}

const FILE_EXTENSIONS: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "png", "jpg", "jpeg", "gif", "zip", "txt",
    "csv", "html", "htm",
];

/// Returns the host named by link text that looks like an address, if any.
fn host_in_text(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.chars().count() > 300 || t.chars().any(char::is_whitespace) {
        return None;
    }
    let candidate = if t.contains("://") {
        t.to_owned()
    } else {
        format!("http://{t}")
    };
    let u = Url::parse(&candidate).ok()?;
    let host = u.host_str()?.to_ascii_lowercase();
    let tld = host.rsplit('.').next()?;
    if !host.contains('.') || FILE_EXTENSIONS.contains(&tld) {
        return None;
    }
    Some(host)
}

fn strip_www(h: &str) -> &str {
    h.strip_prefix("www.").unwrap_or(h)
}

pub(super) fn same_site(a: &str, b: &str) -> bool {
    let (a, b) = (strip_www(a), strip_www(b));
    a == b || a.ends_with(&format!(".{b}")) || b.ends_with(&format!(".{a}"))
}

fn mixes_alphabets(unicode_host: &str) -> bool {
    unicode_host.split('.').any(|label| {
        let ascii_letter = label.chars().any(|c| c.is_ascii_alphabetic());
        let other_letter = label.chars().any(|c| c.is_alphabetic() && !c.is_ascii());
        ascii_letter && other_letter
    })
}

/// Validates a link target found in a message. `None` means "do not make this a
/// link" (unsupported kind, or unreadable); the caller shows the text only.
pub fn analyze_link(href: &str, text: &str) -> Option<Link> {
    if href.chars().count() > MAX_URL_CHARS {
        return None;
    }
    // Browsers ignore tabs and line breaks inside addresses; so must we, or
    // "java\nscript:" style tricks could pass a naive check.
    let cleaned: String = href.chars().filter(|c| !c.is_control()).collect();
    let mut url = Url::parse(cleaned.trim()).ok()?;
    let text = clean_line(text, 300);
    match url.scheme() {
        "mailto" | "tel" => Some(Link {
            text,
            // The URL library keeps spaces inside these addresses; encode them so the
            // shown and copied text is one piece.
            url: url.to_string().replace(' ', "%20"),
            host: String::new(),
            host_ascii: String::new(),
            flags: Vec::new(),
        }),
        "http" | "https" => {
            let mut flags = Vec::new();
            let host_ascii = url.host_str()?.to_ascii_lowercase();
            if host_ascii.is_empty() {
                return None;
            }
            if !url.username().is_empty() || url.password().is_some() {
                flags.push(LinkFlag::HadCredentials);
                let _ = url.set_username("");
                let _ = url.set_password(None);
            }
            let numeric = matches!(url.host(), Some(Host::Ipv4(_) | Host::Ipv6(_)));
            let host = if numeric {
                host_ascii.clone()
            } else {
                idna::domain_to_unicode(&host_ascii).0
            };
            if numeric {
                flags.push(LinkFlag::NumericAddress);
            } else if host != host_ascii {
                flags.push(LinkFlag::SpecialCharacters);
                if mixes_alphabets(&host) {
                    flags.push(LinkFlag::MixedAlphabets);
                }
            }
            if url.scheme() == "http" {
                flags.push(LinkFlag::InsecureHttp);
            }
            if let Some(shown) = host_in_text(&text)
                && !same_site(&shown, &host_ascii)
            {
                flags.push(LinkFlag::TextShowsOtherSite { shown });
            }
            Some(Link {
                text,
                url: url.to_string(),
                host,
                host_ascii,
                flags,
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(href: &str, text: &str) -> Vec<LinkFlag> {
        analyze_link(href, text).unwrap().flags
    }

    #[test]
    fn only_safe_schemes_become_links() {
        for bad in [
            "javascript:alert(1)",
            "JaVaScRiPt:alert(1)",
            "java\nscript:alert(1)",
            "java\tscript:alert(1)",
            "data:text/html,<script>1</script>",
            "file:///etc/passwd",
            "vbscript:x",
            "ftp://example.com/",
            "cid:abc",
            "//example.com/x",
            "/relative/path",
            "",
        ] {
            assert!(
                analyze_link(bad, "x").is_none(),
                "{bad:?} must not become a link"
            );
        }
        assert!(analyze_link("https://example.com/a?b=1#c", "x").is_some());
        assert!(analyze_link("mailto:a@example.com", "x").is_some());
        assert!(analyze_link("tel:+15551234", "x").is_some());
    }

    #[test]
    fn mail_and_phone_links_never_contain_spaces() {
        let l = analyze_link("mailto:a@example.com?subject=hello world", "x").unwrap();
        assert_eq!(l.url, "mailto:a@example.com?subject=hello%20world");
        let l = analyze_link("tel:+1 555 1234", "x").unwrap();
        assert!(!l.url.contains(' '), "{}", l.url);
    }

    #[test]
    fn matching_text_is_not_flagged() {
        assert!(flags("https://example.com/x", "https://example.com/x").is_empty());
        assert!(flags("https://www.example.com/x", "example.com").is_empty());
        assert!(flags("https://example.com", "Click here to continue").is_empty());
        assert!(flags("https://shop.example.com", "example.com").is_empty());
        assert!(
            flags("https://example.com/f", "report.pdf").is_empty(),
            "file names are not addresses"
        );
    }

    #[test]
    fn text_naming_another_site_is_flagged() {
        let f = flags(
            "https://evil.example.net/login",
            "https://www.paypal.com/signin",
        );
        assert_eq!(
            f,
            vec![LinkFlag::TextShowsOtherSite {
                shown: "www.paypal.com".into()
            }]
        );
        let f = flags("https://paypal.com.evil.net/", "paypal.com");
        assert_eq!(
            f,
            vec![LinkFlag::TextShowsOtherSite {
                shown: "paypal.com".into()
            }]
        );
        // A name that merely *contains* the real one is not the same site.
        assert!(!flags("https://notpaypal.com/", "paypal.com").is_empty());
    }

    #[test]
    fn credentials_trick_is_removed_and_flagged() {
        let l = analyze_link("https://paypal.com@evil.net/login", "https://paypal.com").unwrap();
        assert!(l.flags.contains(&LinkFlag::HadCredentials));
        assert_eq!(l.host, "evil.net");
        assert!(!l.url.contains('@'), "{}", l.url);
        assert!(
            l.flags
                .iter()
                .any(|f| matches!(f, LinkFlag::TextShowsOtherSite { .. }))
        );
    }

    #[test]
    fn lookalike_domains_show_both_forms() {
        // Cyrillic "а" (U+0430) in place of Latin "a".
        let l = analyze_link("https://p\u{0430}ypal.com/", "x").unwrap();
        assert!(l.flags.contains(&LinkFlag::SpecialCharacters));
        assert!(l.flags.contains(&LinkFlag::MixedAlphabets));
        assert!(l.host_ascii.starts_with("xn--"), "{}", l.host_ascii);
        assert_eq!(l.host, "p\u{0430}ypal.com");
        // A fully non-Latin name is "special" but not "mixed".
        let l = analyze_link("https://\u{4F8B}\u{3048}.jp/", "x").unwrap();
        assert!(l.flags.contains(&LinkFlag::SpecialCharacters));
        assert!(!l.flags.contains(&LinkFlag::MixedAlphabets));
    }

    #[test]
    fn numeric_hosts_and_plain_http() {
        assert!(flags("http://192.168.0.1/login", "x").contains(&LinkFlag::NumericAddress));
        assert!(flags("https://[::1]/x", "x").contains(&LinkFlag::NumericAddress));
        let f = flags("http://example.com/", "x");
        assert_eq!(f, vec![LinkFlag::InsecureHttp]);
        assert!(
            !analyze_link("http://example.com/", "x")
                .unwrap()
                .is_suspicious(),
            "plain http alone is not alarming"
        );
        // Decimal/hex IPv4 forms normalise to a numeric address.
        assert!(flags("http://2130706433/", "x").contains(&LinkFlag::NumericAddress));
    }

    #[test]
    fn hostile_text_and_length_are_handled() {
        let l = analyze_link("https://example.com/", "bank\u{202E}moc.evil").unwrap();
        assert!(!l.text.contains('\u{202E}'));
        assert!(analyze_link(&format!("https://example.com/{}", "a".repeat(9000)), "x").is_none());
        let long_text = "x".repeat(10_000);
        assert!(
            analyze_link("https://example.com/", &long_text)
                .unwrap()
                .text
                .chars()
                .count()
                <= 300
        );
    }
}
