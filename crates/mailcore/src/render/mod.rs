//! Turning a received message (raw bytes) into something safe to show.
//!
//! The pipeline: parse the MIME structure ([`mime`]), pick the body, and for
//! HTML run it through a strict allow-list sanitiser and then into a small
//! **native document model** ([`Block`]/[`Inline`]) that the interface draws
//! itself. No web engine is involved, so scripts, styles, forms, frames and
//! plug-ins have nothing to run in. Remote pictures are never fetched here; they
//! are listed so the interface can ask first.
//!
//! Everything in a message is hostile input. Limits on size, depth and counts
//! apply throughout, and nothing here panics on malformed data (see tests).

mod headers;
mod html;
mod links;
mod mime;
mod text;
mod trackers;

#[cfg(test)]
mod tests;

pub use links::{Link, LinkFlag, analyze_link};
pub use mime::{AttachmentData, extract_attachment};
pub use text::blocks_to_text;
pub use trackers::{TrackerSummary, strip_tracking};

/// Largest message we will try to render.
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RenderError {
    #[error("this message is too large to display ({0} MB)")]
    TooLarge(usize),
    #[error("this message could not be read: {0}")]
    Unreadable(String),
    #[error("no such attachment")]
    NoSuchAttachment,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderOptions {
    /// Show the plain-text version even if an HTML version exists (the most
    /// private and safest way to read mail).
    pub prefer_plain: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Block {
    Paragraph(Vec<Inline>),
    Heading {
        level: u8,
        content: Vec<Inline>,
    },
    Quote(Vec<Block>),
    List {
        ordered: bool,
        items: Vec<Vec<Block>>,
    },
    Preformatted(String),
    Rule,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Inline {
    Text {
        text: String,
        bold: bool,
        italic: bool,
        code: bool,
    },
    Link(Link),
    Image(ImageRef),
    LineBreak,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageRef {
    pub source: ImageSource,
    pub alt: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageSource {
    /// Picture sent inside the message; look it up in [`RenderedMessage::inline_images`].
    Inline { cid: String },
    /// Picture on the internet, not loaded. Index into [`RenderedMessage::remote_images`].
    Remote { index: usize },
    /// Could not or must not be shown (unsupported address type, missing, damaged).
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteImage {
    pub url: String,
    pub host: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Looks like a tracking pixel (tiny, or a telltale address). A heuristic.
    pub likely_tracker: bool,
    /// Address is plain `http://`.
    pub insecure: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineImage {
    pub cid: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentRisk {
    Normal,
    /// Program or script type that can run code if opened.
    Executable,
    /// A name like `report.pdf.exe` built to hide the real type.
    DisguisedExecutable,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentInfo {
    /// Position for [`extract_attachment`].
    pub index: usize,
    pub filename: String,
    pub content_type: String,
    pub size: usize,
    pub risk: AttachmentRisk,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Caution,
    Warning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    NameLooksLikeOtherAddress,
    ReplyGoesElsewhere,
    SenderHasSpecialCharacters,
    HiddenTextDirection,
    Encrypted,
    Signed,
    Truncated,
    TooComplex,
    UndecodableText,
    RiskyAttachment,
    PicturesWithheld,
}

/// Something the person should know about this message, in plain language.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    pub severity: Severity,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub name: String,
    pub address: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DisplayHeaders {
    pub from: Option<Person>,
    pub reply_to: Vec<Person>,
    pub to: Vec<Person>,
    pub cc: Vec<Person>,
    pub subject: String,
    /// Seconds since 1970-01-01 UTC, if the Date header could be read.
    pub date: Option<i64>,
    /// Without angle brackets.
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub unsubscribe: Option<Unsubscribe>,
}

/// From `List-Unsubscribe` (RFC 2369) and `List-Unsubscribe-Post` (RFC 8058).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unsubscribe {
    pub web: Option<String>,
    pub mailto: Option<String>,
    /// The sender supports one-click unsubscribe by POST.
    pub one_click: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySource {
    Html,
    PlainText,
    Empty,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedMessage {
    pub headers: DisplayHeaders,
    pub body: Vec<Block>,
    pub body_source: BodySource,
    /// Readable text of the whole body (for replies, search and "plain text" view).
    pub plain_text: String,
    pub attachments: Vec<AttachmentInfo>,
    pub inline_images: Vec<InlineImage>,
    pub remote_images: Vec<RemoteImage>,
    pub notices: Vec<Notice>,
}

/// Renders a raw message. Written not to panic on any input (tested with garbage
/// and randomised data); an unreadable message gives an error.
pub fn render(raw: &[u8], opts: &RenderOptions) -> Result<RenderedMessage, RenderError> {
    mime::render(raw, opts)
}

/// Runs HTML through the strict sanitiser (exposed for tests and for exporting).
///
/// Web addresses of remote pictures are kept in the output (the reader lists them as
/// blocked and never loads them). Anything that *displays* this output must not load
/// them either, or the sender learns that the message was opened.
pub fn sanitize_html(html: &str) -> String {
    html::sanitize(html)
}

/// What the message list needs from a header block alone.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HeaderSummary {
    pub headers: DisplayHeaders,
    /// A guess from the top-level `Content-Type` (the real answer needs the body):
    /// true for `multipart/mixed` and for bodies that are not text at all.
    pub maybe_attachments: bool,
}

/// Reads a raw header block (as fetched for the list) without any body.
/// Never panics; unreadable input gives an empty summary.
pub fn summarize_header(raw_header: &[u8]) -> HeaderSummary {
    let mut buf = Vec::with_capacity(raw_header.len() + 4);
    buf.extend_from_slice(raw_header);
    while buf.last().is_some_and(|b| matches!(b, b'\r' | b'\n')) {
        buf.pop();
    }
    buf.extend_from_slice(b"\r\n\r\n");
    let Ok(mail) = mailparse::parse_mail(&buf) else {
        return HeaderSummary::default();
    };
    let mut notices = Vec::new();
    let headers = headers::parse(&mail, &mut notices);
    let ct = mail.ctype.mimetype.to_ascii_lowercase();
    let maybe_attachments = ct == "multipart/mixed"
        || ct.starts_with("application/")
        || ct.starts_with("audio/")
        || ct.starts_with("video/");
    HeaderSummary {
        headers,
        maybe_attachments,
    }
}
