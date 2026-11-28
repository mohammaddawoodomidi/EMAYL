//! Cached messages: headers for the list, bodies on demand, threads, flags.

use std::collections::HashMap;

use rusqlite::{OptionalExtension, Row, Transaction, params};

use super::{MAX_CACHED_MESSAGE_BYTES, MAX_INDEXED_BODY_BYTES, Store};
use crate::imap::{MessageFlags, MessageSummary};
use crate::render::{self, RenderOptions};
use crate::{Error, Result};

pub const FLAG_SEEN: u32 = 1;
pub const FLAG_ANSWERED: u32 = 2;
pub const FLAG_FLAGGED: u32 = 4;
pub const FLAG_DELETED: u32 = 8;
pub const FLAG_DRAFT: u32 = 16;

const MAX_REFS_KEPT: usize = 48;
const MAX_ID_LEN: usize = 998;
const MAX_FIELD_CHARS: usize = 500;
const PREVIEW_CHARS: usize = 200;
/// A Date header more than this far ahead of the arrival time is not believed
/// (otherwise a forged future date would pin a message to the top of the list).
const MAX_FUTURE_SKEW_SECS: i64 = 24 * 60 * 60;

/// System flags as stored (keywords are not kept locally).
pub fn flags_to_bits(f: &MessageFlags) -> u32 {
    let mut b = 0;
    if f.seen {
        b |= FLAG_SEEN;
    }
    if f.answered {
        b |= FLAG_ANSWERED;
    }
    if f.flagged {
        b |= FLAG_FLAGGED;
    }
    if f.deleted {
        b |= FLAG_DELETED;
    }
    if f.draft {
        b |= FLAG_DRAFT;
    }
    b
}

pub fn flags_from_bits(b: u32) -> MessageFlags {
    MessageFlags {
        seen: b & FLAG_SEEN != 0,
        answered: b & FLAG_ANSWERED != 0,
        flagged: b & FLAG_FLAGGED != 0,
        deleted: b & FLAG_DELETED != 0,
        draft: b & FLAG_DRAFT != 0,
        keywords: Vec::new(),
    }
}

/// A message header ready to be stored.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NewMessage {
    pub uid: u32,
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub subject: String,
    pub from_name: String,
    pub from_addr: String,
    /// Recipients as one display string ("Name <a@b>, ...").
    pub to: String,
    pub cc: String,
    /// Seconds since the epoch, already corrected (see [`MAX_FUTURE_SKEW_SECS`]).
    pub date: i64,
    pub internal_date: Option<i64>,
    pub size: Option<u32>,
    pub flags: u32,
    pub maybe_attachments: bool,
}

fn people_text(list: &[render::Person]) -> String {
    let mut out = String::new();
    for p in list.iter().take(50) {
        if !out.is_empty() {
            out.push_str(", ");
        }
        if p.name.is_empty() {
            out.push_str(&p.address);
        } else {
            out.push_str(&p.name);
            out.push_str(" <");
            out.push_str(&p.address);
            out.push('>');
        }
    }
    out
}

fn cap_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

fn clean_id(s: &str) -> Option<String> {
    let t = s
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if t.is_empty()
        || t.len() > MAX_ID_LEN
        || t.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        None
    } else {
        Some(t.to_owned())
    }
}

impl NewMessage {
    /// Builds a row from what the server sent for the message list.
    pub fn from_summary(s: &MessageSummary) -> Self {
        let sum = render::summarize_header(&s.header);
        let h = sum.headers;
        let header_date = h.date.filter(|d| *d > 0);
        let date = match (header_date, s.internal_date) {
            (Some(d), Some(i)) if d > i.saturating_add(MAX_FUTURE_SKEW_SECS) => i,
            (Some(d), _) => d,
            (None, Some(i)) => i,
            (None, None) => 0,
        };
        let (from_name, from_addr) = h
            .from
            .map(|p| {
                (
                    cap_chars(&p.name, MAX_FIELD_CHARS),
                    cap_chars(&p.address, 254),
                )
            })
            .unwrap_or_default();
        let mut refs: Vec<String> = h.references.iter().filter_map(|r| clean_id(r)).collect();
        if refs.len() > MAX_REFS_KEPT {
            let tail = refs.split_off(refs.len() - (MAX_REFS_KEPT - 1));
            refs.truncate(1);
            refs.extend(tail);
        }
        Self {
            uid: s.uid,
            message_id: h.message_id.as_deref().and_then(clean_id),
            in_reply_to: h.in_reply_to.as_deref().and_then(clean_id),
            references: refs,
            subject: cap_chars(&h.subject, MAX_FIELD_CHARS),
            from_name,
            from_addr,
            to: cap_chars(&people_text(&h.to), 2000),
            cc: cap_chars(&people_text(&h.cc), 2000),
            date,
            internal_date: s.internal_date,
            size: s.size,
            flags: flags_to_bits(&s.flags),
            maybe_attachments: sum.maybe_attachments,
        }
    }
}

/// One row of the message list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageRow {
    pub id: i64,
    pub account: String,
    pub folder_id: i64,
    pub uid: u32,
    pub message_id: Option<String>,
    pub thread_key: String,
    pub subject: String,
    pub from_name: String,
    pub from_addr: String,
    pub to: String,
    pub date: i64,
    pub size: Option<u32>,
    pub flags: u32,
    pub maybe_attachments: bool,
    pub has_body: bool,
    pub preview: String,
}

impl MessageRow {
    pub fn is_unread(&self) -> bool {
        self.flags & FLAG_SEEN == 0
    }
    pub fn is_flagged(&self) -> bool {
        self.flags & FLAG_FLAGGED != 0
    }
    /// Pass to `list_messages` as `before` to get the next page.
    pub fn cursor(&self) -> (i64, i64) {
        (self.date, self.id)
    }
}

pub(super) const MSG_COLS: &str =
    "m.id, f.account, m.folder_id, m.uid, m.message_id, m.thread_key, m.subject, m.from_name,
    m.from_addr, m.to_text, m.date, m.size, m.flags, m.maybe_attachments, m.has_body, m.preview";

pub(super) fn msg_row(r: &Row<'_>) -> rusqlite::Result<MessageRow> {
    Ok(MessageRow {
        id: r.get(0)?,
        account: r.get(1)?,
        folder_id: r.get(2)?,
        uid: u32::try_from(r.get::<_, i64>(3)?).unwrap_or(0),
        message_id: r.get(4)?,
        thread_key: r.get(5)?,
        subject: r.get(6)?,
        from_name: r.get(7)?,
        from_addr: r.get(8)?,
        to: r.get(9)?,
        date: r.get(10)?,
        size: r
            .get::<_, Option<i64>>(11)?
            .and_then(|v| u32::try_from(v).ok()),
        flags: u32::try_from(r.get::<_, i64>(12)?).unwrap_or(0),
        maybe_attachments: r.get::<_, i64>(13)? != 0,
        has_body: r.get::<_, i64>(14)? != 0,
        preview: r.get(15)?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListScope {
    Folder(i64),
    /// The inbox of every account.
    AllInboxes,
}

impl ListScope {
    fn sql(&self) -> (&'static str, Option<i64>) {
        match self {
            Self::Folder(id) => ("m.folder_id = ?1 AND (m.flags & 8) = 0", Some(*id)),
            Self::AllInboxes => (
                "f.role = 'inbox' AND ?1 IS NULL AND (m.flags & 8) = 0",
                None,
            ),
        }
    }
}

/// A conversation in the list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub thread_key: String,
    /// Subject of the newest message.
    pub subject: String,
    pub count: u32,
    pub unread: u32,
    pub newest_date: i64,
    /// Row id of the newest message (in scope).
    pub newest_id: i64,
    pub flagged: bool,
    pub maybe_attachments: bool,
    /// Distinct senders, newest first (at most 3 names).
    pub participants: Vec<String>,
    pub preview: String,
    /// The account of the newest message.
    pub account: String,
}

fn sender_label(name: &str, addr: &str) -> String {
    if name.trim().is_empty() {
        addr.to_owned()
    } else {
        name.trim().to_owned()
    }
}

fn fts_sender(name: &str, addr: &str) -> String {
    format!("{name} {addr}")
}

/// Finds the thread a new message belongs to, using only its own headers and
/// messages already stored for the same account.
fn find_thread_key(
    tx: &Transaction<'_>,
    account: &str,
    folder_id: i64,
    m: &NewMessage,
) -> Result<String> {
    let mut candidates: Vec<&str> = Vec::new();
    if let Some(r) = m.in_reply_to.as_deref() {
        candidates.push(r);
    }
    candidates.extend(m.references.iter().rev().map(String::as_str));
    for c in candidates {
        let found: Option<String> = tx
            .query_row(
                "SELECT m.thread_key FROM messages m JOIN folders f ON f.id = m.folder_id
                 WHERE f.account = ?1 AND m.message_id = ?2 LIMIT 1",
                params![account, c],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(k) = found {
            return Ok(k);
        }
    }
    let base = m
        .references
        .first()
        .map(String::as_str)
        .or(m.in_reply_to.as_deref())
        .or(m.message_id.as_deref());
    Ok(match base {
        Some(b) => format!("{account}:{b}"),
        None => format!("{account}:#{folder_id}-{}", m.uid),
    })
}

fn account_of(tx: &Transaction<'_>, folder_id: i64) -> Result<String> {
    tx.query_row(
        "SELECT account FROM folders WHERE id = ?1",
        [folder_id],
        |r| r.get(0),
    )
    .optional()?
    .ok_or_else(|| Error::NotFound(format!("folder {folder_id}")))
}

/// Deletes messages (and their search entries, bodies and waiting actions).
pub(super) fn delete_rows(tx: &Transaction<'_>, folder_id: i64, uids: &[u32]) -> Result<usize> {
    let mut n = 0;
    for uid in uids {
        let id: Option<i64> = tx
            .query_row(
                "SELECT id FROM messages WHERE folder_id = ?1 AND uid = ?2",
                params![folder_id, uid],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            tx.execute("DELETE FROM search WHERE rowid = ?1", [id])?;
            tx.execute("DELETE FROM messages WHERE id = ?1", [id])?;
            n += 1;
        }
        tx.execute(
            "DELETE FROM pending_ops WHERE folder_id = ?1 AND uid = ?2",
            params![folder_id, uid],
        )?;
    }
    Ok(n)
}

fn collapse_ws(s: &str, max_chars: usize) -> String {
    let mut out = String::new();
    let mut last_space = true;
    for c in s.chars() {
        if out.chars().count() >= max_chars {
            break;
        }
        if c.is_whitespace() || c.is_control() {
            if !last_space {
                out.push(' ');
                last_space = true;
            }
        } else {
            out.push(c);
            last_space = false;
        }
    }
    out.trim_end().to_owned()
}

fn truncate_at_char(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

pub(super) fn rebuild_search(store: &Store) -> Result<()> {
    store.with(|c| {
        let tx = c.transaction()?;
        tx.execute("DELETE FROM search", [])?;
        tx.execute(
            "INSERT INTO search (rowid, subject, sender, recipients, body)
             SELECT m.id, m.subject, m.from_name || ' ' || m.from_addr, m.to_text || ' ' || m.cc_text,
                    COALESCE((SELECT b.text FROM bodies b WHERE b.message_rowid = m.id), '')
             FROM messages m",
            [],
        )?;
        tx.commit()?;
        Ok(())
    })
}

impl Store {
    /// Stores headers. Messages already present (same folder and UID) are left as
    /// they are. Returns how many were new.
    pub fn insert_messages(&self, folder_id: i64, msgs: &[NewMessage]) -> Result<usize> {
        if msgs.is_empty() {
            return Ok(0);
        }
        self.with(|c| {
            let tx = c.transaction()?;
            let account = account_of(&tx, folder_id)?;
            let mut inserted = 0;
            for m in msgs {
                let exists: bool = tx
                    .query_row("SELECT 1 FROM messages WHERE folder_id = ?1 AND uid = ?2", params![folder_id, m.uid], |_| Ok(()))
                    .optional()?
                    .is_some();
                if exists {
                    continue;
                }
                let key = find_thread_key(&tx, &account, folder_id, m)?;
                tx.execute(
                    "INSERT INTO messages (folder_id, uid, message_id, in_reply_to, refs, thread_key, subject, from_name,
                        from_addr, to_text, cc_text, date, internal_date, size, flags, maybe_attachments)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                    params![
                        folder_id,
                        m.uid,
                        m.message_id,
                        m.in_reply_to,
                        m.references.join(" "),
                        key,
                        m.subject,
                        m.from_name,
                        m.from_addr,
                        m.to,
                        m.cc,
                        m.date,
                        m.internal_date,
                        m.size,
                        m.flags,
                        i64::from(m.maybe_attachments),
                    ],
                )?;
                let rowid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO search (rowid, subject, sender, recipients, body) VALUES (?1, ?2, ?3, ?4, '')",
                    params![rowid, m.subject, fts_sender(&m.from_name, &m.from_addr), format!("{} {}", m.to, m.cc)],
                )?;
                // Children that arrived before this message and were keyed on a
                // reference of theirs now join this message's thread.
                if let Some(mid) = m.message_id.as_deref() {
                    let own_key = format!("{account}:{mid}");
                    if own_key != key {
                        tx.execute("UPDATE messages SET thread_key = ?1 WHERE thread_key = ?2", params![key, own_key])?;
                    }
                }
                inserted += 1;
            }
            tx.commit()?;
            Ok(inserted)
        })
    }

    /// UIDs cached for a folder, ascending.
    pub fn uids_in_folder(&self, folder_id: i64) -> Result<Vec<u32>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT uid FROM messages WHERE folder_id = ?1 ORDER BY uid")?;
            let v = st
                .query_map([folder_id], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(v.into_iter()
                .filter_map(|u| u32::try_from(u).ok())
                .collect())
        })
    }

    /// Removes messages that are gone from the server.
    pub fn delete_uids(&self, folder_id: i64, uids: &[u32]) -> Result<usize> {
        self.with(|c| {
            let tx = c.transaction()?;
            let n = delete_rows(&tx, folder_id, uids)?;
            tx.commit()?;
            Ok(n)
        })
    }

    /// Applies flag changes seen on the server. A message with a waiting local
    /// flag change is skipped (the local intention wins until it is sent).
    /// Returns how many rows changed.
    pub fn apply_flag_updates(
        &self,
        folder_id: i64,
        updates: &[(u32, MessageFlags)],
    ) -> Result<usize> {
        self.with(|c| {
            let tx = c.transaction()?;
            let mut changed = 0;
            for (uid, flags) in updates {
                let waiting: bool = tx
                    .query_row(
                        "SELECT 1 FROM pending_ops WHERE folder_id = ?1 AND uid = ?2 AND kind IN ('flag+','flag-') LIMIT 1",
                        params![folder_id, uid],
                        |_| Ok(()),
                    )
                    .optional()?
                    .is_some();
                if waiting {
                    continue;
                }
                changed += tx.execute(
                    "UPDATE messages SET flags = ?3 WHERE folder_id = ?1 AND uid = ?2 AND flags <> ?3",
                    params![folder_id, uid, flags_to_bits(flags)],
                )?;
            }
            tx.commit()?;
            Ok(changed)
        })
    }

    /// Local flags of every cached message in a folder (uid → bits).
    pub fn flag_snapshot(&self, folder_id: i64) -> Result<HashMap<u32, u32>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT uid, flags FROM messages WHERE folder_id = ?1")?;
            let rows = st
                .query_map([folder_id], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|(u, f)| Some((u32::try_from(u).ok()?, u32::try_from(f).ok()?)))
                .collect())
        })
    }

    /// One page of the message list, newest first. `before` is the cursor of the
    /// last row of the previous page.
    pub fn list_messages(
        &self,
        scope: &ListScope,
        unread_only: bool,
        before: Option<(i64, i64)>,
        limit: usize,
    ) -> Result<Vec<MessageRow>> {
        let limit = i64::try_from(limit.clamp(1, 500)).unwrap_or(500);
        let (cond, arg) = scope.sql();
        let sql = format!(
            "SELECT {MSG_COLS} FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE {cond} AND (?2 = 0 OR (m.flags & 1) = 0)
               AND (?3 IS NULL OR m.date < ?3 OR (m.date = ?3 AND m.id < ?4))
             ORDER BY m.date DESC, m.id DESC LIMIT ?5"
        );
        self.with(|c| {
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(
                params![
                    arg,
                    i64::from(unread_only),
                    before.map(|b| b.0),
                    before.map_or(0, |b| b.1),
                    limit
                ],
                msg_row,
            )?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// One page of conversations, newest first. `before` is `(newest_date, thread_key)`
    /// of the last conversation of the previous page.
    pub fn list_threads(
        &self,
        scope: &ListScope,
        unread_only: bool,
        before: Option<(i64, String)>,
        limit: usize,
    ) -> Result<Vec<ThreadRow>> {
        let limit = i64::try_from(limit.clamp(1, 500)).unwrap_or(500);
        let (cond, arg) = scope.sql();
        let sql = format!(
            "SELECT m.thread_key, MAX(m.date) AS newest, COUNT(*), SUM((m.flags & 1) = 0), MAX(m.flags & 4),
                    MAX(m.maybe_attachments)
             FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE {cond}
             GROUP BY m.thread_key
             HAVING (?2 = 0 OR SUM((m.flags & 1) = 0) > 0)
                AND (?3 IS NULL OR newest < ?3 OR (newest = ?3 AND m.thread_key < ?4))
             ORDER BY newest DESC, m.thread_key DESC LIMIT ?5"
        );
        self.with(|c| {
            let mut st = c.prepare(&sql)?;
            let heads = st
                .query_map(
                    params![arg, i64::from(unread_only), before.as_ref().map(|b| b.0), before.as_ref().map_or("", |b| b.1.as_str()), limit],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, i64>(2)?,
                            r.get::<_, i64>(3)?,
                            r.get::<_, i64>(4)?,
                            r.get::<_, i64>(5)?,
                        ))
                    },
                )?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            let mut out = Vec::with_capacity(heads.len());
            let (cond2, _) = scope.sql();
            let detail = format!(
                "SELECT m.id, m.subject, m.from_name, m.from_addr, m.preview, f.account FROM messages m JOIN folders f ON f.id = m.folder_id
                 WHERE {cond2} AND m.thread_key = ?2 ORDER BY m.date DESC, m.id DESC LIMIT 20"
            );
            let mut dst = c.prepare(&detail)?;
            for (key, newest, count, unread, flagged, att) in heads {
                let rows = dst
                    .query_map(params![arg, key], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?, r.get::<_, String>(5)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                let Some(first) = rows.first() else { continue };
                let mut participants: Vec<String> = Vec::new();
                for (_, _, n, a, _, _) in &rows {
                    let label = sender_label(n, a);
                    if !label.is_empty() && !participants.contains(&label) {
                        participants.push(label);
                    }
                    if participants.len() >= 3 {
                        break;
                    }
                }
                out.push(ThreadRow {
                    thread_key: key,
                    subject: first.1.clone(),
                    count: u32::try_from(count).unwrap_or(u32::MAX),
                    unread: u32::try_from(unread).unwrap_or(0),
                    newest_date: newest,
                    newest_id: first.0,
                    flagged: flagged != 0,
                    maybe_attachments: att != 0,
                    participants,
                    preview: first.4.clone(),
                    account: first.5.clone(),
                });
            }
            Ok(out)
        })
    }

    /// Every cached message of a conversation (in any folder of the account),
    /// oldest first.
    pub fn thread_messages(&self, thread_key: &str) -> Result<Vec<MessageRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!(
                "SELECT {MSG_COLS} FROM messages m JOIN folders f ON f.id = m.folder_id
                 WHERE m.thread_key = ?1 AND (m.flags & 8) = 0 ORDER BY m.date, m.id LIMIT 500"
            ))?;
            Ok(st
                .query_map([thread_key], msg_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn message(&self, id: i64) -> Result<Option<MessageRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {MSG_COLS} FROM messages m JOIN folders f ON f.id = m.folder_id WHERE m.id = ?1"),
                [id],
                msg_row,
            )
            .optional()?)
        })
    }

    pub fn message_by_uid(&self, folder_id: i64, uid: u32) -> Result<Option<MessageRow>> {
        self.with(|c| {
            Ok(c.query_row(
                &format!("SELECT {MSG_COLS} FROM messages m JOIN folders f ON f.id = m.folder_id WHERE m.folder_id = ?1 AND m.uid = ?2"),
                params![folder_id, uid],
                msg_row,
            )
            .optional()?)
        })
    }

    /// Saves a downloaded message and makes its text searchable. A message that
    /// cannot be rendered is still kept (the interface shows the error then).
    pub fn store_body(&self, message_id: i64, raw: &[u8]) -> Result<()> {
        if raw.len() > MAX_CACHED_MESSAGE_BYTES {
            return Err(Error::InvalidField {
                field: "message",
                reason: "too large to keep",
            });
        }
        // Rendering can take a while for big messages: do it before taking the lock.
        let (text, preview, has_attachments) = match render::render(raw, &RenderOptions::default())
        {
            Ok(r) => {
                let text = truncate_at_char(&r.plain_text, MAX_INDEXED_BODY_BYTES).to_owned();
                let preview = collapse_ws(&r.plain_text, PREVIEW_CHARS);
                (text, preview, Some(!r.attachments.is_empty()))
            }
            Err(_) => (String::new(), String::new(), None),
        };
        let now = now_secs();
        self.with(|c| {
            let tx = c.transaction()?;
            let exists: bool = tx
                .query_row("SELECT 1 FROM messages WHERE id = ?1", [message_id], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                return Err(Error::NotFound(format!("message {message_id}")));
            }
            tx.execute(
                "INSERT INTO bodies (message_rowid, raw, text, fetched_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (message_rowid) DO UPDATE SET raw = excluded.raw, text = excluded.text, fetched_at = excluded.fetched_at",
                params![message_id, raw, text, now],
            )?;
            tx.execute(
                "UPDATE messages SET has_body = 1, preview = ?2,
                    maybe_attachments = COALESCE(?3, maybe_attachments) WHERE id = ?1",
                params![message_id, preview, has_attachments.map(i64::from)],
            )?;
            tx.execute("UPDATE search SET body = ?2 WHERE rowid = ?1", params![message_id, text])?;
            tx.commit()?;
            Ok(())
        })
    }

    /// The newest messages of a folder whose text has not been downloaded yet and that are
    /// not larger than `max_size` bytes (a message of unknown size is included).
    /// Used to fetch the mail the person is most likely to open next.
    pub fn uncached_recent(&self, folder_id: i64, max_size: u32, limit: usize) -> Result<Vec<i64>> {
        let limit = i64::try_from(limit.clamp(1, 500)).unwrap_or(500);
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id FROM messages
                 WHERE folder_id = ?1 AND has_body = 0 AND (flags & 8) = 0
                   AND (size IS NULL OR size <= ?2)
                 ORDER BY date DESC, id DESC LIMIT ?3",
            )?;
            Ok(st
                .query_map(params![folder_id, i64::from(max_size), limit], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<i64>>>()?)
        })
    }

    /// The cached raw message, if it is still cached.
    pub fn body(&self, message_id: i64) -> Result<Option<Vec<u8>>> {
        self.with(|c| {
            let raw: Option<Vec<u8>> = c
                .query_row(
                    "SELECT raw FROM bodies WHERE message_rowid = ?1",
                    [message_id],
                    |r| r.get(0),
                )
                .optional()?;
            Ok(raw.filter(|r| !r.is_empty()))
        })
    }

    /// Keeps the cached raw messages under `max_total_bytes` by dropping the
    /// oldest downloads first. Their search text is kept. Returns bytes freed.
    pub fn trim_bodies(&self, max_total_bytes: u64) -> Result<u64> {
        self.with(|c| {
            let tx = c.transaction()?;
            let total: i64 = tx.query_row("SELECT COALESCE(SUM(length(raw)), 0) FROM bodies", [], |r| r.get(0))?;
            let mut excess = u64::try_from(total).unwrap_or(0).saturating_sub(max_total_bytes);
            let mut freed = 0u64;
            if excess > 0 {
                let victims: Vec<(i64, i64)> = {
                    let mut st = tx.prepare(
                        "SELECT message_rowid, length(raw) FROM bodies WHERE length(raw) > 0 ORDER BY fetched_at, message_rowid",
                    )?;
                    st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
                };
                for (id, len) in victims {
                    if excess == 0 {
                        break;
                    }
                    tx.execute("UPDATE bodies SET raw = x'' WHERE message_rowid = ?1", [id])?;
                    tx.execute("UPDATE messages SET has_body = 0 WHERE id = ?1", [id])?;
                    let len = u64::try_from(len).unwrap_or(0);
                    freed += len;
                    excess = excess.saturating_sub(len);
                }
            }
            tx.commit()?;
            Ok(freed)
        })
    }
}

pub(super) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}
