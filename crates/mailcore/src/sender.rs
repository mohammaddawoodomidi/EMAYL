//! Delivers what is waiting in the outbox, and files a copy in the Sent folder.
//!
//! One SMTP connection is opened per account and reused for every waiting
//! message of that account. If the connection breaks, the message being sent is
//! scheduled for a retry and the others simply stay queued for the next pass.

use std::sync::Arc;

use rustls::ClientConfig;

use crate::Result;
use crate::account::Account;
use crate::email::EmailAddress;
use crate::imap::account_connect::with_credentials;
use crate::imap::{FolderRole, ImapSession, connect_account};
use crate::net::{ConnError, FailureKind, Stage, Timeouts};
use crate::oauth::{OAuthClients, TokenManager, TokenTransport};
use crate::outbox::{AttemptResult, Outbox, OutboxItem, OutboxState, SentCopy};
use crate::retry::random_unit;
use crate::secrets::SecretStore;
use crate::smtp::{SmtpParams, connect};

/// How long a Sent-folder copy is retried before we stop trying (the message
/// itself was delivered, so this is only a convenience).
pub const SENT_COPY_GIVE_UP_SECS: i64 = 24 * 3600;

/// Everything needed to talk to servers.
pub struct Env<'a, T: TokenTransport> {
    pub secrets: &'a Arc<dyn SecretStore>,
    pub tokens: &'a TokenManager<T>,
    pub clients: &'a OAuthClients,
    pub tls: &'a Arc<ClientConfig>,
    pub timeouts: Timeouts,
}

/// Whether we should file our own copy in the Sent folder after sending.
/// Gmail saves sent mail by itself, so a second copy would be a duplicate; for
/// every other provider we check first and only add one if it is missing.
/// (Gmail behaviour: recalled / third-party sources, not tested against Gmail.)
pub fn wants_sent_copy(account: &Account) -> bool {
    let host = account.smtp.host.to_ascii_lowercase();
    !(host == "smtp.gmail.com" || host == "smtp.googlemail.com")
}

impl<T: TokenTransport> std::fmt::Debug for Env<'_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Env").finish_non_exhaustive()
    }
}

fn parked_when_missing_account() -> ConnError {
    ConnError::new(
        Stage::Login,
        FailureKind::AuthMethodUnsupported,
        "the account this message belongs to no longer exists",
    )
}

fn now_jitter() -> f64 {
    random_unit()
}

/// Sends everything due for `account`. Returns the items touched.
pub async fn process_account<T: TokenTransport>(
    env: &Env<'_, T>,
    account: &Account,
    outbox: &Outbox,
    now: i64,
) -> Result<Vec<OutboxItem>> {
    let due: Vec<OutboxItem> = outbox
        .due(now)?
        .into_iter()
        .filter(|i| i.account == account.id)
        .collect();
    if due.is_empty() {
        return Ok(Vec::new());
    }
    let params = SmtpParams {
        server: &account.smtp,
        username: &account.smtp_username,
        ehlo_name: "localhost",
        timeouts: env.timeouts,
    };
    let session = with_credentials(
        account,
        env.secrets,
        env.tokens,
        env.clients,
        async |creds| connect(&params, creds, env.tls).await,
    )
    .await;

    let mut done = Vec::new();
    let mut session = match session {
        Ok(s) => s,
        Err(e) => {
            // Same failure for every waiting message of this account.
            for d in &due {
                outbox.begin_attempt(&d.id)?;
                done.push(outbox.finish_attempt(
                    &d.id,
                    &AttemptResult::Connection(e.clone()),
                    now,
                    now_jitter(),
                )?);
            }
            return Ok(done);
        }
    };

    for d in &due {
        let item = outbox.begin_attempt(&d.id)?;
        let result = match prepare(&item, outbox) {
            Ok((from, rcpts, bytes)) => session.send(&from, &rcpts, &bytes).await.into(),
            Err(reason) => AttemptResult::Unsendable(reason),
        };
        let dead = matches!(result, AttemptResult::Connection(_));
        done.push(outbox.finish_attempt(&d.id, &result, now, now_jitter())?);
        if dead {
            // The connection is gone; the rest wait for the next pass.
            return Ok(done);
        }
    }
    session.quit().await;
    Ok(done)
}

fn prepare(
    item: &OutboxItem,
    outbox: &Outbox,
) -> std::result::Result<(EmailAddress, Vec<EmailAddress>, Vec<u8>), String> {
    let from = EmailAddress::parse(&item.from).map_err(|e| format!("sender address: {e}"))?;
    let rcpts = item
        .pending_recipients()
        .iter()
        .map(|a| EmailAddress::parse(a).map_err(|e| format!("recipient address: {e}")))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let bytes = outbox
        .message_bytes(&item.id)
        .map_err(|e| format!("stored message: {e}"))?;
    Ok((from, rcpts, bytes))
}

/// Sends what is due for every account (one account after the other).
pub async fn process_due<T: TokenTransport>(
    env: &Env<'_, T>,
    accounts: &[Account],
    outbox: &Outbox,
    now: i64,
) -> Result<Vec<OutboxItem>> {
    let mut all = Vec::new();
    for account in accounts {
        all.extend(process_account(env, account, outbox, now).await?);
    }
    // Messages whose account was removed cannot be sent: park them visibly.
    for item in outbox.due(now)? {
        if !accounts.iter().any(|a| a.id == item.account) {
            outbox.begin_attempt(&item.id)?;
            let r = AttemptResult::Connection(parked_when_missing_account());
            all.push(outbox.finish_attempt(&item.id, &r, now, now_jitter())?);
        }
    }
    Ok(all)
}

// ---------------------------------------------------------------------------
// Sent-folder copies
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    AlreadyThere,
    Appended,
    /// The account has no folder we recognise as "Sent".
    NoSentFolder,
}

/// Makes sure a copy of the message is in the Sent folder: looks for the
/// Message-ID first, and appends (marked read) only if it is not there.
pub async fn ensure_in_sent(
    session: &mut ImapSession,
    message_id: &str,
    bytes: &[u8],
) -> std::result::Result<Stored, ConnError> {
    let folders = session.list_folders().await?;
    let Some(sent) = folders
        .iter()
        .find(|f| f.role == FolderRole::Sent && f.selectable)
    else {
        return Ok(Stored::NoSentFolder);
    };
    session.select(&sent.raw_name).await?;
    if !session.uid_search_message_id(message_id).await?.is_empty() {
        return Ok(Stored::AlreadyThere);
    }
    session.append(&sent.raw_name, &["\\Seen"], bytes).await?;
    Ok(Stored::Appended)
}

/// Decides what to record after one Sent-copy attempt. Pure, so it is tested directly.
pub fn decide_sent_copy(
    result: &std::result::Result<Stored, ConnError>,
    sent_at: i64,
    now: i64,
) -> SentCopy {
    match result {
        Ok(Stored::AlreadyThere | Stored::Appended) => SentCopy::Saved,
        Ok(Stored::NoSentFolder) => SentCopy::GaveUp,
        Err(e) => {
            let hopeless = e.kind == FailureKind::Protocol; // the server said no
            if hopeless || now - sent_at > SENT_COPY_GIVE_UP_SECS {
                SentCopy::GaveUp
            } else {
                SentCopy::Pending
            }
        }
    }
}

/// Files the Sent copies still owed for `account`. Returns how many were saved.
pub async fn save_sent_copies<T: TokenTransport>(
    env: &Env<'_, T>,
    account: &Account,
    outbox: &Outbox,
    now: i64,
) -> Result<usize> {
    let waiting: Vec<OutboxItem> = outbox
        .list()?
        .items
        .into_iter()
        .filter(|i| {
            i.account == account.id
                && i.state != OutboxState::Failed
                && i.sent_copy == SentCopy::Pending
                && i.sent_at.is_some()
        })
        .collect();
    if waiting.is_empty() {
        return Ok(0);
    }
    let mut session = match connect_account(
        account,
        env.secrets,
        env.tokens,
        env.clients,
        env.tls,
        env.timeouts,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            for item in &waiting {
                let verdict = decide_sent_copy(&Err(e.clone()), item.sent_at.unwrap_or(now), now);
                if verdict != SentCopy::Pending {
                    outbox.set_sent_copy(&item.id, verdict)?;
                }
            }
            return Ok(0);
        }
    };
    let mut saved = 0;
    for item in &waiting {
        let bytes = match outbox.message_bytes(&item.id) {
            Ok(b) => b,
            Err(_) => {
                outbox.set_sent_copy(&item.id, SentCopy::GaveUp)?;
                continue;
            }
        };
        let result = ensure_in_sent(&mut session, &item.message_id, &bytes).await;
        let verdict = decide_sent_copy(&result, item.sent_at.unwrap_or(now), now);
        if verdict == SentCopy::Saved {
            saved += 1;
        }
        if verdict != SentCopy::Pending {
            outbox.set_sent_copy(&item.id, verdict)?;
        }
        if result.is_err() {
            break; // the connection is probably unusable; try again next time
        }
    }
    session.logout().await;
    Ok(saved)
}

#[cfg(test)]
mod tests;
