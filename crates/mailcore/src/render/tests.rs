use super::*;

fn raw(headers: &str, body: &str) -> Vec<u8> {
    format!(
        "{}\r\n\r\n{}",
        headers.trim().replace('\n', "\r\n"),
        body.replace('\n', "\r\n")
    )
    .into_bytes()
}

const BASE: &str = "From: Bob <bob@example.com>\nTo: alice@example.com\nSubject: Hello\nDate: Tue, 17 Nov 2026 10:00:00 +0000\nMessage-ID: <abc@example.com>\nMIME-Version: 1.0";

fn html_msg(html: &str) -> Vec<u8> {
    raw(
        &format!("{BASE}\nContent-Type: text/html; charset=utf-8"),
        html,
    )
}

fn render_html(html: &str) -> RenderedMessage {
    render(&html_msg(html), &RenderOptions::default()).unwrap()
}

fn text_of(m: &RenderedMessage) -> String {
    blocks_to_text(&m.body)
}

fn links(blocks: &[Block], out: &mut Vec<Link>) {
    fn inl(i: &[Inline], out: &mut Vec<Link>) {
        for x in i {
            if let Inline::Link(l) = x {
                out.push(l.clone());
            }
        }
    }
    for b in blocks {
        match b {
            Block::Paragraph(i) | Block::Heading { content: i, .. } => inl(i, out),
            Block::Quote(q) => links(q, out),
            Block::List { items, .. } => items.iter().for_each(|it| links(it, out)),
            _ => {}
        }
    }
}

fn all_links(m: &RenderedMessage) -> Vec<Link> {
    let mut v = Vec::new();
    links(&m.body, &mut v);
    v
}

// ---------------------------------------------------------------------------
// basics
// ---------------------------------------------------------------------------

#[test]
fn plain_text_message_with_headers() {
    let m = render(
        &raw(
            &format!(
                "{BASE}\nContent-Type: text/plain; charset=utf-8\nCc: Carol <carol@example.org>\nReply-To: bob@example.com\nIn-Reply-To: <prev@example.com>\nReferences: <r1@example.com> <r2@example.com>"
            ),
            "Hi Alice,\nsee https://example.com/doc.\n\n> old text\nBob",
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    assert_eq!(m.body_source, BodySource::PlainText);
    assert_eq!(m.headers.subject, "Hello");
    assert_eq!(m.headers.from.as_ref().unwrap().address, "bob@example.com");
    assert_eq!(m.headers.from.as_ref().unwrap().name, "Bob");
    assert_eq!(m.headers.cc[0].address, "carol@example.org");
    assert_eq!(m.headers.message_id.as_deref(), Some("abc@example.com"));
    assert_eq!(m.headers.in_reply_to.as_deref(), Some("prev@example.com"));
    assert_eq!(
        m.headers.references,
        vec!["r1@example.com", "r2@example.com"]
    );
    assert_eq!(m.headers.date, Some(1_794_909_600));
    assert!(m.plain_text.contains("> old text"));
    assert_eq!(all_links(&m)[0].url, "https://example.com/doc");
    assert!(m.notices.is_empty(), "{:?}", m.notices);
}

#[test]
fn html_structure_is_kept_as_blocks() {
    let m = render_html(
        "<h1>Title</h1><p>Hello <b>bold</b> and <i>italic</i> <a href=\"https://example.com/x\">link</a></p>\
         <ul><li>one</li><li>two</li></ul><ol><li>first</li></ol><blockquote><p>quoted</p></blockquote>\
         <pre>  code\n  here</pre><hr><table><tr><td>cell1</td><td>cell2</td></tr></table>",
    );
    assert_eq!(m.body_source, BodySource::Html);
    assert!(matches!(&m.body[0], Block::Heading { level: 1, .. }));
    let t = text_of(&m);
    assert!(t.contains("Title"), "{t}");
    assert!(
        t.contains("Hello bold and italic link (https://example.com/x)"),
        "{t}"
    );
    assert!(t.contains("- one\n- two"), "{t}");
    assert!(t.contains("1. first"), "{t}");
    assert!(t.contains("> quoted"), "{t}");
    assert!(t.contains("  code\n  here"), "{t}");
    assert!(t.contains("---"), "{t}");
    assert!(t.contains("cell1") && t.contains("cell2"));
    let Block::Paragraph(p) = &m.body[1] else {
        panic!("{:?}", m.body[1])
    };
    assert!(
        p.iter()
            .any(|i| matches!(i, Inline::Text { bold: true, text, .. } if text == "bold"))
    );
    assert!(
        p.iter()
            .any(|i| matches!(i, Inline::Text { italic: true, .. }))
    );
}

#[test]
fn whitespace_in_html_is_collapsed_and_invisible_characters_removed() {
    let m = render_html("<p>  a \n\t b\u{200B}c   </p><p>\u{FEFF}\u{00AD}</p><p>&nbsp;</p>");
    assert_eq!(m.body.len(), 1, "{:?}", m.body); // paragraphs with nothing visible are dropped
    assert_eq!(blocks_to_text(&m.body[..1]), "a bc\n");
}

// ---------------------------------------------------------------------------
// active content and tracking
// ---------------------------------------------------------------------------

#[test]
fn scripts_styles_frames_and_forms_vanish() {
    let m = render_html(
        "<p>Visible</p><script>alert(1)</script><style>p{background:url(https://t.example/css.gif)}</style>\
         <iframe src=\"https://t.example/f\">frame text</iframe><object data=\"https://t.example/o\">obj</object>\
         <embed src=\"https://t.example/e\"><form action=\"https://t.example/post\"><input name=pw><button>Go</button></form>\
         <noscript><img src=\"https://t.example/ns.gif\"></noscript><svg><script>alert(2)</script><text>svg text</text></svg>\
         <math><mi>x</mi></math><title>doc title</title><template><p>tmpl</p></template>",
    );
    let t = text_of(&m);
    assert!(t.contains("Visible") && t.contains("Go"), "{t}");
    for bad in [
        "alert",
        "frame text",
        "obj",
        "svg text",
        "doc title",
        "tmpl",
        "background",
    ] {
        assert!(!t.contains(bad), "{bad} leaked: {t}");
    }
    assert!(m.remote_images.is_empty(), "{:?}", m.remote_images);
}

#[test]
fn every_known_remote_loading_trick_is_neutralised() {
    let tricks = [
        "<img src=\"https://t.example/a.gif\" style=\"display:none\">",
        "<p style=\"background:url(https://t.example/b.gif)\">x</p>",
        "<p style=\"background:URL('https://t.example/c.gif')\">x</p>",
        "<div style=\"background-image: Url( https://t.example/d.gif )\">x</div>",
        "<style>@import url(https://t.example/e.css); p{background:URL(\"https://t.example/e.gif\")}</style>x",
        "<link rel=\"stylesheet\" href=\"https://t.example/f.css\">x",
        "<svg><image href=\"https://t.example/g.gif\"/></svg>",
        "<svg><filter><feImage href=\"https://t.example/h.gif\"/></filter></svg>",
        "<svg><use xlink:href=\"https://t.example/i.svg#x\"/></svg>",
        "<video poster=\"https://t.example/j.gif\" src=\"https://t.example/j.mp4\"></video>",
        "<audio src=\"https://t.example/k.mp3\"></audio>",
        "<input type=\"image\" src=\"https://t.example/l.gif\">",
        "<body background=\"https://t.example/m.gif\">x</body>",
        "<table background=\"https://t.example/n.gif\"><tr><td background=\"https://t.example/n2.gif\">x</td></tr></table>",
        "<object data=\"https://t.example/o.gif\"></object>",
        "<embed src=\"https://t.example/p.gif\">",
        "<meta http-equiv=\"refresh\" content=\"0;url=https://t.example/q\">x",
        "<base href=\"https://t.example/\"><img src=\"r.gif\">",
        "<img src=\"//t.example/s.gif\">",
        "<img src=\"/relative/t.gif\">",
        "<img srcset=\"https://t.example/u.gif 1x\">",
        "<picture><source srcset=\"https://t.example/v.gif\"><img alt=\"p\"></picture>",
        "<a href=\"https://example.com\" ping=\"https://t.example/w\">x</a>",
        "<img src=\"data:image/gif;base64,R0lGODlhAQABAAAAACw=\">",
        "<img src=\"javascript:alert(1)\">",
        "<IMG SRC=\"HTTPS://T.EXAMPLE/X.GIF\" onerror=\"alert(1)\">",
        "<img src=\"ftp://t.example/y.gif\">",
        "<img src=\"file:///etc/passwd\">",
        "<div style=\"content:url(https://t.example/z.gif)\">x</div>",
        "<font face=\"x\" background=\"https://t.example/aa.gif\">x</font>",
    ];
    for t in tricks {
        let m = render_html(t);
        // The only acceptable "remote" entry is a plain <img> whose address we deliberately
        // list-but-never-load; everything else must not even be listed.
        let listed: Vec<&str> = m.remote_images.iter().map(|r| r.url.as_str()).collect();
        let plain_img = t.contains("<IMG SRC=\"HTTPS://T.EXAMPLE/X.GIF\"")
            || t.starts_with("<img src=\"https://t.example/a.gif\"");
        if plain_img {
            assert_eq!(listed.len(), 1, "{t}: {listed:?}");
        } else {
            assert!(listed.is_empty(), "{t}: leaked {listed:?}");
        }
        let expected_links = usize::from(t.starts_with("<a href=\"https://example.com\" ping"));
        assert_eq!(all_links(&m).len(), expected_links, "{t}");
        // And nothing here could ever be fetched: the renderer has no network code.
    }
}

#[test]
fn dangerous_link_addresses_become_plain_text() {
    let m = render_html(
        "<a href=\"javascript:alert(1)\">js</a> <a href=\"JaVaScRiPt:alert(1)\">js2</a> <a href=\"java&#10;script:alert(1)\">js3</a> \
         <a href=\"data:text/html,x\">d</a> <a href=\"vbscript:x\">v</a> <a href=\"file:///etc/passwd\">f</a> \
         <a href=\"#frag\">frag</a> <a href=\"relative.html\">rel</a> <a href=\"https://example.com/ok\">ok</a>",
    );
    let l = all_links(&m);
    assert_eq!(l.len(), 1, "{l:?}");
    assert_eq!(l[0].url, "https://example.com/ok");
    let t = text_of(&m);
    for w in ["js", "js2", "js3", "frag", "rel"] {
        assert!(t.contains(w), "text of dropped link {w} should remain: {t}");
    }
}

#[test]
fn remote_pictures_are_listed_not_loaded_and_trackers_noticed() {
    let m = render_html(
        "<p>News</p><img src=\"https://cdn.example.com/banner.jpg\" alt=\"Banner\" width=\"600\">\
         <img src=\"https://stats.example.net/open?id=1\" width=\"1\" height=\"1\">\
         <img src=\"http://insecure.example.org/x.png\"><img src=\"https://cdn.example.com/banner.jpg\">",
    );
    assert_eq!(
        m.remote_images.len(),
        3,
        "duplicates are merged: {:?}",
        m.remote_images
    );
    assert!(!m.remote_images[0].likely_tracker);
    assert!(m.remote_images[1].likely_tracker);
    assert!(m.remote_images[2].insecure);
    let n = m
        .notices
        .iter()
        .find(|n| n.kind == NoticeKind::PicturesWithheld)
        .unwrap();
    assert!(
        n.text.contains("3 picture") && n.text.contains("1 of them look like tracking"),
        "{}",
        n.text
    );
    let Block::Paragraph(p) = &m.body[1] else {
        panic!("{:?}", m.body)
    };
    assert!(
        matches!(&p[0], Inline::Image(ImageRef { source: ImageSource::Remote { index: 0 }, alt, .. }) if alt == "Banner")
    );
}

#[test]
fn clickable_banner_keeps_link_and_shows_picture_placeholder() {
    let m = render_html(
        "<a href=\"https://shop.example.com/sale\"><img src=\"https://cdn.example.com/b.jpg\" alt=\"Big sale\"></a>",
    );
    let l = all_links(&m);
    assert_eq!(l.len(), 1);
    assert_eq!(l[0].text, "Big sale");
    assert_eq!(m.remote_images.len(), 1);
}

#[test]
fn link_text_that_names_another_site_is_flagged() {
    let m =
        render_html("<a href=\"https://evil.example.net/login\">https://www.paypal.com/signin</a>");
    let l = &all_links(&m)[0];
    assert!(l.is_suspicious());
    assert!(l.flags.contains(&LinkFlag::TextShowsOtherSite {
        shown: "www.paypal.com".into()
    }));
}

// ---------------------------------------------------------------------------
// inline pictures, alternatives, attachments
// ---------------------------------------------------------------------------

const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0, 0, 0];

fn b64(data: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(data)
}

fn related(html: &str, image_ct: &str, image: &[u8]) -> Vec<u8> {
    raw(
        &format!("{BASE}\nContent-Type: multipart/related; boundary=\"R\""),
        &format!(
            "--R\nContent-Type: text/html; charset=utf-8\n\n{html}\n--R\nContent-Type: {image_ct}\nContent-ID: <Logo@Example.Com>\nContent-Transfer-Encoding: base64\nContent-Disposition: inline\n\n{}\n--R--\n",
            b64(image)
        ),
    )
}

#[test]
fn inline_pictures_are_matched_by_content_id() {
    let m = render(&related("<p>Logo: <img src=\"cid:logo@example.com\" alt=\"logo\"></p><img src=\"cid:missing@x\">", "image/png", PNG), &RenderOptions::default()).unwrap();
    assert_eq!(m.inline_images.len(), 1);
    assert_eq!(m.inline_images[0].cid, "logo@example.com");
    assert_eq!(m.inline_images[0].content_type, "image/png");
    assert_eq!(m.inline_images[0].data, PNG);
    let Block::Paragraph(p) = &m.body[0] else {
        panic!()
    };
    assert!(p.iter().any(|i| matches!(i, Inline::Image(ImageRef { source: ImageSource::Inline { cid }, .. }) if cid == "logo@example.com")));
    let Block::Paragraph(p2) = &m.body[1] else {
        panic!("{:?}", m.body)
    };
    assert!(matches!(
        &p2[0],
        Inline::Image(ImageRef {
            source: ImageSource::Dropped,
            ..
        })
    ));
    assert!(m.attachments.is_empty());
}

#[test]
fn fake_pictures_are_not_trusted_as_pictures() {
    // Claims PNG but is HTML: must not become an inline image.
    let m = render(
        &related(
            "<img src=\"cid:logo@example.com\">",
            "image/png",
            b"<html><script>alert(1)</script>",
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    assert!(m.inline_images.is_empty());
    assert_eq!(
        m.attachments.len(),
        1,
        "offered as a plain attachment instead"
    );
    // SVG (can carry scripts and remote references) is never an inline picture.
    let m = render(
        &related(
            "<img src=\"cid:logo@example.com\">",
            "image/svg+xml",
            b"<svg onload=alert(1)/>",
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    assert!(m.inline_images.is_empty());
    assert_eq!(m.attachments.len(), 1);
}

fn alternative() -> Vec<u8> {
    raw(
        &format!("{BASE}\nContent-Type: multipart/alternative; boundary=\"A\""),
        "--A\nContent-Type: text/plain; charset=utf-8\n\nPlain version\n--A\nContent-Type: text/html; charset=utf-8\n\n<p>HTML <b>version</b> <img src=\"https://t.example/p.gif\"></p>\n--A--\n",
    )
}

#[test]
fn alternative_prefers_html_unless_plain_requested() {
    let m = render(&alternative(), &RenderOptions::default()).unwrap();
    assert_eq!(m.body_source, BodySource::Html);
    assert!(text_of(&m).contains("HTML version"));
    assert!(
        m.plain_text.contains("Plain version"),
        "plain text is for replies/search: {}",
        m.plain_text
    );
    assert_eq!(m.remote_images.len(), 1);

    let p = render(&alternative(), &RenderOptions { prefer_plain: true }).unwrap();
    assert_eq!(p.body_source, BodySource::PlainText);
    assert_eq!(text_of(&p).trim(), "Plain version");
    assert!(
        p.remote_images.is_empty(),
        "plain-text mode never even lists pictures"
    );
}

#[test]
fn html_only_message_still_has_plain_text_for_replies() {
    let m = render_html("<p>Hello <a href=\"https://example.com\">there</a></p>");
    assert!(
        m.plain_text.contains("Hello there (https://example.com/)"),
        "{}",
        m.plain_text
    );
    let p = render(
        &html_msg("<p>Only html</p>"),
        &RenderOptions { prefer_plain: true },
    )
    .unwrap();
    assert_eq!(
        text_of(&p).trim(),
        "Only html",
        "falls back to HTML text when there is no plain part"
    );
}

fn with_attachment(name_header: &str, data: &[u8]) -> Vec<u8> {
    raw(
        &format!("{BASE}\nContent-Type: multipart/mixed; boundary=\"M\""),
        &format!(
            "--M\nContent-Type: text/plain\n\nSee attached.\n--M\nContent-Type: application/octet-stream\n{name_header}\nContent-Transfer-Encoding: base64\n\n{}\n--M--\n",
            b64(data)
        ),
    )
}

#[test]
fn attachments_are_listed_and_extractable_byte_for_byte() {
    let data: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let bytes = with_attachment(
        "Content-Disposition: attachment; filename=\"report.pdf\"",
        &data,
    );
    let m = render(&bytes, &RenderOptions::default()).unwrap();
    assert_eq!(text_of(&m).trim(), "See attached.");
    assert_eq!(m.attachments.len(), 1);
    let a = &m.attachments[0];
    assert_eq!(
        (a.filename.as_str(), a.size, a.risk),
        ("report.pdf", 5000, AttachmentRisk::Normal)
    );
    let got = extract_attachment(&bytes, 0).unwrap();
    assert_eq!(got.data, data);
    assert_eq!(got.filename, "report.pdf");
    assert_eq!(
        extract_attachment(&bytes, 1),
        Err(RenderError::NoSuchAttachment)
    );
}

#[test]
fn attachment_names_are_made_safe() {
    for (header, expect) in [
        (
            "Content-Disposition: attachment; filename=\"../../etc/passwd\"",
            "passwd",
        ),
        (
            "Content-Disposition: attachment; filename=\"C:\\\\Windows\\\\evil.txt\"",
            "evil.txt",
        ),
        (
            "Content-Disposition: attachment; filename=\"a<b>:c|d?.txt\"",
            "abcd.txt",
        ),
        ("Content-Disposition: attachment", "attachment-1"),
        (
            "Content-Disposition: attachment; filename=\"\"",
            "attachment-1",
        ),
    ] {
        let m = render(&with_attachment(header, b"x"), &RenderOptions::default()).unwrap();
        assert_eq!(m.attachments[0].filename, expect, "{header}");
    }
    // RFC 2231 encoded name
    let m = render(
        &with_attachment(
            "Content-Disposition: attachment; filename*=UTF-8''na%C3%AFve.txt",
            b"x",
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    assert_eq!(m.attachments[0].filename, "naïve.txt");
}

#[test]
fn risky_attachments_are_flagged_including_disguised_ones() {
    for (name, risk) in [
        ("setup.exe", AttachmentRisk::Executable),
        ("invoice.pdf.exe", AttachmentRisk::DisguisedExecutable),
        ("invoice.pdf .exe", AttachmentRisk::DisguisedExecutable),
        ("photo.JPG.SCR", AttachmentRisk::DisguisedExecutable),
        ("macro.docm", AttachmentRisk::Executable),
        ("disk.iso", AttachmentRisk::Executable),
        ("report.pdf", AttachmentRisk::Normal),
        ("notes.txt", AttachmentRisk::Normal),
        ("noextension", AttachmentRisk::Normal),
        ("archive.tar.gz", AttachmentRisk::Normal),
    ] {
        assert_eq!(mime::classify(name), risk, "{name}");
    }
    let m = render(
        &with_attachment(
            "Content-Disposition: attachment; filename=\"invoice.pdf.exe\"",
            b"MZ",
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    let n = m
        .notices
        .iter()
        .find(|n| n.kind == NoticeKind::RiskyAttachment)
        .unwrap();
    assert_eq!(n.severity, Severity::Warning);
    assert!(n.text.contains("disguised"), "{}", n.text);
}

#[test]
fn direction_tricks_in_file_names_are_removed_and_reported() {
    // "invoice<RLO>fdp.exe" would display as "invoiceexe.pdf".
    let m = render(
        &with_attachment(
            "Content-Disposition: attachment; filename=\"invoice\u{202E}fdp.exe\"",
            b"MZ",
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    assert_eq!(m.attachments[0].filename, "invoicefdp.exe");
    assert_eq!(m.attachments[0].risk, AttachmentRisk::Executable);
    assert!(
        m.notices
            .iter()
            .any(|n| n.kind == NoticeKind::HiddenTextDirection)
    );
}

#[test]
fn signed_and_encrypted_messages_are_explained() {
    let signed = raw(
        &format!(
            "{BASE}\nContent-Type: multipart/signed; protocol=\"application/pgp-signature\"; boundary=\"S\""
        ),
        "--S\nContent-Type: text/plain\n\nSigned text\n--S\nContent-Type: application/pgp-signature\n\n-----BEGIN PGP SIGNATURE-----\nabc\n--S--\n",
    );
    let m = render(&signed, &RenderOptions::default()).unwrap();
    assert!(text_of(&m).contains("Signed text"));
    assert!(
        m.notices
            .iter()
            .any(|n| n.kind == NoticeKind::Signed && n.severity == Severity::Info)
    );
    assert!(
        m.notices.iter().all(|n| !n.text.contains("verified")),
        "must not claim verification"
    );

    let enc = raw(
        &format!(
            "{BASE}\nContent-Type: multipart/encrypted; protocol=\"application/pgp-encrypted\"; boundary=\"E\""
        ),
        "--E\nContent-Type: application/pgp-encrypted\n\nVersion: 1\n--E\nContent-Type: application/octet-stream\n\n-----BEGIN PGP MESSAGE-----\nxyz\n--E--\n",
    );
    let m = render(&enc, &RenderOptions::default()).unwrap();
    assert!(m.notices.iter().any(|n| n.kind == NoticeKind::Encrypted));
    assert_eq!(m.body_source, BodySource::Empty);
    assert!(!m.attachments.is_empty());
}

// ---------------------------------------------------------------------------
// sender tricks
// ---------------------------------------------------------------------------

fn from_msg(from: &str, extra: &str) -> RenderedMessage {
    render(
        &raw(
            &format!(
                "From: {from}\nTo: a@example.com\nSubject: x\n{extra}\nContent-Type: text/plain"
            ),
            "hi",
        ),
        &RenderOptions::default(),
    )
    .unwrap()
}

fn has(m: &RenderedMessage, k: NoticeKind) -> bool {
    m.notices.iter().any(|n| n.kind == k)
}

#[test]
fn name_that_claims_another_address_is_a_warning() {
    let m = from_msg("\"ceo@bank.com\" <attacker@evil.net>", "");
    assert!(has(&m, NoticeKind::NameLooksLikeOtherAddress));
    assert!(m.notices[0].text.contains("attacker@evil.net"));
    // Same domain in the name is fine.
    assert!(!has(
        &from_msg("\"bob@example.com\" <bob@example.com>", ""),
        NoticeKind::NameLooksLikeOtherAddress
    ));
    assert!(!has(
        &from_msg("Bob <bob@example.com>", ""),
        NoticeKind::NameLooksLikeOtherAddress
    ));
}

#[test]
fn reply_to_elsewhere_and_lookalike_senders_are_noted() {
    assert!(has(
        &from_msg("Bob <bob@example.com>", "Reply-To: x@other.net"),
        NoticeKind::ReplyGoesElsewhere
    ));
    assert!(!has(
        &from_msg("Bob <bob@example.com>", "Reply-To: bob2@mail.example.com"),
        NoticeKind::ReplyGoesElsewhere
    ));
    assert!(has(
        &from_msg("Bob <bob@xn--pypal-4ve.com>", ""),
        NoticeKind::SenderHasSpecialCharacters
    ));
    assert!(has(
        &from_msg("Bob <bob@p\u{0430}ypal.com>", ""),
        NoticeKind::SenderHasSpecialCharacters
    ));
}

#[test]
fn text_direction_tricks_in_names_and_subjects_are_stripped() {
    let m = from_msg("\"Bob\u{202E}evil\" <bob@example.com>", "");
    assert!(!m.headers.from.as_ref().unwrap().name.contains('\u{202E}'));
    assert!(has(&m, NoticeKind::HiddenTextDirection));
}

#[test]
fn unsubscribe_header_is_parsed_safely() {
    let m = from_msg(
        "News <n@example.com>",
        "List-Unsubscribe: <mailto:unsub@example.com?subject=x>, <https://example.com/u?id=1>, <javascript:alert(1)>\nList-Unsubscribe-Post: List-Unsubscribe=One-Click",
    );
    let u = m.headers.unsubscribe.unwrap();
    assert_eq!(u.web.as_deref(), Some("https://example.com/u?id=1"));
    assert_eq!(
        u.mailto.as_deref(),
        Some("mailto:unsub@example.com?subject=x")
    );
    assert!(u.one_click);
    // One-click needs an https address.
    let m = from_msg(
        "N <n@example.com>",
        "List-Unsubscribe: <http://example.com/u>\nList-Unsubscribe-Post: List-Unsubscribe=One-Click",
    );
    assert!(!m.headers.unsubscribe.unwrap().one_click);
    assert!(
        from_msg("N <n@example.com>", "List-Unsubscribe: <javascript:x>")
            .headers
            .unsubscribe
            .is_none()
    );
}

// ---------------------------------------------------------------------------
// hostile and broken input
// ---------------------------------------------------------------------------

#[test]
fn empty_and_garbage_input_do_not_panic() {
    for input in [
        &b""[..],
        b"\r\n\r\n",
        b"not an email at all",
        b"\xff\xfe\x00\x01",
        b"From: \r\n\r\n",
        b"Content-Type: multipart/mixed\r\n\r\nx",
    ] {
        let _ = render(input, &RenderOptions::default());
    }
    let m = render(b"Subject: only headers\r\n\r\n", &RenderOptions::default()).unwrap();
    assert_eq!(m.body_source, BodySource::Empty);
}

#[test]
fn invalid_utf8_and_unknown_charsets_still_show_text() {
    let mut bad =
        format!("{BASE}\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nabc ").into_bytes();
    bad.extend([0xFF, 0xFE, b'd', b'e', b'f']);
    let m = render(&bad, &RenderOptions::default()).unwrap();
    assert!(text_of(&m).contains("abc") && text_of(&m).contains("def"));
    let m = render(
        &raw(
            &format!("{BASE}\nContent-Type: text/plain; charset=x-no-such-charset"),
            "hello",
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    assert!(text_of(&m).contains("hello"));
    let m = render(&raw(&format!("{BASE}\nContent-Type: text/plain; charset=iso-8859-1\nContent-Transfer-Encoding: quoted-printable"), "caf=E9"), &RenderOptions::default()).unwrap();
    assert!(text_of(&m).contains("café"));
}

#[test]
fn oversized_messages_are_refused_politely() {
    let big = vec![b'a'; MAX_MESSAGE_BYTES + 1];
    assert!(matches!(
        render(&big, &RenderOptions::default()),
        Err(RenderError::TooLarge(_))
    ));
}

#[test]
fn deeply_nested_html_cannot_crash_us() {
    for depth in [200, 1_000, 20_000, 200_000] {
        let m = render_html(&format!(
            "{}x{}",
            "<div>".repeat(depth),
            "</div>".repeat(depth)
        ));
        assert!(text_of(&m).contains('x'));
        let q = render_html(&format!(
            "{}deep{}",
            "<blockquote>".repeat(depth),
            "</blockquote>".repeat(depth)
        ));
        assert!(text_of(&q).contains("deep"));
        let l = render_html(&format!(
            "{}item{}",
            "<ul><li>".repeat(depth),
            "</li></ul>".repeat(depth)
        ));
        assert!(text_of(&l).contains("item"));
    }
}

#[test]
fn deeply_nested_mime_and_many_parts_are_capped() {
    let mut body = String::new();
    for i in 0..60 {
        body.push_str(&format!(
            "--b{i}\nContent-Type: multipart/mixed; boundary=\"b{}\"\n\n",
            i + 1
        ));
    }
    body.push_str("--b60\nContent-Type: text/plain\n\ndeep text\n");
    let m = render(
        &raw(
            &format!("{BASE}\nContent-Type: multipart/mixed; boundary=\"b0\""),
            &body,
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    assert!(has(&m, NoticeKind::TooComplex) || text_of(&m).contains("deep text"));

    let mut many = String::new();
    for i in 0..3000 {
        many.push_str(&format!("--M\nContent-Type: text/plain\nContent-Disposition: attachment; filename=f{i}.txt\n\nx\n"));
    }
    many.push_str("--M--\n");
    let m = render(
        &raw(
            &format!("{BASE}\nContent-Type: multipart/mixed; boundary=\"M\""),
            &many,
        ),
        &RenderOptions::default(),
    )
    .unwrap();
    assert!(m.attachments.len() <= 2_000);
    assert!(has(&m, NoticeKind::TooComplex));
}

#[test]
fn huge_bodies_and_many_images_are_bounded() {
    let big = "<p>x</p>".repeat(200_000); // ~1.6 MB, fine
    assert!(!render_html(&big).body.is_empty());
    let imgs: String = (0..1000)
        .map(|i| format!("<img src=\"https://t.example/{i}.gif\">"))
        .collect();
    let m = render_html(&imgs);
    assert!(m.remote_images.len() <= 200);
    assert!(has(&m, NoticeKind::Truncated));
    let huge = format!("<p>{}</p>", "y".repeat(6 * 1024 * 1024));
    let m = render_html(&huge);
    assert!(has(&m, NoticeKind::Truncated));
}

#[test]
fn pseudo_random_garbage_never_panics() {
    // A small deterministic generator (no external fuzzer needed).
    let mut x: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    let pieces: [&str; 22] = [
        "<",
        ">",
        "</",
        "/>",
        "<div>",
        "</div>",
        "<a href=\"",
        "\">",
        "<img src=\"cid:",
        "<script>",
        "</style>",
        "&amp;",
        "&#x0;",
        "\u{202E}",
        "javascript:",
        "<!--",
        "-->",
        "<![CDATA[",
        "<svg>",
        "<table><tr>",
        "<li>",
        "\"",
    ];
    for _ in 0..300 {
        let mut s = String::new();
        for _ in 0..(next() % 80) {
            s.push_str(pieces[(next() % 22) as usize]);
            if next() % 3 == 0 {
                s.push(char::from_u32((next() % 0x2FFF) as u32 + 0x20).unwrap_or('?'));
            }
        }
        let m = render_html(&s);
        // Whatever came in, no link may be anything but a safe kind.
        for l in all_links(&m) {
            assert!(
                ["http:", "https:", "mailto:", "tel:"]
                    .iter()
                    .any(|p| l.url.starts_with(p)),
                "{}",
                l.url
            );
        }
        let mut raw_bytes: Vec<u8> = Vec::new();
        for _ in 0..(next() % 200) {
            raw_bytes.push((next() & 0xFF) as u8);
        }
        let _ = render(&raw_bytes, &RenderOptions::default());
        let _ = sanitize_html(&s);
    }
}

#[test]
fn sanitizer_output_contains_no_active_markup() {
    let out = sanitize_html(
        "<p onclick=\"x()\" style=\"color:red\" class=\"c\" id=\"i\">t</p><script>1</script><a href=\"javascript:1\">a</a><a href=\"https://e.com\" target=\"_blank\" rel=\"opener\">b</a><img src=\"cid:x\" onerror=\"1\">",
    );
    for bad in [
        "onclick",
        "style",
        "class",
        "id=",
        "<script",
        "javascript",
        "onerror",
        "target",
        "_blank",
        "rel=\"opener",
    ] {
        assert!(!out.contains(bad), "{bad} survived: {out}");
    }
    assert!(out.contains("noopener noreferrer nofollow"), "{out}");
}

#[test]
fn text_directly_inside_a_list_is_not_lost() {
    let m = render_html("<ul>stray<li>one</li>more</ul>");
    let t = text_of(&m);
    assert!(
        t.contains("stray") && t.contains("one") && t.contains("more"),
        "{t}"
    );
}

#[test]
fn absurdly_deep_html_is_reduced_to_text_quickly_and_reported() {
    let start = std::time::Instant::now();
    let h = format!(
        "<p>intro</p><script>evil()</script>{}x{}<a href=\"https://example.com\">l</a>",
        "<div><b>".repeat(100_000),
        "</b></div>".repeat(100_000)
    );
    let m = render_html(&h);
    assert!(
        start.elapsed() < std::time::Duration::from_secs(10),
        "took {:?}",
        start.elapsed()
    );
    let t = text_of(&m);
    assert!(t.contains("intro") && t.contains('x'), "{t}");
    assert!(!t.contains("evil"), "script text must stay out: {t}");
    assert!(has(&m, NoticeKind::TooComplex));
    assert!(m.remote_images.is_empty());
}

#[test]
fn ordinary_unclosed_old_style_html_keeps_its_structure() {
    // <p> and <li> without end tags are normal in old mail; they must not count as deep nesting.
    let h = format!(
        "{}<a href=\"https://example.com\">link</a>",
        "<p>para".repeat(2000)
    );
    let m = render_html(&h);
    assert!(!has(&m, NoticeKind::TooComplex));
    assert_eq!(all_links(&m).len(), 1);
}

#[test]
fn header_summary_reads_the_list_fields() {
    let h = b"From: Bob <bob@example.com>\r\nTo: alice@example.com\r\nSubject: Hello 101\r\nMessage-ID: <m101@example.com>\r\nContent-Type: multipart/mixed; boundary=x\r\nDate: Tue, 17 Nov 2026 10:00:00 +0000\r\n\r\n";
    for variant in [
        &h[..],
        &h[..h.len() - 2],
        &h[..h.len() - 4],
        &b"Subject: x\n\n"[..],
    ] {
        let s = summarize_header(variant);
        assert!(!s.headers.subject.is_empty(), "{variant:?}");
    }
    let s = summarize_header(h);
    assert_eq!(s.headers.subject, "Hello 101");
    assert_eq!(s.headers.message_id.as_deref(), Some("m101@example.com"));
    assert_eq!(s.headers.from.as_ref().unwrap().address, "bob@example.com");
    assert_eq!(s.headers.date, Some(1_794_909_600));
    assert!(s.maybe_attachments);
    assert_eq!(summarize_header(b""), HeaderSummary::default());
    assert_eq!(summarize_header(&[0xff, 0xfe, 0x00]).headers.subject, "");
}

#[test]
fn spaces_inside_links_and_bold_text_are_kept() {
    let m = render_html(
        "<p>on <a href=\"https://example.org/s\">the team site</a>, see <b>the big plan</b> and <i>more here</i>.</p>",
    );
    let t = text_of(&m);
    assert!(t.contains("on the team site"), "{t}");
    assert!(t.contains("see the big plan and more here."), "{t}");
    let l = all_links(&m);
    assert_eq!(l[0].text, "the team site");
}
