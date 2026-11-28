//! HTML mail → safe document model.
//!
//! Two independent layers:
//! 1. A strict allow-list sanitiser (ammonia) removes everything except basic
//!    text structure, links and images. No styles, scripts, forms, frames, SVG
//!    or MathML survive. (Recent ammonia fixes concerned CSS and SVG; since we
//!    allow neither, those bug classes cannot arise here.)
//! 2. Our own reader walks the cleaned HTML's tokens and builds [`Block`]s. It
//!    only understands a closed list of tags and re-checks every link and image
//!    address itself, so a sanitiser bug alone would not put active content on
//!    screen: the output is data the interface draws, never HTML.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};

use html5ever::tendril::StrTendril;
use html5ever::tokenizer::{
    BufferQueue, Tag, TagKind, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};

use super::links::analyze_link;
use super::{Block, ImageRef, ImageSource, Inline, RemoteImage};
use crate::textsafe::{clean_line, is_invisible};

/// Longest HTML body we will process (larger is cut, and the caller says so).
pub(super) const MAX_HTML_BYTES: usize = 5 * 1024 * 1024;
const MAX_BLOCKS: usize = 50_000;
const MAX_FRAMES: usize = 64;
const MAX_MARKS: usize = 100_000;
/// HTML nested deeper than this is reduced to its text. The sanitiser's running
/// time grows with the square of the depth, so a hostile message could otherwise
/// freeze the program for minutes; real mail stays far below this.
const MAX_NEST_DEPTH: usize = 256;
const MAX_REMOTE_IMAGES: usize = 200;
const MAX_TEXT_BYTES: usize = 8 * 1024 * 1024;

const TAGS: &[&str] = &[
    "a",
    "b",
    "strong",
    "i",
    "em",
    "u",
    "s",
    "strike",
    "del",
    "ins",
    "code",
    "tt",
    "kbd",
    "samp",
    "small",
    "sub",
    "sup",
    "span",
    "p",
    "div",
    "br",
    "hr",
    "blockquote",
    "pre",
    "ul",
    "ol",
    "li",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "table",
    "thead",
    "tbody",
    "tfoot",
    "tr",
    "td",
    "th",
    "caption",
    "center",
    "img",
];

/// Tags whose *content* is removed too (not just the tag).
const DROP_WITH_CONTENT: &[&str] = &[
    "script",
    "style",
    "head",
    "title",
    "noscript",
    "template",
    "svg",
    "math",
    "iframe",
    "object",
    "embed",
    "applet",
    "audio",
    "video",
    "canvas",
    "noframes",
    "noembed",
    "xmp",
    "plaintext",
    "frameset",
    "frame",
    "select",
    "datalist",
];

fn sanitizer() -> ammonia::Builder<'static> {
    let mut b = ammonia::Builder::empty();
    b.tags(TAGS.iter().copied().collect());
    b.clean_content_tags(DROP_WITH_CONTENT.iter().copied().collect());
    let mut attrs: HashMap<&str, HashSet<&str>> = HashMap::new();
    attrs.insert("a", ["href", "title"].into_iter().collect());
    attrs.insert(
        "img",
        ["src", "alt", "width", "height", "title"]
            .into_iter()
            .collect(),
    );
    b.tag_attributes(attrs);
    b.generic_attributes(HashSet::new());
    // `cid:` is for pictures sent inside the message; checked again by our reader.
    b.url_schemes(
        ["http", "https", "mailto", "tel", "cid"]
            .into_iter()
            .collect(),
    );
    b.url_relative(ammonia::UrlRelative::Deny);
    b.link_rel(Some("noopener noreferrer nofollow"));
    b.strip_comments(true);
    b
}

/// Cleans untrusted HTML with the strict allow-list.
pub(super) fn sanitize(html: &str) -> String {
    sanitizer().clean(html).to_string()
}

// ---------------------------------------------------------------------------
// reader
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub(super) struct HtmlResult {
    pub blocks: Vec<Block>,
    pub remote_images: Vec<RemoteImage>,
    pub hit_limits: bool,
    /// The HTML was nested so deeply that only its text was kept.
    pub too_complex: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Root,
    Quote,
    List { ordered: bool },
    Item,
}

struct Frame {
    kind: Kind,
    blocks: Vec<Block>,
    items: Vec<Vec<Block>>,
}

struct LinkCtx {
    href: String,
    start: usize,
}

struct State {
    frames: Vec<Frame>,
    inline: Vec<Inline>,
    bold: u32,
    italic: u32,
    code: u32,
    heading: Option<u8>,
    link: Option<LinkCtx>,
    pre: Option<String>,
    at_line_start: bool,
    marks: Vec<(&'static str, bool)>,
    block_count: usize,
    text_bytes: usize,
    hit_limits: bool,
    cids: HashSet<String>,
    remote: Vec<RemoteImage>,
}

impl State {
    fn new(cids: HashSet<String>) -> Self {
        Self {
            frames: vec![Frame {
                kind: Kind::Root,
                blocks: Vec::new(),
                items: Vec::new(),
            }],
            inline: Vec::new(),
            bold: 0,
            italic: 0,
            code: 0,
            heading: None,
            link: None,
            pre: None,
            at_line_start: true,
            marks: Vec::new(),
            block_count: 0,
            text_bytes: 0,
            hit_limits: false,
            cids,
            remote: Vec::new(),
        }
    }

    fn top(&mut self) -> &mut Frame {
        let last = self.frames.len() - 1;
        &mut self.frames[last]
    }

    fn push_block(&mut self, b: Block) {
        if self.block_count >= MAX_BLOCKS {
            self.hit_limits = true;
            return;
        }
        self.block_count += 1;
        self.top().blocks.push(b);
    }

    /// Ends the current run of inline content as a paragraph or heading.
    fn flush(&mut self) {
        // A link still open here is closed first so its content is not lost.
        if self.link.is_some() {
            self.close_link();
        }
        let mut inl = std::mem::take(&mut self.inline);
        self.at_line_start = true;
        trim_inlines(&mut inl);
        if inl.is_empty() {
            return;
        }
        let block = match self.heading {
            Some(level) => Block::Heading {
                level,
                content: inl,
            },
            None => Block::Paragraph(inl),
        };
        self.push_block(block);
    }

    fn add_text(&mut self, s: &str) {
        if let Some(buf) = self.pre.as_mut() {
            if buf.len() < MAX_TEXT_BYTES {
                buf.extend(s.chars().filter(|c| {
                    !is_invisible(*c) && (!c.is_control() || matches!(c, '\n' | '\t'))
                }));
            }
            return;
        }
        if self.text_bytes > MAX_TEXT_BYTES {
            self.hit_limits = true;
            return;
        }
        let mut out = String::new();
        for c in s.chars() {
            if is_invisible(c) {
                continue;
            }
            if c.is_whitespace() || c.is_control() {
                if c.is_whitespace()
                    && !self.at_line_start
                    && !out.ends_with(' ')
                    // Only text that starts here can duplicate a space that ended the
                    // previous piece; later spaces in the same run are always kept.
                    && !(out.is_empty() && self.ends_with_space())
                {
                    out.push(' ');
                }
                continue;
            }
            out.push(c);
            self.at_line_start = false;
        }
        if out.is_empty() {
            return;
        }
        self.text_bytes += out.len();
        let (bold, italic, code) = (self.bold > 0, self.italic > 0, self.code > 0);
        // Text must not merge into what came before an open link, or the link
        // would lose its label.
        let floor = self.link.as_ref().map_or(0, |l| l.start);
        let can_merge = self.inline.len() > floor;
        if can_merge
            && let Some(Inline::Text {
                text,
                bold: b,
                italic: i,
                code: c,
            }) = self.inline.last_mut()
            && (*b, *i, *c) == (bold, italic, code)
        {
            text.push_str(&out);
            return;
        }
        self.inline.push(Inline::Text {
            text: out,
            bold,
            italic,
            code,
        });
    }

    fn ends_with_space(&self) -> bool {
        matches!(self.inline.last(), Some(Inline::Text { text, .. }) if text.ends_with(' '))
    }

    fn open_link(&mut self, href: &str) {
        if self.pre.is_some() || self.link.is_some() {
            return;
        }
        self.link = Some(LinkCtx {
            href: href.to_owned(),
            start: self.inline.len(),
        });
    }

    fn close_link(&mut self) {
        let Some(ctx) = self.link.take() else { return };
        let inner: Vec<Inline> = self.inline.drain(ctx.start..).collect();
        let mut label = String::new();
        let mut first_alt = String::new();
        for i in &inner {
            match i {
                Inline::Text { text, .. } => label.push_str(text),
                Inline::Image(img) if first_alt.is_empty() => first_alt.clone_from(&img.alt),
                _ => {}
            }
        }
        let label = label.trim().to_owned();
        let shown = if label.is_empty() { first_alt } else { label };
        match analyze_link(&ctx.href, &shown) {
            Some(mut link) => {
                if link.text.is_empty() {
                    link.text = link.url.clone();
                }
                self.inline.push(Inline::Link(link));
                // Pictures that were inside the link stay visible as placeholders.
                self.inline
                    .extend(inner.into_iter().filter(|i| matches!(i, Inline::Image(_))));
            }
            None => self.inline.extend(inner),
        }
    }

    fn add_image(&mut self, attrs: &Attrs) {
        if self.pre.is_some() {
            return;
        }
        let src = attrs.get("src").unwrap_or("").trim();
        let alt = clean_line(attrs.get("alt").unwrap_or(""), 200);
        let width = attrs.get("width").and_then(parse_dimension);
        let height = attrs.get("height").and_then(parse_dimension);
        let lower = src.to_ascii_lowercase();
        let source = if let Some(rest) = lower.strip_prefix("cid:") {
            let cid = normalize_cid(rest);
            if self.cids.contains(&cid) {
                ImageSource::Inline { cid }
            } else {
                ImageSource::Dropped
            }
        } else if lower.starts_with("http://") || lower.starts_with("https://") {
            self.remote_index(src, width, height)
                .map_or(ImageSource::Dropped, |index| ImageSource::Remote { index })
        } else {
            ImageSource::Dropped
        };
        self.inline.push(Inline::Image(ImageRef {
            source,
            alt,
            width,
            height,
        }));
        self.at_line_start = false;
    }

    fn remote_index(
        &mut self,
        src: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Option<usize> {
        let url = url::Url::parse(src).ok()?;
        let host = url.host_str()?.to_owned();
        let url_text = url.to_string();
        if let Some(i) = self.remote.iter().position(|r| r.url == url_text) {
            return Some(i);
        }
        if self.remote.len() >= MAX_REMOTE_IMAGES {
            self.hit_limits = true;
            return None;
        }
        let lower = url_text.to_ascii_lowercase();
        let tiny = width.is_some_and(|w| w <= 2) || height.is_some_and(|h| h <= 2);
        let telltale = ["pixel", "track", "beacon", "1x1", "/open"]
            .iter()
            .any(|k| lower.contains(k));
        self.remote.push(RemoteImage {
            insecure: url.scheme() == "http",
            url: url_text,
            host,
            width,
            height,
            likely_tracker: tiny || telltale,
        });
        Some(self.remote.len() - 1)
    }

    /// Opens a quote, list or list item. Past the nesting limit (or for a stray
    /// `<li>`) no frame is made, but the tag is still remembered so its closing
    /// tag matches up.
    fn open_tag(&mut self, tag: &'static str, kind: Kind) {
        self.flush();
        let room = self.frames.len() < MAX_FRAMES;
        let allowed = match kind {
            Kind::Item => matches!(self.top().kind, Kind::List { .. }),
            _ => true,
        };
        if !room {
            self.hit_limits = true;
        }
        let real = room && allowed;
        if real {
            self.frames.push(Frame {
                kind,
                blocks: Vec::new(),
                items: Vec::new(),
            });
        }
        if self.marks.len() < MAX_MARKS {
            self.marks.push((tag, real));
        } else {
            self.hit_limits = true;
        }
    }

    /// Closes the nearest open tag named in `tags`, and everything opened inside it.
    fn close_tag(&mut self, tags: &[&str]) {
        self.flush();
        let Some(pos) = self.marks.iter().rposition(|(t, _)| tags.contains(t)) else {
            return;
        };
        while self.marks.len() > pos {
            if let Some((_, real)) = self.marks.pop()
                && real
            {
                self.pop_frame();
            }
        }
    }

    fn pop_frame(&mut self) {
        let Some(frame) = self.frames.pop() else {
            return;
        };
        let block = match frame.kind {
            Kind::Root => return,
            Kind::Quote => Some(Block::Quote(frame.blocks)),
            Kind::List { ordered } => {
                // Text placed directly inside a list (outside any item) is kept as its own item.
                let mut items = frame.items;
                if !frame.blocks.is_empty() {
                    items.push(frame.blocks);
                }
                Some(Block::List { ordered, items })
            }
            Kind::Item => {
                // Items belong to the list below; without one, keep the content.
                let parent = self.top();
                if matches!(parent.kind, Kind::List { .. }) {
                    parent.items.push(frame.blocks);
                } else {
                    parent.blocks.extend(frame.blocks);
                }
                None
            }
        };
        if let Some(b) = block {
            self.push_block(b);
        }
    }

    fn finish(mut self) -> HtmlResult {
        self.flush();
        while self.frames.len() > 1 {
            self.pop_frame();
        }
        let blocks = self.frames.pop().map(|f| f.blocks).unwrap_or_default();
        HtmlResult {
            blocks,
            remote_images: self.remote,
            hit_limits: self.hit_limits,
            too_complex: false,
        }
    }

    fn start(&mut self, name: &str, attrs: &Attrs) {
        match name {
            "p" | "div" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th"
            | "caption" | "center" => self.flush(),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                self.heading = name[1..].parse().ok();
            }
            "blockquote" => self.open_tag("blockquote", Kind::Quote),
            "ul" => self.open_tag("ul", Kind::List { ordered: false }),
            "ol" => self.open_tag("ol", Kind::List { ordered: true }),
            "li" => {
                // An unclosed previous item ends here.
                if self.marks.last().is_some_and(|(t, _)| *t == "li") {
                    self.close_tag(&["li"]);
                }
                self.open_tag("li", Kind::Item);
            }
            "pre" => {
                self.flush();
                self.pre = Some(String::new());
            }
            "br" => match self.pre.as_mut() {
                Some(buf) => buf.push('\n'),
                None => {
                    if !matches!(self.inline.last(), Some(Inline::LineBreak))
                        || self.inline.len() < 3
                    {
                        self.inline.push(Inline::LineBreak);
                    }
                    self.at_line_start = true;
                }
            },
            "hr" => {
                self.flush();
                self.push_block(Block::Rule);
            }
            "b" | "strong" => self.bold += 1,
            "i" | "em" => self.italic += 1,
            "code" | "tt" | "kbd" | "samp" => self.code += 1,
            "a" => {
                if let Some(h) = attrs.get("href") {
                    self.open_link(h);
                }
            }
            "img" => self.add_image(attrs),
            _ => {}
        }
    }

    fn end(&mut self, name: &str) {
        match name {
            "p" | "div" | "table" | "thead" | "tbody" | "tfoot" | "tr" | "td" | "th"
            | "caption" | "center" => self.flush(),
            "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                self.flush();
                self.heading = None;
            }
            "blockquote" => self.close_tag(&["blockquote"]),
            "ul" | "ol" => self.close_tag(&["ul", "ol"]),
            "li" => self.close_tag(&["li"]),
            "pre" => {
                self.flush();
                if let Some(buf) = self.pre.take() {
                    let t = buf.trim_matches('\n').to_owned();
                    if !t.trim().is_empty() {
                        self.push_block(Block::Preformatted(t));
                    }
                }
            }
            "b" | "strong" => self.bold = self.bold.saturating_sub(1),
            "i" | "em" => self.italic = self.italic.saturating_sub(1),
            "code" | "tt" | "kbd" | "samp" => self.code = self.code.saturating_sub(1),
            "a" => self.close_link(),
            _ => {}
        }
    }
}

fn trim_inlines(inl: &mut Vec<Inline>) {
    while matches!(inl.first(), Some(Inline::LineBreak)) {
        inl.remove(0);
    }
    while matches!(inl.last(), Some(Inline::LineBreak)) {
        inl.pop();
    }
    if let Some(Inline::Text { text, .. }) = inl.first_mut() {
        *text = text.trim_start().to_owned();
    }
    if let Some(Inline::Text { text, .. }) = inl.last_mut() {
        *text = text.trim_end().to_owned();
    }
    inl.retain(|i| !matches!(i, Inline::Text { text, .. } if text.is_empty()));
}

fn parse_dimension(v: &str) -> Option<u32> {
    let n: u32 = v.trim().trim_end_matches("px").trim().parse().ok()?;
    Some(n.min(10_000))
}

/// Normal form of a Content-ID used for matching: no `cid:`, brackets or case.
pub(super) fn normalize_cid(s: &str) -> String {
    let t = s
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    let t = t
        .strip_prefix("cid:")
        .or_else(|| t.strip_prefix("CID:"))
        .unwrap_or(t);
    percent_decode(t).to_ascii_lowercase()
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = (
                (b[i + 1] as char).to_digit(16),
                (b[i + 2] as char).to_digit(16),
            )
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

struct Attrs(Vec<(String, String)>);

impl Attrs {
    fn get(&self, name: &str) -> Option<&str> {
        self.0
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

struct Sink(RefCell<State>);

impl TokenSink for Sink {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        let mut st = self.0.borrow_mut();
        match token {
            Token::TagToken(Tag {
                kind, name, attrs, ..
            }) => {
                let name = (*name).to_ascii_lowercase();
                match kind {
                    TagKind::StartTag => {
                        let attrs = Attrs(
                            attrs
                                .iter()
                                .map(|a| {
                                    ((*a.name.local).to_ascii_lowercase(), a.value.to_string())
                                })
                                .collect(),
                        );
                        st.start(&name, &attrs);
                    }
                    TagKind::EndTag => st.end(&name),
                }
            }
            Token::CharacterTokens(t) => st.add_text(&t),
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

const VOID_OR_IMPLIED: &[&str] = &[
    "br", "hr", "img", "input", "meta", "link", "area", "base", "col", "embed", "source", "track",
    "wbr", "p", "li", "td", "th", "tr", "tbody", "thead", "tfoot", "dt", "dd", "option",
];

/// Cheap single pass over the raw HTML: how deep does it nest, and what is its text?
struct Scan {
    depth: std::cell::Cell<usize>,
    max_depth: std::cell::Cell<usize>,
    skip: RefCell<Option<String>>,
    text: RefCell<String>,
    want_text: bool,
}

impl TokenSink for Scan {
    type Handle = ();

    fn process_token(&self, token: Token, _line: u64) -> TokenSinkResult<()> {
        match token {
            Token::TagToken(Tag {
                kind,
                name,
                self_closing,
                ..
            }) => {
                let name = (*name).to_ascii_lowercase();
                let counted = !VOID_OR_IMPLIED.contains(&name.as_str()) && !self_closing;
                match kind {
                    TagKind::StartTag => {
                        if counted {
                            let d = self.depth.get() + 1;
                            self.depth.set(d);
                            self.max_depth.set(self.max_depth.get().max(d));
                        }
                        if self.want_text {
                            if DROP_WITH_CONTENT.contains(&name.as_str())
                                && self.skip.borrow().is_none()
                            {
                                *self.skip.borrow_mut() = Some(name);
                            } else if matches!(
                                name.as_str(),
                                "br" | "p"
                                    | "div"
                                    | "li"
                                    | "tr"
                                    | "h1"
                                    | "h2"
                                    | "h3"
                                    | "h4"
                                    | "h5"
                                    | "h6"
                            ) {
                                let mut t = self.text.borrow_mut();
                                if !t.ends_with('\n') {
                                    t.push('\n');
                                }
                            }
                        }
                    }
                    TagKind::EndTag => {
                        if counted {
                            self.depth.set(self.depth.get().saturating_sub(1));
                        }
                        if self.skip.borrow().as_deref() == Some(name.as_str()) {
                            *self.skip.borrow_mut() = None;
                        }
                    }
                }
            }
            Token::CharacterTokens(t) if self.want_text && self.skip.borrow().is_none() => {
                let mut out = self.text.borrow_mut();
                if out.len() < MAX_TEXT_BYTES {
                    out.push_str(&t);
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}

fn scan(html: &str, want_text: bool) -> Scan {
    let tokenizer = Tokenizer::new(
        Scan {
            depth: Default::default(),
            max_depth: Default::default(),
            skip: RefCell::new(None),
            text: RefCell::new(String::new()),
            want_text,
        },
        TokenizerOpts::default(),
    );
    let queue = BufferQueue::default();
    queue.push_back(StrTendril::from_slice(html));
    let _ = tokenizer.feed(&queue);
    tokenizer.end();
    tokenizer.sink
}

/// Sanitises `html`, then reads it into blocks. `cids` lists the Content-IDs of
/// pictures that came with the message; `remote` carries pictures found in
/// earlier parts so indexes keep counting.
pub(super) fn html_to_blocks(
    html: &str,
    cids: &HashSet<String>,
    remote: Vec<RemoteImage>,
) -> HtmlResult {
    if scan(html, false).max_depth.get() > MAX_NEST_DEPTH {
        // Text only, in linear time; the sanitiser is never run on such input.
        let text = scan(html, true).text.into_inner();
        let blocks = super::text::text_to_blocks(&text);
        return HtmlResult {
            blocks,
            remote_images: remote,
            hit_limits: true,
            too_complex: true,
        };
    }
    let clean = sanitize(html);
    let mut state = State::new(cids.clone());
    state.remote = remote;
    let tokenizer = Tokenizer::new(Sink(RefCell::new(state)), TokenizerOpts::default());
    let queue = BufferQueue::default();
    queue.push_back(StrTendril::from_slice(&clean));
    let _ = tokenizer.feed(&queue);
    tokenizer.end();
    tokenizer.sink.0.into_inner().finish()
}
