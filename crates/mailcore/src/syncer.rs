//! Keeps the local database ([`crate::store`]) in step with an IMAP account.
//!
//! The rules follow RFC 4549 ("disconnected IMAP"): the server is the truth,
//! the local copy is a cache, and the only thing that exists solely here is the
//! queue of actions waiting to be sent (see `store::pending`).
//!
//! One pass ([`sync_session`]) does, in order:
//!
//! 1. read the folder list and record it;
//! 2. send waiting actions (read/starred/move/delete) to the server;
//! 3. for each folder: compare `UIDVALIDITY`/`UIDNEXT`/`HIGHESTMODSEQ` with what
//!    was saved ([`crate::imap::sync::plan`]), download headers of new mail
//!    (newest first), notice messages that disappeared, and copy flag changes.
//!
//! A first download only takes the newest [`SyncOptions::initial_window`]
//! messages of each folder; older ones come on request ([`load_older`]).
//! Message bodies are downloaded when opened ([`fetch_body`]).
//!
//! A folder's saved state is updated only after its messages are stored, so an
//! interrupted pass is simply repeated (storing is idempotent).
//!
//! The background loop ([`run_account_loop`]) keeps one connection open and
//! waits with IDLE; it backs off after failures and never retries a rejected
//! password on its own (repeated wrong logins can lock an account).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{Notify, watch};

use crate::Error;
use crate::account::Account;
use crate::imap::sync::{self as plan_mod, FlagSync, LocalFolderState, SyncPlan};
use crate::imap::{FlagChange, FolderRole, ImapSession, MessageFlags, connect_account};
use crate::net::{ConnError, FailureKind};
use crate::oauth::TokenTransport;
use crate::retry::{RetryPolicy, random_unit};
use crate::rules::{Rule, RuleAction, decide};
use crate::sender::Env;
use crate::store::{FolderRow, FolderState, MessageRow, NewMessage, OpKind, Store};

/// Server errors that mean "this folder or command was refused" rather than
/// "the connection or login is broken".
fn is_refusal(e: &ConnError) -> bool {
    e.kind == FailureKind::Protocol
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("{0}")]
    Server(#[from] ConnError),
    #[error("{0}")]
    Store(Error),
    #[error("the message is larger than the {0} byte download limit")]
    TooLarge(usize),
    #[error("the message no longer exists on the server")]
    Gone,
    #[error("the folder was renumbered on the server; refresh and try again")]
    FolderChanged,
}

impl From<Error> for SyncError {
    fn from(e: Error) -> Self {
        Self::Store(e)
    }
}

#[derive(Debug, Clone)]
pub struct SyncOptions {
    /// Newest messages fetched from the inbox on the first download.
    pub initial_window: usize,
    /// Newest messages fetched from every other folder on the first download.
    /// Kept small so the first pass ends quickly; the rest loads on request.
    pub other_window: usize,
    /// Headers requested per round trip.
    pub batch_size: usize,
    /// Gmail-style "All Mail" folders hold a copy of everything; skipped by default.
    pub include_all_mail: bool,
    pub max_message_bytes: usize,
    /// How long one IDLE lasts before it is renewed (servers drop idle clients
    /// after about 30 minutes; RFC 2177 says to renew before 29).
    pub idle_timeout: Duration,
    /// Used instead of IDLE when the server has none.
    pub poll_interval: Duration,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            initial_window: 1000,
            other_window: 200,
            batch_size: 100,
            include_all_mail: false,
            max_message_bytes: crate::store::MAX_CACHED_MESSAGE_BYTES,
            idle_timeout: Duration::from_secs(25 * 60),
            poll_interval: Duration::from_secs(5 * 60),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderProblem {
    pub folder: String,
    pub error: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncReport {
    pub folders_synced: usize,
    pub new_messages: usize,
    pub removed_messages: usize,
    pub flag_changes: usize,
    /// New messages a filter rule did something with.
    pub filtered: usize,
    pub actions_sent: usize,
    pub actions_dropped: usize,
    /// Folders the server renumbered (their cache was rebuilt).
    pub rebuilt_folders: Vec<String>,
    /// Folders that could not be synced this time (others were).
    pub folder_problems: Vec<FolderProblem>,
}

/// How far a pass has got; shown while the first download runs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncProgress {
    /// The folder being downloaded (empty before the first one starts).
    pub folder: String,
    pub folders_done: usize,
    pub folders_total: usize,
    /// Messages stored so far from this folder, and how many it will take.
    pub messages_done: usize,
    pub messages_total: usize,
    /// Every message stored since the pass began (changes whenever mail arrives).
    pub stored_total: usize,
}

/// Receives progress; the loop turns it into [`SyncStatus::Syncing`].
type ProgressSink<'a> = &'a mut (dyn FnMut(&SyncProgress) + Send);

struct Tracker<'a> {
    now: SyncProgress,
    sink: ProgressSink<'a>,
}

impl Tracker<'_> {
    fn publish(&mut self) {
        (self.sink)(&self.now);
    }
}

/// The order folders are downloaded in: what people open first comes first, and
/// the folders they rarely open (spam, bin) come last.
fn sync_priority(role: FolderRole) -> u8 {
    match role {
        FolderRole::Inbox => 0,
        FolderRole::Sent => 1,
        FolderRole::Drafts => 2,
        FolderRole::Archive => 3,
        FolderRole::Other => 4,
        FolderRole::Junk => 5,
        FolderRole::Trash => 6,
        FolderRole::All | FolderRole::Flagged => 7,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncScope {
    /// Every folder.
    All,
    /// Only the inbox (after an IDLE wake-up).
    InboxOnly,
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

/// Runs a blocking store call off the async threads.
async fn db<R: Send + 'static>(
    store: &Arc<Store>,
    f: impl FnOnce(&Store) -> crate::Result<R> + Send + 'static,
) -> Result<R, SyncError> {
    let s = Arc::clone(store);
    tokio::task::spawn_blocking(move || f(&s))
        .await
        .map_err(|e| SyncError::Store(Error::Database(format!("background task failed: {e}"))))?
        .map_err(SyncError::Store)
}

fn should_sync(f: &FolderRow, opts: &SyncOptions) -> bool {
    f.selectable
        && f.role != FolderRole::Flagged
        && (f.role != FolderRole::All || opts.include_all_mail)
}

/// Connects and runs one full pass.
pub async fn sync_account<T: TokenTransport>(
    env: &Env<'_, T>,
    account: &Account,
    store: &Arc<Store>,
    opts: &SyncOptions,
) -> Result<SyncReport, SyncError> {
    let mut session = connect_account(
        account,
        env.secrets,
        env.tokens,
        env.clients,
        env.tls,
        env.timeouts,
    )
    .await?;
    let r = sync_session(
        &mut session,
        account.id.as_str(),
        store,
        opts,
        SyncScope::All,
    )
    .await;
    session.logout().await;
    r
}

/// One pass over an open session.
pub async fn sync_session(
    session: &mut ImapSession,
    account: &str,
    store: &Arc<Store>,
    opts: &SyncOptions,
    scope: SyncScope,
) -> Result<SyncReport, SyncError> {
    sync_session_reporting(session, account, store, opts, scope, &mut |_| {}).await
}

/// [`sync_session`], telling `progress` how far it has got after every batch.
pub async fn sync_session_reporting(
    session: &mut ImapSession,
    account: &str,
    store: &Arc<Store>,
    opts: &SyncOptions,
    scope: SyncScope,
    progress: ProgressSink<'_>,
) -> Result<SyncReport, SyncError> {
    let mut report = SyncReport::default();
    let mut track = Tracker {
        now: SyncProgress::default(),
        sink: progress,
    };
    let acct = account.to_owned();
    let rows = if scope == SyncScope::All {
        let listed = session.list_folders().await?;
        db(store, move |s| s.upsert_folders(&acct, &listed, true)).await?
    } else {
        db(store, move |s| s.folders(&acct)).await?
    };

    // 1. Waiting actions first, so the server agrees with what the person did.
    let acct = account.to_owned();
    for fid in db(store, move |s| s.folders_with_pending(&acct)).await? {
        let Some(row) = rows.iter().find(|r| r.id == fid) else {
            continue;
        };
        match replay_folder(session, store, row, &mut report).await {
            Ok(()) => {}
            Err(SyncError::Server(e)) if is_refusal(&e) => {
                report.folder_problems.push(FolderProblem {
                    folder: row.display_name.clone(),
                    error: e.to_string(),
                })
            }
            Err(e) => return Err(e),
        }
    }

    // 2. Folders, the ones people open first before the rest.
    let mut todo: Vec<&FolderRow> = rows
        .iter()
        .filter(|r| should_sync(r, opts))
        .filter(|r| scope == SyncScope::All || r.role == FolderRole::Inbox)
        .collect();
    todo.sort_by_key(|r| sync_priority(r.role));
    track.now.folders_total = todo.len();
    track.publish();
    for row in todo {
        track.now.folder = row.display_name.clone();
        track.now.messages_done = 0;
        track.now.messages_total = 0;
        track.publish();
        let result = sync_folder(session, store, row.id, opts, &mut report, &mut track).await;
        track.now.folders_done += 1;
        track.publish();
        match result {
            Ok(()) => report.folders_synced += 1,
            Err(SyncError::Server(e)) if is_refusal(&e) => {
                report.folder_problems.push(FolderProblem {
                    folder: row.display_name.clone(),
                    error: e.to_string(),
                })
            }
            Err(e) => return Err(e),
        }
    }
    Ok(report)
}

async fn replay_folder(
    session: &mut ImapSession,
    store: &Arc<Store>,
    row: &FolderRow,
    report: &mut SyncReport,
) -> Result<(), SyncError> {
    let fid = row.id;
    let ops = db(store, move |s| s.pending_for_folder(fid)).await?;
    if ops.is_empty() {
        return Ok(());
    }
    let remote = session.select(&row.raw_name).await?;
    if row.uid_validity != Some(remote.uid_validity) {
        // The UIDs these actions name no longer exist; the folder sync that
        // follows rebuilds the folder and drops them.
        return Ok(());
    }
    for op in ops {
        let uid = op.uid.to_string();
        let res = match op.kind {
            OpKind::AddFlag => session.change_flag(&uid, &op.arg, FlagChange::Add).await,
            OpKind::RemoveFlag => session.change_flag(&uid, &op.arg, FlagChange::Remove).await,
            OpKind::Move => session.move_to(&uid, &op.arg).await.map(|_| ()),
            OpKind::Expunge => session.expunge_uids(&uid).await.map(|_| ()),
        };
        match res {
            Ok(()) => {
                db(store, move |s| s.complete_op(op.id)).await?;
                report.actions_sent += 1;
            }
            Err(e) if is_refusal(&e) => {
                // The server said no (target folder missing, message gone...).
                let text = e.to_string();
                let dropped = db(store, move |s| s.fail_op(op.id, &text, 3)).await?;
                if dropped {
                    report.actions_dropped += 1;
                }
            }
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Downloads the headers of `uids` (newest first, in batches) and stores them.
/// Only messages that were asked for are kept, whatever the server sends.
async fn fetch_and_store(
    session: &mut ImapSession,
    store: &Arc<Store>,
    folder_id: i64,
    uids: &[u32],
    batch: usize,
    track: &mut Tracker<'_>,
) -> Result<usize, SyncError> {
    let wanted: HashSet<u32> = uids.iter().copied().collect();
    let mut stored = 0;
    track.now.messages_done = 0;
    track.now.messages_total = uids.len();
    track.publish();
    for set in plan_mod::newest_first_batches(uids, batch) {
        let sums = session.fetch_headers(&set).await?;
        let msgs: Vec<NewMessage> = sums
            .iter()
            .filter(|s| wanted.contains(&s.uid))
            .map(NewMessage::from_summary)
            .collect();
        let n = db(store, move |s| s.insert_messages(folder_id, &msgs)).await?;
        stored += n;
        track.now.messages_done = (track.now.messages_done + sums.len()).min(uids.len());
        track.now.stored_total += n;
        track.publish();
    }
    Ok(stored)
}

async fn blocked_uids(store: &Arc<Store>, folder_id: i64) -> Result<HashSet<u32>, SyncError> {
    let ops = db(store, move |s| s.pending_for_folder(folder_id)).await?;
    Ok(ops
        .into_iter()
        .filter(|o| matches!(o.kind, OpKind::Move | OpKind::Expunge))
        .map(|o| o.uid)
        .collect())
}

async fn sync_folder(
    session: &mut ImapSession,
    store: &Arc<Store>,
    folder_id: i64,
    opts: &SyncOptions,
    report: &mut SyncReport,
    track: &mut Tracker<'_>,
) -> Result<(), SyncError> {
    let Some(mut row) = db(store, move |s| s.folder(folder_id)).await? else {
        return Ok(());
    };
    let remote = session.select(&row.raw_name).await?;
    let local = match (row.uid_validity, row.uid_next) {
        (Some(v), Some(n)) => Some(LocalFolderState {
            uid_validity: v,
            uid_next: n,
            highest_modseq: row.highest_modseq,
        }),
        _ => None,
    };
    let mut plan = plan_mod::plan(local, remote);
    if plan == SyncPlan::Reset {
        db(store, move |s| s.reset_folder(folder_id)).await?;
        report.rebuilt_folders.push(row.display_name.clone());
        row.floor_uid = 1;
        plan = SyncPlan::Initial;
    }
    let local_uids = db(store, move |s| s.uids_in_folder(folder_id)).await?;
    let blocked = blocked_uids(store, folder_id).await?;
    let mut floor = row.floor_uid.max(1);

    match plan {
        SyncPlan::Reset => {}
        SyncPlan::Initial => {
            let mut all = session.uid_search_all().await?;
            all.sort_unstable();
            all.dedup();
            let size = if row.role == FolderRole::Inbox {
                opts.initial_window
            } else {
                opts.other_window
            };
            let window: Vec<u32> = all.iter().rev().take(size).copied().collect();
            floor = if all.len() <= size {
                1
            } else {
                window.last().copied().unwrap_or(1)
            };
            let have: HashSet<u32> = local_uids.iter().copied().collect();
            let added: Vec<u32> = window
                .into_iter()
                .filter(|u| !have.contains(u) && !blocked.contains(u))
                .collect();
            report.new_messages +=
                fetch_and_store(session, store, folder_id, &added, opts.batch_size, track).await?;
        }
        SyncPlan::Incremental { new_from, flags } => {
            let need_search = new_from.is_some()
                || flags != FlagSync::Skip
                || floor > 1
                || usize::try_from(remote.exists).unwrap_or(usize::MAX) != local_uids.len();
            if need_search {
                let mut all = session.uid_search_all().await?;
                all.sort_unstable();
                all.dedup();
                let window: Vec<u32> = all.iter().copied().filter(|u| *u >= floor).collect();
                let diff = plan_mod::diff_uids(&local_uids, &window);
                let added: Vec<u32> = diff
                    .added
                    .into_iter()
                    .filter(|u| !blocked.contains(u))
                    .collect();
                let new_uids = added.clone();
                report.new_messages +=
                    fetch_and_store(session, store, folder_id, &added, opts.batch_size, track)
                        .await?;
                if row.role == FolderRole::Inbox && !new_uids.is_empty() {
                    report.filtered += db(store, move |s| {
                        Ok(apply_rules_to_uids(s, folder_id, &new_uids))
                    })
                    .await?;
                }
                // A cached message is "removed" only if it is absent from the whole
                // server list (not merely below the window).
                let on_server: HashSet<u32> = all.into_iter().collect();
                let gone: Vec<u32> = local_uids
                    .iter()
                    .copied()
                    .filter(|u| !on_server.contains(u))
                    .collect();
                if !gone.is_empty() {
                    report.removed_messages +=
                        db(store, move |s| s.delete_uids(folder_id, &gone)).await?;
                }
            }
            let since = match flags {
                FlagSync::Skip => None,
                FlagSync::ChangedSince(m) => Some(Some(m)),
                FlagSync::Full => Some(None),
            };
            if let Some(since) = since {
                let updates: Vec<(u32, MessageFlags)> = session
                    .fetch_flags(since)
                    .await?
                    .into_iter()
                    .map(|u| (u.uid, u.flags))
                    .collect();
                report.flag_changes +=
                    db(store, move |s| s.apply_flag_updates(folder_id, &updates)).await?;
            }
        }
    }

    let state = FolderState {
        uid_validity: remote.uid_validity,
        uid_next: remote.uid_next,
        highest_modseq: remote.highest_modseq,
        floor_uid: floor,
        last_synced: now_secs(),
    };
    db(store, move |s| s.set_folder_state(folder_id, state)).await?;
    Ok(())
}

/// Fetches up to `count` older messages of a folder that the first download left out.
/// Returns how many were added (0 means nothing older is left).
pub async fn load_older(
    session: &mut ImapSession,
    store: &Arc<Store>,
    folder_id: i64,
    count: usize,
    opts: &SyncOptions,
) -> Result<usize, SyncError> {
    let Some(row) = db(store, move |s| s.folder(folder_id)).await? else {
        return Err(SyncError::Store(Error::NotFound("folder".into())));
    };
    let (Some(validity), Some(next)) = (row.uid_validity, row.uid_next) else {
        return Err(SyncError::FolderChanged);
    };
    if row.floor_uid <= 1 {
        return Ok(0);
    }
    let remote = session.select(&row.raw_name).await?;
    if remote.uid_validity != validity {
        return Err(SyncError::FolderChanged);
    }
    let mut all = session.uid_search_all().await?;
    all.sort_unstable();
    all.dedup();
    let older: Vec<u32> = all
        .iter()
        .rev()
        .copied()
        .filter(|u| *u < row.floor_uid)
        .collect();
    let take: Vec<u32> = older.iter().take(count.max(1)).copied().collect();
    let new_floor = if older.len() <= take.len() {
        1
    } else {
        take.last().copied().unwrap_or(1)
    };
    let mut quiet = |_: &SyncProgress| {};
    let mut track = Tracker {
        now: SyncProgress::default(),
        sink: &mut quiet,
    };
    let added = fetch_and_store(
        session,
        store,
        folder_id,
        &take,
        opts.batch_size,
        &mut track,
    )
    .await?;
    let state = FolderState {
        uid_validity: validity,
        uid_next: next,
        highest_modseq: row.highest_modseq,
        floor_uid: new_floor,
        last_synced: row.last_synced.unwrap_or_else(now_secs),
    };
    db(store, move |s| s.set_folder_state(folder_id, state)).await?;
    Ok(added)
}

/// Background download of the newest Inbox messages: how many, the largest one, the total.
pub const PREFETCH_COUNT: usize = 20;
pub const PREFETCH_MAX_MESSAGE_BYTES: u32 = 600 * 1024;
pub const PREFETCH_TOTAL_BYTES: u64 = 8 * 1024 * 1024;
/// Assumed size of a message whose size the server did not tell us.
const PREFETCH_UNKNOWN_SIZE: u64 = 100 * 1024;

/// Which messages of an account to download ahead of time so that opening them is instant:
/// the newest Inbox messages whose text is not stored yet, none large, within a total budget
/// (a message that does not fit is skipped, a smaller one after it may still fit).
pub fn prefetch_plan(store: &Store, account: &str) -> Vec<i64> {
    let mut chosen = Vec::new();
    let mut budget = PREFETCH_TOTAL_BYTES;
    for f in store.inbox_folders().unwrap_or_default() {
        if f.account != account {
            continue;
        }
        let ids = store
            .uncached_recent(f.id, PREFETCH_MAX_MESSAGE_BYTES, PREFETCH_COUNT)
            .unwrap_or_default();
        for id in ids {
            let size = store
                .message(id)
                .ok()
                .flatten()
                .and_then(|m| m.size)
                .map_or(PREFETCH_UNKNOWN_SIZE, u64::from);
            if size <= budget {
                budget -= size;
                chosen.push(id);
            }
        }
    }
    chosen
}

/// Returns the raw message, downloading it first if it is not cached.
pub async fn fetch_body(
    session: &mut ImapSession,
    store: &Arc<Store>,
    message_id: i64,
    opts: &SyncOptions,
) -> Result<Vec<u8>, SyncError> {
    if let Some(raw) = db(store, move |s| s.body(message_id)).await? {
        return Ok(raw);
    }
    let Some(msg) = db(store, move |s| s.message(message_id)).await? else {
        return Err(SyncError::Gone);
    };
    // The size from the list is checked first: the library reads a message completely
    // into memory, so a hostile or huge one must be refused before it is requested.
    if msg
        .size
        .is_some_and(|sz| usize::try_from(sz).unwrap_or(usize::MAX) > opts.max_message_bytes)
    {
        return Err(SyncError::TooLarge(opts.max_message_bytes));
    }
    let folder_id = msg.folder_id;
    let Some(row) = db(store, move |s| s.folder(folder_id)).await? else {
        return Err(SyncError::Gone);
    };
    let remote = session.select(&row.raw_name).await?;
    if row.uid_validity != Some(remote.uid_validity) {
        return Err(SyncError::FolderChanged);
    }
    let Some(raw) = session
        .fetch_message(msg.uid, opts.max_message_bytes)
        .await?
    else {
        let uid = msg.uid;
        db(store, move |s| s.delete_uids(folder_id, &[uid])).await?;
        return Err(SyncError::Gone);
    };
    let copy = raw.clone();
    db(store, move |s| s.store_body(message_id, &copy)).await?;
    Ok(raw)
}

// ---------------------------------------------------------------------------
// What the person does to a message. Each call changes the local copy at once
// and queues the server update; call `SyncHandle::wake` afterwards.
// ---------------------------------------------------------------------------

/// Does what the rules say to one message. Returns whether anything was done. A problem with
/// one step (for example, no Junk folder on this account) never stops the others.
pub fn apply_rules_to_message(store: &Store, rules: &[Rule], m: &MessageRow) -> bool {
    let d = decide(rules, m);
    if d.is_empty() {
        return false;
    }
    let mut done = false;
    if d.mark_read && m.is_unread() {
        done |= set_read(store, m, true).is_ok();
    }
    if d.star && !m.is_flagged() {
        done |= set_starred(store, m, true).is_ok();
    }
    match d.move_to {
        Some(RuleAction::Junk) => done |= mark_spam(store, m).is_ok(),
        Some(RuleAction::Archive) => done |= archive(store, m).is_ok(),
        _ => {}
    }
    done
}

/// Applies the stored rules to the given messages of an Inbox. Returns how many were changed.
pub fn apply_rules_to_uids(store: &Store, folder_id: i64, uids: &[u32]) -> usize {
    let rules = store.rules();
    if rules.is_empty() {
        return 0;
    }
    uids.iter()
        .filter_map(|u| store.message_by_uid(folder_id, *u).ok().flatten())
        .filter(|m| apply_rules_to_message(store, &rules, m))
        .count()
}

/// "Apply now": runs the rules over what is already in the Inbox of every account.
/// Returns how many messages were changed.
pub fn apply_rules_to_inboxes(store: &Store) -> crate::Result<usize> {
    let rules = store.rules();
    if rules.is_empty() {
        return Ok(0);
    }
    let mut changed = 0;
    for folder in store.inbox_folders()? {
        for uid in store.uids_in_folder(folder.id)? {
            if let Some(m) = store.message_by_uid(folder.id, uid)?
                && apply_rules_to_message(store, &rules, &m)
            {
                changed += 1;
            }
        }
    }
    Ok(changed)
}

pub fn set_read(store: &Store, m: &MessageRow, read: bool) -> crate::Result<()> {
    store.queue_flag(m.folder_id, m.uid, "\\Seen", read)
}

pub fn set_starred(store: &Store, m: &MessageRow, starred: bool) -> crate::Result<()> {
    store.queue_flag(m.folder_id, m.uid, "\\Flagged", starred)
}

fn find_folder(
    store: &Store,
    m: &MessageRow,
    roles: &[FolderRole],
) -> crate::Result<Option<FolderRow>> {
    let all = store.folders(&m.account)?;
    for role in roles {
        if let Some(f) = all.iter().find(|f| f.role == *role && f.selectable) {
            return Ok(Some(f.clone()));
        }
    }
    Ok(None)
}

fn move_to_role(
    store: &Store,
    m: &MessageRow,
    roles: &[FolderRole],
    what: &str,
) -> crate::Result<()> {
    let Some(dest) = find_folder(store, m, roles)? else {
        return Err(Error::NotFound(format!("{what} folder")));
    };
    if dest.id == m.folder_id {
        return Ok(());
    }
    store.queue_move(m.folder_id, m.uid, &dest.raw_name)
}

/// Moves the message to the archive folder (Gmail: "All Mail").
pub fn archive(store: &Store, m: &MessageRow) -> crate::Result<()> {
    move_to_role(store, m, &[FolderRole::Archive, FolderRole::All], "archive")
}

pub fn mark_spam(store: &Store, m: &MessageRow) -> crate::Result<()> {
    move_to_role(store, m, &[FolderRole::Junk], "junk")
}

/// Takes a message out of Junk, back to the Inbox.
pub fn mark_not_spam(store: &Store, m: &MessageRow) -> crate::Result<()> {
    move_to_role(store, m, &[FolderRole::Inbox], "inbox")
}

/// Moves a message to another folder of the same account.
pub fn move_to_folder(store: &Store, m: &MessageRow, folder_id: i64) -> crate::Result<()> {
    let Some(dest) = store.folder(folder_id)? else {
        return Err(Error::NotFound("folder".into()));
    };
    if dest.account != m.account {
        return Err(Error::InvalidState("folders of different accounts"));
    }
    if dest.id == m.folder_id {
        return Ok(());
    }
    store.queue_move(m.folder_id, m.uid, &dest.raw_name)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Deleted {
    MovedToTrash,
    /// It was already in the trash: gone for good.
    Permanently,
}

/// "Delete": to the trash, or permanently if it is in the trash already.
/// Errors with `NotFound("trash folder")` if the account has no trash folder;
/// the interface should then ask before calling [`delete_permanently`].
pub fn delete(store: &Store, m: &MessageRow) -> crate::Result<Deleted> {
    let here = store.folder(m.folder_id)?;
    if here.as_ref().is_some_and(|f| f.role == FolderRole::Trash) {
        store.queue_expunge(m.folder_id, m.uid)?;
        return Ok(Deleted::Permanently);
    }
    move_to_role(store, m, &[FolderRole::Trash], "trash")?;
    Ok(Deleted::MovedToTrash)
}

pub fn delete_permanently(store: &Store, m: &MessageRow) -> crate::Result<()> {
    store.queue_expunge(m.folder_id, m.uid)
}

// ---------------------------------------------------------------------------
// The background loop
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub enum SyncStatus {
    Starting,
    Connecting,
    /// Downloading mail; says how far it has got.
    Syncing(SyncProgress),
    Online {
        last_sync: i64,
        /// New messages found since the loop started (the interface can notify when it grows).
        new_messages_total: u64,
        pending_actions: usize,
    },
    /// A temporary problem; the loop retries by itself.
    Offline {
        error: String,
        advice: String,
        retry_in: Duration,
    },
    /// Needs the person (wrong password, untrusted certificate...). No automatic retry.
    NeedsAttention {
        error: String,
        advice: String,
    },
    Stopped,
}

/// The interface's side: read the status, wake the loop, stop it.
#[derive(Clone, Debug)]
pub struct SyncHandle {
    stop: Arc<watch::Sender<bool>>,
    wake: Arc<Notify>,
    status: watch::Receiver<SyncStatus>,
}

impl SyncHandle {
    /// Sync now (also retries after "needs attention", and sends queued actions).
    pub fn wake(&self) {
        self.wake.notify_one();
    }
    pub fn stop(&self) {
        let _ = self.stop.send(true);
    }
    pub fn status(&self) -> SyncStatus {
        self.status.borrow().clone()
    }
    pub fn subscribe(&self) -> watch::Receiver<SyncStatus> {
        self.status.clone()
    }
}

/// The loop's side. Hand to [`run_account_loop`].
#[derive(Debug)]
pub struct SyncLoop {
    stop: watch::Receiver<bool>,
    wake: Arc<Notify>,
    status: watch::Sender<SyncStatus>,
}

pub fn sync_channel() -> (SyncHandle, SyncLoop) {
    let (stop_tx, stop_rx) = watch::channel(false);
    let (status_tx, status_rx) = watch::channel(SyncStatus::Starting);
    let wake = Arc::new(Notify::new());
    (
        SyncHandle {
            stop: Arc::new(stop_tx),
            wake: wake.clone(),
            status: status_rx,
        },
        SyncLoop {
            stop: stop_rx,
            wake,
            status: status_tx,
        },
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Woken {
    Timer,
    Wake,
    Stop,
}

impl SyncLoop {
    fn stopped(&self) -> bool {
        *self.stop.borrow()
    }
    fn set(&self, s: SyncStatus) {
        self.status.send_replace(s);
    }
    /// Waits for the timer (if any), a wake-up, or stop. A dropped handle means stop.
    async fn wait(&mut self, d: Option<Duration>) -> Woken {
        let mut stop = self.stop.clone();
        let timer = async {
            match d {
                Some(d) => tokio::time::sleep(d).await,
                None => std::future::pending::<()>().await,
            }
        };
        tokio::select! {
            () = timer => Woken::Timer,
            () = self.wake.notified() => Woken::Wake,
            _ = stop.wait_for(|s| *s) => Woken::Stop,
        }
    }
}

fn classify(e: &SyncError) -> (bool, String, String) {
    match e {
        SyncError::Server(c) => {
            let attention = !c.is_transient() && !is_refusal(c);
            (attention, c.to_string(), c.advice().to_owned())
        }
        other => (
            false,
            other.to_string(),
            "Something went wrong while saving mail on this computer. Check that the disk is not full. We will try again.".to_owned(),
        ),
    }
}

async fn serve_session(
    ctl: &mut SyncLoop,
    mut session: ImapSession,
    account: &str,
    store: &Arc<Store>,
    opts: &SyncOptions,
    failures: &mut u32,
    new_total: &mut u64,
) -> Result<(), SyncError> {
    let mut scope = SyncScope::All;
    loop {
        if ctl.stopped() {
            session.logout().await;
            return Ok(());
        }
        ctl.set(SyncStatus::Syncing(SyncProgress::default()));
        let status = ctl.status.clone();
        let mut report_progress = |p: &SyncProgress| {
            status.send_replace(SyncStatus::Syncing(p.clone()));
        };
        let rep = sync_session_reporting(
            &mut session,
            account,
            store,
            opts,
            scope,
            &mut report_progress,
        )
        .await?;
        *failures = 0;
        *new_total += u64::try_from(rep.new_messages).unwrap_or(0);
        let acct = account.to_owned();
        let pending = db(store, move |s| s.pending_count(&acct))
            .await
            .unwrap_or(0);
        ctl.set(SyncStatus::Online {
            last_sync: now_secs(),
            new_messages_total: *new_total,
            pending_actions: pending,
        });

        // Wait for something to do.
        let mut used_idle = false;
        if session.features().idle && session.select("INBOX").await.is_ok() {
            let wake = ctl.wake.clone();
            let mut stop = ctl.stop.clone();
            let asked = std::sync::atomic::AtomicBool::new(false);
            let interrupt = async {
                tokio::select! {
                    () = wake.notified() => asked.store(true, std::sync::atomic::Ordering::Relaxed),
                    _ = stop.wait_for(|s| *s) => {},
                }
            };
            let (s, outcome) = session.idle_until(opts.idle_timeout, interrupt).await?;
            session = s;
            used_idle = true;
            // New mail in the inbox needs only the inbox looked at; a timeout or an
            // explicit "refresh" is the moment to look at everything.
            scope = match outcome {
                crate::imap::IdleOutcome::Changed
                    if !asked.load(std::sync::atomic::Ordering::Relaxed) =>
                {
                    SyncScope::InboxOnly
                }
                _ => SyncScope::All,
            };
            if ctl.stopped() {
                session.logout().await;
                return Ok(());
            }
        }
        if !used_idle {
            scope = SyncScope::All;
            match ctl.wait(Some(opts.poll_interval)).await {
                Woken::Stop => {
                    session.logout().await;
                    return Ok(());
                }
                Woken::Timer | Woken::Wake => {}
            }
        }
    }
}

/// Runs until stopped: connect, sync, wait for news (IDLE), repeat.
/// Failures back off (5 s doubling to 15 min); rejected credentials and
/// certificate problems stop the retries until the person wakes the loop.
pub async fn run_account_loop<T: TokenTransport>(
    env: &Env<'_, T>,
    account: &Account,
    store: Arc<Store>,
    opts: SyncOptions,
    mut ctl: SyncLoop,
) {
    let id = account.id.as_str().to_owned();
    let mut failures: u32 = 0;
    let mut new_total: u64 = 0;
    loop {
        if ctl.stopped() {
            break;
        }
        ctl.set(SyncStatus::Connecting);
        let conn = connect_account(
            account,
            env.secrets,
            env.tokens,
            env.clients,
            env.tls,
            env.timeouts,
        )
        .await;
        let err: SyncError = match conn {
            Err(e) => e.into(),
            Ok(session) => match serve_session(
                &mut ctl,
                session,
                &id,
                &store,
                &opts,
                &mut failures,
                &mut new_total,
            )
            .await
            {
                Ok(()) => break,
                Err(e) => e,
            },
        };
        let (attention, error, advice) = classify(&err);
        if attention {
            ctl.set(SyncStatus::NeedsAttention { error, advice });
            match ctl.wait(None).await {
                Woken::Stop => break,
                _ => failures = 0,
            }
        } else {
            let retry_in = RetryPolicy::SYNC.delay_for(failures, random_unit());
            failures = failures.saturating_add(1);
            ctl.set(SyncStatus::Offline {
                error,
                advice,
                retry_in,
            });
            match ctl.wait(Some(retry_in)).await {
                Woken::Stop => break,
                Woken::Wake => failures = 0,
                Woken::Timer => {}
            }
        }
    }
    ctl.set(SyncStatus::Stopped);
}

#[cfg(test)]
mod tests;
