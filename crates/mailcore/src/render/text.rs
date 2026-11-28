//! Plain text to blocks (with quote levels and links) and blocks back to plain text.

use super::links::analyze_link;
use super::{Block, Inline};
use crate::textsafe::is_invisible;

const MAX_LINES: usize = 200_000;
const MAX_QUOTE_DEPTH: usize = 12;
const MAX_TEXT_OUT: usize = 8 * 1024 * 1024;

fn plain(text: &str) -> Inline {
    Inline::Text {
        text: text.to_owned(),
        bold: false,
        italic: false,
        code: false,
    }
}

/// Splits a line into text and web links. Only addresses that begin with
/// `http://` or `https://` are turned into links.
fn linkify(line: &str, out: &mut Vec<Inline>) {
    let mut rest = line;
    while let Some(start) = find_scheme(rest) {
        let (before, tail) = rest.split_at(start);
        let end = tail
            .find(|c: char| {
                c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\u{201D}' | '\u{201C}')
            })
            .unwrap_or(tail.len());
        let mut url = &tail[..end];
        // Trailing punctuation belongs to the sentence, not the address.
        while let Some(last) = url.chars().last() {
            let unbalanced_paren =
                last == ')' && url.matches('(').count() >= url.matches(')').count();
            if matches!(last, '.' | ',' | ';' | ':' | '!' | '?' | '\'' | ']' | '}')
                || (last == ')' && !unbalanced_paren)
            {
                url = &url[..url.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        if !before.is_empty() {
            out.push(plain(before));
        }
        match analyze_link(url, url) {
            Some(l) => out.push(Inline::Link(l)),
            None => out.push(plain(url)),
        }
        rest = &tail[url.len()..];
    }
    if !rest.is_empty() {
        out.push(plain(rest));
    }
}

fn find_scheme(s: &str) -> Option<usize> {
    let lower = s.to_ascii_lowercase();
    match (lower.find("https://"), lower.find("http://")) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

fn clean_text_line(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    for c in line.chars() {
        if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() && !is_invisible(c) {
            out.push(c);
        }
    }
    out.trim_end().to_owned()
}

/// `(quote depth, text)` for one line: counts leading `>` markers.
fn split_quote(line: &str) -> (usize, &str) {
    let mut depth = 0;
    let mut rest = line;
    loop {
        let t = rest.trim_start_matches(' ');
        if let Some(r) = t.strip_prefix('>') {
            depth += 1;
            rest = r;
        } else {
            break;
        }
    }
    (
        depth.min(MAX_QUOTE_DEPTH),
        rest.strip_prefix(' ').unwrap_or(rest),
    )
}

fn group(lines: &[(usize, String)], depth: usize) -> Vec<Block> {
    let mut blocks = Vec::new();
    let mut para: Vec<&str> = Vec::new();
    let flush = |para: &mut Vec<&str>, blocks: &mut Vec<Block>| {
        if para.is_empty() {
            return;
        }
        let mut inl = Vec::new();
        for (i, l) in para.iter().enumerate() {
            if i > 0 {
                inl.push(Inline::LineBreak);
            }
            linkify(l, &mut inl);
        }
        blocks.push(Block::Paragraph(inl));
        para.clear();
    };
    let mut i = 0;
    while i < lines.len() {
        let (d, text) = &lines[i];
        if *d > depth {
            flush(&mut para, &mut blocks);
            let start = i;
            while i < lines.len() && lines[i].0 > depth {
                i += 1;
            }
            blocks.push(Block::Quote(group(&lines[start..i], depth + 1)));
            continue;
        }
        if text.trim().is_empty() {
            flush(&mut para, &mut blocks);
        } else {
            para.push(text);
        }
        i += 1;
    }
    flush(&mut para, &mut blocks);
    blocks
}

pub(super) fn text_to_blocks(text: &str) -> Vec<Block> {
    let lines: Vec<(usize, String)> = text
        .lines()
        .take(MAX_LINES)
        .map(|l| {
            let (d, t) = split_quote(l);
            (d, clean_text_line(t))
        })
        .collect();
    group(&lines, 0)
}

// ---------------------------------------------------------------------------
// blocks → plain text
// ---------------------------------------------------------------------------

fn inlines_to_text(inl: &[Inline], out: &mut String) {
    for i in inl {
        match i {
            Inline::Text { text, .. } => out.push_str(text),
            Inline::Link(l) => {
                if l.text.is_empty() || l.text == l.url {
                    out.push_str(&l.url);
                } else {
                    out.push_str(&format!("{} ({})", l.text, l.url));
                }
            }
            Inline::Image(img) => {
                if !img.alt.is_empty() {
                    out.push_str(&format!("[{}]", img.alt));
                }
            }
            Inline::LineBreak => out.push('\n'),
        }
    }
}

fn blocks_into(blocks: &[Block], out: &mut String, indent: &str) {
    for (n, b) in blocks.iter().enumerate() {
        if n > 0 && !out.ends_with("\n\n") {
            out.push_str(if out.ends_with('\n') { "\n" } else { "\n\n" });
        }
        if out.len() > MAX_TEXT_OUT {
            return;
        }
        match b {
            Block::Paragraph(i) | Block::Heading { content: i, .. } => {
                let mut s = String::new();
                inlines_to_text(i, &mut s);
                for line in s.lines() {
                    out.push_str(indent);
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Block::Preformatted(t) => {
                for line in t.lines() {
                    out.push_str(indent);
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Block::Rule => {
                out.push_str(indent);
                out.push_str("---\n");
            }
            Block::Quote(inner) => {
                let mut s = String::new();
                blocks_into(inner, &mut s, "");
                for line in s.trim_end().lines() {
                    out.push_str(indent);
                    out.push_str(if line.is_empty() { ">" } else { "> " });
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Block::List { ordered, items } => {
                for (k, item) in items.iter().enumerate() {
                    let marker = if *ordered {
                        format!("{}. ", k + 1)
                    } else {
                        "- ".to_owned()
                    };
                    let mut s = String::new();
                    blocks_into(item, &mut s, "");
                    for (j, line) in s.trim_end().lines().enumerate() {
                        out.push_str(indent);
                        if j == 0 {
                            out.push_str(&marker);
                        } else {
                            out.push_str(&" ".repeat(marker.len()));
                        }
                        out.push_str(line);
                        out.push('\n');
                    }
                }
            }
        }
    }
}

/// Readable text of a document: used for replies, search and the plain view.
pub fn blocks_to_text(blocks: &[Block]) -> String {
    let mut out = String::new();
    blocks_into(blocks, &mut out, "");
    out.truncate(out.len().min(MAX_TEXT_OUT));
    out.trim_end().to_owned() + if out.trim().is_empty() { "" } else { "\n" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn para_text(b: &Block) -> String {
        let Block::Paragraph(i) = b else {
            panic!("not a paragraph: {b:?}")
        };
        let mut s = String::new();
        inlines_to_text(i, &mut s);
        s
    }

    #[test]
    fn paragraphs_keep_line_breaks_and_split_on_blank_lines() {
        let b = text_to_blocks("Hello,\nworld\n\nSecond paragraph\n");
        assert_eq!(b.len(), 2);
        assert_eq!(para_text(&b[0]), "Hello,\nworld");
        assert_eq!(para_text(&b[1]), "Second paragraph");
    }

    #[test]
    fn quotes_nest_by_marker_depth() {
        let b = text_to_blocks("reply\n> first\n> > deeper\n> back\nend");
        assert_eq!(b.len(), 3);
        let Block::Quote(q) = &b[1] else { panic!() };
        assert_eq!(q.len(), 3);
        assert_eq!(para_text(&q[0]), "first");
        assert!(matches!(&q[1], Block::Quote(_)));
        assert_eq!(para_text(&q[2]), "back");
        assert_eq!(para_text(&b[2]), "end");
    }

    #[test]
    fn web_links_are_found_and_punctuation_is_left_out() {
        let b = text_to_blocks(
            "See https://example.com/a?b=1, and (http://example.org/x). Also <https://example.net/z>.",
        );
        let Block::Paragraph(i) = &b[0] else { panic!() };
        let urls: Vec<_> = i
            .iter()
            .filter_map(|x| {
                if let Inline::Link(l) = x {
                    Some(l.url.clone())
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(
            urls,
            vec![
                "https://example.com/a?b=1",
                "http://example.org/x",
                "https://example.net/z"
            ]
        );
    }

    #[test]
    fn dangerous_or_incomplete_links_stay_text() {
        let b = text_to_blocks("javascript:alert(1) and http:// and https://");
        let Block::Paragraph(i) = &b[0] else { panic!() };
        assert!(i.iter().all(|x| !matches!(x, Inline::Link(_))));
    }

    #[test]
    fn invisible_characters_and_controls_are_removed() {
        let b = text_to_blocks("pay\u{202E}fdp.exe\u{0}\u{7}\ttab");
        assert_eq!(para_text(&b[0]), "payfdp.exe    tab");
    }

    #[test]
    fn extreme_input_is_bounded() {
        let many = "> ".repeat(500) + "deep";
        let b = text_to_blocks(&many);
        let mut depth = 0;
        let mut cur = &b;
        while let Some(Block::Quote(inner)) = cur.first() {
            depth += 1;
            cur = inner;
        }
        assert!(depth <= MAX_QUOTE_DEPTH, "{depth}");
        let lines = "x\n".repeat(MAX_LINES + 50_000);
        assert!(text_to_blocks(&lines).len() <= 1);
    }

    #[test]
    fn round_trip_to_text_for_replies() {
        let b = text_to_blocks("Hi\n\n> quoted line\n> second\n\nBye https://example.com/x");
        let t = blocks_to_text(&b);
        assert_eq!(
            t,
            "Hi\n\n> quoted line\n> second\n\nBye https://example.com/x\n"
        );
    }

    #[test]
    fn lists_and_rules_flatten_sensibly() {
        let blocks = vec![
            Block::List {
                ordered: true,
                items: vec![
                    vec![Block::Paragraph(vec![plain("one")])],
                    vec![Block::Paragraph(vec![plain("two")])],
                ],
            },
            Block::Rule,
        ];
        assert_eq!(blocks_to_text(&blocks), "1. one\n2. two\n\n---\n");
        assert_eq!(blocks_to_text(&[]), "");
    }
}
