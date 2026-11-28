//! Folder rows and their sync state.

use rusqlite::{OptionalExtension, Transaction, params};

use super::Store;
use crate::Result;
use crate::imap::{Folder, FolderRole};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRow {
    pub id: i64,
    pub account: String,
    /// Exactly as the server spells it; use in IMAP commands.
    pub raw_name: String,
    pub display_name: String,
    pub delimiter: Option<String>,
    pub role: FolderRole,
    pub selectable: bool,
    pub uid_validity: Option<u32>,
    pub uid_next: Option<u32>,
    pub highest_modseq: Option<u64>,
    /// Messages with a lower UID are deliberately not downloaded (older history).
    pub floor_uid: u32,
    pub last_synced: Option<i64>,
    pub total: u32,
    pub unread: u32,
}

/// The parts of a folder row that sync updates together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FolderState {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub highest_modseq: Option<u64>,
    pub floor_uid: u32,
    pub last_synced: i64,
}

fn role_str(r: FolderRole) -> &'static str {
    match r {
        FolderRole::Inbox => "inbox",
        FolderRole::Drafts => "drafts",
        FolderRole::Sent => "sent",
        FolderRole::Archive => "archive",
        FolderRole::Junk => "junk",
        FolderRole::Trash => "trash",
        FolderRole::All => "all",
        FolderRole::Flagged => "flagged",
        FolderRole::Other => "other",
    }
}

pub(super) fn role_from_str(s: &str) -> FolderRole {
    match s {
        "inbox" => FolderRole::Inbox,
        "drafts" => FolderRole::Drafts,
        "sent" => FolderRole::Sent,
        "archive" => FolderRole::Archive,
        "junk" => FolderRole::Junk,
        "trash" => FolderRole::Trash,
        "all" => FolderRole::All,
        "flagged" => FolderRole::Flagged,
        _ => FolderRole::Other,
    }
}

const SELECT: &str =
    "SELECT f.id, f.account, f.raw_name, f.display_name, f.delimiter, f.role, f.selectable,
        f.uid_validity, f.uid_next, f.highest_modseq, f.floor_uid, f.last_synced,
        (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id),
        (SELECT COUNT(*) FROM messages m WHERE m.folder_id = f.id AND (m.flags & 1) = 0)
    FROM folders f";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<FolderRow> {
    Ok(FolderRow {
        id: r.get(0)?,
        account: r.get(1)?,
        raw_name: r.get(2)?,
        display_name: r.get(3)?,
        delimiter: r.get(4)?,
        role: role_from_str(&r.get::<_, String>(5)?),
        selectable: r.get::<_, i64>(6)? != 0,
        uid_validity: r
            .get::<_, Option<i64>>(7)?
            .and_then(|v| u32::try_from(v).ok()),
        uid_next: r
            .get::<_, Option<i64>>(8)?
            .and_then(|v| u32::try_from(v).ok()),
        highest_modseq: r
            .get::<_, Option<i64>>(9)?
            .and_then(|v| u64::try_from(v).ok()),
        floor_uid: u32::try_from(r.get::<_, i64>(10)?).unwrap_or(1),
        last_synced: r.get(11)?,
        total: u32::try_from(r.get::<_, i64>(12)?).unwrap_or(0),
        unread: u32::try_from(r.get::<_, i64>(13)?).unwrap_or(0),
    })
}

/// Removes a folder's messages and their search entries (not the folder row).
pub(super) fn clear_folder_messages(tx: &Transaction<'_>, folder_id: i64) -> Result<()> {
    tx.execute(
        "DELETE FROM search WHERE rowid IN (SELECT id FROM messages WHERE folder_id = ?1)",
        [folder_id],
    )?;
    tx.execute("DELETE FROM messages WHERE folder_id = ?1", [folder_id])?;
    Ok(())
}

impl Store {
    /// Records the account's folder list. Folders that no longer exist on the
    /// server are removed along with their cached messages, but only when
    /// `remove_missing` is set and the list is not empty (an empty list is more
    /// likely a server hiccup than "every folder was deleted").
    pub fn upsert_folders(
        &self,
        account: &str,
        folders: &[Folder],
        remove_missing: bool,
    ) -> Result<Vec<FolderRow>> {
        self.with(|c| {
            let tx = c.transaction()?;
            for f in folders {
                tx.execute(
                    "INSERT INTO folders (account, raw_name, display_name, delimiter, role, selectable)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                     ON CONFLICT (account, raw_name) DO UPDATE SET
                        display_name = excluded.display_name, delimiter = excluded.delimiter,
                        role = excluded.role, selectable = excluded.selectable",
                    params![account, f.raw_name, f.display_name, f.delimiter, role_str(f.role), i64::from(f.selectable)],
                )?;
            }
            if remove_missing && !folders.is_empty() {
                let keep: std::collections::HashSet<&str> = folders.iter().map(|f| f.raw_name.as_str()).collect();
                let existing: Vec<(i64, String)> = {
                    let mut st = tx.prepare("SELECT id, raw_name FROM folders WHERE account = ?1")?;
                    st.query_map([account], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<rusqlite::Result<_>>()?
                };
                for (id, name) in existing {
                    if !keep.contains(name.as_str()) {
                        clear_folder_messages(&tx, id)?;
                        tx.execute("DELETE FROM folders WHERE id = ?1", [id])?;
                    }
                }
            }
            tx.commit()?;
            Ok(())
        })?;
        self.folders(account)
    }

    pub fn folders(&self, account: &str) -> Result<Vec<FolderRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("{SELECT} WHERE f.account = ?1 ORDER BY f.id"))?;
            Ok(st
                .query_map([account], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    /// The Inbox of every account.
    pub fn inbox_folders(&self) -> Result<Vec<FolderRow>> {
        self.with(|c| {
            let mut st = c.prepare(&format!("{SELECT} ORDER BY f.id"))?;
            Ok(st
                .query_map([], row)?
                .collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .filter(|f| f.role == crate::imap::FolderRole::Inbox)
                .collect())
        })
    }

    pub fn folder(&self, id: i64) -> Result<Option<FolderRow>> {
        self.with(|c| {
            Ok(c.query_row(&format!("{SELECT} WHERE f.id = ?1"), [id], row)
                .optional()?)
        })
    }

    /// Stores sync progress. Call only after the messages it describes are saved.
    pub fn set_folder_state(&self, id: i64, s: FolderState) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE folders SET uid_validity = ?2, uid_next = ?3, highest_modseq = ?4, floor_uid = ?5, last_synced = ?6
                 WHERE id = ?1",
                params![
                    id,
                    i64::from(s.uid_validity),
                    i64::from(s.uid_next),
                    s.highest_modseq.and_then(|m| i64::try_from(m).ok()),
                    i64::from(s.floor_uid),
                    s.last_synced
                ],
            )?;
            Ok(())
        })
    }

    /// The server renumbered this folder (UIDVALIDITY changed): everything cached
    /// for it is stale. Waiting actions that name its old UIDs are dropped too
    /// (RFC 4549); their targets no longer exist under those numbers.
    pub fn reset_folder(&self, id: i64) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            clear_folder_messages(&tx, id)?;
            tx.execute("DELETE FROM pending_ops WHERE folder_id = ?1", [id])?;
            tx.execute(
                "UPDATE folders SET uid_validity = NULL, uid_next = NULL, highest_modseq = NULL, floor_uid = 1, last_synced = NULL
                 WHERE id = ?1",
                [id],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Forgets an account entirely (messages, search entries, waiting actions).
    pub fn delete_account(&self, account: &str) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            let ids: Vec<i64> = {
                let mut st = tx.prepare("SELECT id FROM folders WHERE account = ?1")?;
                st.query_map([account], |r| r.get(0))?
                    .collect::<rusqlite::Result<_>>()?
            };
            for id in ids {
                clear_folder_messages(&tx, id)?;
            }
            tx.execute("DELETE FROM folders WHERE account = ?1", [account])?;
            tx.commit()?;
            // Old copies of the deleted mail can still sit in the write-ahead log.
            // Folding it into the database and emptying it removes them (they were
            // overwritten with zeros in the database itself by `secure_delete`).
            c.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        })
    }

    /// Deletes the cached mail of every account that is not in `keep`. Mail is only a
    /// cache of what is on the server, so this is safe; it cleans up after a crash or
    /// a race between removing an account and its last sync write.
    /// Returns the number of accounts that were cleaned.
    pub fn retain_accounts(&self, keep: &[&str]) -> Result<usize> {
        let known: Vec<String> = self.with(|c| {
            let mut st = c.prepare("SELECT DISTINCT account FROM folders")?;
            let rows = st
                .query_map([], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            Ok(rows)
        })?;
        let mut n = 0;
        for account in known.iter().filter(|a| !keep.contains(&a.as_str())) {
            self.delete_account(account)?;
            n += 1;
        }
        Ok(n)
    }
}
