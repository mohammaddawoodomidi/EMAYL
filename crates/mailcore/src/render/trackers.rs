//! Tracking: what senders hide in a message to find out that it was opened and what was
//! clicked, and what this program does about it.
//!
//! * Pictures from the internet are never loaded (see `html.rs`), so a tracking pixel never
//!   reaches its server. This module only *counts* them for the person to see.
//! * Links often carry codes that tell the sender who clicked ("utm_source", "fbclid", ...).
//!   [`strip_tracking`] removes the well-known ones before a link is opened or copied.
//!   It only removes names on a fixed list, and only from the query part of a web address,
//!   so a link that needs its parameters keeps working.

use url::Url;

use super::{Block, Inline, RenderedMessage};

/// Query parameters that exist only to track people.
const TRACKING_PARAMS: &[&str] = &[
    "fbclid",
    "gclid",
    "gclsrc",
    "dclid",
    "gbraid",
    "wbraid",
    "msclkid",
    "yclid",
    "twclid",
    "ttclid",
    "li_fat_id",
    "igshid",
    "mc_eid",
    "mc_cid",
    "_hsenc",
    "_hsmi",
    "__hstc",
    "__hssc",
    "__hsfp",
    "hsctatracking",
    "mkt_tok",
    "vero_id",
    "vero_conv",
    "oly_enc_id",
    "oly_anon_id",
    "_openstat",
    "trk",
    "trkemail",
    "s_cid",
];

/// Prefixes of parameter families used by analytics tools.
const TRACKING_PREFIXES: &[&str] = &["utm_", "mtm_", "pk_", "piwik_"];

fn is_tracking_param(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    TRACKING_PARAMS.contains(&n.as_str()) || TRACKING_PREFIXES.iter().any(|p| n.starts_with(p))
}

/// Returns the address without its tracking parameters, and the names that were removed.
/// Anything that is not a plain web address is returned untouched.
pub fn strip_tracking(address: &str) -> (String, Vec<String>) {
    let untouched = || (address.to_owned(), Vec::new());
    let Ok(mut url) = Url::parse(address) else {
        return untouched();
    };
    if !matches!(url.scheme(), "http" | "https") || url.query().is_none() {
        return untouched();
    }
    let mut kept: Vec<(String, String)> = Vec::new();
    let mut removed: Vec<String> = Vec::new();
    for (k, v) in url.query_pairs() {
        if is_tracking_param(&k) {
            if !removed.iter().any(|r| r.eq_ignore_ascii_case(&k)) {
                removed.push(k.into_owned());
            }
        } else {
            kept.push((k.into_owned(), v.into_owned()));
        }
    }
    if removed.is_empty() {
        return untouched();
    }
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut().clear().extend_pairs(kept);
    }
    (url.to_string(), removed)
}

/// What a message tried to do, in numbers.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrackerSummary {
    /// Pictures the message wanted to load from the internet (none were loaded).
    pub pictures: usize,
    /// Of those, the ones that look like tracking pixels or beacons.
    pub tracking_pictures: usize,
    /// Sites those pictures would have contacted, without repeats (at most 6).
    pub hosts: Vec<String>,
    /// Links in the text that carry tracking codes.
    pub tracking_links: usize,
}

impl TrackerSummary {
    pub fn is_empty(&self) -> bool {
        self.pictures == 0 && self.tracking_links == 0
    }
}

fn count_links(blocks: &[Block], n: &mut usize) {
    for b in blocks {
        match b {
            Block::Paragraph(inl) | Block::Heading { content: inl, .. } => {
                for i in inl {
                    if let Inline::Link(l) = i
                        && !strip_tracking(&l.url).1.is_empty()
                    {
                        *n += 1;
                    }
                }
            }
            Block::Quote(inner) => count_links(inner, n),
            Block::List { items, .. } => items.iter().for_each(|i| count_links(i, n)),
            Block::Preformatted(_) | Block::Rule => {}
        }
    }
}

impl RenderedMessage {
    pub fn tracker_summary(&self) -> TrackerSummary {
        let mut s = TrackerSummary {
            pictures: self.remote_images.len(),
            ..TrackerSummary::default()
        };
        for img in &self.remote_images {
            if img.likely_tracker {
                s.tracking_pictures += 1;
            }
            if s.hosts.len() < 6 && !img.host.is_empty() && !s.hosts.contains(&img.host) {
                s.hosts.push(img.host.clone());
            }
        }
        count_links(&self.body, &mut s.tracking_links);
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_tracking_codes_are_removed_and_the_rest_is_kept() {
        let (clean, gone) = strip_tracking(
            "https://shop.example/item?id=42&utm_source=news&UTM_Medium=email&fbclid=abc&size=m#reviews",
        );
        assert_eq!(clean, "https://shop.example/item?id=42&size=m#reviews");
        assert_eq!(gone, vec!["utm_source", "UTM_Medium", "fbclid"]);
    }

    #[test]
    fn a_link_with_only_tracking_codes_loses_the_whole_query() {
        let (clean, gone) = strip_tracking("https://a.example/page?utm_campaign=x&gclid=1");
        assert_eq!(clean, "https://a.example/page");
        assert_eq!(gone.len(), 2);
    }

    #[test]
    fn links_that_need_their_parameters_are_left_exactly_alone() {
        for u in [
            "https://bank.example/confirm?token=AbC%20123&user=7",
            "https://a.example/",
            "https://a.example/?x=1&y=2",
            "mailto:me@example.org?subject=Hi",
            "not a url",
            "tel:+123456",
        ] {
            let (clean, gone) = strip_tracking(u);
            assert_eq!(clean, u);
            assert!(gone.is_empty(), "{u}");
        }
    }

    #[test]
    fn names_that_only_look_similar_are_not_removed() {
        let (clean, gone) = strip_tracking("https://a.example/?utmost=1&refid=2&gclidx=3");
        assert!(gone.is_empty());
        assert_eq!(clean, "https://a.example/?utmost=1&refid=2&gclidx=3");
    }

    #[test]
    fn the_summary_counts_pictures_hosts_and_tracking_links() {
        let raw = b"From: a@b.example\r\nSubject: s\r\nContent-Type: text/html\r\n\r\n\
            <p>Hi <a href=\"https://x.example/?utm_source=a\">one</a> \
            <a href=\"https://x.example/plain\">two</a></p>\
            <img src=\"https://t.example/pixel.gif\" width=\"1\" height=\"1\">\
            <img src=\"https://cdn.example/logo.png\" width=\"200\">\
            <img src=\"https://t.example/other-pixel.gif\">";
        let rm = crate::render::render(raw, &crate::render::RenderOptions::default()).unwrap();
        let s = rm.tracker_summary();
        assert_eq!(s.pictures, 3);
        assert_eq!(s.tracking_pictures, 2);
        assert_eq!(s.tracking_links, 1);
        assert_eq!(s.hosts, vec!["t.example", "cdn.example"]);
        assert!(!s.is_empty());
        let plain = b"From: a@b.example\r\nSubject: s\r\n\r\nhello";
        let rm = crate::render::render(plain, &crate::render::RenderOptions::default()).unwrap();
        assert!(rm.tracker_summary().is_empty());
    }
}
