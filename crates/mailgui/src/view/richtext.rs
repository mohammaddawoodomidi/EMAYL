//! Draws a message body (the safe block model from `mailcore::render`) with
//! native widgets. Nothing here can run code or fetch anything.

use iced::widget::text::{Span, Wrapping};
use iced::widget::{button, column, container, rich_text, row, span, text};
use iced::{Color, Element, Font, Length, font};
use mailcore::render::{Block, Inline, Link, RenderedMessage};

use super::ui::{BOLD, muted};
use crate::app::Message;
use crate::theme;

const MONO: Font = Font::MONOSPACE;

fn italic() -> Font {
    Font {
        style: font::Style::Italic,
        ..Font::DEFAULT
    }
}

fn spans<'a>(inlines: &'a [Inline], base: Color, heading: bool) -> Vec<Span<'a, Link, Font>> {
    let mut out: Vec<Span<'a, Link, Font>> = Vec::new();
    for inline in inlines {
        match inline {
            Inline::Text {
                text,
                bold,
                italic: it,
                code,
            } => {
                let mut s = span(text.as_str()).color(base);
                if *code {
                    s = s
                        .font(MONO)
                        .background(iced::Background::Color(theme::c().panel));
                } else if *bold || heading {
                    s = s.font(BOLD);
                } else if *it {
                    s = s.font(italic());
                }
                out.push(s);
            }
            Inline::Link(link) => {
                let color = if link.is_suspicious() {
                    theme::c().warn
                } else {
                    theme::c().accent
                };
                out.push(
                    span(link_label(link))
                        .color(color)
                        .underline(true)
                        .link(link.clone()),
                );
            }
            // Pictures are not loaded, and a note saying so for each one only gets in the
            // way of reading. The message's details say how many were left out.
            Inline::Image(_) => {}
            Inline::LineBreak => out.push(span("\n")),
        }
    }
    out
}

/// What a link shows. A long raw address becomes just the site (`example.com/…`), because
/// the full address is one click away in the confirmation. A link with a warning keeps its
/// text exactly as the sender wrote it, so that a mismatch stays visible.
fn link_label(link: &Link) -> String {
    let text = link.text.trim();
    if link.is_suspicious() {
        return if text.is_empty() {
            link.url.clone()
        } else {
            text.to_owned()
        };
    }
    if link.host.is_empty() {
        // mailto: and tel: links read fine as they are.
        return if text.is_empty() {
            link.url.clone()
        } else {
            text.to_owned()
        };
    }
    if text.is_empty() {
        return link.host.clone();
    }
    let lower = text.to_ascii_lowercase();
    let is_address =
        lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("www.");
    if !is_address || text.chars().any(char::is_whitespace) {
        return text.to_owned();
    }
    let rest = lower
        .split_once("://")
        .map_or(lower.as_str(), |(_, r)| r)
        .trim_start_matches("www.");
    let has_path = rest
        .split_once('/')
        .is_some_and(|(_, tail)| !tail.trim_matches('/').is_empty())
        || rest.contains('?');
    if has_path {
        format!("{}/…", link.host)
    } else {
        link.host.clone()
    }
}

/// Whether some of these inlines would show anything a person reads.
fn has_visible(inlines: &[Inline]) -> bool {
    inlines.iter().any(|i| match i {
        Inline::Text { text, .. } => !text.trim().is_empty(),
        Inline::Link(_) => true,
        Inline::Image(_) | Inline::LineBreak => false,
    })
}

fn block_has_text(b: &Block) -> bool {
    match b {
        Block::Paragraph(i) | Block::Heading { content: i, .. } => has_visible(i),
        Block::Quote(inner) => inner.iter().any(block_has_text),
        Block::List { items, .. } => items.iter().any(|it| it.iter().any(block_has_text)),
        Block::Preformatted(s) => !s.trim().is_empty(),
        Block::Rule => false,
    }
}

/// A short line just before a quote that says whose words follow ("On Monday, Ann wrote:").
fn is_attribution(b: &Block) -> bool {
    let Block::Paragraph(inl) = b else {
        return false;
    };
    let t: String = inl
        .iter()
        .map(|i| match i {
            Inline::Text { text, .. } => text.as_str(),
            Inline::Link(l) => l.text.as_str(),
            _ => " ",
        })
        .collect();
    let t = t.trim();
    t.chars().count() <= 250 && t.ends_with(':')
}

/// Which top-level blocks are earlier messages quoted in a reply (the quote itself and the
/// line that introduces it). Empty when there is nothing else to read: a forwarded message
/// that is all quote must stay visible.
fn quoted_indexes(blocks: &[Block]) -> Vec<usize> {
    let main = blocks
        .iter()
        .any(|b| !matches!(b, Block::Quote(_)) && block_has_text(b) && !is_attribution(b));
    if !main {
        return Vec::new();
    }
    let mut hidden = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if matches!(b, Block::Quote(_)) {
            if i > 0 && is_attribution(&blocks[i - 1]) && !hidden.contains(&(i - 1)) {
                hidden.push(i - 1);
            }
            hidden.push(i);
        }
    }
    hidden
}

fn heading_size(level: u8) -> f32 {
    match level {
        1 => 24.0,
        2 => 20.0,
        3 => 18.0,
        _ => 16.0,
    }
}

fn blocks_view<'a>(
    blocks: &'a [Block],
    rm: &'a RenderedMessage,
    base: Color,
    depth: usize,
) -> Element<'a, Message> {
    let mut col = column![].spacing(10).width(Length::Fill);
    for b in blocks {
        if matches!(b, Block::Paragraph(i) if !has_visible(i)) {
            continue;
        }
        col = col.push(block_view(b, rm, base, depth));
    }
    col.into()
}

fn block_view<'a>(
    b: &'a Block,
    rm: &'a RenderedMessage,
    base: Color,
    depth: usize,
) -> Element<'a, Message> {
    match b {
        Block::Paragraph(inl) => {
            let s = spans(inl, base, false);
            if s.is_empty() {
                return column![].into();
            }
            rich_text(s)
                .size(15)
                .line_height(1.45)
                .wrapping(Wrapping::WordOrGlyph)
                .width(Length::Fill)
                .on_link_click(Message::LinkClicked)
                .into()
        }
        Block::Heading { level, content } => rich_text(spans(content, base, true))
            .size(heading_size(*level))
            .wrapping(Wrapping::WordOrGlyph)
            .width(Length::Fill)
            .on_link_click(Message::LinkClicked)
            .into(),
        Block::Quote(inner) => {
            if depth >= 6 {
                return muted("[earlier messages not shown]").into();
            }
            row![
                container(iced::widget::space())
                    .width(3)
                    .height(Length::Fill)
                    .style(theme::quote_bar),
                container(blocks_view(inner, rm, theme::c().muted, depth + 1))
                    .padding(iced::Padding {
                        left: 4.0,
                        ..iced::Padding::ZERO
                    })
                    .width(Length::Fill)
            ]
            .spacing(10)
            .height(Length::Shrink)
            .into()
        }
        Block::List { ordered, items } => {
            let mut col = column![].spacing(6).width(Length::Fill);
            for (i, item) in items.iter().enumerate() {
                let marker = if *ordered {
                    format!("{}.", i + 1)
                } else {
                    "•".to_owned()
                };
                col = col.push(
                    row![
                        container(text(marker).size(15).color(theme::c().muted)).width(24),
                        blocks_view(item, rm, base, depth + 1)
                    ]
                    .spacing(4),
                );
            }
            col.into()
        }
        Block::Preformatted(s) => container(
            text(s.as_str())
                .font(MONO)
                .size(13)
                .color(base)
                .wrapping(Wrapping::WordOrGlyph),
        )
        .padding(10)
        .width(Length::Fill)
        .style(theme::code_block)
        .into(),
        Block::Rule => container(iced::widget::space())
            .width(Length::Fill)
            .height(1)
            .style(theme::quote_bar)
            .into(),
    }
}

/// The main text of a message. Earlier messages quoted in a reply are folded away behind one
/// small button; `quotes_open` unfolds them.
pub fn body<'a>(rm: &'a RenderedMessage, id: i64, quotes_open: bool) -> Element<'a, Message> {
    if rm.body.is_empty() {
        return muted("(This message has no text.)").into();
    }
    if !rm.body.iter().any(block_has_text) {
        return muted("(This message is made of pictures only.)").into();
    }
    let hidden = quoted_indexes(&rm.body);
    if hidden.is_empty() {
        return blocks_view(&rm.body, rm, theme::c().text, 0);
    }
    let toggle = |open: bool| -> Element<'a, Message> {
        button(
            text(if open {
                "Hide quoted text"
            } else {
                "Show quoted text"
            })
            .size(12)
            .color(theme::c().muted),
        )
        .padding(iced::Padding::from([3, 10]))
        .style(theme::ghost)
        .on_press(Message::ToggleQuotes(id))
        .into()
    };
    let mut col = column![].spacing(10).width(Length::Fill);
    let mut placed = false;
    for (i, b) in rm.body.iter().enumerate() {
        if hidden.contains(&i) {
            if !placed {
                col = col.push(toggle(quotes_open));
                placed = true;
            }
            if !quotes_open {
                continue;
            }
        }
        if matches!(b, Block::Paragraph(inl) if !has_visible(inl)) {
            continue;
        }
        col = col.push(block_view(b, rm, theme::c().text, 0));
    }
    col.into()
}

#[cfg(test)]
mod tests {
    use mailcore::render::{ImageRef, ImageSource, LinkFlag};

    use super::*;

    fn t(s: &str) -> Inline {
        Inline::Text {
            text: s.into(),
            bold: false,
            italic: false,
            code: false,
        }
    }
    fn para(s: &str) -> Block {
        Block::Paragraph(vec![t(s)])
    }
    fn link(text: &str, url: &str, host: &str, flags: Vec<LinkFlag>) -> Link {
        Link {
            text: text.into(),
            url: url.into(),
            host: host.into(),
            host_ascii: host.into(),
            flags,
        }
    }

    #[test]
    fn raw_addresses_are_shortened_to_the_site() {
        let l = link(
            "https://www.example.com/track?id=9f8e7d6c5b4a&u=123456",
            "https://www.example.com/track?id=9f8e7d6c5b4a&u=123456",
            "www.example.com",
            vec![],
        );
        assert_eq!(link_label(&l), "www.example.com/…");
        let bare = link(
            "https://example.com/",
            "https://example.com/",
            "example.com",
            vec![],
        );
        assert_eq!(link_label(&bare), "example.com");
        let empty = link("", "https://example.com/a", "example.com", vec![]);
        assert_eq!(link_label(&empty), "example.com");
    }

    #[test]
    fn readable_link_text_is_kept() {
        let l = link(
            "Read the full report",
            "https://example.com/r/1",
            "example.com",
            vec![],
        );
        assert_eq!(link_label(&l), "Read the full report");
        let mail = link("write to Ann", "mailto:ann@example.com", "", vec![]);
        assert_eq!(link_label(&mail), "write to Ann");
        // Text with spaces is a sentence, even if it starts like an address.
        let s = link(
            "https://example.com is our home",
            "https://example.com/",
            "example.com",
            vec![],
        );
        assert_eq!(link_label(&s), "https://example.com is our home");
    }

    #[test]
    fn a_link_with_a_warning_is_never_rewritten() {
        let shown = "https://mybank.com/login";
        let l = link(
            shown,
            "https://evil.example/login",
            "evil.example",
            vec![LinkFlag::TextShowsOtherSite {
                shown: "mybank.com".into(),
            }],
        );
        assert_eq!(link_label(&l), shown);
    }

    #[test]
    fn quoted_replies_are_found_with_their_introduction() {
        let body = vec![
            para("Sounds good, Thursday works."),
            para("On Mon, 3 Nov, Ann wrote:"),
            Block::Quote(vec![para("Can we meet Thursday?")]),
        ];
        assert_eq!(quoted_indexes(&body), vec![1, 2]);
        // A colon-ended line that does not lead into a quote is ordinary text.
        let body = vec![para("Things to bring:"), para("Your laptop.")];
        assert!(quoted_indexes(&body).is_empty());
    }

    #[test]
    fn a_message_that_is_only_a_quote_stays_fully_visible() {
        let fwd = vec![
            para("Forwarded message:"),
            Block::Quote(vec![para("The whole content.")]),
        ];
        assert!(quoted_indexes(&fwd).is_empty());
        let just_quote = vec![Block::Quote(vec![para("Only this.")])];
        assert!(quoted_indexes(&just_quote).is_empty());
    }

    #[test]
    fn pictures_alone_are_not_text() {
        let img = Inline::Image(ImageRef {
            alt: "logo".into(),
            source: ImageSource::Dropped,
            width: None,
            height: None,
        });
        assert!(!has_visible(&[img.clone(), Inline::LineBreak, t("  ")]));
        assert!(has_visible(&[img, t("Hello")]));
    }
}
