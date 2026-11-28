//! Secret storage. Passwords and OAuth refresh tokens go to the OS credential
//! store and nowhere else.
//!
//! * Windows: Credential Manager. Linux: Secret Service (GNOME Keyring, KWallet,
//!   KeePassXC). If no store is available the call **fails**; there is no
//!   plaintext fallback.
//! * The calls are blocking; the Secret Service can take tens to hundreds of
//!   milliseconds per call, so async callers should use `spawn_blocking`.
//! * Windows Credential Manager does not guarantee call ordering across threads,
//!   so [`KeyringStore`] serialises its calls behind one process-wide lock.

use std::collections::HashMap;
use std::sync::Mutex;

use secrecy::{ExposeSecret, SecretString};

use crate::Result;
use crate::account::AccountId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SecretKind {
    Password,
    OAuthRefreshToken,
}

impl SecretKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Password => "password",
            Self::OAuthRefreshToken => "oauth-refresh-token",
        }
    }
}

pub trait SecretStore: Send + Sync {
    fn set(&self, account: &AccountId, kind: SecretKind, value: &SecretString) -> Result<()>;
    /// `Ok(None)` means "no such secret"; `Err` means the store itself failed.
    fn get(&self, account: &AccountId, kind: SecretKind) -> Result<Option<SecretString>>;
    /// Deleting a secret that does not exist is not an error.
    fn delete(&self, account: &AccountId, kind: SecretKind) -> Result<()>;
}

/// In-memory store for tests and the CLI harness only. **Not secure persistence**:
/// contents vanish on exit (values are zeroised on drop by `secrecy`).
#[derive(Debug, Default)]
pub struct MemorySecretStore {
    inner: Mutex<HashMap<(AccountId, SecretKind), SecretString>>,
}

impl MemorySecretStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<(AccountId, SecretKind), SecretString>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl SecretStore for MemorySecretStore {
    fn set(&self, account: &AccountId, kind: SecretKind, value: &SecretString) -> Result<()> {
        let copy = SecretString::from(value.expose_secret().to_owned());
        self.lock().insert((account.clone(), kind), copy);
        Ok(())
    }

    fn get(&self, account: &AccountId, kind: SecretKind) -> Result<Option<SecretString>> {
        Ok(self
            .lock()
            .get(&(account.clone(), kind))
            .map(|s| SecretString::from(s.expose_secret().to_owned())))
    }

    fn delete(&self, account: &AccountId, kind: SecretKind) -> Result<()> {
        self.lock().remove(&(account.clone(), kind));
        Ok(())
    }
}

#[cfg(any(windows, target_os = "linux"))]
pub use os::KeyringStore;

#[cfg(any(windows, target_os = "linux"))]
mod os {
    use std::sync::Mutex;

    use secrecy::{ExposeSecret, SecretString};

    use super::{SecretKind, SecretStore};
    use crate::account::AccountId;
    use crate::{Error, Result};

    static KEYRING_LOCK: Mutex<()> = Mutex::new(());

    /// OS credential store backed by the `keyring` crate.
    #[derive(Debug, Clone)]
    pub struct KeyringStore {
        service: String,
    }

    impl KeyringStore {
        pub fn new(service: &str) -> Self {
            Self {
                service: service.to_owned(),
            }
        }

        fn entry(&self, account: &AccountId, kind: SecretKind) -> Result<keyring::Entry> {
            let user = format!("{}:{}", account.as_str(), kind.as_str());
            keyring::Entry::new(&self.service, &user).map_err(map_err)
        }
    }

    fn map_err(e: keyring::Error) -> Error {
        Error::SecretStore(e.to_string())
    }

    impl SecretStore for KeyringStore {
        fn set(&self, account: &AccountId, kind: SecretKind, value: &SecretString) -> Result<()> {
            let _guard = KEYRING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            self.entry(account, kind)?
                .set_password(value.expose_secret())
                .map_err(map_err)
        }

        fn get(&self, account: &AccountId, kind: SecretKind) -> Result<Option<SecretString>> {
            let _guard = KEYRING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            match self.entry(account, kind)?.get_password() {
                Ok(p) => Ok(Some(SecretString::from(p))),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(e) => Err(map_err(e)),
            }
        }

        fn delete(&self, account: &AccountId, kind: SecretKind) -> Result<()> {
            let _guard = KEYRING_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            match self.entry(account, kind)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(e) => Err(map_err(e)),
            }
        }
    }
}

/// Reads from the current store first and, when a secret is only found in the
/// old one (the store of a previous name of this program), moves it across.
/// Used once after the program was renamed so nobody has to sign in again.
/// A secret is removed from the old store only after the new store has it.
pub struct MigratingStore {
    current: std::sync::Arc<dyn SecretStore>,
    legacy: std::sync::Arc<dyn SecretStore>,
}

impl std::fmt::Debug for MigratingStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MigratingStore")
    }
}

impl MigratingStore {
    pub fn new(
        current: std::sync::Arc<dyn SecretStore>,
        legacy: std::sync::Arc<dyn SecretStore>,
    ) -> Self {
        Self { current, legacy }
    }
}

impl SecretStore for MigratingStore {
    fn set(&self, account: &AccountId, kind: SecretKind, value: &SecretString) -> Result<()> {
        self.current.set(account, kind, value)?;
        // No stale copy may linger in the old store.
        let _ = self.legacy.delete(account, kind);
        Ok(())
    }

    fn get(&self, account: &AccountId, kind: SecretKind) -> Result<Option<SecretString>> {
        if let Some(v) = self.current.get(account, kind)? {
            return Ok(Some(v));
        }
        match self.legacy.get(account, kind) {
            Ok(Some(v)) => {
                // Only forget the old copy once the new one is safely stored.
                if self.current.set(account, kind, &v).is_ok() {
                    let _ = self.legacy.delete(account, kind);
                }
                Ok(Some(v))
            }
            // An old store that is missing or locked must not break the new one.
            Ok(None) | Err(_) => Ok(None),
        }
    }

    fn delete(&self, account: &AccountId, kind: SecretKind) -> Result<()> {
        self.current.delete(account, kind)?;
        let _ = self.legacy.delete(account, kind);
        Ok(())
    }
}

/// Removes every secret belonging to an account (call after deleting it).
pub fn purge_account(store: &dyn SecretStore, account: &AccountId) -> Result<()> {
    store.delete(account, SecretKind::Password)?;
    store.delete(account, SecretKind::OAuthRefreshToken)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_secret_from_the_old_name_is_moved_to_the_new_store_once() {
        use std::sync::Arc;
        let old = Arc::new(MemorySecretStore::new());
        let new = Arc::new(MemorySecretStore::new());
        let id = AccountId::generate();
        old.set(
            &id,
            SecretKind::Password,
            &SecretString::from("pw".to_owned()),
        )
        .unwrap();
        let both = MigratingStore::new(new.clone(), old.clone());
        let got = both.get(&id, SecretKind::Password).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "pw");
        assert!(new.get(&id, SecretKind::Password).unwrap().is_some());
        assert!(old.get(&id, SecretKind::Password).unwrap().is_none());
        // Nothing found anywhere stays nothing.
        assert!(
            both.get(&id, SecretKind::OAuthRefreshToken)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn new_secrets_go_to_the_new_store_and_clear_stale_old_ones() {
        use std::sync::Arc;
        let old = Arc::new(MemorySecretStore::new());
        let new = Arc::new(MemorySecretStore::new());
        let id = AccountId::generate();
        old.set(
            &id,
            SecretKind::Password,
            &SecretString::from("stale".to_owned()),
        )
        .unwrap();
        let both = MigratingStore::new(new.clone(), old.clone());
        both.set(
            &id,
            SecretKind::Password,
            &SecretString::from("fresh".to_owned()),
        )
        .unwrap();
        assert_eq!(
            new.get(&id, SecretKind::Password)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "fresh"
        );
        assert!(old.get(&id, SecretKind::Password).unwrap().is_none());
        both.delete(&id, SecretKind::Password).unwrap();
        assert!(both.get(&id, SecretKind::Password).unwrap().is_none());
    }

    #[test]
    fn memory_store_round_trip_and_purge() {
        let store = MemorySecretStore::new();
        let id = AccountId::generate();
        assert!(store.get(&id, SecretKind::Password).unwrap().is_none());

        store
            .set(
                &id,
                SecretKind::Password,
                &SecretString::from("hunter2".to_owned()),
            )
            .unwrap();
        store
            .set(
                &id,
                SecretKind::OAuthRefreshToken,
                &SecretString::from("tok".to_owned()),
            )
            .unwrap();
        let got = store.get(&id, SecretKind::Password).unwrap().unwrap();
        assert_eq!(got.expose_secret(), "hunter2");

        purge_account(&store, &id).unwrap();
        assert!(store.get(&id, SecretKind::Password).unwrap().is_none());
        assert!(
            store
                .get(&id, SecretKind::OAuthRefreshToken)
                .unwrap()
                .is_none()
        );
        // Deleting again is fine.
        purge_account(&store, &id).unwrap();
    }

    #[test]
    fn secrets_are_per_account_and_per_kind() {
        let store = MemorySecretStore::new();
        let (a, b) = (AccountId::generate(), AccountId::generate());
        store
            .set(
                &a,
                SecretKind::Password,
                &SecretString::from("pa".to_owned()),
            )
            .unwrap();
        assert!(store.get(&b, SecretKind::Password).unwrap().is_none());
        assert!(
            store
                .get(&a, SecretKind::OAuthRefreshToken)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn debug_output_never_contains_the_secret() {
        let s = SecretString::from("topsecret-value".to_owned());
        assert!(!format!("{s:?}").contains("topsecret-value"));
    }

    /// Needs a running Secret Service / Credential Manager, so it is opt-in:
    /// `cargo test -p mailcore -- --ignored os_keyring`
    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    #[ignore = "requires an OS credential store"]
    fn os_keyring_round_trip() {
        let store = KeyringStore::new("emayl-test");
        let id = AccountId::generate();
        store
            .set(
                &id,
                SecretKind::Password,
                &SecretString::from("pw".to_owned()),
            )
            .unwrap();
        assert_eq!(
            store
                .get(&id, SecretKind::Password)
                .unwrap()
                .unwrap()
                .expose_secret(),
            "pw"
        );
        store.delete(&id, SecretKind::Password).unwrap();
        assert!(store.get(&id, SecretKind::Password).unwrap().is_none());
    }
}
