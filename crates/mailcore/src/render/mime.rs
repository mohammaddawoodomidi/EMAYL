//! Walking a message's MIME tree: choosing the body, listing attachments and
//! collecting pictures that travel inside the message.

use std::collections::HashSet;

use mailparse::{DispositionType, MailHeaderMap, ParsedMail, parse_mail};

use super::headers;
use super::html::{MAX_HTML_BYTES, html_to_blocks, normalize_cid};
use super::text::{blocks_to_text, text_to_blocks};
use super::{
    AttachmentInfo, AttachmentRisk, Block, BodySource, InlineImage, MAX_MESSAGE_BYTES, Notice,
    NoticeKind, RenderError, RenderOptions, RenderedMessage, Severity,
};
use crate::compose::sanitize_filename;
use crate::textsafe::has_bidi_control;

const MAX_PARTS: usize = 2_000;
const MAX_DEPTH: usize = 24;
const MAX_TEXT_PART_BYTES: usize = 8 * 1024 * 1024;
const MAX_INLINE_IMAGE_BYTES: usize = 10 * 1024 * 1024;
const MAX_INLINE_IMAGES_TOTAL: usize = 30 * 1024 * 1024;
const MAX_INLINE_IMAGES: usize = 100;

const EXECUTABLE: &[&str] = &[
    "exe",
    "scr",
    "com",
    "pif",
    "bat",
    "cmd",
    "msi",
    "msp",
    "js",
    "jse",
    "vbs",
    "vbe",
    "wsf",
    "wsh",
    "hta",
    "cpl",
    "jar",
    "lnk",
    "reg",
    "ps1",
    "psm1",
    "dll",
    "sys",
    "appx",
    "msix",
    "iso",
    "img",
    "vhd",
    "vhdx",
    "chm",
    "scf",
    "inf",
    "url",
    "gadget",
    "application",
    "apk",
    "dmg",
    "pkg",
    "sh",
    "run",
    "bin",
    "appimage",
    "desktop",
    "deb",
    "rpm",
    "docm",
    "xlsm",
    "pptm",
    "dotm",
    "xlam",
    "ppam",
];
const DOCUMENT_LOOKING: &[&str] = &[
    "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "rtf", "csv", "jpg", "jpeg", "png",
    "gif", "mp3", "mp4", "zip",
];

#[derive(Default)]
struct BodyUnit {
    html: Option<String>,
    text: Option<String>,
}

struct AttRef<'a> {
    part: &'a ParsedMail<'a>,
    filename: String,
    content_type: String,
}

#[derive(Default)]
struct Collector<'a> {
    units: Vec<BodyUnit>,
    atts: Vec<AttRef<'a>>,
    pending_inline: Vec<(AttRef<'a>, InlineImage)>,
    inline_bytes: usize,
    parts_seen: usize,
    too_complex: bool,
    truncated: bool,
    undecodable: bool,
    encrypted: bool,
    signed: bool,
    bidi_names: bool,
}

pub(super) fn classify(name: &str) -> AttachmentRisk {
    let lower = name.to_lowercase();
    let parts: Vec<&str> = lower.split('.').map(str::trim).collect();
    let Some(ext) = parts.last().filter(|_| parts.len() > 1) else {
        return AttachmentRisk::Normal;
    };
    if !EXECUTABLE.contains(ext) {
        return AttachmentRisk::Normal;
    }
    if parts.len() >= 3 && DOCUMENT_LOOKING.contains(&parts[parts.len() - 2]) {
        AttachmentRisk::DisguisedExecutable
    } else {
        AttachmentRisk::Executable
    }
}

fn sniff_image(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        Some("image/png")
    } else if data.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("image/jpeg")
    } else if data.starts_with(b"GIF87a") || data.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn cut_at_boundary(s: &mut String, max: usize) -> bool {
    if s.len() <= max {
        return false;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
    true
}

impl<'a> Collector<'a> {
    fn decode(&mut self, part: &ParsedMail<'_>) -> String {
        let mut s = match part.get_body() {
            Ok(s) => s,
            Err(_) => {
                self.undecodable = true;
                String::from_utf8_lossy(&part.get_body_raw().unwrap_or_default()).into_owned()
            }
        };
        if cut_at_boundary(&mut s, MAX_TEXT_PART_BYTES) {
            self.truncated = true;
        }
        s
    }

    fn walk(&mut self, part: &'a ParsedMail<'a>, depth: usize) {
        self.parts_seen += 1;
        if self.parts_seen > MAX_PARTS || depth > MAX_DEPTH {
            self.too_complex = true;
            return;
        }
        let mt = part.ctype.mimetype.to_ascii_lowercase();
        if let Some(kind) = mt.strip_prefix("multipart/") {
            self.multipart(part, kind, depth);
        } else {
            self.leaf(part, &mt);
        }
    }

    fn multipart(&mut self, part: &'a ParsedMail<'a>, kind: &str, depth: usize) {
        match kind {
            "alternative" => {
                let (mut html, mut text): (Option<String>, Option<String>) = (None, None);
                for sub in &part.subparts {
                    let before = self.units.len();
                    self.walk(sub, depth + 1);
                    let produced: Vec<BodyUnit> = self.units.drain(before..).collect();
                    let join = |f: fn(&BodyUnit) -> Option<&String>| {
                        let v: Vec<&str> = produced
                            .iter()
                            .filter_map(|u| f(u).map(String::as_str))
                            .collect();
                        (!v.is_empty()).then(|| v.join("\n"))
                    };
                    // The last version of each kind is the richest (RFC 2046).
                    if let Some(h) = join(|u| u.html.as_ref()) {
                        html = Some(h);
                    }
                    if let Some(t) = join(|u| u.text.as_ref()) {
                        text = Some(t);
                    }
                }
                if html.is_some() || text.is_some() {
                    self.units.push(BodyUnit { html, text });
                }
            }
            "signed" => {
                self.signed = true;
                if let Some(first) = part.subparts.first() {
                    self.walk(first, depth + 1);
                }
            }
            other => {
                if other == "encrypted" {
                    self.encrypted = true;
                }
                for sub in &part.subparts {
                    self.walk(sub, depth + 1);
                }
            }
        }
    }

    fn leaf(&mut self, part: &'a ParsedMail<'a>, mt: &str) {
        let disp = part.get_content_disposition();
        let raw_name = disp
            .params
            .get("filename")
            .or_else(|| part.ctype.params.get("name"))
            .cloned();
        if raw_name.as_deref().is_some_and(has_bidi_control) {
            self.bidi_names = true;
        }
        let is_attachment = disp.disposition == DispositionType::Attachment
            || (raw_name.is_some() && disp.disposition != DispositionType::Inline);

        if (mt == "text/plain" || mt == "text/html") && !is_attachment {
            let body = self.decode(part);
            let unit = if mt == "text/html" {
                BodyUnit {
                    html: Some(body),
                    text: None,
                }
            } else {
                BodyUnit {
                    html: None,
                    text: Some(body),
                }
            };
            self.units.push(unit);
            return;
        }

        let name = match raw_name.as_deref().map(sanitize_filename) {
            Some(n) if n != "attachment" => n,
            _ => match mt {
                "message/rfc822" => "message.eml".to_owned(),
                "text/calendar" => "invite.ics".to_owned(),
                _ => format!(
                    "attachment-{}",
                    self.atts.len() + self.pending_inline.len() + 1
                ),
            },
        };
        let att = AttRef {
            part,
            filename: name,
            content_type: clean_type(mt),
        };

        if let Some(cid) = part.headers.get_first_value("Content-ID")
            && disp.disposition != DispositionType::Attachment
            && matches!(
                mt,
                "image/png" | "image/jpeg" | "image/jpg" | "image/gif" | "image/webp"
            )
        {
            let data = part.get_body_raw().unwrap_or_default();
            let sniffed = sniff_image(&data);
            let n_inline = self.pending_inline.len();
            if let Some(ct) = sniffed
                && data.len() <= MAX_INLINE_IMAGE_BYTES
                && self.inline_bytes + data.len() <= MAX_INLINE_IMAGES_TOTAL
                && n_inline < MAX_INLINE_IMAGES
            {
                self.inline_bytes += data.len();
                let image = InlineImage {
                    cid: normalize_cid(&cid),
                    content_type: ct.to_owned(),
                    data,
                };
                self.pending_inline.push((
                    AttRef {
                        content_type: ct.to_owned(),
                        ..att
                    },
                    image,
                ));
                return;
            }
            // Not a genuine small picture: offer it as an ordinary attachment instead.
        }
        self.atts.push(att);
    }
}

fn clean_type(mt: &str) -> String {
    let ok = mt.len() <= 100
        && mt
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/.+-_".contains(c));
    if ok && mt.contains('/') {
        mt.to_owned()
    } else {
        "application/octet-stream".to_owned()
    }
}

fn notice(kind: NoticeKind, severity: Severity, text: impl Into<String>) -> Notice {
    Notice {
        kind,
        severity,
        text: text.into(),
    }
}

/// Collects everything, then lets `f` use the result (parts borrow the parsed tree).
fn with_collected<R>(
    raw: &[u8],
    f: impl FnOnce(Collector<'_>, &ParsedMail<'_>) -> R,
) -> Result<R, RenderError> {
    if raw.len() > MAX_MESSAGE_BYTES {
        return Err(RenderError::TooLarge(raw.len() / (1024 * 1024)));
    }
    let mail = parse_mail(raw).map_err(|e| RenderError::Unreadable(e.to_string()))?;
    let mut c = Collector::default();
    c.walk(&mail, 0);
    // Inline pictures only make sense next to HTML; otherwise they are attachments.
    let has_html = c.units.iter().any(|u| u.html.is_some());
    if !has_html {
        let pending = std::mem::take(&mut c.pending_inline);
        c.atts.extend(pending.into_iter().map(|(a, _)| a));
    }
    Ok(f(c, &mail))
}

pub(super) fn render(raw: &[u8], opts: &RenderOptions) -> Result<RenderedMessage, RenderError> {
    with_collected(raw, |c, mail| build(c, mail, opts))
}

fn build(c: Collector<'_>, mail: &ParsedMail<'_>, opts: &RenderOptions) -> RenderedMessage {
    let mut notices = Vec::new();
    let headers = headers::parse(mail, &mut notices);

    let inline_images: Vec<InlineImage> = c.pending_inline.iter().map(|(_, i)| i.clone()).collect();
    let cids: HashSet<String> = inline_images.iter().map(|i| i.cid.clone()).collect();

    let use_html = !opts.prefer_plain && c.units.iter().any(|u| u.html.is_some());
    let (mut shown, mut plain): (Vec<Block>, Vec<Block>) = (Vec::new(), Vec::new());
    let mut remote = Vec::new();
    let mut hit_limits = false;
    let mut too_complex = false;
    for u in &c.units {
        let text_blocks = u.text.as_deref().map(text_to_blocks);
        let need_html = u.html.is_some() && (use_html || text_blocks.is_none());
        let html_blocks = match (&u.html, need_html) {
            (Some(h), true) => {
                let mut h = h.clone();
                if cut_at_boundary(&mut h, MAX_HTML_BYTES) {
                    hit_limits = true;
                }
                let res = html_to_blocks(&h, &cids, std::mem::take(&mut remote));
                remote = res.remote_images;
                hit_limits |= res.hit_limits;
                too_complex |= res.too_complex;
                Some(res.blocks)
            }
            _ => None,
        };
        let (s, p) = match (use_html, text_blocks, html_blocks) {
            (true, t, Some(h)) => (h.clone(), t.unwrap_or(h)),
            (_, Some(t), _) => (t.clone(), t),
            (_, None, Some(h)) => (h.clone(), h),
            _ => (Vec::new(), Vec::new()),
        };
        shown.extend(s);
        plain.extend(p);
    }

    let used_html = use_html && c.units.iter().any(|u| u.html.is_some());
    let referenced_remote = used_html && !remote.is_empty();
    let body_source = if shown.is_empty() {
        BodySource::Empty
    } else if used_html {
        BodySource::Html
    } else {
        BodySource::PlainText
    };

    let attachments: Vec<AttachmentInfo> = c
        .atts
        .iter()
        .enumerate()
        .map(|(index, a)| AttachmentInfo {
            index,
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            size: a.part.get_body_raw().map_or(0, |b| b.len()),
            risk: classify(&a.filename),
        })
        .collect();

    for a in attachments
        .iter()
        .filter(|a| a.risk != AttachmentRisk::Normal)
    {
        notices.push(notice(
            NoticeKind::RiskyAttachment,
            Severity::Warning,
            if a.risk == AttachmentRisk::DisguisedExecutable {
                format!("The attachment \"{}\" is a program disguised as a document. Do not open it.", a.filename)
            } else {
                format!("The attachment \"{}\" is a program or script type. Opening it can run code on your computer.", a.filename)
            },
        ));
    }
    if c.bidi_names {
        notices.push(notice(
            NoticeKind::HiddenTextDirection,
            Severity::Warning,
            "An attachment name used hidden text-direction characters, which can disguise its real type. They were removed.",
        ));
    }
    if c.encrypted {
        notices.push(notice(
            NoticeKind::Encrypted,
            Severity::Caution,
            "This message is encrypted (PGP or S/MIME). This version cannot decrypt it; the encrypted data is listed as attachments.",
        ));
    }
    if c.signed {
        notices.push(notice(
            NoticeKind::Signed,
            Severity::Info,
            "This message carries a digital signature. This version does not check signatures, so do not treat it as proof of who sent it.",
        ));
    }
    if c.too_complex || too_complex {
        notices.push(notice(
            NoticeKind::TooComplex,
            Severity::Caution,
            "This message has an unusually complicated structure; part of it is not shown.",
        ));
    }
    if c.truncated || hit_limits {
        notices.push(notice(
            NoticeKind::Truncated,
            Severity::Caution,
            "This message is very large; only the first part is shown.",
        ));
    }
    if c.undecodable {
        notices.push(notice(
            NoticeKind::UndecodableText,
            Severity::Info,
            "Some text uses an unknown character encoding and may show wrong characters.",
        ));
    }
    if referenced_remote {
        let trackers = remote.iter().filter(|r| r.likely_tracker).count();
        let mut t = format!(
            "{} picture(s) on the internet were not loaded, to protect your privacy.",
            remote.len()
        );
        if trackers > 0 {
            t.push_str(&format!(" {trackers} of them look like tracking pixels."));
        }
        notices.push(notice(NoticeKind::PicturesWithheld, Severity::Info, t));
    }

    RenderedMessage {
        headers,
        plain_text: blocks_to_text(&plain),
        body: shown,
        body_source,
        attachments,
        inline_images: if used_html { inline_images } else { Vec::new() },
        remote_images: if used_html { remote } else { Vec::new() },
        notices,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentData {
    pub filename: String,
    pub content_type: String,
    pub data: Vec<u8>,
}

/// Returns the bytes of attachment number `index` (as listed by `render`).
/// The caller decides where to save it; the name is already cleaned and safe
/// to use as a file name.
pub fn extract_attachment(raw: &[u8], index: usize) -> Result<AttachmentData, RenderError> {
    with_collected(raw, |c, _| {
        let a = c.atts.get(index).ok_or(RenderError::NoSuchAttachment)?;
        let data = a
            .part
            .get_body_raw()
            .map_err(|e| RenderError::Unreadable(e.to_string()))?;
        Ok(AttachmentData {
            filename: a.filename.clone(),
            content_type: a.content_type.clone(),
            data,
        })
    })?
}
