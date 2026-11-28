//! Email address and hostname validation.
//!
//! Deliberately stricter than RFC 5322: quoted local parts, comments, IP-literal
//! domains and non-ASCII (SMTPUTF8) local parts are rejected in v1. Everything
//! accepted here is safe to place in a URL query, an IMAP/SMTP command (after
//! normal quoting) and a header without enabling injection.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::{Error, Result};

const MAX_ADDRESS_LEN: usize = 254;
const MAX_LOCAL_LEN: usize = 64;
const MAX_DOMAIN_LEN: usize = 253;
const MAX_LABEL_LEN: usize = 63;

/// A validated email address. The domain is stored lowercase, IDNA/punycode ASCII.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct EmailAddress {
    local: String,
    domain: String,
}

impl EmailAddress {
    pub fn parse(input: &str) -> Result<Self> {
        let s = input.trim();
        if s.is_empty() {
            return Err(Error::InvalidEmail("address is empty"));
        }
        if s.len() > MAX_ADDRESS_LEN {
            return Err(Error::InvalidEmail("address is too long"));
        }
        let (local, domain) = s
            .rsplit_once('@')
            .ok_or(Error::InvalidEmail("missing '@'"))?;
        if local.is_empty() {
            return Err(Error::InvalidEmail("missing part before '@'"));
        }
        if local.len() > MAX_LOCAL_LEN {
            return Err(Error::InvalidEmail("part before '@' is too long"));
        }
        if !local.chars().all(is_local_char) {
            return Err(Error::InvalidEmail(
                "unsupported character before '@' (quoted, commented and non-ASCII forms are not supported)",
            ));
        }
        if local.starts_with('.') || local.ends_with('.') || local.contains("..") {
            return Err(Error::InvalidEmail("misplaced '.' before '@'"));
        }
        let domain = normalize_hostname(domain).map_err(Error::InvalidEmail)?;
        Ok(Self {
            local: local.to_owned(),
            domain,
        })
    }

    pub fn local_part(&self) -> &str {
        &self.local
    }

    /// Lowercase ASCII (punycode) domain.
    pub fn domain(&self) -> &str {
        &self.domain
    }
}

impl fmt::Display for EmailAddress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{}", self.local, self.domain)
    }
}

impl Serialize for EmailAddress {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for EmailAddress {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).map_err(serde::de::Error::custom)
    }
}

fn is_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric()
        || matches!(
            c,
            '.' | '!'
                | '#'
                | '$'
                | '%'
                | '&'
                | '\''
                | '*'
                | '+'
                | '-'
                | '/'
                | '='
                | '?'
                | '^'
                | '_'
                | '`'
                | '{'
                | '|'
                | '}'
                | '~'
        )
}

/// Normalise a DNS name to lowercase ASCII (punycode) and validate it.
///
/// Requires at least two labels and rejects IP addresses and single-label names
/// such as `localhost`; the TLS layer verifies certificates against DNS names.
pub(crate) fn normalize_hostname(input: &str) -> std::result::Result<String, &'static str> {
    let trimmed = input.strip_suffix('.').unwrap_or(input);
    if trimmed.is_empty() {
        return Err("name is empty");
    }
    let ascii = idna::domain_to_ascii(trimmed)
        .map_err(|_| "name is not a valid domain")?
        .to_ascii_lowercase();
    if ascii.is_empty() || ascii.len() > MAX_DOMAIN_LEN {
        return Err("name has an invalid length");
    }
    let mut labels = 0usize;
    let mut last = "";
    for label in ascii.split('.') {
        if label.is_empty() || label.len() > MAX_LABEL_LEN {
            return Err("name has an empty or too long label");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("a label may not start or end with '-'");
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        {
            return Err("name contains characters not allowed in a hostname");
        }
        labels += 1;
        last = label;
    }
    if labels < 2 {
        return Err("name needs at least two labels (for example mail.example.com)");
    }
    if last.bytes().all(|b| b.is_ascii_digit()) {
        return Err("IP addresses are not supported; use a hostname");
    }
    Ok(ascii)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_normal_addresses() {
        let a = EmailAddress::parse("  First.Last+tag@Example.COM ").unwrap();
        assert_eq!(a.local_part(), "First.Last+tag");
        assert_eq!(a.domain(), "example.com");
        assert_eq!(a.to_string(), "First.Last+tag@example.com");
    }

    #[test]
    fn converts_idn_domains_to_punycode() {
        let a = EmailAddress::parse("user@bücher.de").unwrap();
        assert_eq!(a.domain(), "xn--bcher-kva.de");
    }

    #[test]
    fn rejects_malformed_addresses() {
        let long_local = format!("{}@example.com", "a".repeat(65));
        let cases = [
            "",
            "plain",
            "a@b",
            "@example.com",
            "a@@example.com",
            "a b@example.com",
            "a..b@example.com",
            ".a@example.com",
            "a.@example.com",
            "a@-bad.example.com",
            "a@exa_mple.com",
            "a@1.2.3.4",
            "a@[127.0.0.1]",
            "üser@example.com",
            "a@localhost",
            long_local.as_str(),
        ];
        for case in cases {
            assert!(EmailAddress::parse(case).is_err(), "should reject {case:?}");
        }
    }

    #[test]
    fn rejects_header_injection_attempts() {
        assert!(EmailAddress::parse("a@example.com\r\nBcc: x@y.example").is_err());
        assert!(EmailAddress::parse("a@example.com,b@example.com").is_err());
        assert!(EmailAddress::parse("a\0@example.com").is_err());
    }

    #[test]
    fn trailing_newline_is_trimmed_not_smuggled() {
        let a = EmailAddress::parse("a@example.com\n").unwrap();
        assert_eq!(a.to_string(), "a@example.com");
    }

    #[test]
    fn hostname_rules() {
        assert_eq!(
            normalize_hostname("IMAP.Example.com.").unwrap(),
            "imap.example.com"
        );
        assert!(normalize_hostname("example").is_err());
        assert!(normalize_hostname("10.0.0.1").is_err());
        assert!(normalize_hostname("exa mple.com").is_err());
    }
}
