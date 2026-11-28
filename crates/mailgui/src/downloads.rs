//! Saving a received attachment into the person's Downloads folder.
//!
//! The file name comes from a stranger, so it is cleaned again here (Windows
//! device names, trailing dots), an existing file is never overwritten, and on
//! Windows the file is marked as coming from the internet so that SmartScreen
//! and Office protected view treat it with suspicion, as they do for browser
//! downloads.

use std::io::Write;
use std::path::{Path, PathBuf};

use mailcore::compose::sanitize_filename;

const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// The Downloads folder, or the home folder if the system names none.
pub fn downloads_dir() -> Option<PathBuf> {
    let dirs = directories::UserDirs::new()?;
    Some(
        dirs.download_dir()
            .map_or_else(|| dirs.home_dir().to_path_buf(), Path::to_path_buf),
    )
}

/// A name that is safe on Windows and Linux alike.
pub fn safe_local_name(name: &str) -> String {
    let n = sanitize_filename(name);
    let n = n.trim_end_matches(['.', ' ']).to_owned();
    let n = if n.is_empty() {
        "attachment".to_owned()
    } else {
        n
    };
    // Windows ignores spaces before the extension ("CON .txt" is still the console) and also
    // reserves the superscript forms (COM¹, LPT²...), so compare on a cleaned-up stem.
    let stem: String = n
        .split('.')
        .next()
        .unwrap_or("")
        .trim_end()
        .to_lowercase()
        .chars()
        .map(|c| match c {
            '\u{b9}' => '1',
            '\u{b2}' => '2',
            '\u{b3}' => '3',
            other => other,
        })
        .collect();
    if RESERVED.contains(&stem.as_str()) {
        format!("_{n}")
    } else {
        n
    }
}

fn candidate(name: &str, n: u32) -> String {
    if n == 0 {
        return name.to_owned();
    }
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{name} ({n})"),
    }
}

fn open_new(path: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Never executable, whatever the file claims to be.
        o.mode(0o644);
    }
    o.open(path)
}

/// Writes `data` as a new file in `dir` and returns where it went.
pub fn save_unique(dir: &Path, filename: &str, data: &[u8]) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("The folder “{}” could not be used: {e}", dir.display()))?;
    let name = safe_local_name(filename);
    for n in 0..1000 {
        let path = dir.join(candidate(&name, n));
        match open_new(&path) {
            Ok(mut f) => {
                let written = f.write_all(data).and_then(|()| f.flush());
                if let Err(e) = written {
                    drop(f);
                    let _ = std::fs::remove_file(&path);
                    return Err(format!(
                        "The file could not be saved ({e}). Check that the disk is not full."
                    ));
                }
                mark_from_internet(&path);
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("The file could not be saved: {e}")),
        }
    }
    Err("Too many files with this name already exist in the folder.".to_owned())
}

#[cfg(windows)]
fn mark_from_internet(path: &Path) {
    // The "Mark of the Web": an alternate data stream that Windows reads.
    let mut stream = path.as_os_str().to_owned();
    stream.push(":Zone.Identifier");
    let _ = std::fs::write(stream, "[ZoneTransfer]\r\nZoneId=3\r\n");
}

#[cfg(not(windows))]
fn mark_from_internet(_path: &Path) {}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("emayl-dl-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn names_are_made_safe() {
        assert_eq!(safe_local_name("../../etc/passwd"), "passwd");
        assert_eq!(safe_local_name("C:\\x\\evil.exe"), "evil.exe");
        assert_eq!(safe_local_name("con.txt"), "_con.txt");
        assert_eq!(safe_local_name("NUL"), "_NUL");
        assert_eq!(safe_local_name("CON .txt"), "_CON .txt");
        assert_eq!(safe_local_name("COM\u{b9}.txt"), "_COM\u{b9}.txt");
        assert_eq!(safe_local_name("lpt\u{b3}"), "_lpt\u{b3}");
        assert_eq!(safe_local_name("console.txt"), "console.txt");
        assert_eq!(safe_local_name("report. . "), "report");
        assert_eq!(safe_local_name("..."), "attachment");
        assert_eq!(safe_local_name(""), "attachment");
        assert!(safe_local_name(&"a".repeat(500)).chars().count() <= 120);
    }

    #[test]
    fn never_overwrites_and_numbers_the_copies() {
        let d = tmp("unique");
        let a = save_unique(&d, "a.pdf", b"one").unwrap();
        let b = save_unique(&d, "a.pdf", b"two").unwrap();
        let c = save_unique(&d, "noext", b"3").unwrap();
        let c2 = save_unique(&d, "noext", b"4").unwrap();
        assert_eq!(a.file_name().unwrap(), "a.pdf");
        assert_eq!(b.file_name().unwrap(), "a (1).pdf");
        assert_eq!(c2.file_name().unwrap(), "noext (1)");
        assert_eq!(std::fs::read(&a).unwrap(), b"one");
        assert_eq!(std::fs::read(&b).unwrap(), b"two");
        assert_eq!(std::fs::read(&c).unwrap(), b"3");
        std::fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn saved_files_are_never_executable() {
        use std::os::unix::fs::PermissionsExt;
        let d = tmp("mode");
        let p = save_unique(&d, "run.sh", b"#!/bin/sh\n").unwrap();
        let mode = std::fs::metadata(p).unwrap().permissions().mode();
        assert_eq!(mode & 0o111, 0);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn an_existing_symlink_is_not_followed() {
        let d = tmp("link");
        std::fs::create_dir_all(&d).unwrap();
        let target = d.join("target.txt");
        std::fs::write(&target, b"keep").unwrap();
        std::os::unix::fs::symlink(&target, d.join("x.txt")).unwrap();
        let p = save_unique(&d, "x.txt", b"new").unwrap();
        assert_eq!(p.file_name().unwrap(), "x (1).txt");
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
        std::fs::remove_dir_all(d).unwrap();
    }
}
