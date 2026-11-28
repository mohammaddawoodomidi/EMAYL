//! Writing messages: new, reply, reply to all and forward; drafts that save
//! themselves; sending through the outbox; and the Outbox screen's actions.

use std::path::PathBuf;

use iced::Task;
use iced::widget::text_editor;
use mailcore::compose::{
    Attachment, MAX_ATTACHMENT_BYTES, MAX_ATTACHMENTS, OutgoingMessage, sanitize_filename,
};
use mailcore::drafts::{Draft, DraftSummary};
use mailcore::outbox::{OutboxItem, OutboxState};
use mailcore::render::AttachmentRisk;
use mailcore::replies::{
    Original, ReplyKind, format_recipients, parse_recipients, parse_recipients_lenient, prepare,
};

use crate::app::{App, BodyState, Dialog, Message, Screen, blocking};
use crate::services::Services;
use crate::util;

/// Seconds without typing before a draft is saved.
const AUTOSAVE_AFTER_SECS: i64 = 3;

/// Reasons to ask "send anyway?" before sending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendWarning {
    NoSubject,
    ForgotAttachment,
}

/// Text that suggests an attachment was meant to be included. A short list of
/// common words (English, German, Spanish, French, Italian), not a language detector.
const ATTACHMENT_WORDS: &[&str] = &[
    "attach",
    "enclosed",
    "anhang",
    "angehängt",
    "adjunto",
    "adjunta",
    "pièce jointe",
    "pièces jointes",
    "ci-joint",
    "allegato",
    "in allegato",
];

/// Whether the words someone wrote (not the quoted older mail) mention an attachment.
pub fn mentions_attachment(text: &str) -> bool {
    text.lines()
        .filter(|l| !l.trim_start().starts_with('>'))
        .any(|l| {
            let l = l.to_lowercase();
            ATTACHMENT_WORDS.iter().any(|w| l.contains(w))
        })
}

/// Files chosen by the person, and what could not be added (with the reason).
#[derive(Debug, Clone, Default)]
pub struct Picked {
    pub attachments: Vec<Attachment>,
    pub skipped: Vec<String>,
}

#[derive(Debug, Clone)]
pub enum ComposeMsg {
    New,
    /// Go back to the message being written.
    Show,
    Reply(ReplyKind),
    Account(String),
    To(String),
    Cc(String),
    Bcc(String),
    Subject(String),
    Body(text_editor::Action),
    ToggleCc,
    Attach,
    Attached(Result<Picked, String>),
    Dropped(PathBuf),
    RemoveAttachment(usize),
    ForwardAttachments(u64, Result<(Vec<Attachment>, usize), String>),
    Send,
    SendConfirmed,
    Queued(Result<Box<OutboxItem>, String>),
    Close,
    Closed(Result<(), String>),
    AskDiscard,
    Discard,
    Autosaved(Result<(), String>),
    OpenDraft(String),
    DraftLoaded(Result<Box<Draft>, String>),
    DeleteDraft(String),
    DraftsListed(Vec<DraftSummary>),
    /// Paste a saved template (by id) at the end of the text.
    InsertTemplate(String),
    /// Take the message that is waiting to be sent back.
    Undo,
    UndoResult(Result<(), String>),
}

#[derive(Debug, Clone)]
pub enum OutboxMsg {
    Show,
    Retry(String),
    Cancel(String),
    Forget(String),
    Done(Result<String, String>),
}

/// The message being written.
#[derive(Debug)]
pub struct ComposeState {
    pub draft_id: String,
    /// The account the message is sent from.
    pub account: String,
    pub to: String,
    pub cc: String,
    pub bcc: String,
    pub show_cc: bool,
    pub subject: String,
    pub body: text_editor::Content,
    pub attachments: Vec<Attachment>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    /// A problem with what was typed, shown above the buttons.
    pub error: Option<String>,
    /// Something worth knowing (not an error).
    pub notice: Option<String>,
    /// The person has changed something (so closing keeps a draft).
    pub touched: bool,
    /// Changed since the last save.
    pub dirty: bool,
    pub changed_at: i64,
    pub saving: bool,
    pub sending: bool,
    pub loading_attachments: bool,
    fwd_token: u64,
}

impl ComposeState {
    fn blank(account: String) -> Self {
        Self {
            draft_id: Services::new_draft_id(),
            account,
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            show_cc: false,
            subject: String::new(),
            body: text_editor::Content::new(),
            attachments: Vec::new(),
            in_reply_to: None,
            references: Vec::new(),
            error: None,
            notice: None,
            touched: false,
            dirty: false,
            changed_at: 0,
            saving: false,
            sending: false,
            loading_attachments: false,
            fwd_token: 0,
        }
    }

    fn from_message(account: String, id: String, msg: &OutgoingMessage) -> Self {
        let mut s = Self::blank(account);
        s.draft_id = id;
        s.to = format_recipients(&msg.to);
        s.cc = format_recipients(&msg.cc);
        s.bcc = format_recipients(&msg.bcc);
        s.show_cc = !s.cc.is_empty() || !s.bcc.is_empty();
        s.subject = msg.subject.clone();
        s.body = text_editor::Content::with_text(&msg.text);
        s.attachments = msg.attachments.clone();
        s.in_reply_to = msg.in_reply_to.clone();
        s.references = msg.references.clone();
        s
    }

    pub fn body_text(&self) -> String {
        let t = self.body.text();
        let t = t.trim_end();
        if t.is_empty() {
            String::new()
        } else {
            format!("{t}\n")
        }
    }

    pub fn attachment_bytes(&self) -> usize {
        self.attachments.iter().map(|a| a.data.len()).sum()
    }

    /// Everything typed so far, tolerating half-written addresses (for drafts).
    fn lenient_message(&self) -> OutgoingMessage {
        OutgoingMessage {
            from: None,
            to: parse_recipients_lenient(&self.to),
            cc: parse_recipients_lenient(&self.cc),
            bcc: parse_recipients_lenient(&self.bcc),
            subject: self.subject.clone(),
            text: self.body_text(),
            html: None,
            attachments: self.attachments.clone(),
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
        }
    }

    /// The message to send. Problems are explained in plain words.
    pub fn strict_message(&self) -> Result<OutgoingMessage, String> {
        let read =
            |label: &str, text: &str| parse_recipients(text).map_err(|e| format!("{label}: {e}"));
        let to = read("To", &self.to)?;
        let cc = read("Cc", &self.cc)?;
        let bcc = read("Bcc", &self.bcc)?;
        if to.is_empty() && cc.is_empty() && bcc.is_empty() {
            return Err("Add at least one recipient in the “To” field.".to_owned());
        }
        Ok(OutgoingMessage {
            to,
            cc,
            bcc,
            ..self.lenient_message()
        })
    }

    fn warnings(&self) -> Vec<SendWarning> {
        let mut w = Vec::new();
        if self.subject.trim().is_empty() {
            w.push(SendWarning::NoSubject);
        }
        if self.attachments.is_empty() && mentions_attachment(&self.body.text()) {
            w.push(SendWarning::ForgotAttachment);
        }
        w
    }

    fn touch(&mut self, now: i64) {
        self.touched = true;
        self.dirty = true;
        self.changed_at = now;
        self.error = None;
    }
}

fn guess_content_type(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map_or("", |(_, e)| e).to_lowercase();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "txt" | "log" => "text/plain",
        "csv" => "text/csv",
        "html" | "htm" => "text/html",
        "json" => "application/json",
        "zip" => "application/zip",
        "doc" => "application/msword",
        "docx" => "application/vnd.openxmlformats-officedocument.wordprocessingml.document",
        "xls" => "application/vnd.ms-excel",
        "xlsx" => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
        "ppt" => "application/vnd.ms-powerpoint",
        "pptx" => "application/vnd.openxmlformats-officedocument.presentationml.presentation",
        "mp3" => "audio/mpeg",
        "mp4" => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// Reads the chosen files. A file that does not fit (too big, not a plain file,
/// unreadable) is skipped with a reason; the others are still added.
pub fn read_files(paths: &[PathBuf], used_bytes: usize, used_count: usize) -> Picked {
    let mut out = Picked::default();
    let mut total = used_bytes;
    let mut count = used_count;
    for path in paths {
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let shown = util::clip(&name, 60);
        let meta = match std::fs::metadata(path) {
            Ok(m) => m,
            Err(_) => {
                out.skipped.push(format!("“{shown}” could not be opened."));
                continue;
            }
        };
        if !meta.is_file() {
            out.skipped.push(format!("“{shown}” is not a file."));
            continue;
        }
        if count >= MAX_ATTACHMENTS {
            out.skipped
                .push(format!("“{shown}” was not added: too many attachments."));
            continue;
        }
        let len = usize::try_from(meta.len()).unwrap_or(usize::MAX);
        if total.saturating_add(len) > MAX_ATTACHMENT_BYTES {
            out.skipped.push(format!(
                "“{shown}” ({}) is too big: the attachments of one message can total about {} MB. Send big files with a file-sharing link instead.",
                util::size_text(len),
                MAX_ATTACHMENT_BYTES / (1024 * 1024)
            ));
            continue;
        }
        match std::fs::read(path) {
            Ok(data) => {
                total += data.len();
                count += 1;
                let filename = sanitize_filename(&name);
                out.attachments.push(Attachment {
                    content_type: guess_content_type(&filename).to_owned(),
                    filename,
                    data,
                });
            }
            Err(_) => out.skipped.push(format!("“{shown}” could not be read.")),
        }
    }
    out
}

fn outbox_in_flight(item: &OutboxItem) -> bool {
    matches!(
        item.state,
        OutboxState::Queued | OutboxState::Sending | OutboxState::Deferred
    )
}

fn outbox_needs_attention(item: &OutboxItem) -> bool {
    matches!(item.state, OutboxState::Failed | OutboxState::Bounced)
}

/// (waiting to go out, needs the person's attention)
pub fn outbox_counts(items: &[OutboxItem]) -> (usize, usize) {
    (
        items.iter().filter(|i| outbox_in_flight(i)).count(),
        items.iter().filter(|i| outbox_needs_attention(i)).count(),
    )
}

impl App {
    fn account_for_new_message(&self) -> Option<String> {
        if let crate::app::Nav::Folder { account, .. } = &self.nav
            && self
                .accounts
                .iter()
                .any(|a| a.account.id.as_str() == account)
        {
            return Some(account.clone());
        }
        self.accounts
            .first()
            .map(|a| a.account.id.as_str().to_owned())
    }

    /// Writes a message that is open (if the person changed it) to a draft, and closes it.
    fn park_compose(&mut self) -> Task<Message> {
        let Some(c) = self.compose.take() else {
            return Task::none();
        };
        self.save_closed(c)
    }

    fn save_closed(&mut self, c: ComposeState) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        if !c.touched {
            return Task::none();
        }
        let msg = c.lenient_message();
        let (id, account) = (c.draft_id.clone(), c.account.clone());
        Task::perform(
            blocking(move || svc.save_draft(Some(&id), Some(&account), &msg).map(|_| ())),
            |r| {
                Message::Compose(ComposeMsg::Closed(
                    r.unwrap_or_else(|| Err("The draft could not be saved.".into())),
                ))
            },
        )
    }

    fn focus(id: &'static str) -> Task<Message> {
        iced::widget::operation::focus(id)
    }

    pub(crate) fn update_compose(&mut self, m: ComposeMsg) -> Task<Message> {
        let now = self.now;
        match m {
            ComposeMsg::New => {
                let Some(account) = self.account_for_new_message() else {
                    return Task::none();
                };
                let park = self.park_compose();
                self.compose = Some(ComposeState::blank(account));
                self.screen = Screen::Compose;
                Task::batch([park, Self::focus("compose-to"), self.load_templates()])
            }
            ComposeMsg::Show => {
                if self.compose.is_some() {
                    self.screen = Screen::Compose;
                }
                Task::none()
            }
            ComposeMsg::Reply(kind) => Task::batch([self.start_reply(kind), self.load_templates()]),
            ComposeMsg::Undo => {
                let Some(svc) = self.svc.clone() else {
                    return Task::none();
                };
                let Some(u) = &mut self.undo else {
                    return Task::none();
                };
                if u.working {
                    return Task::none();
                }
                u.working = true;
                let id = u.outbox_id.clone();
                Task::perform(blocking(move || svc.cancel_outbox(&id)), |r| {
                    Message::Compose(ComposeMsg::UndoResult(
                        r.unwrap_or_else(|| Err("It did not finish.".into())),
                    ))
                })
            }
            ComposeMsg::UndoResult(r) => {
                let Some(u) = self.undo.take() else {
                    return Task::none();
                };
                match r {
                    Ok(()) => {
                        let park = self.park_compose();
                        let mut c = *u.compose;
                        c.sending = false;
                        c.touch(now);
                        self.compose = Some(c);
                        self.screen = Screen::Compose;
                        self.toast("Not sent. Your message is open again.", false);
                        park
                    }
                    Err(_) => {
                        self.toast(
                            "Too late to take it back: the message is already being sent.",
                            true,
                        );
                        Task::none()
                    }
                }
            }
            ComposeMsg::InsertTemplate(id) => {
                let Some(t) = self.templates.iter().find(|t| t.id == id).cloned() else {
                    return Task::none();
                };
                if let Some(c) = &mut self.compose {
                    use text_editor::{Action as A, Edit, Motion};
                    c.body.perform(A::Move(Motion::DocumentEnd));
                    let lead = if c.body.text().trim().is_empty() {
                        ""
                    } else {
                        "\n\n"
                    };
                    c.body
                        .perform(A::Edit(Edit::Paste(std::sync::Arc::new(format!(
                            "{lead}{}",
                            t.body
                        )))));
                    c.touch(now);
                }
                Task::none()
            }
            ComposeMsg::Account(id) => {
                if let Some(c) = &mut self.compose {
                    c.account = id;
                    c.touch(now);
                }
                Task::none()
            }
            ComposeMsg::To(t) => self.edit(|c| c.to = t),
            ComposeMsg::Cc(t) => self.edit(|c| c.cc = t),
            ComposeMsg::Bcc(t) => self.edit(|c| c.bcc = t),
            ComposeMsg::Subject(t) => self.edit(|c| c.subject = t),
            ComposeMsg::Body(action) => {
                if let Some(c) = &mut self.compose {
                    let is_edit = action.is_edit();
                    c.body.perform(action);
                    if is_edit {
                        c.touch(now);
                    }
                }
                Task::none()
            }
            ComposeMsg::ToggleCc => {
                if let Some(c) = &mut self.compose {
                    c.show_cc = !c.show_cc;
                }
                Task::none()
            }
            ComposeMsg::Attach => {
                let Some(c) = &self.compose else {
                    return Task::none();
                };
                let (bytes, count) = (c.attachment_bytes(), c.attachments.len());
                Task::perform(
                    async move {
                        let Some(handles) = rfd::AsyncFileDialog::new()
                            .set_title("Attach files")
                            .pick_files()
                            .await
                        else {
                            return Ok(Picked::default());
                        };
                        let paths: Vec<PathBuf> =
                            handles.iter().map(|h| h.path().to_path_buf()).collect();
                        blocking(move || read_files(&paths, bytes, count))
                            .await
                            .ok_or_else(|| "The files could not be read.".to_owned())
                    },
                    |r| Message::Compose(ComposeMsg::Attached(r)),
                )
            }
            ComposeMsg::Dropped(path) => {
                let Some(c) = &self.compose else {
                    return Task::none();
                };
                if self.screen != Screen::Compose {
                    return Task::none();
                }
                let (bytes, count) = (c.attachment_bytes(), c.attachments.len());
                Task::perform(blocking(move || read_files(&[path], bytes, count)), |r| {
                    Message::Compose(ComposeMsg::Attached(
                        r.ok_or_else(|| "The file could not be read.".to_owned()),
                    ))
                })
            }
            ComposeMsg::Attached(r) => {
                if let Some(c) = &mut self.compose {
                    match r {
                        Ok(p) => {
                            if !p.attachments.is_empty() {
                                c.attachments.extend(p.attachments);
                                c.touch(now);
                            }
                            if !p.skipped.is_empty() {
                                c.error = Some(p.skipped.join(" "));
                            }
                        }
                        Err(e) => c.error = Some(e),
                    }
                }
                Task::none()
            }
            ComposeMsg::RemoveAttachment(i) => {
                if let Some(c) = &mut self.compose
                    && i < c.attachments.len()
                {
                    c.attachments.remove(i);
                    c.touch(now);
                }
                Task::none()
            }
            ComposeMsg::ForwardAttachments(token, r) => {
                if let Some(c) = &mut self.compose
                    && c.fwd_token == token
                {
                    c.loading_attachments = false;
                    match r {
                        Ok((files, skipped)) => {
                            c.attachments.extend(files);
                            if skipped > 0 {
                                c.notice = Some(format!(
                                    "{skipped} attachment(s) of the original message were left out (too large or unreadable)."
                                ));
                            }
                        }
                        Err(e) => {
                            c.notice = Some(format!(
                                "The attachments of the original message could not be added: {e}"
                            ));
                        }
                    }
                }
                Task::none()
            }
            ComposeMsg::Send => self.try_send(false),
            ComposeMsg::SendConfirmed => {
                self.dialog = None;
                self.try_send(true)
            }
            ComposeMsg::Queued(r) => self.on_queued(r),
            ComposeMsg::Close => {
                if self.compose.as_ref().is_some_and(|c| c.sending) {
                    return Task::none();
                }
                let t = self.park_compose();
                if self.screen == Screen::Compose {
                    self.screen = Screen::Mailbox;
                }
                t
            }
            ComposeMsg::Closed(r) => {
                match r {
                    Ok(()) => {
                        self.toast(
                            "Draft saved. You can finish it later from the Outbox.",
                            false,
                        );
                    }
                    Err(e) => self.toast(e, true),
                }
                self.list_drafts()
            }
            ComposeMsg::AskDiscard => {
                match &self.compose {
                    Some(c) if c.touched => self.dialog = Some(Dialog::DiscardDraft),
                    Some(_) => return self.update_compose(ComposeMsg::Discard),
                    None => {}
                }
                Task::none()
            }
            ComposeMsg::Discard => {
                self.dialog = None;
                let Some(c) = self.compose.take() else {
                    return Task::none();
                };
                if self.screen == Screen::Compose {
                    self.screen = Screen::Mailbox;
                }
                self.delete_draft_task(c.draft_id)
            }
            ComposeMsg::Autosaved(r) => {
                if let Some(c) = &mut self.compose {
                    c.saving = false;
                    if let Err(e) = r {
                        c.notice = Some(e);
                    }
                }
                Task::none()
            }
            ComposeMsg::OpenDraft(id) => {
                let Some(svc) = self.svc.clone() else {
                    return Task::none();
                };
                Task::perform(blocking(move || svc.load_draft(&id)), |r| {
                    Message::Compose(ComposeMsg::DraftLoaded(
                        r.unwrap_or_else(|| Err("The draft could not be opened.".into()))
                            .map(Box::new),
                    ))
                })
            }
            ComposeMsg::DraftLoaded(r) => match r {
                Ok(d) => {
                    let account = d
                        .account
                        .as_ref()
                        .map(|a| a.as_str().to_owned())
                        .filter(|a| self.accounts.iter().any(|s| s.account.id.as_str() == a))
                        .or_else(|| self.account_for_new_message());
                    let Some(account) = account else {
                        return Task::none();
                    };
                    let park = self.park_compose();
                    self.compose = Some(ComposeState::from_message(account, d.id, &d.message));
                    self.screen = Screen::Compose;
                    park
                }
                Err(e) => {
                    self.toast(e, true);
                    Task::none()
                }
            },
            ComposeMsg::DeleteDraft(id) => {
                self.drafts.retain(|d| d.id != id);
                self.delete_draft_task(id)
            }
            ComposeMsg::DraftsListed(list) => {
                self.drafts = list;
                Task::none()
            }
        }
    }

    fn edit(&mut self, f: impl FnOnce(&mut ComposeState)) -> Task<Message> {
        let now = self.now;
        if let Some(c) = &mut self.compose {
            f(c);
            c.touch(now);
        }
        Task::none()
    }

    pub(crate) fn list_drafts(&self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        Task::perform(blocking(move || svc.list_drafts()), |r| {
            Message::Compose(ComposeMsg::DraftsListed(r.unwrap_or_default()))
        })
    }

    fn delete_draft_task(&self, id: String) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        Task::perform(
            blocking(move || {
                svc.delete_draft(&id);
                svc.list_drafts()
            }),
            |r| Message::Compose(ComposeMsg::DraftsListed(r.unwrap_or_default())),
        )
    }

    fn try_send(&mut self, confirmed: bool) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let Some(c) = &mut self.compose else {
            return Task::none();
        };
        if c.sending {
            return Task::none();
        }
        if c.loading_attachments {
            c.error = Some("Wait a moment: the attachments are still being added.".into());
            return Task::none();
        }
        let msg = match c.strict_message() {
            Ok(m) => m,
            Err(e) => {
                c.error = Some(e);
                return Task::none();
            }
        };
        if !confirmed {
            let w = c.warnings();
            if !w.is_empty() {
                self.dialog = Some(Dialog::ConfirmSend(w));
                return Task::none();
            }
        }
        c.sending = true;
        c.error = None;
        let account = c.account.clone();
        let delay = u32::from(self.prefs.send_delay);
        Task::perform(
            blocking(move || svc.queue_message(&account, msg, delay)),
            |r| {
                Message::Compose(ComposeMsg::Queued(
                    r.unwrap_or_else(|| Err("The message could not be prepared.".into()))
                        .map(Box::new),
                ))
            },
        )
    }

    fn on_queued(&mut self, r: Result<Box<OutboxItem>, String>) -> Task<Message> {
        match r {
            Ok(item) => {
                let Some(c) = self.compose.take() else {
                    return Task::none();
                };
                if self.screen == Screen::Compose {
                    self.screen = Screen::Mailbox;
                }
                let draft_id = c.draft_id.clone();
                if self.prefs.send_delay > 0 {
                    self.toast = None;
                    self.undo = Some(crate::app::UndoSend {
                        outbox_id: item.id.clone(),
                        subject: c.subject.clone(),
                        until: self.now + i64::from(self.prefs.send_delay),
                        compose: Box::new(c),
                        working: false,
                    });
                } else {
                    self.toast("Message queued. It is being sent now.", false);
                }
                self.delete_draft_task(draft_id)
            }
            Err(e) => {
                if let Some(c) = &mut self.compose {
                    c.sending = false;
                    c.error = Some(e);
                }
                Task::none()
            }
        }
    }

    fn start_reply(&mut self, kind: ReplyKind) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let Some(reader) = &self.reader else {
            return Task::none();
        };
        let Some(msg) = reader.messages.last().cloned() else {
            return Task::none();
        };
        let Some(BodyState::Ready(rm)) = self.bodies.get(&msg.id).cloned() else {
            self.toast("Wait until the message has finished loading.", true);
            return Task::none();
        };
        let Some(acc) = self
            .accounts
            .iter()
            .find(|a| a.account.id.as_str() == msg.account)
            .map(|a| a.account.clone())
        else {
            return Task::none();
        };
        let date_text = rm
            .headers
            .date
            .map(|d| util::long_when(d, self.tz))
            .unwrap_or_default();
        let prepared = prepare(
            kind,
            &Original {
                headers: &rm.headers,
                text: &rm.plain_text,
                date_text: &date_text,
            },
            std::slice::from_ref(&acc.email),
        );
        let park = self.park_compose();
        let mut state = ComposeState::from_message(
            acc.id.as_str().to_owned(),
            Services::new_draft_id(),
            &prepared,
        );
        if kind != ReplyKind::Forward
            && let (Some(rt), Some(from)) = (rm.headers.reply_to.first(), rm.headers.from.as_ref())
            && !rt.address.eq_ignore_ascii_case(&from.address)
        {
            state.notice = Some(format!(
                "The sender asked for replies to go to {} instead of {}.",
                rt.address, from.address
            ));
        }
        let mut tasks = vec![park];
        if kind == ReplyKind::Forward && !rm.attachments.is_empty() {
            state.loading_attachments = true;
            state.fwd_token = 1;
            tasks.push(Task::perform(svc.forward_attachments(msg.id), |r| {
                Message::Compose(ComposeMsg::ForwardAttachments(1, r))
            }));
        }
        let focus = if kind == ReplyKind::Forward {
            "compose-to"
        } else {
            "compose-body"
        };
        self.compose = Some(state);
        self.screen = Screen::Compose;
        tasks.push(Self::focus(focus));
        Task::batch(tasks)
    }

    /// Called every couple of seconds: saves a draft when typing has paused.
    pub(crate) fn autosave(&mut self) -> Task<Message> {
        let now = self.now;
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let Some(c) = &mut self.compose else {
            return Task::none();
        };
        if !c.touched
            || !c.dirty
            || c.saving
            || c.sending
            || now - c.changed_at < AUTOSAVE_AFTER_SECS
        {
            return Task::none();
        }
        c.saving = true;
        c.dirty = false;
        let msg = c.lenient_message();
        let (id, account) = (c.draft_id.clone(), c.account.clone());
        Task::perform(
            blocking(move || svc.save_draft(Some(&id), Some(&account), &msg).map(|_| ())),
            |r| {
                Message::Compose(ComposeMsg::Autosaved(
                    r.unwrap_or_else(|| Err("The draft could not be saved.".into())),
                ))
            },
        )
    }

    // ---- attachments of received mail -------------------------------------

    pub(crate) fn save_attachment(
        &mut self,
        message: i64,
        index: usize,
        confirmed: bool,
    ) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let Some(BodyState::Ready(rm)) = self.bodies.get(&message) else {
            return Task::none();
        };
        let Some(info) = rm.attachments.iter().find(|a| a.index == index) else {
            return Task::none();
        };
        if info.risk != AttachmentRisk::Normal && !confirmed {
            self.dialog = Some(Dialog::SaveRisky {
                message,
                index,
                name: info.filename.clone(),
                disguised: info.risk == AttachmentRisk::DisguisedExecutable,
            });
            return Task::none();
        }
        let read = svc.read_attachment(message, index);
        Task::perform(
            async move {
                let data = read.await?;
                blocking(move || {
                    let dir = crate::downloads::downloads_dir().ok_or_else(|| {
                        "This computer has no Downloads folder to save into.".to_owned()
                    })?;
                    let path = crate::downloads::save_unique(&dir, &data.filename, &data.data)?;
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    Ok(format!(
                        "Saved “{}” in {}.",
                        util::clip(&name, 50),
                        dir.display()
                    ))
                })
                .await
                .unwrap_or_else(|| Err("The file could not be saved.".to_owned()))
            },
            Message::AttachmentSaved,
        )
    }

    // ---- the Outbox ----------------------------------------------------

    pub(crate) fn update_outbox(&mut self, m: OutboxMsg) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let run = |f: Box<dyn FnOnce() -> Result<String, String> + Send>| {
            Task::perform(blocking(f), |r| {
                Message::Outbox(OutboxMsg::Done(
                    r.unwrap_or_else(|| Err("The action did not finish.".into())),
                ))
            })
        };
        match m {
            OutboxMsg::Show => {
                self.screen = Screen::Outbox;
                self.list_drafts()
            }
            OutboxMsg::Retry(id) => run(Box::new(move || {
                svc.retry_outbox(&id)
                    .map(|()| "Trying again now.".to_owned())
            })),
            OutboxMsg::Cancel(id) => run(Box::new(move || {
                svc.cancel_outbox(&id)
                    .map(|()| "The message was removed and will not be sent.".to_owned())
            })),
            OutboxMsg::Forget(id) => run(Box::new(move || {
                svc.forget_outbox(&id).map(|()| String::new())
            })),
            OutboxMsg::Done(r) => {
                match r {
                    Ok(t) if !t.is_empty() => self.toast(t, false),
                    Ok(_) => {}
                    Err(e) => self.toast(e, true),
                }
                self.sync_outbox_view();
                Task::none()
            }
        }
    }

    /// Takes over the list kept by the sending loop. Tells the person when a
    /// message has been sent or has failed.
    pub(crate) fn sync_outbox_view(&mut self) {
        let Some(svc) = self.svc.clone() else {
            return;
        };
        let (generation, items) = svc.outbox_snapshot();
        if generation == self.outbox_gen {
            return;
        }
        if self.outbox_primed {
            for new in &items {
                let before = self.outbox.iter().find(|o| o.id == new.id);
                let was = before.map(|b| b.state);
                if was == Some(new.state) {
                    continue;
                }
                let subject = util::clip(&new.subject, 50);
                match new.state {
                    OutboxState::Sent if was.is_some() => {
                        self.toast(format!("Sent: “{subject}”"), false);
                    }
                    OutboxState::Failed | OutboxState::Bounced => {
                        self.toast(
                            format!("“{subject}” could not be sent. Open the Outbox to see why."),
                            true,
                        );
                    }
                    _ => {}
                }
            }
        }
        self.outbox = items;
        self.outbox_gen = generation;
        self.outbox_primed = true;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn notices_a_missing_attachment_only_in_new_words() {
        assert!(mentions_attachment(
            "Hi,\nPlease find the invoice attached.\n"
        ));
        assert!(mentions_attachment("anbei im Anhang"));
        assert!(!mentions_attachment("Thanks!\n\n> see the attached file\n"));
        assert!(!mentions_attachment("Lunch tomorrow?"));
    }

    #[test]
    fn content_types_follow_the_extension() {
        assert_eq!(guess_content_type("a.PDF"), "application/pdf");
        assert_eq!(guess_content_type("noext"), "application/octet-stream");
        assert_eq!(guess_content_type("x.tar.gz"), "application/octet-stream");
    }

    #[test]
    fn files_are_read_with_limits_and_reasons() {
        let dir = std::env::temp_dir().join(format!("emayl-att-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = dir.join("a.txt");
        std::fs::write(&a, b"hello").unwrap();
        let p = read_files(&[a.clone(), dir.clone(), dir.join("missing.bin")], 0, 0);
        assert_eq!(p.attachments.len(), 1);
        assert_eq!(p.attachments[0].filename, "a.txt");
        assert_eq!(p.attachments[0].content_type, "text/plain");
        assert_eq!(p.skipped.len(), 2, "{:?}", p.skipped);
        // Nothing fits when the allowance is used up.
        let p = read_files(std::slice::from_ref(&a), MAX_ATTACHMENT_BYTES, 0);
        assert!(p.attachments.is_empty() && p.skipped[0].contains("too big"));
        let p = read_files(&[a], 0, MAX_ATTACHMENTS);
        assert!(p.attachments.is_empty() && p.skipped[0].contains("too many"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn sending_needs_a_recipient_and_explains_bad_addresses() {
        let mut c = ComposeState::blank("acc".into());
        assert!(c.strict_message().unwrap_err().contains("recipient"));
        c.to = "ann@example.com, nobody".into();
        assert!(c.strict_message().unwrap_err().starts_with("To:"));
        c.to = "ann@example.com".into();
        c.bcc = "bob@example.org".into();
        let m = c.strict_message().unwrap();
        assert_eq!((m.to.len(), m.bcc.len()), (1, 1));
    }

    #[test]
    fn warnings_cover_subject_and_forgotten_files() {
        let mut c = ComposeState::blank("acc".into());
        assert_eq!(c.warnings(), vec![SendWarning::NoSubject]);
        c.subject = "Invoice".into();
        c.body = text_editor::Content::with_text("See attached.");
        assert_eq!(c.warnings(), vec![SendWarning::ForgotAttachment]);
        c.attachments.push(Attachment {
            filename: "i.pdf".into(),
            content_type: "application/pdf".into(),
            data: vec![1],
        });
        assert!(c.warnings().is_empty());
    }

    #[test]
    fn drafts_keep_half_typed_addresses_out_but_the_text_in() {
        let mut c = ComposeState::blank("acc".into());
        c.to = "ann@example.com, bo".into();
        c.body = text_editor::Content::with_text("hello");
        let m = c.lenient_message();
        assert_eq!(m.to.len(), 1);
        assert_eq!(m.text, "hello\n");
    }
}
