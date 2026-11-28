//! Writing and sending: drafts, the outbox, the background sender, and reading
//! attachments out of received messages.

use std::sync::{Arc, Weak};
use std::time::Duration;

use mailcore::compose::{
    Attachment, ComposeError, MAX_ATTACHMENT_BYTES, Mailbox, OutgoingMessage, compose,
};
use mailcore::drafts::{Draft, DraftSummary};
use mailcore::outbox::OutboxItem;
use mailcore::render::{self, AttachmentData, RenderOptions};
use mailcore::sender::{self, wants_sent_copy};

use super::{Services, lock};

/// How often the sender looks for work when nobody wakes it.
const SEND_POLL: Duration = Duration::from_secs(15);
/// Finished messages are listed for this long.
const KEEP_FINISHED_SECS: i64 = 3 * 24 * 3600;

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// A sentence a person can act on.
pub fn describe_compose_error(e: &ComposeError) -> String {
    match e {
        ComposeError::NoRecipients => "Add at least one recipient.".to_owned(),
        ComposeError::TooManyRecipients => "There are too many recipients.".to_owned(),
        ComposeError::TooManyAttachments => "There are too many attachments.".to_owned(),
        ComposeError::AttachmentsTooLarge => format!(
            "The attachments are too large. Most mail providers accept about {} MB in total; remove a file or send it through a file-sharing link instead.",
            MAX_ATTACHMENT_BYTES / (1024 * 1024)
        ),
        ComposeError::BodyTooLarge => "The message text is too long.".to_owned(),
        ComposeError::Invalid { field, reason } => format!("The {field} {reason}."),
        ComposeError::Encoding => "The message could not be prepared for sending.".to_owned(),
    }
}

impl Services {
    /// The current list for the Outbox screen. The first value changes whenever the list does.
    pub fn outbox_snapshot(&self) -> (u64, Vec<OutboxItem>) {
        lock(&self.outbox_view).clone()
    }

    /// Reads the outbox from disk into the list the window shows.
    pub fn refresh_outbox(&self) {
        let Ok(listing) = self.outbox.list() else {
            return;
        };
        let mut g = lock(&self.outbox_view);
        if g.1 != listing.items {
            g.0 += 1;
            g.1 = listing.items;
        }
    }

    pub fn wake_sender(&self) {
        self.send_wake.notify_one();
    }

    /// Starts the background loop that delivers queued messages. It holds the
    /// program's services only while it works, so it ends when they are dropped.
    pub fn start_sender(self: &Arc<Self>) {
        let weak: Weak<Self> = Arc::downgrade(self);
        let wake = Arc::clone(&self.send_wake);
        drop(self.worker.run(move || async move {
            loop {
                let Some(me) = weak.upgrade() else {
                    return;
                };
                me.send_pass().await;
                drop(me);
                tokio::select! {
                    () = wake.notified() => {}
                    () = tokio::time::sleep(SEND_POLL) => {}
                }
            }
        }));
    }

    async fn send_pass(&self) {
        let now = unix_now();
        let accounts = self.accounts();
        let env = self.env();
        if let Err(e) = sender::process_due(&env, &accounts, &self.outbox, now).await {
            tracing::warn!("sending pass failed: {e}");
        }
        for a in &accounts {
            if let Err(e) = sender::save_sent_copies(&env, a, &self.outbox, now).await {
                tracing::warn!("saving sent copies failed: {e}");
            }
        }
        let _ = self.outbox.purge_finished(now, KEEP_FINISHED_SECS);
        self.refresh_outbox();
    }

    /// Prepares the message and writes it to the outbox (disk first, network later).
    /// Blocking: call from a background task.
    pub fn queue_message(
        &self,
        account_id: &str,
        mut msg: OutgoingMessage,
        delay_secs: u32,
    ) -> Result<OutboxItem, String> {
        let account = self
            .account(account_id)
            .ok_or_else(|| "This account no longer exists.".to_owned())?;
        msg.from = Some(Mailbox {
            name: account.display_name.clone(),
            address: account.email.clone(),
        });
        let now = unix_now();
        let composed = compose(&msg, now).map_err(|e| describe_compose_error(&e))?;
        let item = self
            .outbox
            .enqueue_after(&account.id, &composed, now, wants_sent_copy(&account), delay_secs)
            .map_err(|e| {
                format!("The message could not be saved for sending: {e}. Check that the disk is not full.")
            })?;
        self.refresh_outbox();
        self.wake_sender();
        if delay_secs > 0 {
            // The sending loop only checks now and then; wake it when the time has come.
            let wake = Arc::clone(&self.send_wake);
            let wait = std::time::Duration::from_secs(u64::from(delay_secs.min(300)) + 1);
            drop(self.worker.run(move || async move {
                tokio::time::sleep(wait).await;
                wake.notify_one();
            }));
        }
        Ok(item)
    }

    /// Tries again right now (blocking).
    pub fn retry_outbox(&self, id: &str) -> Result<(), String> {
        self.outbox
            .retry_now(id, unix_now())
            .map_err(|e| e.to_string())?;
        self.refresh_outbox();
        self.wake_sender();
        Ok(())
    }

    /// Takes a message back out of the queue (blocking). Returns its text so the
    /// person can edit it again.
    pub fn cancel_outbox(&self, id: &str) -> Result<(), String> {
        self.outbox.cancel(id).map_err(|e| e.to_string())?;
        self.refresh_outbox();
        Ok(())
    }

    /// Removes a finished entry from the list (blocking).
    pub fn forget_outbox(&self, id: &str) -> Result<(), String> {
        self.outbox.forget(id).map_err(|e| e.to_string())?;
        self.refresh_outbox();
        Ok(())
    }

    /// Re-queues what was parked for this account after its password was fixed.
    pub fn retry_account_outbox(&self, account_id: &str) {
        if let Some(a) = self.account(account_id)
            && self.outbox.retry_account(&a.id, unix_now()).unwrap_or(0) > 0
        {
            self.refresh_outbox();
            self.wake_sender();
        }
    }

    // ---- drafts (all blocking) ------------------------------------------

    pub fn new_draft_id() -> String {
        mailcore::drafts::Drafts::new_draft_id()
    }

    pub fn save_draft(
        &self,
        id: Option<&str>,
        account_id: Option<&str>,
        msg: &OutgoingMessage,
    ) -> Result<String, String> {
        let account = account_id.and_then(|a| self.account(a)).map(|a| a.id);
        self.drafts
            .save(id, account.as_ref(), msg, unix_now())
            .map_err(|e| format!("The draft could not be saved: {e}"))
    }

    pub fn load_draft(&self, id: &str) -> Result<Draft, String> {
        self.drafts
            .load(id)
            .map_err(|e| format!("The draft could not be opened: {e}"))
    }

    pub fn list_drafts(&self) -> Vec<DraftSummary> {
        self.drafts.list().unwrap_or_default()
    }

    pub fn delete_draft(&self, id: &str) {
        if let Err(e) = self.drafts.delete(id) {
            tracing::warn!("could not delete a draft: {e}");
        }
    }

    // ---- attachments of received messages --------------------------------

    /// The bytes of one attachment (downloads the message first if needed).
    pub fn read_attachment(
        self: &Arc<Self>,
        message_id: i64,
        index: usize,
    ) -> impl std::future::Future<Output = Result<AttachmentData, String>> + Send + use<> {
        let me = Arc::clone(self);
        let job = self.worker.run(move || async move {
            let raw = me
                .raw_message(message_id)
                .await
                .map_err(super::describe_sync_error)?;
            tokio::task::spawn_blocking(move || {
                render::extract_attachment(&raw, index).map_err(|e| e.to_string())
            })
            .await
            .map_err(|e| e.to_string())?
        });
        async move {
            job.await
                .unwrap_or_else(|| Err("The mail engine stopped unexpectedly.".to_owned()))
        }
    }

    /// Every attachment of a message, for forwarding. Returns what fits within the
    /// size limit and how many were left out.
    pub fn forward_attachments(
        self: &Arc<Self>,
        message_id: i64,
    ) -> impl std::future::Future<Output = Result<(Vec<Attachment>, usize), String>> + Send + use<>
    {
        let me = Arc::clone(self);
        let job = self.worker.run(move || async move {
            let raw = me
                .raw_message(message_id)
                .await
                .map_err(super::describe_sync_error)?;
            tokio::task::spawn_blocking(move || collect_attachments(&raw))
                .await
                .map_err(|e| e.to_string())
        });
        async move {
            job.await
                .unwrap_or_else(|| Err("The mail engine stopped unexpectedly.".to_owned()))
        }
    }
}

/// Pulls the attachments out of a raw message, stopping before the total gets too big.
pub fn collect_attachments(raw: &[u8]) -> (Vec<Attachment>, usize) {
    let Ok(rendered) = render::render(raw, &RenderOptions::default()) else {
        return (Vec::new(), 0);
    };
    let mut kept = Vec::new();
    let mut total = 0usize;
    let mut skipped = 0usize;
    for info in &rendered.attachments {
        if total + info.size > MAX_ATTACHMENT_BYTES {
            skipped += 1;
            continue;
        }
        match render::extract_attachment(raw, info.index) {
            Ok(a) => {
                total += a.data.len();
                kept.push(Attachment {
                    filename: a.filename,
                    content_type: a.content_type,
                    data: a.data,
                });
            }
            Err(_) => skipped += 1,
        }
    }
    (kept, skipped)
}
