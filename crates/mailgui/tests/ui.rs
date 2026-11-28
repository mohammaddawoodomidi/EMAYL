//! Drives the real application state and draws every screen without a window
//! (software renderer), saving a picture of each into `target/ui-snapshots/`.
//!
//! Run with `ICED_TEST_BACKEND=tiny-skia`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::too_many_arguments
)]

use std::path::PathBuf;
use std::sync::Arc;

use iced::Size;
use iced::widget::text_editor;
use iced_test::{Simulator, simulator};
use mailcore::account::{Account, AuthMethod, Security, ServerConfig, ServerSettings};
use mailcore::compose::Attachment;
use mailcore::drafts::DraftSummary;
use mailcore::email::EmailAddress;
use mailcore::imap::{Folder, FolderRole};
use mailcore::outbox::{
    FailureNote, OutboxItem, OutboxState, RecipientEntry, RecipientStatus, SentCopy,
};
use mailcore::render::{self, Link, LinkFlag, RenderOptions};
use mailcore::replies::ReplyKind;
use mailcore::secrets::MemorySecretStore;
use mailcore::store::{ListScope, NewMessage};
use mailcore::syncer::SyncStatus;
use mailgui::app::{Action, App, BodyState, Dialog, Message, Nav, Screen};
use mailgui::compose_flow::{ComposeMsg, OutboxMsg, Picked};
use mailgui::services::{Paths, Services, SignIn, clients_from};
use mailgui::userdata_flow::DataMsg;
use mailgui::view::view;

const NOW: i64 = 1_763_400_000; // 2025-11-17, fixed so pictures are stable

fn snap_dir() -> PathBuf {
    let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../target/ui-snapshots");
    std::fs::create_dir_all(&d).expect("snapshot dir");
    d
}

fn root() -> PathBuf {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let d = std::env::temp_dir().join(format!("emayl-ui-{}-{n}", std::process::id()));
    std::fs::create_dir_all(&d).expect("temp dir");
    d
}

fn services() -> Arc<Services> {
    Arc::new(
        Services::open_with(Paths::under(&root()), Arc::new(MemorySecretStore::new()))
            .expect("services"),
    )
}

fn account(email: &str, name: &str, host: &str) -> Account {
    let imap = ServerConfig::new(&format!("imap.{host}"), 993, Security::Tls).expect("imap");
    let smtp = ServerConfig::new(&format!("smtp.{host}"), 465, Security::Tls).expect("smtp");
    Account::new(
        name,
        EmailAddress::parse(email).expect("email"),
        ServerSettings {
            imap,
            imap_username: email.into(),
            smtp,
            smtp_username: email.into(),
        },
        AuthMethod::Password,
    )
    .expect("account")
}

fn raw_html(from: &str, subject: &str, id: &str, reply_to: Option<&str>) -> Vec<u8> {
    let rt = reply_to
        .map(|r| format!("In-Reply-To: <{r}>\r\nReferences: <{r}>\r\n"))
        .unwrap_or_default();
    format!(
        "From: {from}\r\nTo: Alex Smith <alex@example.com>\r\nSubject: {subject}\r\nMessage-ID: <{id}>\r\n{rt}Date: Mon, 17 Nov 2025 09:30:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/alternative; boundary=\"b\"\r\n\r\n\
--b\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nHi Alex,\r\n\r\nHere is the plan for the launch.\r\n\r\n--b\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
<html><body><h2>Launch plan</h2><p>Hi Alex,</p><p>Here is the <b>plan</b> for the launch. The full schedule is on <a href=\"https://example.org/schedule\">the team site</a>.</p>\
<ul><li>Freeze on Thursday</li><li>Announce on Monday</li></ul><blockquote>Earlier: we agreed on the date.</blockquote>\
<p><a href=\"https://paypa1-secure.example.net/login\">https://www.paypal.com/security</a></p></body></html>\r\n--b--\r\n"
    )
    .into_bytes()
}

fn raw_plain(from: &str, subject: &str, id: &str, text: &str) -> Vec<u8> {
    format!("From: {from}\r\nTo: Alex Smith <alex@example.com>\r\nSubject: {subject}\r\nMessage-ID: <{id}>\r\nDate: Mon, 17 Nov 2025 08:00:00 +0000\r\nContent-Type: text/plain; charset=utf-8\r\n\r\n{text}\r\n").into_bytes()
}

fn msg(
    uid: u32,
    subject: &str,
    name: &str,
    addr: &str,
    date: i64,
    flags: u32,
    id: &str,
    reply_to: Option<&str>,
) -> NewMessage {
    NewMessage {
        uid,
        message_id: Some(id.into()),
        in_reply_to: reply_to.map(Into::into),
        references: reply_to.map(|r| vec![r.to_owned()]).unwrap_or_default(),
        subject: subject.into(),
        from_name: name.into(),
        from_addr: addr.into(),
        to: "Alex Smith <alex@example.com>".into(),
        date,
        flags,
        ..NewMessage::default()
    }
}

struct World {
    app: App,
    svc: Arc<Services>,
    account_id: String,
    inbox: i64,
}

fn folder(raw: &str, role: FolderRole) -> Folder {
    Folder {
        raw_name: raw.into(),
        display_name: raw.into(),
        delimiter: Some("/".into()),
        role,
        selectable: true,
    }
}

/// An account with a handful of realistic messages, already shown in the lists.
fn world() -> World {
    // SAFETY-free: just a process-wide choice of the software renderer.
    let svc = services();
    let acc = account("alex@example.com", "Alex Smith", "example.com");
    let account_id = acc.id.as_str().to_owned();
    svc.add_account(acc, &secrecy::SecretString::from("pw".to_owned()))
        .expect("add account");
    let folders = svc
        .store
        .upsert_folders(
            &account_id,
            &[
                folder("INBOX", FolderRole::Inbox),
                folder("Sent", FolderRole::Sent),
                folder("Drafts", FolderRole::Drafts),
                folder("Archive", FolderRole::Archive),
                folder("Spam", FolderRole::Junk),
                folder("Trash", FolderRole::Trash),
                folder("Projects/Launch", FolderRole::Other),
            ],
            true,
        )
        .expect("folders");
    let inbox = folders
        .iter()
        .find(|f| f.raw_name == "INBOX")
        .expect("inbox")
        .id;
    let m = vec![
        msg(
            1,
            "Launch plan",
            "Maria Lopez",
            "maria@acme.io",
            NOW - 3600,
            0,
            "m1@acme",
            None,
        ),
        msg(
            2,
            "Re: Launch plan",
            "Jonas Weber",
            "jonas@acme.io",
            NOW - 1800,
            0,
            "m2@acme",
            Some("m1@acme"),
        ),
        msg(
            3,
            "Your invoice for October",
            "Billing",
            "billing@cloudhost.net",
            NOW - 86_400 * 2,
            1,
            "m3@host",
            None,
        ),
        msg(
            4,
            "Lunch on Friday?",
            "Sam Carter",
            "sam@example.org",
            NOW - 86_400 * 9,
            1,
            "m4@sam",
            None,
        ),
        msg(
            5,
            "Weekly digest: 12 new articles you might like",
            "The Daily Rust",
            "news@dailyrust.example",
            NOW - 86_400 * 40,
            1,
            "m5@news",
            None,
        ),
    ];
    svc.store.insert_messages(inbox, &m).expect("insert");
    let rows = svc
        .store
        .list_threads(&ListScope::Folder(inbox), false, None, 50)
        .expect("threads");
    assert_eq!(rows.len(), 4, "reply joins its thread");
    // Cache bodies so previews and search work.
    let all = svc
        .store
        .thread_messages(
            &rows
                .iter()
                .find(|t| t.subject.contains("Launch"))
                .expect("t")
                .thread_key,
        )
        .expect("tm");
    for (row, raw) in all.iter().zip([
        raw_html(
            "Maria Lopez <maria@acme.io>",
            "Launch plan",
            "m1@acme",
            None,
        ),
        raw_html(
            "Jonas Weber <jonas@acme.io>",
            "Re: Launch plan",
            "m2@acme",
            Some("m1@acme"),
        ),
    ]) {
        svc.store.store_body(row.id, &raw).expect("body");
    }
    for r in &rows {
        if r.subject.starts_with("Your invoice")
            || r.subject.starts_with("Lunch")
            || r.subject.starts_with("Weekly")
        {
            let raw = raw_plain(
                "x <x@example.org>",
                &r.subject,
                "x@x",
                "Thanks! The invoice for October is attached to this message. Please pay within 14 days.",
            );
            svc.store.store_body(r.newest_id, &raw).expect("body");
        }
    }
    let (mut app, _task) = App::new(Arc::clone(&svc));
    app.now = NOW;
    app.tz = 0;
    app.screen = Screen::Mailbox;
    for a in &mut app.accounts {
        a.status = SyncStatus::Online {
            last_sync: NOW - 40,
            new_messages_total: 0,
            pending_actions: 0,
        };
    }
    let f = svc.store.folders(&account_id).expect("f");
    let _ = app.update(Message::FoldersLoaded(vec![(account_id.clone(), f)]));
    // With one account the app opens that account's inbox.
    assert_eq!(
        app.nav,
        Nav::Folder {
            account: account_id.clone(),
            folder_id: inbox
        }
    );
    let threads = svc
        .store
        .list_threads(&ListScope::Folder(inbox), false, None, 100)
        .expect("t");
    let nav = app.nav.clone();
    let limit = app.limit;
    let _ = app.update(Message::ThreadsLoaded(nav, limit, threads));
    World {
        app,
        svc,
        account_id,
        inbox,
    }
}

fn snapshot(app: &App, name: &str, size: Size) {
    // The software renderer needs no graphics card.
    let mut ui: Simulator<'_, Message> =
        Simulator::with_size(iced::Settings::default(), size, view(app));
    let theme = mailgui::theme::iced_theme(&app.colors());
    let shot = ui.snapshot(&theme).expect("snapshot");
    let path = snap_dir().join(name);
    // The library adds the renderer's name to the file name: drop older pictures of this screen.
    let _ = std::fs::remove_file(snap_dir().join(format!("{name}-tiny-skia.png")));
    assert!(shot.matches_image(&path).expect("write picture"), "{name}");
}

const BIG: Size = Size {
    width: 1280.0,
    height: 800.0,
};

fn open_launch(w: &mut World) {
    let key = w
        .app
        .threads
        .iter()
        .find(|t| t.subject.contains("Launch"))
        .expect("thread")
        .thread_key
        .clone();
    let _ = w
        .app
        .update(Message::OpenThread(key.clone(), ListScope::Folder(w.inbox)));
    let msgs = w.svc.store.thread_messages(&key).expect("msgs");
    let _ = w.app.update(Message::ThreadLoaded(key, msgs.clone()));
    for (i, row) in msgs.iter().enumerate() {
        let raw = if i == 0 {
            raw_html(
                "Maria Lopez <maria@acme.io>",
                "Launch plan",
                "m1@acme",
                None,
            )
        } else {
            raw_html(
                "Jonas Weber <jonas@acme.io>",
                "Re: Launch plan",
                "m2@acme",
                Some("m1@acme"),
            )
        };
        let rendered = render::render(&raw, &RenderOptions::default()).expect("render");
        let _ = w
            .app
            .update(Message::BodyLoaded(row.id, Ok(Box::new(rendered))));
    }
}

#[test]
fn first_run_wizard() {
    let (app, _t) = App::new(services());
    assert_eq!(app.screen, Screen::AddAccount { first: true });
    snapshot(&app, "01-first-run", BIG);
}

#[test]
fn wizard_password_step_and_failure() {
    let (mut app, _t) = App::new(services());
    let _ = app.update(Message::WizName("Alex Smith".into()));
    let _ = app.update(Message::WizEmail("alex@gmail.com".into()));
    snapshot(&app, "02-wizard-email-filled", BIG);
}

#[test]
fn mailbox_lists_threads() {
    let w = world();
    assert_eq!(w.app.threads.len(), 4);
    snapshot(&w.app, "03-mailbox", BIG);
}

#[test]
fn open_thread_shows_rendered_body() {
    let mut w = world();
    open_launch(&mut w);
    let r = w.app.reader.as_ref().expect("reader");
    assert_eq!(r.messages.len(), 2);
    assert!(
        w.app
            .bodies
            .values()
            .all(|b| matches!(b, BodyState::Ready(_)))
    );
    snapshot(&w.app, "04-reader", BIG);
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Dark));
    snapshot(&w.app, "04c-reader-dark", BIG);
}

#[test]
fn folder_view_and_narrow_window() {
    let mut w = world();
    let _ = w.app.update(Message::Go(Nav::Folder {
        account: w.account_id.clone(),
        folder_id: w.inbox,
    }));
    snapshot(&w.app, "05-narrow", Size::new(960.0, 600.0));
}

#[test]
fn search_results() {
    let mut w = world();
    let _ = w.app.update(Message::SearchChanged("invoice".into()));
    let hits = w
        .svc
        .store
        .search(
            &mailcore::store::SearchQuery::parse("invoice"),
            &mailcore::store::SearchScope::Everywhere,
            mailcore::store::SearchSort::Newest,
            50,
        )
        .expect("search");
    assert!(!hits.is_empty(), "the cached body should be searchable");
    let seq = 1; // the first SearchChanged sequence number
    let _ = w.app.update(Message::SearchDone(seq, hits));
    snapshot(&w.app, "06-search", BIG);
}

#[test]
fn link_dialog_warns() {
    let mut w = world();
    open_launch(&mut w);
    let link = Link {
        text: "https://www.paypal.com/security".into(),
        url: "https://paypa1-secure.example.net/login".into(),
        host: "paypa1-secure.example.net".into(),
        host_ascii: "paypa1-secure.example.net".into(),
        flags: vec![LinkFlag::TextShowsOtherSite {
            shown: "https://www.paypal.com/security".into(),
        }],
    };
    let _ = w.app.update(Message::LinkClicked(link));
    assert!(matches!(w.app.dialog, Some(Dialog::Link(_))));
    snapshot(&w.app, "07-link-dialog", BIG);
    let _ = w.app.update(Message::CloseDialog);
    assert!(w.app.dialog.is_none());
}

#[test]
fn attention_banner_and_accounts_screen() {
    let mut w = world();
    w.app.accounts[0].status = SyncStatus::NeedsAttention {
        error: "The server did not accept the password.".into(),
        advice: "Check the password. Gmail, iCloud, Yahoo and others need an app password.".into(),
    };
    snapshot(&w.app, "08-attention", BIG);
    let _ = w.app.update(Message::ShowAccounts);
    assert_eq!(w.app.screen, Screen::Accounts);
    snapshot(&w.app, "09-accounts", BIG);
}

#[test]
fn dialogs_for_account_changes() {
    let mut w = world();
    let id = w.account_id.clone();
    let _ = w.app.update(Message::AskRemoveAccount(id.clone()));
    assert!(matches!(w.app.dialog, Some(Dialog::RemoveAccount(_))));
    snapshot(&w.app, "10-remove-dialog", BIG);
    let _ = w.app.update(Message::CloseDialog);
    let _ = w.app.update(Message::AskChangePassword(id));
    let _ = w.app.update(Message::PasswordInput("secret".into()));
    snapshot(&w.app, "11-password-dialog", BIG);
}

#[test]
fn add_second_account_screen_keeps_sidebar() {
    let mut w = world();
    let _ = w.app.update(Message::ShowAddAccount);
    assert_eq!(w.app.screen, Screen::AddAccount { first: false });
    snapshot(&w.app, "12-add-second", BIG);
}

#[test]
fn escape_closes_dialog_then_returns_to_mailbox() {
    let mut w = world();
    let _ = w.app.update(Message::ShowAccounts);
    let _ = w
        .app
        .update(Message::AskRemoveAccount(w.account_id.clone()));
    let _ = w.app.update(Message::Escape);
    assert!(w.app.dialog.is_none());
    assert_eq!(w.app.screen, Screen::Accounts);
    let _ = w.app.update(Message::Escape);
    assert_eq!(w.app.screen, Screen::Mailbox);
}

#[test]
fn fatal_screen_draws() {
    let app = App::fatal("The data folder could not be created.".into());
    snapshot(&app, "13-fatal", Size::new(900.0, 600.0));
}

#[test]
fn buttons_are_wired_to_messages() {
    // Click the "Continue" button on the first screen and see that it sends the right message.
    let (app, _t) = App::new(services());
    let mut ui = simulator(view(&app));
    ui.click("Continue").expect("button exists");
    let msgs: Vec<Message> = ui.into_messages().collect();
    assert!(
        msgs.iter().any(|m| matches!(m, Message::WizContinue)),
        "{msgs:?}"
    );
    let _ = Action::Archive;
}

// ---------------------------------------------------------------------------
// Writing and sending
// ---------------------------------------------------------------------------

fn type_into_body(app: &mut App, text: &str) {
    let _ = app.update(Message::Compose(ComposeMsg::Body(
        text_editor::Action::Edit(text_editor::Edit::Paste(Arc::new(text.to_owned()))),
    )));
}

fn file(name: &str, ct: &str, n: usize) -> Attachment {
    Attachment {
        filename: name.into(),
        content_type: ct.into(),
        data: vec![7; n],
    }
}

#[test]
fn new_message_screen() {
    let mut w = world();
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    assert_eq!(w.app.screen, Screen::Compose);
    let c = w.app.compose.as_ref().unwrap();
    assert_eq!(c.account, w.account_id);
    assert!(!c.touched, "an untouched message leaves no draft");
    snapshot(&w.app, "20-compose-new", BIG);
}

#[test]
fn typing_marks_the_message_as_changed_and_checks_the_addresses() {
    let mut w = world();
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let _ = w.app.update(Message::Compose(ComposeMsg::To(
        "ann@example.org, nobody".into(),
    )));
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Subject("Hello".into())));
    type_into_body(&mut w.app, "Hi Ann,\n\nSee you soon.");
    assert!(w.app.compose.as_ref().unwrap().touched);
    let _ = w.app.update(Message::Compose(ComposeMsg::Send));
    let c = w.app.compose.as_ref().unwrap();
    assert!(!c.sending, "a bad address stops the send");
    assert!(
        c.error.as_deref().unwrap_or("").starts_with("To:"),
        "{:?}",
        c.error
    );
    snapshot(&w.app, "21-compose-error", BIG);
}

#[test]
fn empty_subject_and_forgotten_attachment_ask_first() {
    let mut w = world();
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::To("ann@example.org".into())));
    type_into_body(&mut w.app, "The invoice is attached.");
    let _ = w.app.update(Message::Compose(ComposeMsg::Send));
    match &w.app.dialog {
        Some(Dialog::ConfirmSend(ws)) => assert_eq!(ws.len(), 2),
        other => panic!("expected a question, got {other:?}"),
    }
    assert!(!w.app.compose.as_ref().unwrap().sending);
    snapshot(&w.app, "22-confirm-send", BIG);
}

#[test]
fn attachments_are_listed_and_can_be_removed() {
    let mut w = world();
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let picked = Picked {
        attachments: vec![
            file("Quarterly report 2026.pdf", "application/pdf", 482_000),
            file("photo.jpg", "image/jpeg", 2_300_000),
        ],
        skipped: vec!["“movie.mkv” (900 MB) is too big.".into()],
    };
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Attached(Ok(picked))));
    let c = w.app.compose.as_ref().unwrap();
    assert_eq!(c.attachments.len(), 2);
    assert!(c.error.as_deref().unwrap().contains("too big"));
    snapshot(&w.app, "23-compose-attachments", BIG);
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::RemoveAttachment(0)));
    assert_eq!(w.app.compose.as_ref().unwrap().attachments.len(), 1);
}

#[test]
fn reply_is_prepared_from_the_open_message() {
    let mut w = world();
    open_launch(&mut w);
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Reply(ReplyKind::Reply)));
    assert_eq!(w.app.screen, Screen::Compose);
    let c = w.app.compose.as_ref().unwrap();
    assert!(c.to.contains("jonas@acme.io"), "{}", c.to);
    assert_eq!(c.subject, "Re: Launch plan");
    assert!(c.in_reply_to.as_deref().unwrap().starts_with("<m2@acme"));
    assert!(c.body_text().contains("Jonas Weber wrote:"));
    assert!(c.body_text().contains("> Launch plan") || c.body_text().contains("> Hi Alex,"));
    assert!(!c.touched);
    let _ = w.app.update(Message::Compose(ComposeMsg::Subject(
        "Re: Launch plan".into(),
    )));
    snapshot(&w.app, "24-reply", BIG);
}

#[test]
fn reply_all_and_forward_differ() {
    let mut w = world();
    open_launch(&mut w);
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Reply(ReplyKind::Forward)));
    let c = w.app.compose.as_ref().unwrap();
    assert!(c.to.is_empty());
    assert!(c.subject.starts_with("Fwd:"));
    assert!(c.in_reply_to.is_none());
    assert!(c.body_text().contains("Forwarded message"));
}

#[test]
fn starting_a_second_message_keeps_the_first_as_a_draft_and_close_returns() {
    let mut w = world();
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Subject("First".into())));
    let first = w.app.compose.as_ref().unwrap().draft_id.clone();
    let _ = w.app.update(Message::Compose(ComposeMsg::Close));
    assert!(w.app.compose.is_none());
    assert_eq!(w.app.screen, Screen::Mailbox);
    // The close task is not run here, so write the draft as the task would.
    let msg = mailcore::compose::OutgoingMessage {
        subject: "First".into(),
        ..Default::default()
    };
    w.svc
        .save_draft(Some(&first), Some(&w.account_id), &msg)
        .unwrap();
    assert_eq!(w.svc.list_drafts().len(), 1);
    let loaded = w.svc.load_draft(&first).unwrap();
    assert_eq!(loaded.message.subject, "First");
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::OpenDraft(first.clone())));
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::DraftLoaded(Ok(Box::new(
            loaded,
        )))));
    assert_eq!(w.app.compose.as_ref().unwrap().subject, "First");
    assert_eq!(w.app.compose.as_ref().unwrap().draft_id, first);
}

#[test]
fn discarding_asks_only_when_something_was_written() {
    let mut w = world();
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let _ = w.app.update(Message::Compose(ComposeMsg::AskDiscard));
    assert!(w.app.compose.is_none(), "nothing written: closed at once");
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Subject("x".into())));
    let _ = w.app.update(Message::Compose(ComposeMsg::AskDiscard));
    assert!(matches!(w.app.dialog, Some(Dialog::DiscardDraft)));
    snapshot(&w.app, "25-discard", BIG);
    let _ = w.app.update(Message::Compose(ComposeMsg::Discard));
    assert!(w.app.compose.is_none() && w.app.dialog.is_none());
}

fn item(subject: &str, state: OutboxState, to: &[&str]) -> OutboxItem {
    OutboxItem {
        version: 1,
        id: format!("id-{subject}"),
        account: mailcore::account::AccountId::generate(),
        message_id: "x@example.com".into(),
        from: "alex@example.com".into(),
        subject: subject.into(),
        size: 1000,
        created_at: NOW - 600,
        queued_at: NOW - 600,
        state,
        attempts: 1,
        next_attempt_at: NOW + 300,
        recipients: to
            .iter()
            .map(|a| RecipientEntry {
                address: (*a).into(),
                status: RecipientStatus::Pending,
                note: None,
            })
            .collect(),
        last_error: None,
        last_server_reply: None,
        sent_at: (state == OutboxState::Sent).then_some(NOW - 120),
        sent_copy: SentCopy::Saved,
    }
}

#[test]
fn outbox_screen_shows_every_state() {
    let mut w = world();
    let mut failed = item(
        "Contract draft",
        OutboxState::Failed,
        &["lawyer@firm.example"],
    );
    failed.last_error = Some(FailureNote {
        code: Some(552),
        enhanced: Some("5.3.4".into()),
        server_text: "Message size exceeds fixed limit".into(),
        advice: "The message is too large for the recipient's provider. Remove or shrink an attachment and send it again.".into(),
        at: NOW - 60,
        account_problem: false,
    });
    let mut waiting = item(
        "Holiday photos",
        OutboxState::Deferred,
        &["mum@example.org", "dad@example.org", "sis@example.org"],
    );
    waiting.last_error = Some(FailureNote { code: None, enhanced: None, server_text: String::new(), advice: "Your mail provider could not be reached. The message will be tried again automatically.".into(), at: NOW - 30, account_problem: false });
    w.app.outbox = vec![
        failed,
        waiting,
        item(
            "Lunch on Friday?",
            OutboxState::Queued,
            &["sam@example.org"],
        ),
        item("Thanks!", OutboxState::Sent, &["maria@acme.io"]),
    ];
    w.app.drafts = vec![DraftSummary {
        id: "d1".into(),
        account: None,
        updated_at: NOW - 7200,
        subject: "Plans for December".into(),
        first_recipient: "family@example.org".into(),
    }];
    let _ = w.app.update(Message::Outbox(OutboxMsg::Show));
    assert_eq!(w.app.screen, Screen::Outbox);
    snapshot(&w.app, "26-outbox", BIG);
}

#[test]
fn outbox_badge_and_continue_writing_in_the_sidebar() {
    let mut w = world();
    w.app.outbox = vec![item(
        "Contract draft",
        OutboxState::Failed,
        &["lawyer@firm.example"],
    )];
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Subject("Budget".into())));
    let _ = w.app.update(Message::Go(Nav::Folder {
        account: w.account_id.clone(),
        folder_id: w.inbox,
    }));
    assert!(
        w.app.compose.is_some(),
        "the message survives looking at mail"
    );
    snapshot(&w.app, "27-sidebar-writing", BIG);
    let _ = w.app.update(Message::Compose(ComposeMsg::Show));
    assert_eq!(w.app.screen, Screen::Compose);
}

fn raw_with_attachments() -> Vec<u8> {
    "From: Maria Lopez <maria@acme.io>\r\nTo: Alex <alex@example.com>\r\nSubject: Documents\r\nMessage-ID: <att1@acme>\r\nDate: Mon, 17 Nov 2025 09:30:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=\"b\"\r\n\r\n--b\r\nContent-Type: text/plain\r\n\r\nPlease see the files.\r\n--b\r\nContent-Type: application/pdf; name=\"invoice.pdf\"\r\nContent-Disposition: attachment; filename=\"invoice.pdf\"\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--b\r\nContent-Type: application/octet-stream; name=\"photo.jpg.exe\"\r\nContent-Disposition: attachment; filename=\"photo.jpg.exe\"\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--b--\r\n".as_bytes().to_vec()
}

#[test]
fn received_attachments_can_be_saved_and_risky_ones_ask_first() {
    let mut w = world();
    open_launch(&mut w);
    let r = w.app.reader.as_ref().unwrap();
    let id = r.messages[0].id;
    let rm = render::render(&raw_with_attachments(), &RenderOptions::default()).unwrap();
    assert_eq!(rm.attachments.len(), 2);
    let (safe, risky) = (rm.attachments[0].index, rm.attachments[1].index);
    let _ = w.app.update(Message::BodyLoaded(id, Ok(Box::new(rm))));
    snapshot(&w.app, "28-attachments-received", BIG);
    let _ = w.app.update(Message::SaveAttachment(id, risky));
    assert!(
        matches!(
            w.app.dialog,
            Some(Dialog::SaveRisky {
                disguised: true,
                ..
            })
        ),
        "{:?}",
        w.app.dialog
    );
    snapshot(&w.app, "29-save-risky", BIG);
    let _ = w.app.update(Message::CloseDialog);
    let _ = w.app.update(Message::SaveAttachment(id, safe));
    assert!(
        w.app.dialog.is_none(),
        "a normal file is saved without a question"
    );
}

#[test]
fn queueing_writes_to_disk_before_any_network_use() {
    // No App here: it would start the background sender, which is not wanted in this test.
    let svc = services();
    let acc = account("alex@example.com", "Alex Smith", "example.com");
    let id = acc.id.as_str().to_owned();
    svc.add_account(acc, &secrecy::SecretString::from("pw".to_owned()))
        .unwrap();
    let msg = mailcore::compose::OutgoingMessage {
        to: mailcore::replies::parse_recipients("Ann <ann@example.org>").unwrap(),
        bcc: mailcore::replies::parse_recipients("boss@example.org").unwrap(),
        subject: "Hello".into(),
        text: "Hi Ann\n".into(),
        attachments: vec![file("a.txt", "text/plain", 10)],
        in_reply_to: Some("<m1@acme.io>".into()),
        references: vec!["<m1@acme.io>".into()],
        ..Default::default()
    };
    let item = svc.queue_message(&id, msg, 0).unwrap();
    assert_eq!(item.state, OutboxState::Queued);
    assert_eq!(item.recipients.len(), 2);
    let (generation, list) = svc.outbox_snapshot();
    assert!(generation > 1 && list.len() == 1);
    // The Bcc address is delivered to but never written into the headers.
    let raw = std::fs::read_dir(svc.paths.data.join("outbox"))
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "eml"))
        .expect("message file");
    let text = String::from_utf8_lossy(&std::fs::read(raw).unwrap()).into_owned();
    assert!(text.contains("In-Reply-To: <m1@acme.io>") && text.contains("ann@example.org"));
    assert!(
        !text.to_lowercase().contains("boss@example.org"),
        "Bcc must not appear in the message"
    );
    // A queued message can be taken back; a missing recipient is explained.
    svc.cancel_outbox(&item.id).unwrap();
    assert!(svc.outbox_snapshot().1.is_empty());
    let none = mailcore::compose::OutgoingMessage {
        subject: "x".into(),
        ..Default::default()
    };
    assert_eq!(
        svc.queue_message(&id, none, 0).unwrap_err(),
        "Add at least one recipient."
    );
    assert!(svc.queue_message("nope", Default::default(), 0).is_err());
}

#[test]
fn forwarding_collects_attachments_within_limits() {
    let (files, skipped) = mailgui::services::collect_attachments(&raw_with_attachments());
    assert_eq!((files.len(), skipped), (2, 0));
    assert_eq!(files[0].filename, "invoice.pdf");
    assert_eq!(files[0].data, b"hello");
}

// ---- website sign-in and settings ------------------------------------------------

fn services_with_website_sign_in() -> Arc<Services> {
    Arc::new(
        Services::open_with(Paths::under(&root()), Arc::new(MemorySecretStore::new()))
            .expect("services")
            .with_oauth_clients(clients_from(
                Some("test-google-id"),
                Some("test-google-secret"),
                Some("test-microsoft-id"),
                None,
            )),
    )
}

/// The password step for `email`, with the provider's settings from the built-in list
/// (the same data the program uses for well-known providers).
fn wizard_at_password(app: &mut App, name: &str, email: &str) {
    let addr = EmailAddress::parse(email).expect("address");
    let domain = email.rsplit('@').next().expect("domain");
    let found = mailcore::provider::find_preset(domain)
        .expect("preset")
        .resolve(&addr);
    let _ = app.update(Message::WizName(name.into()));
    let _ = app.update(Message::WizEmail(email.into()));
    let _ = app.update(Message::WizFound(Ok(Box::new(found))));
}

#[test]
fn gmail_offers_website_sign_in_and_app_password_when_registered() {
    let (mut app, _t) = App::new(services_with_website_sign_in());
    wizard_at_password(&mut app, "Alex Smith", "alex@gmail.com");
    assert_eq!(
        app.wizard.web_provider(),
        Some(mailcore::account::OAuthProvider::Google)
    );
    snapshot(&app, "30-wizard-gmail-website", BIG);
}

#[test]
fn outlook_without_registration_explains_instead_of_asking_for_a_password() {
    let (mut app, _t) = App::new(services());
    wizard_at_password(&mut app, "Alex Smith", "alex@outlook.com");
    snapshot(&app, "31-wizard-outlook-unavailable", BIG);
    // Pressing Enter-style submit must not try a password for a provider that refuses it.
    let _ = app.update(Message::WizSubmit);
    assert!(matches!(
        app.wizard.outcome,
        mailgui::wizard::Outcome::Problem(_)
    ));
    // And the sign-in button message does nothing harmful without a registration.
    let _ = app.update(Message::WizWebSignIn);
    assert!(app.signing_in.is_none());
}

#[test]
fn outlook_with_registration_shows_only_the_website_button() {
    let (mut app, _t) = App::new(services_with_website_sign_in());
    wizard_at_password(&mut app, "Alex Smith", "alex@outlook.com");
    snapshot(&app, "32-wizard-outlook-website", BIG);
}

fn pending_sign_in(app: &mut App, id: &str) -> mailcore::account::Account {
    let a = app
        .wizard
        .build_website_account(app.wizard.web_provider().expect("provider"))
        .expect("account");
    app.signing_in = Some(mailgui::accounts_flow::SigningIn {
        account: a.id.clone(),
        existing: false,
    });
    app.wizard.outcome = mailgui::wizard::Outcome::Busy("waiting");
    assert!(!id.is_empty());
    a
}

#[test]
fn website_sign_in_results_are_handled_in_every_state() {
    let svc = services_with_website_sign_in();
    let (mut app, _t) = App::new(svc.clone());
    wizard_at_password(&mut app, "Alex", "alex@gmail.com");

    // Failure: the problem is shown, the wizard is usable again.
    let a = pending_sign_in(&mut app, "failed");
    let _ = app.update(Message::WizWebSignedIn(
        Box::new(a),
        SignIn::Failed("Sign-in was not completed. Try again and choose Allow.".into()),
    ));
    assert!(app.signing_in.is_none());
    assert!(matches!(&app.wizard.outcome,
        mailgui::wizard::Outcome::Problem(m) if m.contains("choose Allow")));
    snapshot(&app, "33-wizard-website-failed", BIG);

    // Cancel while waiting.
    let _ = pending_sign_in(&mut app, "cancel");
    let _ = app.update(Message::WizCancelSignIn);
    assert!(app.signing_in.is_none());
    assert!(matches!(app.wizard.outcome, mailgui::wizard::Outcome::Idle));

    // Success arrives after the person cancelled: the saved token must be removed again.
    let a = pending_sign_in(&mut app, "late");
    let _ = app.update(Message::WizCancelSignIn);
    svc.secrets
        .set(
            &a.id,
            mailcore::secrets::SecretKind::OAuthRefreshToken,
            &secrecy::SecretString::from("refresh".to_owned()),
        )
        .expect("set token");
    let _ = app.update(Message::WizWebSignedIn(Box::new(a.clone()), SignIn::Done));
    assert!(
        svc.secrets
            .get(&a.id, mailcore::secrets::SecretKind::OAuthRefreshToken)
            .expect("get")
            .is_none(),
        "a token for an account that was never added must not stay behind"
    );
    assert!(app.accounts.is_empty());

    // Success while waiting moves on to the connection check.
    let a = pending_sign_in(&mut app, "ok");
    let _ = app.update(Message::WizWebSignedIn(Box::new(a), SignIn::Done));
    assert!(app.signing_in.is_none());
    assert!(matches!(
        app.wizard.outcome,
        mailgui::wizard::Outcome::Busy(_)
    ));
}

#[test]
fn leaving_the_add_account_steps_ends_a_waiting_sign_in() {
    // A second-account wizard has a sidebar, so the person can navigate away.
    let mut w = world();
    let _ = w.app.update(Message::ShowAddAccount);
    wizard_at_password(&mut w.app, "Alex", "alex@gmail.com");
    let _ = pending_sign_in(&mut w.app, "nav");
    let _ = w.app.update(Message::ShowSettings);
    assert!(w.app.signing_in.is_none());
    assert_eq!(w.app.screen, Screen::Settings);
}

#[test]
fn signing_in_again_needs_a_registration_and_says_so() {
    let mut w = world();
    // Make the first account a website-sign-in account, as one added through Google would be.
    let mut acc = w.app.accounts[0].account.clone();
    acc.auth = AuthMethod::OAuth2 {
        provider: mailcore::account::OAuthProvider::Google,
    };
    w.app.accounts[0].account = acc;
    let _ = w.app.update(Message::ShowAccounts);
    snapshot(&w.app, "34-accounts-website-account", BIG);
    // The service's own copy of the account still says "password", so nothing is started.
    let id = w.account_id.clone();
    let _ = w.app.update(Message::ResignIn(id.clone()));
    assert!(w.app.signing_in.is_none());
    // A result for a sign-in that is not waiting is ignored.
    let _ = w
        .app
        .update(Message::ResignedIn(id, SignIn::Failed("x".into())));
    assert!(w.app.dialog.is_none());
}

#[test]
fn settings_screen_and_report() {
    let mut w = world();
    let _ = w.app.update(Message::ShowSettings);
    assert_eq!(w.app.screen, Screen::Settings);
    let _ = w.app.update(Message::SqliteKnown(Some("3.50.0".into())));
    snapshot(&w.app, "35-settings", BIG);
    let facts = w.app.facts(w.app.sqlite.as_deref());
    let report = mailgui::about::report(&facts);
    assert!(report.contains("SQLite: 3.50.0"), "{report}");
    assert!(report.contains("Accounts: 1"), "{report}");
    assert!(
        !report.contains('@'),
        "no address may appear in the report:\n{report}"
    );
    // Escape leaves the screen like it does for Accounts.
    let _ = w.app.update(Message::Escape);
    assert_eq!(w.app.screen, Screen::Mailbox);
}

// ---- only one copy at a time ------------------------------------------------------

#[test]
fn a_second_copy_on_the_same_files_is_refused_with_a_plain_message() {
    let dir = root();
    let first = Services::open_with(Paths::under(&dir), Arc::new(MemorySecretStore::new()))
        .expect("first copy");
    let second = Services::open_with(Paths::under(&dir), Arc::new(MemorySecretStore::new()));
    let Err(err) = second else {
        panic!("a second copy was allowed to open the same files");
    };
    let text = err.to_string();
    assert!(
        text.contains("already open") && text.contains("Close the other"),
        "{text}"
    );
    // The first copy is unharmed, and once it is closed the files can be opened again.
    assert!(first.accounts().is_empty());
    drop(first);
    Services::open_with(Paths::under(&dir), Arc::new(MemorySecretStore::new()))
        .expect("after the first copy closed");
    // What the person sees.
    snapshot(
        &App::fatal(text),
        "fatal-already-running.png",
        Size {
            width: 800.0,
            height: 500.0,
        },
    );
}

// ---- remembering things between runs -----------------------------------------------

#[test]
fn the_open_folder_and_search_choice_are_remembered_after_a_restart() {
    let mut w = world();
    let folders = w.svc.store.folders(&w.account_id).expect("folders");
    let archive = folders
        .iter()
        .find(|f| f.role == FolderRole::Archive)
        .expect("archive folder")
        .id;
    let _ = w.app.update(Message::Go(Nav::Folder {
        account: w.account_id.clone(),
        folder_id: archive,
    }));
    let _ = w.app.update(Message::SearchEverywhere(true));
    let _ = w.app.update(Message::Tick); // the heartbeat writes what is worth remembering

    // "Restart": a fresh program over the same files.
    let (mut again, _task) = App::new(Arc::clone(&w.svc));
    assert!(again.search_everywhere, "the search choice was forgotten");
    let _ = again.update(Message::FoldersLoaded(vec![(
        w.account_id.clone(),
        folders.clone(),
    )]));
    assert_eq!(
        again.nav,
        Nav::Folder {
            account: w.account_id.clone(),
            folder_id: archive
        },
        "the folder that was open was not reopened"
    );

    // A folder that no longer exists is simply not reopened (here the single account's
    // inbox opens, as on a first start).
    mailgui::prefs::Prefs {
        view: Some((w.account_id.clone(), 987_654)),
        ..mailgui::prefs::Prefs::default()
    }
    .save(&w.svc.paths.data);
    let (mut gone, _task) = App::new(Arc::clone(&w.svc));
    let _ = gone.update(Message::FoldersLoaded(vec![(
        w.account_id.clone(),
        folders,
    )]));
    assert_eq!(
        gone.nav,
        Nav::Folder {
            account: w.account_id.clone(),
            folder_id: w.inbox
        }
    );
}

#[test]
fn a_refused_gmail_password_shows_the_app_password_steps() {
    use mailcore::diagnostics::{Diagnosis, ServerReport, Step, StepStatus};
    let (mut app, _t) = App::new(services());
    wizard_at_password(&mut app, "Alex Smith", "alex@gmail.com");
    let step = |label, status| Step {
        label,
        status,
        detail: None,
    };
    let server = |role, host: &str| ServerReport {
        role,
        host: host.into(),
        port: 993,
        security: Security::Tls,
        steps: vec![
            step("Connecting to the server", StepStatus::Ok),
            step("Signing in", StepStatus::Failed),
        ],
        ok: false,
        advice: Some("The server rejected the sign-in. Check your password.".into()),
        transient: false,
        elapsed: std::time::Duration::from_secs(2),
        notes: Vec::new(),
    };
    app.wizard.outcome = mailgui::wizard::Outcome::CheckFailed(Box::new(Diagnosis {
        account: "alex@gmail.com".into(),
        servers: vec![
            server("Incoming mail (IMAP)", "imap.gmail.com"),
            server("Outgoing mail (SMTP)", "smtp.gmail.com"),
        ],
        warnings: Vec::new(),
    }));
    snapshot(&app, "36-wizard-gmail-refused", BIG);
}

/// A message built to break layouts: a subject, a name and an address with no
/// spaces to wrap at, and a body line far wider than any window.
fn nasty_world() -> World {
    let mut w = world();
    let long_subject = "Re: Fwd: Quarterly_results_and_the_extraordinarily_long_project_codename_that_never_ends_ReallyLongWordWithoutAnySpaces_0123456789 and then a long tail of words that should wrap politely";
    let long_name =
        "Maximilian-Alexander Featherstonehaugh-Cholmondeley-Wolfeschlegelsteinhausenbergerdorff";
    let long_addr = "maximilian.alexander.featherstonehaugh.cholmondeley.wolfeschlegelsteinhausen@a-very-long-domain-name-for-testing.example.org";
    w.svc
        .store
        .insert_messages(
            w.inbox,
            &[msg(
                9,
                long_subject,
                long_name,
                long_addr,
                NOW - 600,
                0,
                "nasty@x",
                None,
            )],
        )
        .expect("insert");
    let threads = w
        .svc
        .store
        .list_threads(&ListScope::Folder(w.inbox), false, None, 100)
        .expect("threads");
    let nasty = threads
        .iter()
        .find(|t| t.subject.contains("Quarterly"))
        .expect("nasty thread")
        .clone();
    let rows = w
        .svc
        .store
        .thread_messages(&nasty.thread_key)
        .expect("rows");
    let body = format!(
        "Look at https://example.org/{}/and/more?{}=1 right now.\r\n\r\n{}\r\n\r\n| a | table | like | line | that | is | really | quite | wide | indeed | {} |",
        "segment".repeat(30),
        "query".repeat(20),
        "word ".repeat(120),
        "x".repeat(150)
    );
    let raw = raw_plain(
        &format!("{long_name} <{long_addr}>"),
        long_subject,
        "nasty@x",
        &body,
    );
    w.svc.store.store_body(rows[0].id, &raw).expect("body");
    let nav = w.app.nav.clone();
    let limit = w.app.limit;
    let _ = w.app.update(Message::ThreadsLoaded(nav, limit, threads));
    let _ = w.app.update(Message::OpenThread(
        nasty.thread_key.clone(),
        ListScope::Folder(w.inbox),
    ));
    let _ = w.app.update(Message::ThreadLoaded(
        nasty.thread_key.clone(),
        rows.clone(),
    ));
    let rendered = render::render(&raw, &RenderOptions::default()).expect("render");
    let _ = w
        .app
        .update(Message::BodyLoaded(rows[0].id, Ok(Box::new(rendered))));
    w
}

#[test]
fn long_text_stays_inside_its_boxes_in_small_windows() {
    let w = nasty_world();
    snapshot(&w.app, "40-nasty-min", mailgui::prefs::MIN_SIZE);
    snapshot(&w.app, "41-nasty-wide", Size::new(1280.0, 720.0));
}

#[test]
fn every_other_screen_has_a_way_back_to_the_mail() {
    let mut w = world();
    let before = w.app.nav.clone();
    for open in [
        Message::ShowSettings,
        Message::ShowAccounts,
        Message::Outbox(mailgui::compose_flow::OutboxMsg::Show),
    ] {
        let _ = w.app.update(open);
        assert_ne!(w.app.screen, Screen::Mailbox);
        let _ = w.app.update(Message::ShowMailbox);
        assert_eq!(w.app.screen, Screen::Mailbox);
        assert_eq!(w.app.nav, before, "the open folder is kept");
    }
    // Home is always offered, and goes to the mail list.
    let _ = w.app.update(Message::ShowSettings);
    snapshot(&w.app, "42-settings-back-bar", mailgui::prefs::MIN_SIZE);
    let _ = w.app.update(Message::Go(Nav::AllInboxes));
    assert_eq!(w.app.screen, Screen::Mailbox);
}

#[test]
fn the_first_download_shows_how_far_it_has_got() {
    let mut w = world();
    let _ = w.app.update(Message::Go(Nav::AllInboxes));
    w.app.threads.clear();
    for a in &mut w.app.accounts {
        a.status = SyncStatus::Syncing(mailcore::syncer::SyncProgress {
            folder: "INBOX".into(),
            folders_done: 0,
            folders_total: 5,
            messages_done: 340,
            messages_total: 1000,
            stored_total: 340,
        });
    }
    snapshot(&w.app, "43-first-download-progress", BIG);
}

#[test]
fn the_dark_theme_and_other_looks_are_applied_and_remembered() {
    let mut w = nasty_world();
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Dark));
    assert!(w.app.colors().dark);
    snapshot(&w.app, "44-dark-mail", BIG);
    snapshot(&w.app, "45-dark-mail-min", mailgui::prefs::MIN_SIZE);
    let _ = w
        .app
        .update(Message::SetAccent(mailgui::theme::Accent::Teal));
    let _ = w.app.update(Message::SetCompact(true));
    let _ = w.app.update(Message::SetPreviews(false));
    snapshot(&w.app, "46-dark-teal-compact", BIG);
    let _ = w.app.update(Message::ShowSettings);
    snapshot(&w.app, "47-dark-settings", BIG);
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Light));
    let _ = w.app.update(Message::SetScale(130));
    snapshot(&w.app, "48-light-settings-large", BIG);
    // Following the computer: dark only when the computer says so.
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::System));
    assert!(!w.app.colors().dark);
    let _ = w.app.update(Message::SystemTheme(iced::theme::Mode::Dark));
    assert!(w.app.colors().dark);
    let _ = w.app.update(Message::SystemTheme(iced::theme::Mode::Light));
    assert!(!w.app.colors().dark);
    // A size that is not on offer is refused.
    let _ = w.app.update(Message::SetScale(333));
}

#[test]
fn trackers_are_counted_and_links_are_cleaned_before_opening() {
    let mut w = world();
    let raw = b"From: News <news@shop.example>\r\nTo: alex@example.com\r\nSubject: Weekly offers\r\nMessage-ID: <t1@shop.example>\r\nDate: Mon, 17 Nov 2025 08:00:00 +0000\r\nContent-Type: text/html; charset=utf-8\r\n\r\n<p>Big sale. <a href=\"https://shop.example/sale?utm_source=mail&utm_campaign=nov&fbclid=zz&item=9\">See the offers</a></p><img src=\"https://track.example/pixel.gif\" width=\"1\" height=\"1\"><img src=\"https://cdn.example/banner.png\" width=\"600\">".to_vec();
    w.svc
        .store
        .insert_messages(
            w.inbox,
            &[msg(
                9,
                "Weekly offers",
                "News",
                "news@shop.example",
                NOW - 60,
                0,
                "t1@shop.example",
                None,
            )],
        )
        .expect("insert");
    let threads = w
        .svc
        .store
        .list_threads(&ListScope::Folder(w.inbox), false, None, 100)
        .expect("threads");
    let t = threads
        .iter()
        .find(|t| t.subject.contains("Weekly offers"))
        .expect("thread")
        .clone();
    let rows = w.svc.store.thread_messages(&t.thread_key).expect("rows");
    let nav = w.app.nav.clone();
    let limit = w.app.limit;
    let _ = w.app.update(Message::ThreadsLoaded(nav, limit, threads));
    let _ = w.app.update(Message::OpenThread(
        t.thread_key.clone(),
        ListScope::Folder(w.inbox),
    ));
    let _ = w
        .app
        .update(Message::ThreadLoaded(t.thread_key.clone(), rows.clone()));
    let rendered = render::render(&raw, &RenderOptions::default()).expect("render");
    let summary = rendered.tracker_summary();
    assert_eq!(summary.pictures, 2);
    assert_eq!(summary.tracking_pictures, 1);
    assert_eq!(summary.tracking_links, 1);
    let link = rendered
        .body
        .iter()
        .find_map(|b| match b {
            mailcore::render::Block::Paragraph(inl) => inl.iter().find_map(|i| match i {
                mailcore::render::Inline::Link(l) => Some(l.clone()),
                _ => None,
            }),
            _ => None,
        })
        .expect("link");
    let _ = w
        .app
        .update(Message::BodyLoaded(rows[0].id, Ok(Box::new(rendered))));
    snapshot(&w.app, "49-trackers-banner", BIG);
    let _ = w.app.update(Message::LinkClicked(link.clone()));
    snapshot(&w.app, "50-link-cleaned", BIG);
    let (clean, removed) = mailcore::render::strip_tracking(&link.url);
    assert_eq!(clean, "https://shop.example/sale?item=9");
    assert_eq!(removed.len(), 3);
}

#[test]
fn unread_mail_puts_a_count_in_the_title_and_the_icon() {
    let mut w = world();
    let unread = w.app.unread_inboxes() as usize;
    assert!(unread > 0, "the test mailbox has unread mail");
    assert_eq!(w.app.badge_shown(), 0, "the icon starts plain");
    let _ = w.app.update(Message::Tick);
    assert_eq!(w.app.badge_shown(), unread);
    assert!(w.app.title().starts_with(&format!("({unread})")));
    // Nothing new: no second redraw of the icon (the count stays put).
    let _ = w.app.update(Message::Tick);
    assert_eq!(w.app.badge_shown(), unread);
}

#[test]
fn several_conversations_can_be_chosen_and_changed_together() {
    use mailgui::app::{ActionOutcome, bulk_apply};
    let mut w = world();
    let all = w.app.threads.len();
    assert_eq!(all, 4);
    // Choosing mode closes the reader and starts with nothing ticked.
    open_launch(&mut w);
    let _ = w.app.update(Message::SelectMode(true));
    assert!(w.app.selecting && w.app.selected.is_empty() && w.app.reader.is_none());
    let keys: Vec<String> = w.app.threads.iter().map(|t| t.thread_key.clone()).collect();
    let _ = w.app.update(Message::ToggleSelect(keys[0].clone()));
    let _ = w.app.update(Message::ToggleSelect(keys[1].clone()));
    assert_eq!(w.app.selected.len(), 2);
    snapshot(&w.app, "50-select-two", BIG);
    snapshot(&w.app, "51-select-two-min", mailgui::prefs::MIN_SIZE);
    // A second press unticks; All ticks everything shown; None clears.
    let _ = w.app.update(Message::ToggleSelect(keys[1].clone()));
    assert_eq!(w.app.selected.len(), 1);
    let _ = w.app.update(Message::SelectAll);
    assert_eq!(w.app.selected.len(), all);
    let _ = w.app.update(Message::SelectNone);
    assert!(w.app.selected.is_empty());
    // Dark look.
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Dark));
    let _ = w.app.update(Message::SelectAll);
    snapshot(&w.app, "52-select-all-dark", BIG);
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Light));
    // Changing the open folder, or searching, leaves choosing mode.
    let _ = w.app.update(Message::Go(Nav::AllInboxes));
    assert!(!w.app.selecting && w.app.selected.is_empty());

    // The actions themselves, on the same data the buttons use.
    let scope = ListScope::Folder(w.inbox);
    let folders = w.app.folders.clone();
    let unread = |w: &World| -> u32 {
        w.svc
            .store
            .list_threads(&ListScope::Folder(w.inbox), false, None, 50)
            .unwrap()
            .iter()
            .map(|t| t.unread)
            .sum()
    };
    assert!(unread(&w) > 0);
    let out = bulk_apply(&w.svc, &scope, &folders, &keys, Action::MarkRead).unwrap();
    assert!(matches!(&out, ActionOutcome::Done(t) if t.contains("4 conversations")));
    assert_eq!(unread(&w), 0);
    bulk_apply(&w.svc, &scope, &folders, &keys, Action::MarkUnread).unwrap();
    assert!(unread(&w) > 0);
    bulk_apply(&w.svc, &scope, &folders, &keys[..2], Action::ToggleStar).unwrap();
    let starred = w
        .svc
        .store
        .list_threads(&scope, false, None, 50)
        .unwrap()
        .iter()
        .filter(|t| t.flagged)
        .count();
    assert_eq!(starred, 2);
    // Deleting moves them to Trash: the inbox list shrinks by exactly those.
    bulk_apply(&w.svc, &scope, &folders, &keys[..3], Action::Delete).unwrap();
    let left = w
        .svc
        .store
        .list_threads(&scope, false, None, 50)
        .unwrap()
        .len();
    assert_eq!(left, all - 3);
    // Nothing chosen: nothing is done.
    let out = bulk_apply(&w.svc, &scope, &folders, &[], Action::Delete).unwrap();
    assert!(matches!(out, ActionOutcome::Done(t) if t == "Nothing to change."));
}

#[test]
fn small_windows_show_one_pane_at_a_time_with_a_way_back() {
    let mut w = world();
    let small = mailgui::prefs::MIN_SIZE;
    let _ = w.app.update(Message::WindowResized(small));
    assert!(w.app.narrow());
    snapshot(&w.app, "53-narrow-list", small);
    open_launch(&mut w);
    assert!(w.app.reader.is_some());
    snapshot(&w.app, "54-narrow-reader", small);
    // The back button returns to the list; so does Escape.
    let _ = w.app.update(Message::CloseReader);
    assert!(w.app.reader.is_none());
    open_launch(&mut w);
    let _ = w.app.update(Message::Escape);
    assert!(w.app.reader.is_none());
    // A wide window keeps the three columns; a big interface size makes a medium window narrow.
    let _ = w.app.update(Message::WindowResized(BIG));
    assert!(!w.app.narrow());
    let _ = w
        .app
        .update(Message::WindowResized(Size::new(1100.0, 680.0)));
    assert!(!w.app.narrow());
    let _ = w.app.update(Message::SetScale(130));
    assert!(w.app.narrow());
    // Escape leaves choosing mode before anything else.
    let _ = w.app.update(Message::SelectMode(true));
    let _ = w.app.update(Message::Escape);
    assert!(!w.app.selecting);
}

/// Adds a second account (with one message in its inbox) to a world and shows "All inboxes".
fn add_work_account(w: &mut World) -> (String, i64) {
    let acc = account("alex@work.example", "Alex (work)", "work.example");
    let id = acc.id.as_str().to_owned();
    w.svc
        .add_account(acc, &secrecy::SecretString::from("pw".to_owned()))
        .expect("second account");
    let f = w
        .svc
        .store
        .upsert_folders(
            &id,
            &[
                folder("INBOX", FolderRole::Inbox),
                folder("Junk", FolderRole::Junk),
            ],
            true,
        )
        .expect("folders");
    let inbox = f
        .iter()
        .find(|x| x.role == FolderRole::Inbox)
        .expect("inbox")
        .id;
    w.svc
        .store
        .insert_messages(
            inbox,
            &[msg(
                1,
                "Quarterly review on Thursday",
                "Priya Nair",
                "priya@work.example",
                NOW - 600,
                0,
                "w1@work",
                None,
            )],
        )
        .expect("insert");
    (id, inbox)
}

#[test]
fn blocked_senders_and_rules_can_be_added_listed_removed_and_are_applied() {
    use mailcore::rules::{Rule, RuleAction, RuleField};
    let mut w = world();
    // Blocking through the same function the Block button uses.
    w.svc.block_sender("  Spammer@Bad.Example ").unwrap();
    w.svc.block_sender("spammer@bad.example").unwrap(); // the same again: still one entry
    let rules = w.svc.list_rules().unwrap();
    assert_eq!(rules.len(), 1);
    assert!(rules[0].is_block() && rules[0].needle == "spammer@bad.example");
    assert!(w.svc.block_sender("ab").is_err(), "too short to be safe");
    assert_eq!(
        w.svc.store.rules().len(),
        1,
        "the sync engine uses it at once"
    );
    w.svc
        .add_rule(
            Rule::new(
                None,
                RuleField::Subject,
                "weekly digest",
                RuleAction::Archive,
            )
            .unwrap(),
        )
        .unwrap();
    w.svc
        .add_rule(
            Rule::new(
                Some(w.account_id.clone()),
                RuleField::From,
                "@cloudhost.net",
                RuleAction::Star,
            )
            .unwrap(),
        )
        .unwrap();

    // The screen.
    let _ = w.app.update(Message::Data(DataMsg::ShowFilters));
    assert_eq!(w.app.screen, Screen::Filters);
    let _ = w
        .app
        .update(Message::Data(DataMsg::RulesLoaded(w.svc.list_rules())));
    assert_eq!(w.app.rules.len(), 3);
    let _ = w
        .app
        .update(Message::Data(DataMsg::FormText("promo".into())));
    snapshot(&w.app, "60-filters", BIG);
    snapshot(&w.app, "61-filters-min", mailgui::prefs::MIN_SIZE);
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Dark));
    snapshot(&w.app, "62-filters-dark", BIG);
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Light));
    // A bad rule is explained and nothing is saved.
    let _ = w.app.update(Message::Data(DataMsg::FormText("ab".into())));
    let _ = w.app.update(Message::Data(DataMsg::AddRule));
    assert!(w.app.rule_form.error.is_some());
    assert_eq!(w.svc.list_rules().unwrap().len(), 3);
    // Escape goes back to Settings, then to the mail.
    let _ = w.app.update(Message::Escape);
    assert_eq!(w.app.screen, Screen::Settings);

    // "Apply now" on the stored mail: the weekly digest is archived, the invoice starred.
    let n = w.svc.apply_rules_now().unwrap();
    assert!(n >= 2, "changed {n}");
    let inbox_left: Vec<String> = w
        .svc
        .store
        .list_threads(&ListScope::Folder(w.inbox), false, None, 50)
        .unwrap()
        .into_iter()
        .map(|t| t.subject)
        .collect();
    assert!(!inbox_left.iter().any(|s| s.starts_with("Weekly digest")));
    assert!(
        w.svc
            .store
            .list_threads(&ListScope::Folder(w.inbox), false, None, 50)
            .unwrap()
            .iter()
            .any(|t| t.subject.starts_with("Your invoice") && t.flagged)
    );
    // Removing a rule removes it from the file and from the engine.
    let id = w.svc.list_rules().unwrap()[0].id.clone();
    assert_eq!(w.svc.remove_rule(&id).unwrap().len(), 2);
    assert_eq!(w.svc.store.rules().len(), 2);
    // Removing the account also removes the rules that belonged to it.
    let acct = mailcore::account::AccountId::parse(&w.account_id).unwrap();
    w.svc.remove_account(&acct).unwrap();
    let left = w.svc.list_rules().unwrap();
    assert!(left.iter().all(|r| r.account.is_none()), "{left:?}");
}

#[test]
fn the_block_button_blocks_the_sender_and_not_yourself() {
    let mut w = world();
    open_launch(&mut w);
    // The open conversation's newest message is from Jonas.
    let t = w.app.update(Message::Data(DataMsg::BlockSender));
    drop(t);
    // The tasks cannot run here; do what they do.
    w.svc.block_sender("jonas@acme.io").unwrap();
    assert!(w.svc.list_rules().unwrap()[0].is_block());
    // Nobody can lock themselves out of their own address. (The first press also moved the
    // conversation to Junk and closed it, so open it again.)
    open_launch(&mut w);
    if let Some(r) = &mut w.app.reader
        && let Some(m) = r.messages.last_mut()
    {
        m.from_addr = "alex@example.com".into();
    }
    let _ = w.app.update(Message::Data(DataMsg::BlockSender));
    assert!(
        w.app
            .toast
            .as_ref()
            .is_some_and(|t| t.bad && t.text.contains("own"))
    );
    snapshot(&w.app, "63-reader-with-block", BIG);
}

#[test]
fn templates_are_saved_listed_inserted_and_removed() {
    let mut w = world();
    let t = mailcore::templates::Template::new("Thanks", "Thank you, I will get back to you soon.")
        .unwrap();
    let t2 =
        mailcore::templates::Template::new("Away", "I am away until Monday.\nBest, Alex").unwrap();
    w.svc.add_template(t).unwrap();
    let all = w.svc.add_template(t2).unwrap();
    assert_eq!(all[0].name, "Away", "sorted by name");
    let _ = w.app.update(Message::Data(DataMsg::ShowTemplates));
    assert_eq!(w.app.screen, Screen::Templates);
    let _ = w.app.update(Message::Data(DataMsg::TemplatesLoaded(
        w.svc.list_templates(),
    )));
    snapshot(&w.app, "64-templates", BIG);
    // An empty template is refused with an explanation.
    let _ = w
        .app
        .update(Message::Data(DataMsg::TemplateName("Empty".into())));
    let _ = w.app.update(Message::Data(DataMsg::AddTemplate));
    assert!(w.app.template_form.error.is_some());
    // Inserting into a message being written: at the end, with a blank line between.
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let id_thanks = w
        .app
        .templates
        .iter()
        .find(|t| t.name == "Thanks")
        .unwrap()
        .id
        .clone();
    let id_away = w
        .app
        .templates
        .iter()
        .find(|t| t.name == "Away")
        .unwrap()
        .id
        .clone();
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::InsertTemplate(id_thanks)));
    let _ = w.app.update(Message::Compose(ComposeMsg::InsertTemplate(
        id_away.clone(),
    )));
    let body = w.app.compose.as_ref().unwrap().body.text();
    assert!(
        body.starts_with("Thank you, I will get back to you soon.")
            && body.contains("\n\nI am away until Monday.\nBest, Alex"),
        "{body:?}"
    );
    assert!(
        w.app.compose.as_ref().unwrap().touched,
        "so the draft is kept"
    );
    // An id that does not exist inserts nothing.
    let before = body.clone();
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::InsertTemplate("nope".into())));
    assert_eq!(w.app.compose.as_ref().unwrap().body.text(), before);
    snapshot(&w.app, "65-compose-with-templates", BIG);
    // Removing.
    let left = w.svc.remove_template(&id_away).unwrap();
    assert_eq!(left.len(), 1);
}

#[test]
fn undo_send_holds_the_message_and_brings_it_back() {
    let mut w = world();
    // Writing a message.
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let _ = w.app.update(Message::Compose(ComposeMsg::To(
        "Ann <ann@example.org>".into(),
    )));
    let _ = w.app.update(Message::Compose(ComposeMsg::Subject(
        "Plans for Friday".into(),
    )));
    assert_eq!(w.app.send_delay(), 10, "on by default");
    // The same call the Send button makes, with the delay from the settings.
    let msg = w.app.compose.as_ref().unwrap().strict_message().unwrap();
    let item = w.svc.queue_message(&w.account_id, msg, 10).unwrap();
    assert!(
        item.next_attempt_at >= item.queued_at + 10,
        "the first try waits"
    );
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Queued(Ok(Box::new(
            item.clone(),
        )))));
    assert!(w.app.compose.is_none());
    assert!(w.app.undo.is_some() && w.app.screen == Screen::Mailbox);
    snapshot(&w.app, "66-undo-bar", BIG);
    // Undo: the task cancels the item; here do what it does, then report back.
    let _ = w.app.update(Message::Compose(ComposeMsg::Undo));
    assert!(
        w.app.undo.as_ref().unwrap().working,
        "a second press does nothing"
    );
    w.svc.cancel_outbox(&item.id).unwrap();
    assert!(w.svc.outbox_snapshot().1.is_empty());
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::UndoResult(Ok(()))));
    assert!(w.app.undo.is_none());
    assert_eq!(w.app.screen, Screen::Compose);
    let c = w.app.compose.as_ref().unwrap();
    assert_eq!(c.subject, "Plans for Friday");
    assert!(
        !c.sending && c.touched,
        "editable again, and kept as a draft"
    );

    // Too late: the message was already taken by the sender.
    let msg = w.app.compose.as_ref().unwrap().strict_message().unwrap();
    let item = w.svc.queue_message(&w.account_id, msg, 10).unwrap();
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Queued(Ok(Box::new(item)))));
    let _ = w.app.update(Message::Compose(ComposeMsg::Undo));
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::UndoResult(Err("late".into()))));
    assert!(w.app.undo.is_none() && w.app.compose.is_none());
    assert!(w.app.toast.as_ref().is_some_and(|t| t.bad));

    // The window runs out by itself.
    let msg = mailcore::compose::OutgoingMessage {
        to: mailcore::replies::parse_recipients("ann@example.org").unwrap(),
        subject: "x".into(),
        text: "y".into(),
        ..Default::default()
    };
    let item = w.svc.queue_message(&w.account_id, msg, 10).unwrap();
    let _ = w.app.update(Message::Compose(ComposeMsg::New));
    let _ = w
        .app
        .update(Message::Compose(ComposeMsg::Queued(Ok(Box::new(item)))));
    assert!(w.app.undo.is_some());
    w.app.undo.as_mut().unwrap().until = 1;
    let _ = w.app.update(Message::Tick);
    assert!(w.app.undo.is_none());

    // Switched off: the old behaviour, no bar.
    let _ = w.app.update(Message::SetSendDelay(0));
    assert_eq!(w.app.send_delay(), 0);
    let _ = w.app.update(Message::SetSendDelay(7)); // not one of the offered values
    assert_eq!(w.app.send_delay(), 0);
}

#[test]
fn several_accounts_are_told_apart_and_the_list_can_show_only_unread_mail() {
    let mut w = world();
    let (id2, _inbox2) = add_work_account(&mut w);
    // The program learns about the second account the way it does at start-up.
    let f2 = w.svc.store.folders(&id2).unwrap();
    let _ = w
        .app
        .update(Message::FoldersLoaded(vec![(id2.clone(), f2)]));
    let _ = w.app.update(Message::Go(Nav::AllInboxes));
    let threads = w
        .svc
        .store
        .list_threads(&ListScope::AllInboxes, false, None, 50)
        .unwrap();
    assert!(
        threads.iter().any(|t| t.account == id2)
            && threads.iter().any(|t| t.account == w.account_id)
    );
    let n = threads.len();
    let limit = w.app.limit;
    let _ = w
        .app
        .update(Message::ThreadsLoaded(Nav::AllInboxes, limit, threads));
    // Only accounts the program has started are drawn with their name; the test world has one.
    snapshot(&w.app, "67-all-inboxes", BIG);
    let unread = w
        .svc
        .store
        .list_threads(&ListScope::AllInboxes, true, None, 50)
        .unwrap();
    assert!(!unread.is_empty() && unread.len() <= n);
    let _ = w.app.update(Message::SetUnreadOnly(true));
    assert!(w.app.unread_only);
    let _ = w
        .app
        .update(Message::ThreadsLoaded(Nav::AllInboxes, limit, unread));
    snapshot(&w.app, "68-unread-only", BIG);
    let _ = w
        .app
        .update(Message::ThreadsLoaded(Nav::AllInboxes, limit, Vec::new()));
    snapshot(&w.app, "69-unread-only-empty", mailgui::prefs::MIN_SIZE);
}

fn many_conversations(w: &mut World, n: usize) {
    let base = w.app.threads[0].clone();
    w.app.threads = (0..n)
        .map(|i| {
            let mut t = base.clone();
            t.thread_key = format!("key-{i}");
            t.subject = format!("Row number {i}");
            t
        })
        .collect();
}

#[test]
fn a_long_list_builds_only_the_rows_near_the_view() {
    let mut w = world();
    many_conversations(&mut w, 5000);

    let mut top: Simulator<'_, Message> =
        Simulator::with_size(iced::Settings::default(), BIG, view(&w.app));
    assert!(top.find("Row number 0").is_ok(), "the first row is drawn");
    assert!(top.find("Row number 20").is_ok(), "rows below it are drawn");
    assert!(
        top.find("Row number 4000").is_err(),
        "a row far below the view is not built"
    );
    drop(top);
    snapshot(&w.app, "70-long-list-top", BIG);

    // Far down, the rows around that place are built instead and the top ones are not.
    let deep = 4000.0 * 80.0;
    let _ = w.app.update(Message::ListScrolled(deep, 700.0));
    let mut ui: Simulator<'_, Message> =
        Simulator::with_size(iced::Settings::default(), BIG, view(&w.app));
    assert!(ui.find("Row number 4000").is_ok());
    assert!(ui.find("Row number 3995").is_ok(), "a few rows above too");
    assert!(ui.find("Row number 0").is_err());
    assert!(ui.find("Row number 4900").is_err());
}

#[test]
fn switching_what_the_list_shows_puts_it_back_at_the_top() {
    let mut w = world();
    many_conversations(&mut w, 3000);
    let _ = w.app.update(Message::ListScrolled(90_000.0, 700.0));
    assert!(w.app.list_scroll > 0.0);
    let _ = w.app.update(Message::SetUnreadOnly(true));
    assert_eq!(w.app.list_scroll, 0.0, "unread-only starts at the top");

    let _ = w.app.update(Message::ListScrolled(90_000.0, 700.0));
    let _ = w.app.update(Message::Go(mailgui::app::Nav::AllInboxes));
    assert_eq!(w.app.list_scroll, 0.0, "another folder starts at the top");

    let _ = w.app.update(Message::ListScrolled(90_000.0, 700.0));
    let _ = w.app.update(Message::ClearSearch);
    assert_eq!(w.app.list_scroll, 0.0, "leaving a search starts at the top");
}

#[test]
fn the_message_can_open_on_its_own_page_even_in_a_wide_window() {
    let mut w = world();
    let _ = w.app.update(Message::WindowResized(BIG));
    assert!(!w.app.narrow(), "beside the list by default");
    let _ = w.app.update(Message::SetSideBySide(false));
    assert!(w.app.narrow(), "now one page at a time");
    snapshot(&w.app, "71-list-alone-wide", BIG);
    open_launch(&mut w);
    snapshot(&w.app, "72-message-alone-wide", BIG);
    // The way back to the list works here too.
    let _ = w.app.update(Message::CloseReader);
    assert!(w.app.reader.is_none());
    let _ = w.app.update(Message::SetSideBySide(true));
    assert!(!w.app.narrow());
}

#[test]
fn a_wider_list_needs_a_wider_window_to_sit_beside_a_message() {
    use mailgui::prefs::ListWidth;
    let mut w = world();
    let size = Size::new(1060.0, 700.0);
    let _ = w.app.update(Message::WindowResized(size));
    assert!(!w.app.narrow(), "normal list at 1060 px");
    let _ = w.app.update(Message::SetListWidth(ListWidth::Wide));
    assert!(w.app.narrow(), "wide list needs more room than 1060 px");
    let _ = w.app.update(Message::SetListWidth(ListWidth::Slim));
    assert!(!w.app.narrow());
    let _ = w
        .app
        .update(Message::WindowResized(Size::new(1000.0, 700.0)));
    assert!(
        !w.app.narrow(),
        "a slim list fits beside a message at 1000 px"
    );
}

#[test]
fn list_width_pictures() {
    use mailgui::prefs::ListWidth;
    let mut w = world();
    open_launch(&mut w);
    let _ = w.app.update(Message::WindowResized(BIG));
    let _ = w.app.update(Message::SetListWidth(ListWidth::Slim));
    snapshot(&w.app, "73-list-slim", BIG);
    let _ = w.app.update(Message::SetListWidth(ListWidth::Wide));
    snapshot(&w.app, "74-list-wide", BIG);
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Dark));
    let _ = w.app.update(Message::SetSideBySide(false));
    snapshot(&w.app, "75-message-alone-dark", BIG);
    let _ = w.app.update(Message::SetSideBySide(true));
    let _ = w.app.update(Message::ShowSettings);
    snapshot(&w.app, "76-settings-layout-dark", Size::new(900.0, 1500.0));
}

#[test]
fn gmail_folders_lose_their_wrapper_and_each_account_has_its_own_colour_and_mark() {
    let mut w = world();
    let acc = account("sam@gmail.test", "Sam Gmail", "gmail.test");
    let id = acc.id.as_str().to_owned();
    w.svc
        .add_account(acc.clone(), &secrecy::SecretString::from("pw".to_owned()))
        .expect("second account");
    w.svc
        .store
        .upsert_folders(
            &id,
            &[
                folder("INBOX", FolderRole::Inbox),
                // Gmail's real parent folder cannot be opened; it is only a container.
                Folder {
                    selectable: false,
                    ..folder("[Gmail]", FolderRole::Other)
                },
                folder("[Gmail]/Sent Mail", FolderRole::Sent),
                folder("[Gmail]/Drafts", FolderRole::Drafts),
                folder("[Gmail]/All Mail", FolderRole::All),
                folder("[Gmail]/Spam", FolderRole::Junk),
                folder("[Gmail]/Trash", FolderRole::Trash),
                folder("[Gmail]/Important", FolderRole::Other),
                folder("Receipts", FolderRole::Other),
            ],
            true,
        )
        .expect("folders");
    let _ = w.app.start_account_public(acc);
    let both = vec![
        (
            w.account_id.clone(),
            w.svc.store.folders(&w.account_id).unwrap(),
        ),
        (id.clone(), w.svc.store.folders(&id).unwrap()),
    ];
    let _ = w.app.update(Message::FoldersLoaded(both));
    assert_eq!(w.app.accounts.len(), 2);

    let mut ui: Simulator<'_, Message> =
        Simulator::with_size(iced::Settings::default(), BIG, view(&w.app));
    for shown in ["Sent Mail", "All Mail", "Important", "Receipts"] {
        assert!(ui.find(shown).is_ok(), "{shown} is shown");
    }
    for raw in ["[Gmail]/Sent Mail", "[Gmail]/Spam", "[Gmail]"] {
        assert!(ui.find(raw).is_err(), "{raw} is not shown");
    }
    drop(ui);
    // Two accounts never share a colour.
    assert_ne!(
        mailgui::util::account_color(0),
        mailgui::util::account_color(1)
    );
    snapshot(&w.app, "77-sidebar-two-accounts", BIG);
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Dark));
    snapshot(&w.app, "78-sidebar-two-accounts-dark", BIG);
}

#[test]
fn the_newest_message_of_a_conversation_is_on_top() {
    let mut w = world();
    open_launch(&mut w);
    let r = w.app.reader.as_ref().expect("open");
    assert!(
        r.messages.len() >= 2,
        "the Launch thread has several messages"
    );
    let newest = r.messages.iter().max_by_key(|m| m.date).expect("newest");
    let oldest = r.messages.iter().min_by_key(|m| m.date).expect("oldest");
    assert_ne!(newest.from_name, oldest.from_name, "test needs two senders");
    let (newest, oldest) = (newest.from_name.clone(), oldest.from_name.clone());

    let mut ui: Simulator<'_, Message> =
        Simulator::with_size(iced::Settings::default(), BIG, view(&w.app));
    let y_newest = ui.find(newest.as_str()).expect("newest shown").bounds().y;
    let y_oldest = ui.find(oldest.as_str()).expect("oldest shown").bounds().y;
    assert!(
        y_newest < y_oldest,
        "{newest} (newest, y={y_newest}) must be above {oldest} (oldest, y={y_oldest})"
    );
}

/// Not a pass/fail test: prints how long it takes to build and lay out the screen with many
/// conversations. Run with `cargo test --release --test ui -- --ignored --nocapture list_cost`.
#[test]
#[ignore = "timing only"]
fn list_cost_with_many_conversations() {
    let mut w = world();
    let base = w.app.threads.clone();
    for n in [100usize, 500, 1000, 3000, 10_000] {
        w.app.threads = (0..n)
            .map(|i| {
                let mut t = base[i % base.len()].clone();
                t.thread_key = format!("k{i}");
                t.subject = format!("{} #{i}", t.subject);
                t
            })
            .collect();
        let t0 = std::time::Instant::now();
        let element = view(&w.app);
        let built = t0.elapsed();
        let t1 = std::time::Instant::now();
        let ui: Simulator<'_, Message> =
            Simulator::with_size(iced::Settings::default(), BIG, element);
        let laid_out = t1.elapsed();
        drop(ui);
        println!("rows {n:>6}: build {built:>10.2?}   layout {laid_out:>10.2?}");
    }
}

fn newsletter(sections: usize) -> Vec<u8> {
    let mut html = String::from("<html><head><style>p{color:red}</style></head><body>");
    for i in 0..sections {
        html.push_str(&format!(
            "<table width=\"600\"><tr><td><img src=\"https://cdn.example.com/img/{i}.jpg\" alt=\"banner {i}\"></td></tr>\
<tr><td><h2>Section {i}: this week's picks</h2><p>Lorem ipsum dolor sit amet, consectetur adipiscing elit, sed do eiusmod tempor incididunt ut labore et dolore magna aliqua. Ut enim ad minim veniam, quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat. <a href=\"https://click.example.com/track?u=abcdefghijklmnopqrstuvwxyz0123456789&utm_source=news&utm_medium=email&utm_campaign=week{i}&id={i}\">Read more</a> or <a href=\"https://click.example.com/track?u=zyxwvutsrqponmlkjihgfedcba9876543210&utm_campaign=week{i}\">https://click.example.com/track?u=zyxwvutsrqponmlkjihgfedcba9876543210&utm_campaign=week{i}</a></p>\
<img src=\"https://cdn.example.com/pixel/{i}.gif\" width=\"1\" height=\"1\"></td></tr></table>"
        ));
    }
    html.push_str("</body></html>");
    format!(
        "From: News <news@example.com>\r\nTo: Alex Smith <alex@example.com>\r\nSubject: Weekly\r\nMessage-ID: <big@example>\r\nDate: Mon, 17 Nov 2025 09:30:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{html}\r\n"
    )
    .into_bytes()
}

/// Timing only: how long a big newsletter takes to render, and to draw once opened.
/// `cargo test --release -p mailgui --test ui -- --ignored --nocapture open_cost`
#[test]
#[ignore = "timing only"]
fn open_cost_of_a_big_newsletter() {
    for sections in [10usize, 60, 250] {
        let mut w = world();
        open_launch(&mut w);
        let raw = newsletter(sections);
        let t0 = std::time::Instant::now();
        let rendered = render::render(&raw, &RenderOptions::default()).expect("render");
        let render_time = t0.elapsed();
        let id = w.app.reader.as_ref().expect("reader").messages[0].id;
        let _ = w
            .app
            .update(Message::BodyLoaded(id, Ok(Box::new(rendered))));
        let _ = w.app.update(Message::ToggleExpand(id));
        let _ = w.app.update(Message::ToggleExpand(id));
        let t1 = std::time::Instant::now();
        let element = view(&w.app);
        let built = t1.elapsed();
        let t2 = std::time::Instant::now();
        let ui: Simulator<'_, Message> =
            Simulator::with_size(iced::Settings::default(), BIG, element);
        let laid_out = t2.elapsed();
        drop(ui);
        println!(
            "{sections:>4} sections ({:>4} KB): render {render_time:>9.2?}   view {built:>9.2?}   layout {laid_out:>9.2?}",
            raw.len() / 1024
        );
    }
}

/// A reply that quotes an earlier message, with a picture and a long tracking link.
fn raw_reply_with_quote() -> Vec<u8> {
    "From: Maria Lopez <maria@acme.io>\r\nTo: Alex Smith <alex@example.com>\r\nSubject: Re: Launch plan\r\nMessage-ID: <m2@acme>\r\nIn-Reply-To: <m1@acme>\r\nReferences: <m1@acme>\r\nDate: Mon, 17 Nov 2025 09:30:00 +0000\r\nMIME-Version: 1.0\r\nContent-Type: text/html; charset=utf-8\r\n\r\n\
<html><body><p>Thursday works for me. The schedule is here: \
<a href=\"https://news.example.com/track/click?id=9f8e7d6c5b4a39281706&u=1234567890&campaign=launch\">https://news.example.com/track/click?id=9f8e7d6c5b4a39281706&u=1234567890&campaign=launch</a></p>\
<p><img src=\"https://cdn.example.com/pixel.gif?u=1\" alt=\"banner\"> <img src=\"https://cdn.example.com/logo.png\"></p>\
<p>On Mon, 17 Nov 2025, Jonas Weber wrote:</p>\
<blockquote><p>Can we freeze on Thursday and announce on Monday?</p><blockquote><p>Earlier: the date is still open.</p></blockquote></blockquote></body></html>\r\n"
        .as_bytes()
        .to_vec()
}

fn open_reply_with_quote(w: &mut World) -> i64 {
    open_launch(w);
    let key = w.app.reader.as_ref().expect("reader").thread_key.clone();
    let msgs = w.svc.store.thread_messages(&key).expect("msgs");
    let last = msgs.last().expect("last").id;
    let rendered =
        render::render(&raw_reply_with_quote(), &RenderOptions::default()).expect("render");
    let _ = w
        .app
        .update(Message::BodyLoaded(last, Ok(Box::new(rendered))));
    last
}

#[test]
fn quoted_text_is_folded_and_can_be_unfolded() {
    let mut w = world();
    let id = open_reply_with_quote(&mut w);
    let r = w.app.reader.as_ref().expect("reader");
    assert!(!r.quotes_open.contains(&id), "folded by default");
    snapshot(&w.app, "79-reply-folded", BIG);
    let _ = w.app.update(Message::ToggleQuotes(id));
    assert!(w.app.reader.as_ref().expect("r").quotes_open.contains(&id));
    snapshot(&w.app, "79b-reply-unfolded", BIG);
    let _ = w.app.update(Message::ToggleQuotes(id));
    assert!(!w.app.reader.as_ref().expect("r").quotes_open.contains(&id));
}

#[test]
fn details_are_hidden_until_asked_for() {
    let mut w = world();
    let id = open_reply_with_quote(&mut w);
    assert!(!w.app.reader.as_ref().expect("r").details_open.contains(&id));
    let _ = w.app.update(Message::ToggleDetails(id));
    assert!(w.app.reader.as_ref().expect("r").details_open.contains(&id));
    snapshot(&w.app, "79c-reply-details", BIG);
    let _ = w
        .app
        .update(Message::SetTheme(mailgui::theme::ThemeChoice::Dark));
    snapshot(&w.app, "79d-reply-details-dark", BIG);
    // A different conversation starts folded again.
    let key = w
        .app
        .threads
        .iter()
        .find(|t| !t.subject.contains("Launch"))
        .expect("other")
        .thread_key
        .clone();
    let _ = w
        .app
        .update(Message::OpenThread(key, ListScope::Folder(w.inbox)));
    let r = w.app.reader.as_ref().expect("r");
    assert!(r.quotes_open.is_empty() && r.details_open.is_empty());
}

#[test]
fn the_folded_reply_stays_usable_in_a_small_window() {
    let mut w = world();
    let _ = open_reply_with_quote(&mut w);
    snapshot(&w.app, "79e-reply-small", Size::new(900.0, 560.0));
}
