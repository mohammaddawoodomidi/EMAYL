//! Crash-safe private file helpers shared by the outbox and drafts.
//!
//! Files are written to a temporary name, flushed to disk, then renamed into
//! place, so a power cut leaves either the old file or the new one, never half
//! of each. On Unix, files are 0600 and directories 0700.

use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use crate::{Error, Result};

pub(crate) fn io_err(path: &Path, source: std::io::Error) -> Error {
    Error::Io {
        path: path.to_path_buf(),
        source,
    }
}

pub(crate) fn create_private_dir(dir: &Path) -> Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir).map_err(|e| io_err(dir, e))?;
    // `mode` above only applies to folders created now. A folder that already
    // existed (older version, or made by hand) with looser rights is tightened;
    // if that is not possible the folder is not ours and we must not use it.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let meta = fs::metadata(dir).map_err(|e| io_err(dir, e))?;
        if meta.permissions().mode() & 0o077 != 0 {
            fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
                .map_err(|e| io_err(dir, e))?;
        }
    }
    Ok(())
}

/// Creates a brand-new private file. Anything already at that name (a leftover from
/// a crash, or something planted there, such as a link to another file) is removed
/// first and never written through: `create_new` refuses to follow a link.
fn open_private(path: &Path) -> std::io::Result<fs::File> {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    opts.open(path)
}

fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

/// Atomically replaces `path` with `data`.
pub(crate) fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let mut tmp: PathBuf = path.to_path_buf();
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    tmp.set_file_name(format!(".{name}.tmp"));
    let result = (|| {
        let mut f = open_private(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if let Err(e) = result {
        let _ = fs::remove_file(&tmp);
        return Err(io_err(path, e));
    }
    sync_dir(dir);
    Ok(())
}

/// Removes temporary files that a crash left behind (`.<name>.tmp`). They can hold
/// a complete message or draft, so they must not stay on disk forever.
pub(crate) fn remove_stale_temp_files(dir: &Path) -> usize {
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    let mut n = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') && name.ends_with(".tmp") && fs::remove_file(entry.path()).is_ok()
        {
            n += 1;
        }
    }
    n
}

/// Reads a file, refusing anything larger than `max` bytes.
pub(crate) fn read_limited(path: &Path, max: u64) -> Result<Vec<u8>> {
    let f = fs::File::open(path).map_err(|e| io_err(path, e))?;
    let mut buf = Vec::new();
    f.take(max + 1)
        .read_to_end(&mut buf)
        .map_err(|e| io_err(path, e))?;
    if buf.len() as u64 > max {
        return Err(Error::CorruptFile {
            path: path.to_path_buf(),
            reason: format!("larger than the {max} byte limit"),
        });
    }
    Ok(buf)
}

pub(crate) fn remove_if_exists(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(io_err(path, e)),
    }
}

/// UUID-shaped identifiers only: blocks path traversal through ids that came
/// from a UI or a file name.
pub(crate) fn is_valid_id(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(i, b)| match i {
            8 | 13 | 18 | 23 => b == b'-',
            _ => b.is_ascii_digit() || (b'a'..=b'f').contains(&b),
        })
}

pub(crate) fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("emayl-fs-{}", new_id()));
        create_private_dir(&d).unwrap();
        d
    }

    #[test]
    fn atomic_write_replaces_and_leaves_no_temp_file() {
        let d = tmp();
        let p = d.join("a.json");
        write_atomic(&p, b"one").unwrap();
        write_atomic(&p, b"two").unwrap();
        assert_eq!(fs::read(&p).unwrap(), b"two");
        let names: Vec<_> = fs::read_dir(&d)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names.len(), 1, "{names:?}");
        fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn files_and_dirs_are_private() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let p = d.join("x");
        write_atomic(&p, b"secret").unwrap();
        assert_eq!(
            fs::metadata(&p).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn read_limit_is_enforced() {
        let d = tmp();
        let p = d.join("big");
        write_atomic(&p, &[0u8; 100]).unwrap();
        assert!(read_limited(&p, 100).is_ok());
        assert!(matches!(
            read_limited(&p, 99),
            Err(Error::CorruptFile { .. })
        ));
        fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn id_validation_blocks_traversal() {
        assert!(is_valid_id(&new_id()));
        for bad in [
            "",
            "../etc/passwd",
            "12345678-1234-1234-1234-12345678901/",
            "ABCDEFAB-1234-1234-1234-123456789012",
            &"a".repeat(36),
        ] {
            assert!(!is_valid_id(bad), "{bad:?}");
        }
    }

    #[test]
    fn removing_a_missing_file_is_fine() {
        let d = tmp();
        remove_if_exists(&d.join("nope")).unwrap();
        fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_link_planted_at_the_temp_name_is_not_written_through() {
        let d = tmp();
        let victim = d.join("victim.txt");
        fs::write(&victim, b"precious").unwrap();
        // An attacker (or an old crash) left a link where the temp file will be made.
        std::os::unix::fs::symlink(&victim, d.join(".target.json.tmp")).unwrap();
        write_atomic(&d.join("target.json"), b"new data").unwrap();
        assert_eq!(fs::read(&victim).unwrap(), b"precious");
        assert_eq!(fs::read(d.join("target.json")).unwrap(), b"new data");
        assert!(!d.join(".target.json.tmp").exists());
    }

    #[cfg(unix)]
    #[test]
    fn an_old_loose_temp_file_does_not_keep_its_loose_rights() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp();
        let t = d.join(".x.json.tmp");
        fs::write(&t, b"old").unwrap();
        fs::set_permissions(&t, fs::Permissions::from_mode(0o666)).unwrap();
        write_atomic(&d.join("x.json"), b"data").unwrap();
        let mode = fs::metadata(d.join("x.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_loose_folder_is_tightened() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("emayl-fs-{}", new_id()));
        fs::create_dir_all(&d).unwrap();
        fs::set_permissions(&d, fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&d).unwrap();
        assert_eq!(
            fs::metadata(&d).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn leftover_temp_files_are_removed_but_real_files_stay() {
        let d = tmp();
        fs::write(d.join(".a.json.tmp"), b"half a message").unwrap();
        fs::write(d.join("a.json"), b"{}").unwrap();
        fs::write(d.join("notes.tmp"), b"not ours").unwrap();
        assert_eq!(remove_stale_temp_files(&d), 1);
        assert!(!d.join(".a.json.tmp").exists());
        assert!(d.join("a.json").exists() && d.join("notes.tmp").exists());
    }
}
