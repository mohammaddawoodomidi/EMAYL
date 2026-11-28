//! Actions the person took that still have to reach the server (RFC 4549 style
//! "disconnected operations"). The local copy is changed at once so the
//! interface feels instant; the server catches up when it can.

use rusqlite::{OptionalExtension, params};

use super::Store;
use super::messages::{
    FLAG_ANSWERED, FLAG_DELETED, FLAG_DRAFT, FLAG_FLAGGED, FLAG_SEEN, delete_rows, now_secs,
};
use crate::{Error, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// Set a flag on the server.
    AddFlag,
    /// Clear a flag on the server.
    RemoveFlag,
    /// Move to another folder (`arg` is its raw name).
    Move,
    /// Delete for good.
    Expunge,
}

impl OpKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::AddFlag => "flag+",
            Self::RemoveFlag => "flag-",
            Self::Move => "move",
            Self::Expunge => "expunge",
        }
    }
    fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "flag+" => Self::AddFlag,
            "flag-" => Self::RemoveFlag,
            "move" => Self::Move,
            "expunge" => Self::Expunge,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingOp {
    pub id: i64,
    pub folder_id: i64,
    pub uid: u32,
    pub kind: OpKind,
    /// Flag name (`\Seen`) or destination folder name.
    pub arg: String,
    pub attempts: u32,
    pub last_error: Option<String>,
}

fn system_flag_bit(flag: &str) -> Option<u32> {
    match flag.to_ascii_lowercase().as_str() {
        "\\seen" => Some(FLAG_SEEN),
        "\\answered" => Some(FLAG_ANSWERED),
        "\\flagged" => Some(FLAG_FLAGGED),
        "\\deleted" => Some(FLAG_DELETED),
        "\\draft" => Some(FLAG_DRAFT),
        _ => None,
    }
}

/// Only the five standard flags can be sent to the server.
fn valid_flag(flag: &str) -> bool {
    system_flag_bit(flag).is_some()
}

impl Store {
    /// Records "set/clear this flag" and applies it locally right away.
    /// A newer choice about the same flag replaces an older one that was not sent yet.
    pub fn queue_flag(&self, folder_id: i64, uid: u32, flag: &str, on: bool) -> Result<()> {
        if !valid_flag(flag) {
            return Err(Error::InvalidField {
                field: "flag",
                reason: "not an allowed flag name",
            });
        }
        let kind = if on {
            OpKind::AddFlag
        } else {
            OpKind::RemoveFlag
        };
        self.with(|c| {
            let tx = c.transaction()?;
            let exists: bool = tx
                .query_row("SELECT 1 FROM messages WHERE folder_id = ?1 AND uid = ?2", params![folder_id, uid], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                return Err(Error::NotFound(format!("message {uid}")));
            }
            let moving: bool = tx
                .query_row(
                    "SELECT 1 FROM pending_ops WHERE folder_id = ?1 AND uid = ?2 AND kind IN ('move','expunge')",
                    params![folder_id, uid],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if moving {
                return Ok(());
            }
            tx.execute(
                "DELETE FROM pending_ops WHERE folder_id = ?1 AND uid = ?2 AND kind IN ('flag+','flag-') AND lower(arg) = lower(?3)",
                params![folder_id, uid, flag],
            )?;
            tx.execute(
                "INSERT INTO pending_ops (folder_id, uid, kind, arg, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![folder_id, uid, kind.as_str(), flag, now_secs()],
            )?;
            if let Some(bit) = system_flag_bit(flag) {
                if on {
                    tx.execute("UPDATE messages SET flags = flags | ?3 WHERE folder_id = ?1 AND uid = ?2", params![folder_id, uid, bit])?;
                } else {
                    tx.execute("UPDATE messages SET flags = flags & ~?3 WHERE folder_id = ?1 AND uid = ?2", params![folder_id, uid, bit])?;
                }
            }
            tx.commit()?;
            Ok(())
        })
    }

    /// Records a move and removes the message from this folder's list at once.
    /// (It shows up in the destination folder at that folder's next sync.)
    pub fn queue_move(&self, folder_id: i64, uid: u32, destination_raw_name: &str) -> Result<()> {
        if destination_raw_name.is_empty()
            || destination_raw_name.len() > 512
            || destination_raw_name.chars().any(char::is_control)
        {
            return Err(Error::InvalidField {
                field: "folder",
                reason: "not a usable folder name",
            });
        }
        self.queue_removal(folder_id, uid, OpKind::Move, destination_raw_name)
    }

    /// Records a permanent delete and removes the message locally at once.
    pub fn queue_expunge(&self, folder_id: i64, uid: u32) -> Result<()> {
        self.queue_removal(folder_id, uid, OpKind::Expunge, "")
    }

    fn queue_removal(&self, folder_id: i64, uid: u32, kind: OpKind, arg: &str) -> Result<()> {
        self.with(|c| {
            let tx = c.transaction()?;
            let exists: bool = tx
                .query_row("SELECT 1 FROM messages WHERE folder_id = ?1 AND uid = ?2", params![folder_id, uid], |_| Ok(()))
                .optional()?
                .is_some();
            if !exists {
                return Err(Error::NotFound(format!("message {uid}")));
            }
            // Flag changes for a message that is leaving are pointless; clear them
            // (delete_rows clears all waiting actions for the uid), then queue the removal.
            delete_rows(&tx, folder_id, &[uid])?;
            tx.execute(
                "INSERT INTO pending_ops (folder_id, uid, kind, arg, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![folder_id, uid, kind.as_str(), arg, now_secs()],
            )?;
            tx.commit()?;
            Ok(())
        })
    }

    /// Waiting actions for one folder, oldest first.
    pub fn pending_for_folder(&self, folder_id: i64) -> Result<Vec<PendingOp>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id, folder_id, uid, kind, arg, attempts, last_error FROM pending_ops WHERE folder_id = ?1 ORDER BY id",
            )?;
            let rows = st
                .query_map([folder_id], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, i64>(5)?,
                        r.get::<_, Option<String>>(6)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .filter_map(|(id, folder_id, uid, kind, arg, attempts, last_error)| {
                    Some(PendingOp {
                        id,
                        folder_id,
                        uid: u32::try_from(uid).ok()?,
                        kind: OpKind::parse(&kind)?,
                        arg,
                        attempts: u32::try_from(attempts).unwrap_or(u32::MAX),
                        last_error,
                    })
                })
                .collect())
        })
    }

    /// Folders of the account that have waiting actions.
    pub fn folders_with_pending(&self, account: &str) -> Result<Vec<i64>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT DISTINCT p.folder_id FROM pending_ops p JOIN folders f ON f.id = p.folder_id
                 WHERE f.account = ?1 ORDER BY p.folder_id",
            )?;
            Ok(st
                .query_map([account], |r| r.get(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn pending_count(&self, account: &str) -> Result<usize> {
        self.with(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM pending_ops p JOIN folders f ON f.id = p.folder_id WHERE f.account = ?1",
                [account],
                |r| r.get(0),
            )?;
            Ok(usize::try_from(n).unwrap_or(0))
        })
    }

    /// The server has it: forget the action.
    pub fn complete_op(&self, id: i64) -> Result<()> {
        self.with(|c| {
            c.execute("DELETE FROM pending_ops WHERE id = ?1", [id])?;
            Ok(())
        })
    }

    /// The attempt failed. Returns `true` if the action was dropped for failing
    /// `drop_after` times.
    pub fn fail_op(&self, id: i64, error: &str, drop_after: u32) -> Result<bool> {
        let short: String = error.chars().take(300).collect();
        self.with(|c| {
            c.execute(
                "UPDATE pending_ops SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
                params![id, short],
            )?;
            let attempts: Option<i64> = c
                .query_row(
                    "SELECT attempts FROM pending_ops WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .optional()?;
            if attempts.is_some_and(|a| a >= i64::from(drop_after)) {
                c.execute("DELETE FROM pending_ops WHERE id = ?1", [id])?;
                return Ok(true);
            }
            Ok(false)
        })
    }

    /// Drops an action regardless of attempts (e.g. the server said the message is gone).
    pub fn drop_op(&self, id: i64) -> Result<()> {
        self.complete_op(id)
    }
}
