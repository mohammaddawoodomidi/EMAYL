//! The persistent outbox: every message is written to disk *before* any network
//! attempt, so nothing typed is lost to a crash, a dead connection or a closed
//! laptop lid.
//!
//! Item states: `Queued` (not tried yet), `Sending` (an attempt is running),
//! `Deferred` (temporary failure, waiting for the next retry), `Sent`, `Failed`
//! (gave up, or needs the user), `Bounced` (accepted by our server, bounced
//! later). Each recipient is tracked on its own, so a message accepted for some
//! recipients is never re-sent to them.
//!
//! Delivery is **at least once**: if the connection dies after the server took
//! the message but before it said so, the retry may deliver a duplicate. That is
//! the safe direction (losing a mail is worse than duplicating one).
//!
//! Layout: `<dir>/<id>.json` (status) and `<dir>/<id>.eml` (the message, removed
//! once sent). Both are written atomically with owner-only permissions.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::bounce::{BounceAction, BounceKind, BounceReport};
use crate::compose::ComposedMessage;
use crate::fsutil::{
    create_private_dir, io_err, is_valid_id, new_id, read_limited, remove_if_exists,
    remove_stale_temp_files, write_atomic,
};
use crate::net::ConnError;
use crate::retry::RetryPolicy;
use crate::smtp::{Phase, Rejection, SendError, SendReport, explain};
use crate::{Error, Result};

const FORMAT_VERSION: u32 = 1;
const MAX_META_BYTES: u64 = 1024 * 1024;
const MAX_MESSAGE_BYTES: u64 = 64 * 1024 * 1024;

/// Retry schedule: 1 min, 2, 4 ... capped at 6 hours (each with jitter).
pub const RETRY: RetryPolicy = RetryPolicy {
    base: Duration::from_secs(60),
    cap: Duration::from_secs(6 * 3600),
    max_attempts: u32::MAX,
};
/// A message still undeliverable after this long is given up on (design choice;
/// common mail servers use a few days).
pub const GIVE_UP_AFTER_SECS: i64 = 72 * 3600;
pub const MAX_ATTEMPTS: u32 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OutboxState {
    Queued,
    Sending,
    Deferred,
    Sent,
    Failed,
    Bounced,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RecipientStatus {
    Pending,
    Delivered,
    Failed,
}

/// Where the "copy in the Sent folder" stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SentCopy {
    /// The provider saves it itself, or it is not wanted.
    NotNeeded,
    Pending,
    Saved,
    GaveUp,
}

/// A recorded problem, in words a person can act on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FailureNote {
    pub code: Option<u16>,
    pub enhanced: Option<String>,
    /// What the server said (cleaned, shortened).
    pub server_text: String,
    /// What it means and what to do.
    pub advice: String,
    pub at: i64,
    /// True if the account itself needs attention (password, certificate...).
    pub account_problem: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecipientEntry {
    pub address: String,
    pub status: RecipientStatus,
    pub note: Option<FailureNote>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxItem {
    pub version: u32,
    pub id: String,
    pub account: AccountId,
    /// Without angle brackets.
    pub message_id: String,
    pub from: String,
    pub subject: String,
    pub size: u64,
    pub created_at: i64,
    /// When this item last entered the queue (differs from `created_at` after a manual retry).
    pub queued_at: i64,
    pub state: OutboxState,
    pub attempts: u32,
    pub next_attempt_at: i64,
    pub recipients: Vec<RecipientEntry>,
    pub last_error: Option<FailureNote>,
    pub last_server_reply: Option<String>,
    pub sent_at: Option<i64>,
    pub sent_copy: SentCopy,
}

impl OutboxItem {
    pub fn pending_recipients(&self) -> Vec<String> {
        self.recipients
            .iter()
            .filter(|r| r.status == RecipientStatus::Pending)
            .map(|r| r.address.clone())
            .collect()
    }

    pub fn has_failed_recipients(&self) -> bool {
        self.recipients
            .iter()
            .any(|r| r.status == RecipientStatus::Failed)
    }
}

/// What one delivery attempt produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptResult {
    Delivered {
        accepted: Vec<String>,
        rejected: Vec<Rejection>,
        server_reply: String,
    },
    Refused {
        phase: Phase,
        code: u16,
        enhanced: Option<String>,
        text: String,
        permanent: bool,
        rejections: Vec<Rejection>,
    },
    TooLarge {
        limit: u64,
    },
    Connection(ConnError),
    /// The message could not even be attempted (stored file damaged, bad address).
    Unsendable(String),
}

impl From<std::result::Result<SendReport, SendError>> for AttemptResult {
    fn from(r: std::result::Result<SendReport, SendError>) -> Self {
        match r {
            Ok(rep) => Self::Delivered {
                accepted: rep.accepted.iter().map(ToString::to_string).collect(),
                rejected: rep.rejected,
                server_reply: rep.server_reply,
            },
            Err(SendError::Conn(e)) => Self::Connection(e),
            Err(SendError::TooLarge { limit }) => Self::TooLarge { limit },
            Err(SendError::Refused(r)) => Self::Refused {
                phase: r.phase,
                code: r.code,
                enhanced: r.enhanced.map(|e| e.to_string()),
                text: r.text,
                permanent: r.permanent,
                rejections: r.rejections,
            },
        }
    }
}

fn note_from_rejection(r: &Rejection, now: i64) -> FailureNote {
    let enhanced = r
        .enhanced
        .as_deref()
        .and_then(crate::smtp::EnhancedStatus::parse);
    FailureNote {
        code: Some(r.code),
        enhanced: r.enhanced.clone(),
        server_text: r.text.clone(),
        advice: explain(r.code, enhanced).to_owned(),
        at: now,
        account_problem: false,
    }
}

fn find_recipient<'a>(item: &'a mut OutboxItem, address: &str) -> Option<&'a mut RecipientEntry> {
    item.recipients
        .iter_mut()
        .find(|r| r.address.eq_ignore_ascii_case(address))
}

fn fail_all_pending(item: &mut OutboxItem, note: &FailureNote) {
    for r in item
        .recipients
        .iter_mut()
        .filter(|r| r.status == RecipientStatus::Pending)
    {
        r.status = RecipientStatus::Failed;
        r.note = Some(note.clone());
    }
}

/// Applies one attempt's result to an item: updates recipients, decides the next
/// state and schedules the retry. Pure (no I/O) so every rule is unit-tested.
/// `jitter` is in `[0, 1]`.
pub fn apply_attempt(
    item: &mut OutboxItem,
    result: &AttemptResult,
    now: i64,
    jitter: f64,
    send_copy: bool,
) {
    match result {
        AttemptResult::Delivered {
            accepted,
            rejected,
            server_reply,
        } => {
            for a in accepted {
                if let Some(r) = find_recipient(item, a) {
                    r.status = RecipientStatus::Delivered;
                    r.note = None;
                }
            }
            apply_rejections(item, rejected, now);
            item.last_server_reply = Some(server_reply.clone());
        }
        AttemptResult::Refused {
            phase,
            code,
            enhanced,
            text,
            permanent,
            rejections,
        } => {
            apply_rejections(item, rejections, now);
            // Failures that are not about one recipient hit everyone still pending.
            if *phase != Phase::Recipients || rejections.is_empty() {
                let e = enhanced
                    .as_deref()
                    .and_then(crate::smtp::EnhancedStatus::parse);
                let note = FailureNote {
                    code: Some(*code),
                    enhanced: enhanced.clone(),
                    server_text: text.clone(),
                    advice: explain(*code, e).to_owned(),
                    at: now,
                    account_problem: false,
                };
                if *permanent {
                    fail_all_pending(item, &note);
                } else {
                    item.last_error = Some(note);
                }
            }
        }
        AttemptResult::TooLarge { limit } => {
            let note = FailureNote {
                code: Some(552),
                enhanced: Some("5.3.4".into()),
                server_text: format!("limit is {limit} bytes"),
                advice: format!(
                    "The message is larger than this provider accepts ({} MB). Remove or shrink attachments, or send them as a link.",
                    limit / 1_000_000
                ),
                at: now,
                account_problem: false,
            };
            fail_all_pending(item, &note);
        }
        AttemptResult::Unsendable(reason) => {
            let note = FailureNote {
                code: None,
                enhanced: None,
                server_text: reason.clone(),
                advice: "This message could not be prepared for sending. Open it from the Outbox, check it and send it again.".into(),
                at: now,
                account_problem: false,
            };
            fail_all_pending(item, &note);
            item.last_error = Some(note);
        }
        AttemptResult::Connection(e) => {
            let note = FailureNote {
                code: None,
                enhanced: None,
                server_text: e.to_string(),
                advice: e.advice().to_owned(),
                at: now,
                account_problem: !e.is_transient(),
            };
            let account_problem = note.account_problem;
            item.last_error = Some(note);
            if account_problem {
                // Not the message's fault: park it until the account is fixed.
                item.state = OutboxState::Failed;
                return;
            }
        }
    }
    finalize(item, now, jitter, send_copy);
}

fn apply_rejections(item: &mut OutboxItem, rejections: &[Rejection], now: i64) {
    for rej in rejections {
        let note = note_from_rejection(rej, now);
        if let Some(r) = find_recipient(item, &rej.address) {
            if rej.permanent {
                r.status = RecipientStatus::Failed;
            }
            r.note = Some(note);
        }
    }
}

fn finalize(item: &mut OutboxItem, now: i64, jitter: f64, send_copy: bool) {
    let pending = item
        .recipients
        .iter()
        .any(|r| r.status == RecipientStatus::Pending);
    if !pending {
        let any_delivered = item
            .recipients
            .iter()
            .any(|r| r.status == RecipientStatus::Delivered);
        if any_delivered {
            item.state = OutboxState::Sent;
            item.sent_at = Some(now);
            item.last_error = None;
            item.sent_copy = if send_copy {
                SentCopy::Pending
            } else {
                SentCopy::NotNeeded
            };
        } else {
            item.state = OutboxState::Failed;
            if item.last_error.is_none() {
                item.last_error = item.recipients.iter().find_map(|r| r.note.clone());
            }
        }
        return;
    }
    let expired = now - item.queued_at > GIVE_UP_AFTER_SECS || item.attempts >= MAX_ATTEMPTS;
    if expired {
        let note = FailureNote {
            code: None,
            enhanced: None,
            server_text: item.last_error.as_ref().map(|n| n.server_text.clone()).unwrap_or_default(),
            advice: "We kept trying for 3 days but could not deliver this message. Check the addresses and your connection, then use Retry.".into(),
            at: now,
            account_problem: false,
        };
        fail_all_pending(item, &note);
        item.last_error = Some(note);
        let any_delivered = item
            .recipients
            .iter()
            .any(|r| r.status == RecipientStatus::Delivered);
        item.state = if any_delivered {
            OutboxState::Sent
        } else {
            OutboxState::Failed
        };
        if any_delivered {
            item.sent_at = Some(now);
            item.sent_copy = if send_copy {
                SentCopy::Pending
            } else {
                SentCopy::NotNeeded
            };
        }
        return;
    }
    let delay = RETRY.delay_for(item.attempts.saturating_sub(1), jitter);
    item.state = OutboxState::Deferred;
    item.next_attempt_at = now + i64::try_from(delay.as_secs()).unwrap_or(i64::MAX / 2);
}

/// Marks recipients as failed because a bounce said so.
pub fn apply_bounce(item: &mut OutboxItem, report: &BounceReport, now: i64) {
    let mut any_failed = false;
    for rec in &report.recipients {
        if rec.action != BounceAction::Failed {
            continue;
        }
        if let Some(r) = find_recipient(item, &rec.address) {
            r.status = RecipientStatus::Failed;
            r.note = Some(FailureNote {
                code: None,
                enhanced: rec.status.clone(),
                server_text: rec.diagnostic.clone().unwrap_or_default(),
                advice: rec.explanation().to_owned(),
                at: now,
                account_problem: false,
            });
            any_failed = true;
        }
    }
    let hard_unspecific = report.recipients.is_empty() && report.kind == BounceKind::Hard;
    if any_failed || hard_unspecific {
        item.state = OutboxState::Bounced;
        if hard_unspecific {
            item.last_error = Some(FailureNote {
                code: None,
                enhanced: None,
                server_text: String::new(),
                advice: "A returned-mail notice says this message could not be delivered to at least one recipient.".into(),
                at: now,
                account_problem: false,
            });
        }
    }
}

#[derive(Debug)]
pub struct Outbox {
    dir: PathBuf,
    /// Held during every change. The window and the background sender use the same
    /// outbox from different threads; without this, "Cancel" and "start sending" could
    /// both succeed for the same message. (Other *programs* are kept away by
    /// [`crate::instance::InstanceLock`].)
    changes: std::sync::Mutex<()>,
}

#[derive(Debug, Default)]
pub struct Listing {
    pub items: Vec<OutboxItem>,
    /// Status files that could not be read (left untouched on disk).
    pub unreadable: Vec<PathBuf>,
}

impl Outbox {
    /// Opens (creating if needed) the outbox directory. Items left in `Sending`
    /// by a crash go back to `Queued`.
    pub fn open(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
        let me = Self {
            dir: dir.to_path_buf(),
            changes: std::sync::Mutex::new(()),
        };
        remove_stale_temp_files(dir);
        me.remove_orphan_messages();
        for mut item in me.list()?.items {
            if item.state == OutboxState::Sending {
                item.state = OutboxState::Queued;
                me.save(&item)?;
            }
        }
        Ok(me)
    }

    /// Waits for any other change in progress. A panic elsewhere must not lock the outbox
    /// for good, so a poisoned lock is simply taken over (the files are always complete:
    /// they are written atomically).
    fn changing(&self) -> std::sync::MutexGuard<'_, ()> {
        self.changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn meta_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn eml_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.eml"))
    }

    fn check_id(id: &str) -> Result<()> {
        if is_valid_id(id) {
            Ok(())
        } else {
            Err(Error::NotFound(id.chars().take(40).collect()))
        }
    }

    fn save(&self, item: &OutboxItem) -> Result<()> {
        let json = serde_json::to_vec_pretty(item).map_err(|e| Error::CorruptFile {
            path: self.meta_path(&item.id),
            reason: e.to_string(),
        })?;
        write_atomic(&self.meta_path(&item.id), &json)
    }

    fn load(&self, path: &Path) -> Result<OutboxItem> {
        let raw = read_limited(path, MAX_META_BYTES)?;
        let item: OutboxItem = serde_json::from_slice(&raw).map_err(|e| Error::CorruptFile {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if item.version != FORMAT_VERSION || item.id != stem || !is_valid_id(&item.id) {
            return Err(Error::CorruptFile {
                path: path.to_path_buf(),
                reason: "unsupported version or mismatched id".into(),
            });
        }
        Ok(item)
    }

    pub fn get(&self, id: &str) -> Result<OutboxItem> {
        Self::check_id(id)?;
        let p = self.meta_path(id);
        if !p.exists() {
            return Err(Error::NotFound(id.to_owned()));
        }
        self.load(&p)
    }

    pub fn list(&self) -> Result<Listing> {
        let mut out = Listing::default();
        for entry in fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))? {
            let path = entry.map_err(|e| io_err(&self.dir, e))?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            match self.load(&path) {
                Ok(i) => out.items.push(i),
                Err(_) => out.unreadable.push(path),
            }
        }
        out.items.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    /// The raw message to send (only present until the item is sent).
    pub fn message_bytes(&self, id: &str) -> Result<Vec<u8>> {
        Self::check_id(id)?;
        read_limited(&self.eml_path(id), MAX_MESSAGE_BYTES)
    }

    /// Writes the message to disk and queues it. Returns the item id. The
    /// message file is stored first; the status file makes it visible.
    pub fn enqueue(
        &self,
        account: &AccountId,
        msg: &ComposedMessage,
        now: i64,
        want_sent_copy: bool,
    ) -> Result<OutboxItem> {
        self.enqueue_after(account, msg, now, want_sent_copy, 0)
    }

    /// Like [`enqueue`](Self::enqueue), but the first attempt waits `delay_secs` ("undo send").
    /// Until then the message can be taken back with [`cancel`](Self::cancel).
    pub fn enqueue_after(
        &self,
        account: &AccountId,
        msg: &ComposedMessage,
        now: i64,
        want_sent_copy: bool,
        delay_secs: u32,
    ) -> Result<OutboxItem> {
        let _changing = self.changing();
        let id = new_id();
        write_atomic(&self.eml_path(&id), &msg.bytes)?;
        let item = OutboxItem {
            version: FORMAT_VERSION,
            id: id.clone(),
            account: account.clone(),
            message_id: msg.message_id.clone(),
            from: msg.from.to_string(),
            subject: msg.subject.chars().take(200).collect(),
            size: msg.bytes.len() as u64,
            created_at: now,
            queued_at: now,
            state: OutboxState::Queued,
            attempts: 0,
            next_attempt_at: now.saturating_add(i64::from(delay_secs.min(300))),
            recipients: msg
                .recipients
                .iter()
                .map(|r| RecipientEntry {
                    address: r.to_string(),
                    status: RecipientStatus::Pending,
                    note: None,
                })
                .collect(),
            last_error: None,
            last_server_reply: None,
            sent_at: None,
            // Remembered now; applied if the message is eventually sent.
            sent_copy: if want_sent_copy {
                SentCopy::Pending
            } else {
                SentCopy::NotNeeded
            },
        };
        if let Err(e) = self.save(&item) {
            let _ = remove_if_exists(&self.eml_path(&id));
            return Err(e);
        }
        Ok(item)
    }

    /// Items whose time has come, oldest first.
    pub fn due(&self, now: i64) -> Result<Vec<OutboxItem>> {
        Ok(self
            .list()?
            .items
            .into_iter()
            .filter(|i| {
                matches!(i.state, OutboxState::Queued | OutboxState::Deferred)
                    && i.next_attempt_at <= now
            })
            .collect())
    }

    /// Records that an attempt is starting (so a crash is noticed and a second
    /// worker does not pick the same item).
    pub fn begin_attempt(&self, id: &str) -> Result<OutboxItem> {
        let _changing = self.changing();
        let mut item = self.get(id)?;
        if !matches!(item.state, OutboxState::Queued | OutboxState::Deferred) {
            return Err(Error::InvalidState("only waiting messages can be sent"));
        }
        item.state = OutboxState::Sending;
        item.attempts += 1;
        self.save(&item)?;
        Ok(item)
    }

    /// Stores the result of an attempt started with [`Self::begin_attempt`].
    pub fn finish_attempt(
        &self,
        id: &str,
        result: &AttemptResult,
        now: i64,
        jitter: f64,
    ) -> Result<OutboxItem> {
        let _changing = self.changing();
        let mut item = self.get(id)?;
        if item.state != OutboxState::Sending {
            return Err(Error::InvalidState(
                "no attempt is running for this message",
            ));
        }
        let want_copy = item.sent_copy == SentCopy::Pending;
        apply_attempt(&mut item, result, now, jitter, want_copy);
        // The stored message is only needed until every recipient is settled.
        // A message whose Sent-folder copy is still to be made must stay on disk.
        let done = item.state == OutboxState::Sent && item.sent_copy != SentCopy::Pending;
        self.save(&item)?;
        if done {
            remove_if_exists(&self.eml_path(id))?;
        }
        Ok(item)
    }

    /// Re-queues a failed message right away (user pressed "Retry").
    pub fn retry_now(&self, id: &str, now: i64) -> Result<OutboxItem> {
        let _changing = self.changing();
        self.retry_now_locked(id, now)
    }

    fn retry_now_locked(&self, id: &str, now: i64) -> Result<OutboxItem> {
        let mut item = self.get(id)?;
        if item.state != OutboxState::Failed {
            return Err(Error::InvalidState("only failed messages can be retried"));
        }
        if !self.eml_path(id).exists() {
            return Err(Error::InvalidState("the message text is no longer stored"));
        }
        for r in item
            .recipients
            .iter_mut()
            .filter(|r| r.status == RecipientStatus::Failed)
        {
            r.status = RecipientStatus::Pending;
        }
        item.state = OutboxState::Queued;
        item.attempts = 0;
        item.queued_at = now;
        item.next_attempt_at = now;
        self.save(&item)?;
        Ok(item)
    }

    /// Re-queues everything that was parked because this account needed attention
    /// (call after the user fixes the password or signs in again).
    pub fn retry_account(&self, account: &AccountId, now: i64) -> Result<usize> {
        let _changing = self.changing();
        let mut n = 0;
        for item in self.list()?.items {
            let parked = item.state == OutboxState::Failed
                && item.account == *account
                && item.last_error.as_ref().is_some_and(|e| e.account_problem);
            if parked && self.retry_now_locked(&item.id, now).is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }

    /// Takes back a message that has not been sent yet.
    pub fn cancel(&self, id: &str) -> Result<()> {
        let _changing = self.changing();
        let item = self.get(id)?;
        if !matches!(
            item.state,
            OutboxState::Queued | OutboxState::Deferred | OutboxState::Failed
        ) {
            return Err(Error::InvalidState(
                "this message is already being sent or was sent",
            ));
        }
        remove_if_exists(&self.meta_path(id))?;
        remove_if_exists(&self.eml_path(id))
    }

    /// Removes a finished entry from the list (the person pressed "Dismiss").
    /// Only messages that are done with can be forgotten: sent or bounced ones whose
    /// Sent-folder copy is no longer waiting, and ones that failed for good.
    pub fn forget(&self, id: &str) -> Result<()> {
        let _changing = self.changing();
        let item = self.get(id)?;
        let ok = match item.state {
            OutboxState::Failed => true,
            OutboxState::Sent | OutboxState::Bounced => item.sent_copy != SentCopy::Pending,
            _ => false,
        };
        if !ok {
            return Err(Error::InvalidState("this message is still being handled"));
        }
        remove_if_exists(&self.meta_path(id))?;
        remove_if_exists(&self.eml_path(id))
    }

    /// Updates the Sent-folder bookkeeping.
    pub fn set_sent_copy(&self, id: &str, status: SentCopy) -> Result<()> {
        let _changing = self.changing();
        let mut item = self.get(id)?;
        item.sent_copy = status;
        self.save(&item)?;
        if matches!(item.state, OutboxState::Sent | OutboxState::Bounced)
            && status != SentCopy::Pending
        {
            remove_if_exists(&self.eml_path(id))?;
        }
        Ok(())
    }

    /// Deletes message files that have no status file. They are left when the program
    /// stops between the two writes of `enqueue`; nobody can see or send them.
    fn remove_orphan_messages(&self) {
        let Ok(entries) = fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("eml") {
                continue;
            }
            let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
            if !is_valid_id(stem) || !self.meta_path(stem).exists() {
                let _ = fs::remove_file(&path);
            }
        }
    }

    /// Deletes everything queued or kept for an account (used when it is removed).
    /// Returns how many messages were deleted.
    pub fn forget_account(&self, account: &AccountId) -> Result<usize> {
        let _changing = self.changing();
        let mut n = 0;
        for item in self.list()?.items {
            if &item.account == account {
                remove_if_exists(&self.eml_path(&item.id))?;
                remove_if_exists(&self.meta_path(&item.id))?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Forgets finished items older than `keep_secs`.
    pub fn purge_finished(&self, now: i64, keep_secs: i64) -> Result<usize> {
        let _changing = self.changing();
        let mut n = 0;
        for item in self.list()?.items {
            let finished = matches!(item.state, OutboxState::Sent | OutboxState::Bounced)
                && item.sent_copy != SentCopy::Pending
                && now - item.sent_at.unwrap_or(item.created_at) > keep_secs;
            if finished {
                remove_if_exists(&self.meta_path(&item.id))?;
                remove_if_exists(&self.eml_path(&item.id))?;
                n += 1;
            }
        }
        Ok(n)
    }

    /// Finds an item by Message-ID (for bounce matching) within one account.
    pub fn find_by_message_id(
        &self,
        account: &AccountId,
        message_id: &str,
    ) -> Result<Option<OutboxItem>> {
        Ok(self
            .list()?
            .items
            .into_iter()
            .find(|i| i.account == *account && i.message_id == message_id))
    }

    /// Applies a bounce to the matching item of `account`. Returns whether one matched.
    pub fn record_bounce(
        &self,
        account: &AccountId,
        report: &BounceReport,
        now: i64,
    ) -> Result<bool> {
        let _changing = self.changing();
        let Some(mut item) = self.find_by_message_id(account, &report.original_message_id)? else {
            return Ok(false);
        };
        if item.state == OutboxState::Sending {
            return Ok(false);
        }
        apply_bounce(&mut item, report, now);
        self.save(&item)?;
        Ok(true)
    }
}

#[cfg(test)]
mod tests;
