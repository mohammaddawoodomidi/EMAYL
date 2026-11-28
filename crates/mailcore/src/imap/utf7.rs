//! IMAP "modified UTF-7" folder names (RFC 3501, section 5.1.3, as recalled).
//!
//! Servers send non-ASCII folder names such as `Entw&APw-rfe` ("Entwürfe").
//! The raw form is what the server wants back in commands, so the raw name is
//! the folder's identity; the decoded form is for display only. Decoding is
//! lenient: a malformed name decodes to `None` and the caller shows the raw name.

use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig};

const ENGINE: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new()
        .with_encode_padding(false)
        .with_decode_padding_mode(DecodePaddingMode::Indifferent)
        .with_decode_allow_trailing_bits(true),
);

/// Decodes a modified UTF-7 name. `None` if it is malformed.
pub fn decode(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(pos) = rest.find('&') {
        let (plain, after) = rest.split_at(pos);
        if !plain.is_ascii() {
            return None; // raw names must be ASCII
        }
        out.push_str(plain);
        let after = &after[1..];
        let end = after.find('-')?;
        let body = &after[..end];
        if body.is_empty() {
            out.push('&');
        } else {
            let std_b64 = body.replace(',', "/");
            let bytes = ENGINE.decode(std_b64.as_bytes()).ok()?;
            if bytes.len() % 2 != 0 {
                return None;
            }
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|c| u16::from_be_bytes([c[0], c[1]]))
                .collect();
            out.push_str(&String::from_utf16(&units).ok()?);
        }
        rest = &after[end + 1..];
    }
    if !rest.is_ascii() {
        return None;
    }
    out.push_str(rest);
    Some(out)
}

/// Encodes a display name into modified UTF-7 for use in commands.
pub fn encode(name: &str) -> String {
    fn flush(run: &mut Vec<u16>, out: &mut String) {
        if run.is_empty() {
            return;
        }
        let mut bytes = Vec::with_capacity(run.len() * 2);
        for u in run.iter() {
            bytes.extend_from_slice(&u.to_be_bytes());
        }
        out.push('&');
        out.push_str(&ENGINE.encode(&bytes).replace('/', ","));
        out.push('-');
        run.clear();
    }

    let mut out = String::with_capacity(name.len());
    let mut run: Vec<u16> = Vec::new();
    for ch in name.chars() {
        if ('\u{20}'..='\u{7e}').contains(&ch) {
            flush(&mut run, &mut out);
            if ch == '&' {
                out.push_str("&-");
            } else {
                out.push(ch);
            }
        } else {
            let mut buf = [0u16; 2];
            run.extend_from_slice(ch.encode_utf16(&mut buf));
        }
    }
    flush(&mut run, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_ascii_is_unchanged() {
        assert_eq!(
            decode("INBOX/Sent Items").as_deref(),
            Some("INBOX/Sent Items")
        );
        assert_eq!(encode("INBOX/Sent Items"), "INBOX/Sent Items");
    }

    #[test]
    fn ampersand_is_escaped() {
        assert_eq!(decode("Q&-A").as_deref(), Some("Q&A"));
        assert_eq!(encode("Q&A"), "Q&-A");
    }

    #[test]
    fn umlaut_example() {
        assert_eq!(decode("Entw&APw-rfe").as_deref(), Some("Entwürfe"));
        assert_eq!(encode("Entwürfe"), "Entw&APw-rfe");
    }

    #[test]
    fn rfc_style_mixed_example() {
        // Japanese + ASCII, the classic example from the specification text.
        let raw = "~peter/mail/&ZeVnLIqe-/&U,BTFw-";
        let decoded = decode(raw).unwrap();
        assert_eq!(decoded, "~peter/mail/日本語/台北");
        assert_eq!(encode(&decoded), raw);
    }

    #[test]
    fn round_trips_including_emoji_surrogate_pairs() {
        for s in [
            "Привет",
            "📥 Inbox",
            "a&b&c",
            "日本語&English",
            "tab\there",
            "",
        ] {
            assert_eq!(decode(&encode(s)).as_deref(), Some(s), "{s:?}");
        }
    }

    #[test]
    fn malformed_input_is_none_not_a_panic() {
        assert_eq!(decode("&"), None); // unterminated shift
        assert_eq!(decode("&AAA"), None); // unterminated shift
        assert_eq!(decode("&!!!-"), None); // not base64
        assert_eq!(decode("&AA-"), None); // odd number of bytes
        assert_eq!(decode("&2D0-"), None); // lone surrogate half
        assert_eq!(decode("caf\u{e9}"), None); // raw names are ASCII
    }
}
