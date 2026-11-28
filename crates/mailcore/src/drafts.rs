//! Drafts saved on disk as the user types, so nothing is lost to a crash.
//!
//! A draft is `<id>.json` (text, recipients, attachment list) plus one
//! `<id>.<n>.att` file per attachment. The JSON file is written last and is the
//! commit point: attachment files without a matching entry are ignored and
//! cleaned up on the next save.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::account::AccountId;
use crate::compose::{Attachment, OutgoingMessage};
use crate::fsutil::{
    create_private_dir, io_err, is_valid_id, new_id, read_limited, remove_if_exists,
    remove_stale_temp_files, write_atomic,
};
use crate::{Error, Result};

const FORMAT_VERSION: u32 = 1;
const MAX_META_BYTES: u64 = 8 * 1024 * 1024;
const MAX_ATTACHMENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ATTACHMENTS: usize = 100;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AttachmentMeta {
    filename: String,
    content_type: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DraftFile {
    version: u32,
    id: String,
    account: Option<AccountId>,
    updated_at: i64,
    /// Attachments are stored in separate files; this copy of the message has none.
    message: OutgoingMessage,
    attachments: Vec<AttachmentMeta>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Draft {
    pub id: String,
    pub account: Option<AccountId>,
    pub updated_at: i64,
    pub message: OutgoingMessage,
}

/// What the "Drafts" list shows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSummary {
    pub id: String,
    pub account: Option<AccountId>,
    pub updated_at: i64,
    pub subject: String,
    pub first_recipient: String,
}

/// How many deleted draft ids are remembered (they are never reused, so a short memory
/// is enough to catch a save that was already on its way).
const REMEMBER_DELETED: usize = 256;

#[derive(Debug)]
pub struct Drafts {
    dir: PathBuf,
    /// Held while a draft is written or deleted, with the ids deleted during this run.
    /// The window saves in the background; a save that is still running when the person
    /// discards or sends the message must not bring the draft back.
    changes: std::sync::Mutex<std::collections::VecDeque<String>>,
}

impl Drafts {
    pub fn open(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
        remove_stale_temp_files(dir);
        Ok(Self {
            dir: dir.to_path_buf(),
            changes: std::sync::Mutex::new(std::collections::VecDeque::new()),
        })
    }

    /// A fresh id for a draft that is about to be written (so saving the same
    /// draft twice always replaces it instead of making a second one).
    pub fn new_draft_id() -> String {
        new_id()
    }

    fn json_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn att_path(&self, id: &str, n: usize) -> PathBuf {
        self.dir.join(format!("{id}.{n}.att"))
    }

    fn check_id(id: &str) -> Result<()> {
        if is_valid_id(id) {
            Ok(())
        } else {
            Err(Error::NotFound(id.chars().take(40).collect()))
        }
    }

    /// Creates a draft (`id = None`) or replaces an existing one. Returns its id.
    pub fn save(
        &self,
        id: Option<&str>,
        account: Option<&AccountId>,
        message: &OutgoingMessage,
        now: i64,
    ) -> Result<String> {
        if message.attachments.len() > MAX_ATTACHMENTS {
            return Err(Error::InvalidState("too many attachments"));
        }
        let id = match id {
            Some(i) => {
                Self::check_id(i)?;
                i.to_owned()
            }
            None => new_id(),
        };
        let deleted = self
            .changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if deleted.iter().any(|d| *d == id) {
            // Already discarded or sent: this save arrived too late and is dropped.
            return Ok(id);
        }
        let old_count = self
            .read_file(&self.json_path(&id))
            .map(|f| f.attachments.len())
            .unwrap_or(0);

        for (n, a) in message.attachments.iter().enumerate() {
            write_atomic(&self.att_path(&id, n), &a.data)?;
        }
        let mut stored = message.clone();
        stored.attachments = Vec::new();
        let file = DraftFile {
            version: FORMAT_VERSION,
            id: id.clone(),
            account: account.cloned(),
            updated_at: now,
            message: stored,
            attachments: message
                .attachments
                .iter()
                .map(|a| AttachmentMeta {
                    filename: a.filename.clone(),
                    content_type: a.content_type.clone(),
                })
                .collect(),
        };
        let json = serde_json::to_vec_pretty(&file).map_err(|e| Error::CorruptFile {
            path: self.json_path(&id),
            reason: e.to_string(),
        })?;
        write_atomic(&self.json_path(&id), &json)?;
        // Only now are attachments that were removed from the draft deleted.
        for n in message.attachments.len()..old_count {
            remove_if_exists(&self.att_path(&id, n))?;
        }
        Ok(id)
    }

    fn read_file(&self, path: &Path) -> Result<DraftFile> {
        let raw = read_limited(path, MAX_META_BYTES)?;
        let f: DraftFile = serde_json::from_slice(&raw).map_err(|e| Error::CorruptFile {
            path: path.to_path_buf(),
            reason: e.to_string(),
        })?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if f.version != FORMAT_VERSION
            || f.id != stem
            || !is_valid_id(&f.id)
            || f.attachments.len() > MAX_ATTACHMENTS
        {
            return Err(Error::CorruptFile {
                path: path.to_path_buf(),
                reason: "unsupported version or mismatched id".into(),
            });
        }
        Ok(f)
    }

    pub fn load(&self, id: &str) -> Result<Draft> {
        Self::check_id(id)?;
        let path = self.json_path(id);
        if !path.exists() {
            return Err(Error::NotFound(id.to_owned()));
        }
        let f = self.read_file(&path)?;
        let mut message = f.message;
        for (n, meta) in f.attachments.iter().enumerate() {
            let data = read_limited(&self.att_path(id, n), MAX_ATTACHMENT_BYTES)?;
            message.attachments.push(Attachment {
                filename: meta.filename.clone(),
                content_type: meta.content_type.clone(),
                data,
            });
        }
        Ok(Draft {
            id: f.id,
            account: f.account,
            updated_at: f.updated_at,
            message,
        })
    }

    /// Newest first. Unreadable files are skipped (and left on disk).
    pub fn list(&self) -> Result<Vec<DraftSummary>> {
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|e| io_err(&self.dir, e))? {
            let path = entry.map_err(|e| io_err(&self.dir, e))?.path();
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            if let Ok(f) = self.read_file(&path) {
                out.push(DraftSummary {
                    id: f.id,
                    account: f.account,
                    updated_at: f.updated_at,
                    subject: f.message.subject.chars().take(200).collect(),
                    first_recipient: f
                        .message
                        .to
                        .first()
                        .map(|m| m.address.to_string())
                        .unwrap_or_default(),
                });
            }
        }
        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.id.cmp(&b.id))
        });
        Ok(out)
    }

    /// Deletes every draft written from an account (used when it is removed).
    pub fn delete_account(&self, account: &AccountId) -> Result<usize> {
        let mut n = 0;
        for d in self.list()? {
            if d.account.as_ref() == Some(account) {
                self.delete(&d.id)?;
                n += 1;
            }
        }
        Ok(n)
    }

    pub fn delete(&self, id: &str) -> Result<()> {
        Self::check_id(id)?;
        let mut deleted = self
            .changes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if deleted.len() >= REMEMBER_DELETED {
            deleted.pop_front();
        }
        deleted.push_back(id.to_owned());
        let count = self
            .read_file(&self.json_path(id))
            .map(|f| f.attachments.len())
            .unwrap_or(0);
        // The JSON goes first: without it the draft no longer exists.
        remove_if_exists(&self.json_path(id))?;
        for n in 0..count.max(MAX_ATTACHMENTS) {
            let p = self.att_path(id, n);
            if !p.exists() && n >= count {
                break;
            }
            remove_if_exists(&p)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compose::Mailbox;
    use crate::email::EmailAddress;

    fn tmp() -> PathBuf {
        std::env::temp_dir().join(format!("emayl-drafts-{}", new_id()))
    }

    fn msg(subject: &str, atts: usize) -> OutgoingMessage {
        OutgoingMessage {
            to: vec![Mailbox {
                name: "Bob".into(),
                address: EmailAddress::parse("bob@example.org").unwrap(),
            }],
            subject: subject.into(),
            text: "draft text\n".into(),
            attachments: (0..atts)
                .map(|i| Attachment {
                    filename: format!("f{i}.bin"),
                    content_type: "application/octet-stream".into(),
                    data: vec![i as u8; 1000 + i],
                })
                .collect(),
            ..OutgoingMessage::default()
        }
    }

    #[test]
    fn round_trips_text_and_attachments_byte_for_byte() {
        let dir = tmp();
        let d = Drafts::open(&dir).unwrap();
        let acct = AccountId::generate();
        let id = d.save(None, Some(&acct), &msg("Plan", 2), 100).unwrap();
        let back = d.load(&id).unwrap();
        assert_eq!(back.message, msg("Plan", 2));
        assert_eq!(back.account, Some(acct));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn autosave_replaces_and_removed_attachments_are_deleted() {
        let dir = tmp();
        let d = Drafts::open(&dir).unwrap();
        let id = d.save(None, None, &msg("v1", 3), 100).unwrap();
        let id2 = d.save(Some(&id), None, &msg("v2", 1), 200).unwrap();
        assert_eq!(id, id2);
        let back = d.load(&id).unwrap();
        assert_eq!(
            (
                back.message.subject.as_str(),
                back.message.attachments.len(),
                back.updated_at
            ),
            ("v2", 1, 200)
        );
        assert!(!d.att_path(&id, 1).exists() && !d.att_path(&id, 2).exists());
        assert_eq!(d.list().unwrap().len(), 1);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn list_is_newest_first() {
        let dir = tmp();
        let d = Drafts::open(&dir).unwrap();
        d.save(None, None, &msg("old", 0), 100).unwrap();
        d.save(None, None, &msg("new", 0), 300).unwrap();
        d.save(None, None, &msg("mid", 0), 200).unwrap();
        let subjects: Vec<_> = d.list().unwrap().into_iter().map(|s| s.subject).collect();
        assert_eq!(subjects, vec!["new", "mid", "old"]);
        assert_eq!(d.list().unwrap()[0].first_recipient, "bob@example.org");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn delete_removes_everything_and_is_repeatable() {
        let dir = tmp();
        let d = Drafts::open(&dir).unwrap();
        let id = d.save(None, None, &msg("x", 2), 1).unwrap();
        d.delete(&id).unwrap();
        d.delete(&id).unwrap();
        assert!(d.load(&id).is_err());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 0);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hostile_ids_and_corrupt_files_are_handled() {
        let dir = tmp();
        let d = Drafts::open(&dir).unwrap();
        for bad in ["../x", "", "a/b", ".."] {
            assert!(d.load(bad).is_err());
            assert!(d.delete(bad).is_err());
            assert!(d.save(Some(bad), None, &msg("x", 0), 1).is_err());
        }
        let good = d.save(None, None, &msg("ok", 0), 1).unwrap();
        fs::write(dir.join(format!("{}.json", new_id())), b"{broken").unwrap();
        assert_eq!(d.list().unwrap().len(), 1);
        assert!(d.load(&good).is_ok());
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_crash_between_attachment_and_json_keeps_the_old_draft_intact() {
        let dir = tmp();
        let d = Drafts::open(&dir).unwrap();
        let id = d.save(None, None, &msg("v1", 1), 1).unwrap();
        // Simulate a crash after the new attachment was written but before the JSON was replaced.
        fs::write(d.att_path(&id, 5), b"orphan").unwrap();
        let back = d.load(&id).unwrap();
        assert_eq!(
            (
                back.message.subject.as_str(),
                back.message.attachments.len()
            ),
            ("v1", 1)
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn too_many_attachments_rejected() {
        let dir = tmp();
        let d = Drafts::open(&dir).unwrap();
        assert!(
            d.save(None, None, &msg("x", MAX_ATTACHMENTS + 1), 1)
                .is_err()
        );
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deleting_an_account_deletes_only_its_drafts_and_attachments() {
        let dir = std::env::temp_dir().join(format!("emayl-drafts-{}", new_id()));
        let d = Drafts::open(&dir).unwrap();
        let (gone, kept) = (AccountId::generate(), AccountId::generate());
        let mk = |acct: Option<&AccountId>, subject: &str| {
            d.save(None, acct, &msg(subject, 1), 5).unwrap()
        };
        let a = mk(Some(&gone), "a");
        let b = mk(Some(&kept), "b");
        let c = mk(None, "c");
        assert_eq!(d.delete_account(&gone).unwrap(), 1);
        assert!(d.load(&a).is_err());
        assert!(d.load(&b).is_ok() && d.load(&c).is_ok());
    }

    #[test]
    fn a_late_autosave_cannot_bring_back_a_discarded_draft() {
        // The window saves in the background. If "Discard" (or "Send") runs while a save
        // is still on its way, the save must not undo it: the text was meant to be gone.
        let dir = tmp();
        let d = Drafts::open(&dir).unwrap();
        let id = Drafts::new_draft_id();
        d.save(Some(&id), None, &msg("secret plans", 1), 5).unwrap();
        assert_eq!(d.list().unwrap().len(), 1);
        d.delete(&id).unwrap();
        // The late save arrives.
        d.save(Some(&id), None, &msg("secret plans", 1), 6).unwrap();
        assert!(d.list().unwrap().is_empty(), "the draft came back");
        assert!(d.load(&id).is_err());
        let leftovers: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(&id))
            .collect();
        assert!(leftovers.is_empty(), "files left behind: {leftovers:?}");
        // Other drafts are not affected.
        let other = d.save(None, None, &msg("another", 0), 7).unwrap();
        assert!(d.load(&other).is_ok());
        let _ = fs::remove_dir_all(&dir);
    }
}
