//! On-disk account registry (`accounts.toml`).
//!
//! * No secrets are stored here.
//! * The file is not trusted: size-capped, version-checked, and every account is
//!   re-validated on load. A damaged file is reported and left untouched; it is
//!   never silently replaced by an empty one.
//! * Writes are atomic (temp file, fsync, rename). On Unix the file is created
//!   `0600` and the directory `0700`.
//! * There is no cap on the number of accounts; only the file size is capped
//!   (16 MiB, which is tens of thousands of accounts).
//!
//! Known limit: two processes saving at the same moment is last-writer-wins.

use std::collections::HashSet;
use std::fs;
use std::io::{ErrorKind, Read, Write};
use std::path::{Path, PathBuf};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

use crate::account::{Account, AccountId};
use crate::{Error, Result};

const FORMAT_VERSION: u32 = 1;
const MAX_FILE_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Deserialize)]
struct FileOwned {
    version: u32,
    #[serde(default)]
    accounts: Vec<Account>,
}

#[derive(Serialize)]
struct FileRef<'a> {
    version: u32,
    accounts: &'a [Account],
}

#[derive(Debug)]
pub struct AccountRegistry {
    path: PathBuf,
    accounts: Vec<Account>,
}

impl AccountRegistry {
    /// Platform default location, for example `~/.config/EMAYL/accounts.toml`
    /// on Linux or `%APPDATA%\emayl\EMAYL\config\accounts.toml` on Windows.
    pub fn default_path() -> Option<PathBuf> {
        ProjectDirs::from("dev", "emayl", "EMAYL").map(|d| d.config_dir().join("accounts.toml"))
    }

    /// Loads the registry, or starts empty if the file does not exist.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        let accounts = match fs::File::open(&path) {
            Ok(file) => read_accounts(&path, file)?,
            Err(e) if e.kind() == ErrorKind::NotFound => Vec::new(),
            Err(source) => return Err(Error::Io { path, source }),
        };
        Ok(Self { path, accounts })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn accounts(&self) -> &[Account] {
        &self.accounts
    }

    pub fn len(&self) -> usize {
        self.accounts.len()
    }

    pub fn is_empty(&self) -> bool {
        self.accounts.is_empty()
    }

    pub fn get(&self, id: &AccountId) -> Option<&Account> {
        self.accounts.iter().find(|a| &a.id == id)
    }

    /// Validates, rejects duplicates, and persists. If saving fails the
    /// in-memory state is rolled back.
    pub fn add(&mut self, account: Account) -> Result<()> {
        account.validate()?;
        if self.accounts.iter().any(|a| a.id == account.id) {
            return Err(Error::DuplicateAccount(account.id.as_str().to_owned()));
        }
        if self.accounts.iter().any(|a| same_mailbox(a, &account)) {
            return Err(Error::DuplicateAccount(account.email.to_string()));
        }
        self.accounts.push(account);
        if let Err(e) = self.save() {
            self.accounts.pop();
            return Err(e);
        }
        Ok(())
    }

    /// Removes and persists. Deleting the account's keyring secrets is the
    /// caller's job (see [`crate::secrets::SecretStore`]).
    pub fn remove(&mut self, id: &AccountId) -> Result<Account> {
        let idx = self
            .accounts
            .iter()
            .position(|a| &a.id == id)
            .ok_or_else(|| Error::AccountNotFound(id.as_str().to_owned()))?;
        let removed = self.accounts.remove(idx);
        if let Err(e) = self.save() {
            self.accounts.insert(idx, removed);
            return Err(e);
        }
        Ok(removed)
    }

    fn save(&self) -> Result<()> {
        let io = |source| Error::Io {
            path: self.path.clone(),
            source,
        };
        let parent = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        create_private_dir(parent).map_err(io)?;

        let text = toml::to_string_pretty(&FileRef {
            version: FORMAT_VERSION,
            accounts: &self.accounts,
        })
        .map_err(|e| Error::CorruptAccountFile {
            path: self.path.clone(),
            reason: format!("could not serialise accounts: {e}"),
        })?;

        let tmp = parent.join(format!(".accounts.{}.tmp", std::process::id()));
        let result =
            write_private_file(&tmp, text.as_bytes()).and_then(|()| fs::rename(&tmp, &self.path));
        if let Err(e) = result {
            let _ = fs::remove_file(&tmp);
            return Err(io(e));
        }
        sync_dir(parent);
        Ok(())
    }
}

fn same_mailbox(a: &Account, b: &Account) -> bool {
    a.email == b.email && a.imap.host == b.imap.host
}

fn read_accounts(path: &Path, file: fs::File) -> Result<Vec<Account>> {
    let corrupt = |reason: String| Error::CorruptAccountFile {
        path: path.to_path_buf(),
        reason,
    };
    let mut text = String::new();
    match file.take(MAX_FILE_BYTES + 1).read_to_string(&mut text) {
        Ok(_) => {}
        Err(e) if e.kind() == ErrorKind::InvalidData => {
            return Err(corrupt("file is not valid UTF-8".into()));
        }
        Err(source) => {
            return Err(Error::Io {
                path: path.to_path_buf(),
                source,
            });
        }
    }
    if text.len() as u64 > MAX_FILE_BYTES {
        return Err(Error::AccountFileTooLarge {
            path: path.to_path_buf(),
            limit: MAX_FILE_BYTES,
        });
    }
    let parsed: FileOwned = toml::from_str(&text).map_err(|e| corrupt(e.to_string()))?;
    if parsed.version != FORMAT_VERSION {
        return Err(corrupt(format!(
            "unsupported file version {} (this build understands {FORMAT_VERSION})",
            parsed.version
        )));
    }
    let mut ids = HashSet::new();
    let mut boxes = HashSet::new();
    for account in &parsed.accounts {
        account.validate().map_err(|e| corrupt(e.to_string()))?;
        if !ids.insert(account.id.clone()) {
            return Err(corrupt("duplicate account id".into()));
        }
        if !boxes.insert((account.email.clone(), account.imap.host.clone())) {
            return Err(corrupt(format!("duplicate account for {}", account.email)));
        }
    }
    Ok(parsed.accounts)
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut opts = fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut file = opts.open(path)?;
    file.write_all(data)?;
    file.sync_all()
}

/// Best-effort durability of the rename itself (meaningful on Unix only).
fn sync_dir(dir: &Path) {
    #[cfg(unix)]
    if let Ok(d) = fs::File::open(dir) {
        let _ = d.sync_all();
    }
    #[cfg(not(unix))]
    let _ = dir;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account::{AuthMethod, Security, ServerConfig, ServerSettings};
    use crate::email::EmailAddress;

    fn account(email: &str, host: &str) -> Account {
        Account::new(
            "T",
            EmailAddress::parse(email).unwrap(),
            ServerSettings {
                imap: ServerConfig::new(host, 993, Security::Tls).unwrap(),
                imap_username: email.into(),
                smtp: ServerConfig::new("smtp.example.com", 465, Security::Tls).unwrap(),
                smtp_username: email.into(),
            },
            AuthMethod::Password,
        )
        .unwrap()
    }

    fn tmp_path(name: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "mailcore-test-{}-{}-{name}",
            std::process::id(),
            AccountId::generate().as_str()
        ));
        (dir.join("nested").join("accounts.toml"), dir)
    }

    #[test]
    fn missing_file_opens_empty_and_add_persists() {
        let (path, dir) = tmp_path("add");
        let mut reg = AccountRegistry::open(&path).unwrap();
        assert!(reg.is_empty());
        reg.add(account("a@example.com", "imap.example.com"))
            .unwrap();
        reg.add(account("b@example.com", "imap.example.com"))
            .unwrap();

        let again = AccountRegistry::open(&path).unwrap();
        assert_eq!(again.len(), 2);
        assert_eq!(again.accounts(), reg.accounts());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn many_accounts_are_supported() {
        let (path, dir) = tmp_path("many");
        let mut reg = AccountRegistry::open(&path).unwrap();
        for i in 0..200 {
            reg.add(account(&format!("user{i}@example.com"), "imap.example.com"))
                .unwrap();
        }
        assert_eq!(AccountRegistry::open(&path).unwrap().len(), 200);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn rejects_duplicate_mailboxes_but_allows_same_address_on_other_server() {
        let (path, dir) = tmp_path("dup");
        let mut reg = AccountRegistry::open(&path).unwrap();
        reg.add(account("a@example.com", "imap.example.com"))
            .unwrap();
        assert!(matches!(
            reg.add(account("a@example.com", "imap.example.com")),
            Err(Error::DuplicateAccount(_))
        ));
        reg.add(account("a@example.com", "imap.other.example.net"))
            .unwrap();
        assert_eq!(reg.len(), 2);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn remove_persists_and_unknown_id_errors() {
        let (path, dir) = tmp_path("rm");
        let mut reg = AccountRegistry::open(&path).unwrap();
        let a = account("a@example.com", "imap.example.com");
        let id = a.id.clone();
        reg.add(a).unwrap();
        reg.remove(&id).unwrap();
        assert!(AccountRegistry::open(&path).unwrap().is_empty());
        assert!(matches!(reg.remove(&id), Err(Error::AccountNotFound(_))));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn corrupt_file_is_reported_and_not_overwritten() {
        let (path, dir) = tmp_path("corrupt");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "this is = not [valid").unwrap();
        let err = AccountRegistry::open(&path).unwrap_err();
        assert!(matches!(err, Error::CorruptAccountFile { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), "this is = not [valid");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn future_version_and_invalid_utf8_and_oversize_are_rejected() {
        let (path, dir) = tmp_path("ver");
        fs::create_dir_all(path.parent().unwrap()).unwrap();

        fs::write(&path, "version = 99\n").unwrap();
        assert!(matches!(
            AccountRegistry::open(&path),
            Err(Error::CorruptAccountFile { .. })
        ));

        fs::write(&path, [0xff_u8, 0xfe, 0x00, 0x41]).unwrap();
        assert!(matches!(
            AccountRegistry::open(&path),
            Err(Error::CorruptAccountFile { .. })
        ));

        let big = vec![b'#'; (MAX_FILE_BYTES + 10) as usize];
        fs::write(&path, big).unwrap();
        assert!(matches!(
            AccountRegistry::open(&path),
            Err(Error::AccountFileTooLarge { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn tampered_account_in_file_is_rejected() {
        let (path, dir) = tmp_path("tamper");
        let mut reg = AccountRegistry::open(&path).unwrap();
        reg.add(account("a@example.com", "imap.example.com"))
            .unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let tampered = text.replace("imap.example.com", "imap.exa mple.com");
        fs::write(&path, tampered).unwrap();
        assert!(matches!(
            AccountRegistry::open(&path),
            Err(Error::CorruptAccountFile { .. })
        ));
        let _ = fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn file_and_directory_are_private_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let (path, dir) = tmp_path("perm");
        let mut reg = AccountRegistry::open(&path).unwrap();
        reg.add(account("a@example.com", "imap.example.com"))
            .unwrap();
        let file_mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let dir_mode = fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(file_mode, 0o600);
        assert_eq!(dir_mode, 0o700);
        let _ = fs::remove_dir_all(dir);
    }
}
