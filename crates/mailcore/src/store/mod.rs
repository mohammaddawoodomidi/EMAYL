//! The local mail database (SQLite, WAL mode) and its search index (FTS5).
//!
//! Treat everything here as a **cache of what the server has**, in the sense of
//! RFC 4549, with two exceptions that exist only locally and must never be lost:
//! the queue of actions waiting to be sent to the server (`pending_ops`) and
//! nothing else. That is why commits are fully synchronous (`synchronous=FULL`).
//!
//! The database file is ours, but it is still opened defensively (no extension
//! loading, no trusted schema, defensive mode) and every value read from it is
//! validated again where it matters. A damaged database is set aside and
//! rebuilt from the server instead of crashing the program
//! ([`Store::open_or_recover`]).
//!
//! All methods are blocking. Call them from `spawn_blocking` (or a worker
//! thread), never from the interface thread.
//!
//! Not encrypted at rest: use full-disk encryption (BitLocker, LUKS) to protect
//! stored mail. (SQLCipher would pull in OpenSSL, which this project excludes.)

mod folders;
mod messages;
mod pending;
mod search;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use rusqlite::{Connection, OpenFlags};

use crate::fsutil::create_private_dir;
use crate::{Error, Result};

pub use folders::{FolderRow, FolderState};
pub use messages::{
    FLAG_ANSWERED, FLAG_DELETED, FLAG_DRAFT, FLAG_FLAGGED, FLAG_SEEN, ListScope, MessageRow,
    NewMessage, ThreadRow, flags_from_bits, flags_to_bits,
};
pub use pending::{OpKind, PendingOp};
pub use search::{SearchHit, SearchQuery, SearchScope, SearchSort, Snippet};

const SCHEMA_VERSION: i32 = 1;
const DB_FILE: &str = "mail.db";

/// Largest message kept in the cache.
pub const MAX_CACHED_MESSAGE_BYTES: usize = 64 * 1024 * 1024;
/// How much of a message's text goes into the search index.
pub const MAX_INDEXED_BODY_BYTES: usize = 100 * 1024;

const SCHEMA_V1: &str = "
CREATE TABLE folders (
    id             INTEGER PRIMARY KEY,
    account        TEXT NOT NULL,
    raw_name       TEXT NOT NULL,
    display_name   TEXT NOT NULL,
    delimiter      TEXT,
    role           TEXT NOT NULL,
    selectable     INTEGER NOT NULL,
    uid_validity   INTEGER,
    uid_next       INTEGER,
    highest_modseq INTEGER,
    floor_uid      INTEGER NOT NULL DEFAULT 1,
    last_synced    INTEGER,
    UNIQUE (account, raw_name)
);

CREATE TABLE messages (
    id                INTEGER PRIMARY KEY,
    folder_id         INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid               INTEGER NOT NULL,
    message_id        TEXT,
    in_reply_to       TEXT,
    refs              TEXT NOT NULL DEFAULT '',
    thread_key        TEXT NOT NULL,
    subject           TEXT NOT NULL,
    from_name         TEXT NOT NULL,
    from_addr         TEXT NOT NULL,
    to_text           TEXT NOT NULL,
    cc_text           TEXT NOT NULL DEFAULT '',
    date              INTEGER NOT NULL,
    internal_date     INTEGER,
    size              INTEGER,
    flags             INTEGER NOT NULL DEFAULT 0,
    maybe_attachments INTEGER NOT NULL DEFAULT 0,
    has_body          INTEGER NOT NULL DEFAULT 0,
    preview           TEXT NOT NULL DEFAULT '',
    UNIQUE (folder_id, uid)
);
CREATE INDEX messages_by_date   ON messages (folder_id, date DESC, id DESC);
CREATE INDEX messages_by_msgid  ON messages (message_id);
CREATE INDEX messages_by_thread ON messages (thread_key, date);

CREATE TABLE bodies (
    message_rowid INTEGER PRIMARY KEY REFERENCES messages(id) ON DELETE CASCADE,
    raw           BLOB NOT NULL,   -- emptied (not the row) when the cache is trimmed
    text          TEXT NOT NULL DEFAULT '',   -- what went into the search index
    fetched_at    INTEGER NOT NULL
);

CREATE TABLE pending_ops (
    id         INTEGER PRIMARY KEY,
    folder_id  INTEGER NOT NULL REFERENCES folders(id) ON DELETE CASCADE,
    uid        INTEGER NOT NULL,
    kind       TEXT NOT NULL,
    arg        TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    attempts   INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
);
CREATE INDEX pending_by_folder ON pending_ops (folder_id, id);

CREATE VIRTUAL TABLE search USING fts5 (
    subject, sender, recipients, body,
    tokenize = 'unicode61 remove_diacritics 2',
    prefix = '2 3'
);
";

pub struct Store {
    pub(crate) conn: Mutex<Connection>,
    /// Filter rules applied to new mail while syncing (kept in `rules.json`, loaded by the program).
    rules: Mutex<Vec<crate::rules::Rule>>,
    path: Option<PathBuf>,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Store")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// What happened when opening the database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Recovery {
    /// Opened normally.
    None,
    /// The old file was unusable and was set aside; a fresh, empty database is in
    /// use and everything will be downloaded again. Actions that were waiting
    /// to be sent to the server were lost with it.
    Recreated { backup: PathBuf, reason: String },
}

fn configure(conn: &Connection) -> Result<()> {
    use rusqlite::config::DbConfig;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_ENABLE_FKEY, true)?;
    conn.busy_timeout(std::time::Duration::from_secs(10))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = FULL;
         PRAGMA temp_store = MEMORY;
         PRAGMA secure_delete = ON;
         PRAGMA trusted_schema = OFF;
         PRAGMA cell_size_check = ON;
         PRAGMA journal_size_limit = 67108864;",
    )?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<()> {
    let version: i32 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
    match version {
        0 => {
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA_V1)?;
            tx.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            tx.commit()?;
            Ok(())
        }
        v if v == SCHEMA_VERSION => Ok(()),
        v => Err(Error::Database(format!(
            "the database was made by a newer version of this program (format {v}, this one understands {SCHEMA_VERSION})"
        ))),
    }
}

impl Store {
    /// Replaces the rules used for new mail.
    pub fn set_rules(&self, rules: Vec<crate::rules::Rule>) {
        *self
            .rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = rules;
    }

    pub fn rules(&self) -> Vec<crate::rules::Rule> {
        self.rules
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn from_connection(mut conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        configure(&conn)?;
        migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            rules: Mutex::new(Vec::new()),
            path,
        })
    }

    /// Opens (creating if needed) the database inside `dir`.
    pub fn open(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
        let path = dir.join(DB_FILE);
        create_private_file_if_missing(&path)?;
        let conn = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let me = Self::from_connection(conn, Some(path))?;
        me.quick_check()?;
        Ok(me)
    }

    /// Like [`Self::open`], but a damaged database is moved aside and replaced by
    /// a new empty one (all mail is cached data and can be downloaded again).
    /// A database from a *newer* program version is never touched: that is an error.
    pub fn open_or_recover(dir: &Path) -> Result<(Self, Recovery)> {
        match Self::open(dir) {
            Ok(s) => Ok((s, Recovery::None)),
            Err(Error::Database(msg)) if msg.contains("newer version") => Err(Error::Database(msg)),
            Err(Error::Database(reason)) => {
                let path = dir.join(DB_FILE);
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_secs());
                let backup = dir.join(format!("{DB_FILE}.damaged-{stamp}"));
                std::fs::rename(&path, &backup).map_err(|e| crate::fsutil::io_err(&path, e))?;
                for ext in ["-wal", "-shm"] {
                    let side = dir.join(format!("{DB_FILE}{ext}"));
                    let _ = std::fs::remove_file(side);
                }
                let fresh = Self::open(dir)?;
                Ok((fresh, Recovery::Recreated { backup, reason }))
            }
            Err(e) => Err(e),
        }
    }

    /// A throw-away database in memory (tests).
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::from_connection(conn, None)
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Connection) -> Result<R>) -> Result<R> {
        let mut g = self
            .conn
            .lock()
            .map_err(|_| Error::Database("database lock poisoned".into()))?;
        f(&mut g)
    }

    /// Fast structural check. Errors mean the file is damaged.
    pub fn quick_check(&self) -> Result<()> {
        self.with(|c| {
            let res: String = c.query_row("PRAGMA quick_check(1)", [], |r| r.get(0))?;
            if res == "ok" {
                Ok(())
            } else {
                Err(Error::Database(format!("integrity check failed: {res}")))
            }
        })
    }

    /// Full check including the search index. Slow on large databases.
    pub fn full_check(&self) -> Result<()> {
        self.with(|c| {
            let res: String = c.query_row("PRAGMA integrity_check(1)", [], |r| r.get(0))?;
            if res != "ok" {
                return Err(Error::Database(format!("integrity check failed: {res}")));
            }
            c.execute("INSERT INTO search(search) VALUES ('integrity-check')", [])
                .map_err(|e| Error::Database(format!("search index damaged: {e}")))?;
            Ok(())
        })
    }

    /// Rebuilds the search index from the stored messages' own fields (body text
    /// is re-read from cached bodies).
    pub fn rebuild_search_index(&self) -> Result<()> {
        messages::rebuild_search(self)
    }

    /// Housekeeping for shutdown or idle time: refresh statistics and shrink the WAL file.
    pub fn maintain(&self) -> Result<()> {
        self.with(|c| {
            c.execute_batch("PRAGMA optimize; PRAGMA wal_checkpoint(TRUNCATE);")?;
            Ok(())
        })
    }

    /// Version of the SQLite library in use.
    pub fn sqlite_version(&self) -> Result<String> {
        self.with(|c| Ok(c.query_row("SELECT sqlite_version()", [], |r| r.get(0))?))
    }
}

#[cfg(unix)]
fn create_private_file_if_missing(path: &Path) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    if !path.exists() {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| crate::fsutil::io_err(path, e))?;
    }
    Ok(())
}

#[cfg(not(unix))]
fn create_private_file_if_missing(_path: &Path) -> Result<()> {
    // On Windows the file inherits the user-profile folder's access rights.
    Ok(())
}
