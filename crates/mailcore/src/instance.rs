//! Makes sure only one copy of the program works on one set of mail files.
//!
//! Two copies at once would both send the same waiting message, and the second one to
//! save the account list would silently undo the first one's change. The guard is an
//! exclusive lock on a small file that the operating system releases when the program
//! ends, however it ends (including a crash), so there is no stale lock to clean up.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::Path;

use crate::error::{Error, Result};
use crate::fsutil::{create_private_dir, io_err};

const LOCK_FILE: &str = "emayl.lock";

/// Held for as long as the program runs; dropping it releases the lock.
#[derive(Debug)]
pub struct InstanceLock {
    _file: File,
}

impl InstanceLock {
    /// Takes the lock in `dir`, or fails with [`Error::AlreadyRunning`] if another
    /// copy of the program holds it. Never waits.
    pub fn acquire(dir: &Path) -> Result<Self> {
        create_private_dir(dir)?;
        let path = dir.join(LOCK_FILE);
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let file = opts.open(&path).map_err(|e| io_err(&path, e))?;
        match file.try_lock() {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(Error::AlreadyRunning),
            Err(TryLockError::Error(e)) => Err(io_err(&path, e)),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("emayl-lock-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn a_second_copy_is_refused_and_the_lock_is_released_afterwards() {
        let dir = tmp("two");
        let first = InstanceLock::acquire(&dir).unwrap();
        assert!(matches!(
            InstanceLock::acquire(&dir),
            Err(Error::AlreadyRunning)
        ));
        drop(first);
        // Closing the first copy (or its crash) frees the files for the next start.
        let again = InstanceLock::acquire(&dir).unwrap();
        drop(again);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn different_folders_do_not_block_each_other() {
        let (a, b) = (tmp("a"), tmp("b"));
        let _x = InstanceLock::acquire(&a).unwrap();
        let _y = InstanceLock::acquire(&b).unwrap();
        let _ = std::fs::remove_dir_all(&a);
        let _ = std::fs::remove_dir_all(&b);
    }

    #[test]
    fn the_message_tells_the_person_what_to_do() {
        let t = Error::AlreadyRunning.to_string();
        assert!(t.contains("already open") && t.contains("Close the other"));
    }

    #[cfg(unix)]
    #[test]
    fn the_lock_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tmp("perm");
        let _l = InstanceLock::acquire(&dir).unwrap();
        let mode = std::fs::metadata(dir.join(LOCK_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "{mode:o}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
