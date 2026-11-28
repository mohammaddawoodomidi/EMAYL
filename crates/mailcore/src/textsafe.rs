//! Cleaning text that came from outside before it is shown to a person.
//!
//! Beyond ordinary control characters this removes *invisible* characters that
//! attackers use to disguise things: text-direction overrides (which make
//! `invoice\u{202E}fdp.exe` look like `invoiceexe.pdf`), zero-width characters
//! and soft hyphens.

/// True for characters that change how text is laid out without being visible.
pub(crate) fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'                    // soft hyphen
        | '\u{034F}'                  // combining grapheme joiner
        | '\u{061C}'                  // Arabic letter mark
        | '\u{180E}'                  // Mongolian vowel separator
        | '\u{200B}'..='\u{200F}'     // zero-width space/joiners, LRM, RLM
        | '\u{202A}'..='\u{202E}'     // embeddings and overrides
        | '\u{2060}'..='\u{2064}'     // word joiner, invisible operators
        | '\u{2066}'..='\u{2069}'     // isolates
        | '\u{FEFF}'                  // byte-order mark / zero-width no-break space
    )
}

/// Directional formatting characters only (the ones that can reorder text).
pub(crate) fn is_bidi_control(c: char) -> bool {
    matches!(c, '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
}

pub(crate) fn has_bidi_control(s: &str) -> bool {
    s.chars().any(is_bidi_control)
}

/// Removes control and invisible characters, collapses runs of whitespace to one
/// space, and keeps at most `max_chars` characters.
pub(crate) fn clean_line(s: &str, max_chars: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_chars * 2));
    let mut pending_space = false;
    let mut count = 0;
    for c in s.chars() {
        if is_invisible(c) {
            continue;
        }
        if c.is_whitespace() || c.is_control() {
            // Control characters other than whitespace are dropped; whitespace becomes a space.
            if c.is_whitespace() {
                pending_space = !out.is_empty();
            }
            continue;
        }
        // A space only counts if a character follows it within the limit.
        let needed = if pending_space { 2 } else { 1 };
        if count + needed > max_chars {
            break;
        }
        if pending_space {
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        out.push(c);
        count += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_direction_overrides_and_zero_width() {
        assert_eq!(clean_line("invoice\u{202E}fdp.exe", 100), "invoicefdp.exe");
        assert_eq!(clean_line("a\u{200B}b\u{FEFF}c\u{00AD}d", 100), "abcd");
        assert!(has_bidi_control("x\u{2067}y"));
        assert!(!has_bidi_control("plain"));
    }

    #[test]
    fn collapses_whitespace_and_drops_controls() {
        assert_eq!(clean_line("  a \t\r\n b\u{0}\u{7}c  ", 100), "a bc");
        assert_eq!(clean_line("", 10), "");
    }

    #[test]
    fn truncates_by_characters_not_bytes() {
        assert_eq!(clean_line("ééééé", 3), "ééé");
        assert_eq!(clean_line("ab cd", 3), "ab");
    }
}
