//! Files this program keeps for itself (window size, start-up notes, the problem log) are
//! readable by their owner only, like the rest of the data folder. The folder itself is
//! already private; this is a second lock on the files inside it.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;

fn open(path: &Path, append: bool) -> std::io::Result<File> {
    let mut o = OpenOptions::new();
    o.create(true).write(true);
    if append {
        o.append(true);
    } else {
        o.truncate(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    let file = o.open(path)?;
    // `mode` only applies to a file that is created now; one made by an older version
    // (or by hand) with looser rights is tightened.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(file)
}

/// Replaces the file's contents (owner-only).
pub fn write(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut f = open(path, false)?;
    f.write_all(data)
}

/// Opens the file for adding to the end (owner-only).
pub fn append(path: &Path) -> std::io::Result<File> {
    open(path, true)
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &Path) -> u32 {
        fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn new_files_are_owner_only_and_old_loose_ones_are_tightened() {
        let d = std::env::temp_dir().join(format!("emayl-priv-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let a = d.join("a");
        write(&a, b"one").unwrap();
        assert_eq!(mode(&a), 0o600);
        // A file left by an older version with open rights.
        let b = d.join("b");
        fs::write(&b, b"old").unwrap();
        fs::set_permissions(&b, fs::Permissions::from_mode(0o644)).unwrap();
        write(&b, b"new").unwrap();
        assert_eq!(mode(&b), 0o600);
        assert_eq!(fs::read(&b).unwrap(), b"new");
        let c = d.join("c");
        writeln!(append(&c).unwrap(), "x").unwrap();
        writeln!(append(&c).unwrap(), "y").unwrap();
        assert_eq!(fs::read_to_string(&c).unwrap(), "x\ny\n");
        assert_eq!(mode(&c), 0o600);
        let _ = fs::remove_dir_all(&d);
    }
}
