//! Everything the window needs from the mail engine, in one place: accounts,
//! the local database, secrets, background sync and on-demand downloads.
//!
//! The interface never talks to servers directly; it calls these functions from
//! background tasks and gets plain values back.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use mailcore::account::{Account, AccountId};
use mailcore::diagnostics::{Diagnosis, diagnose};
use mailcore::drafts::Drafts;
use mailcore::imap::{ImapSession, connect_account};
use mailcore::net::{Timeouts, client_config};
use mailcore::oauth::{HttpTransport, OAuthClients, TokenManager};
use mailcore::outbox::{Outbox, OutboxItem};
use mailcore::registry::AccountRegistry;
use mailcore::render::{self, RenderOptions, RenderedMessage};
use mailcore::secrets::{SecretKind, SecretStore, purge_account};
use mailcore::sender::Env;
use mailcore::store::{Recovery, Store};
use mailcore::syncer::{self, SyncError, SyncHandle, SyncOptions};
use mailcore::{Error, Result};
use secrecy::SecretString;

#[cfg(not(any(windows, target_os = "linux")))]
compile_error!("EMAYL supports Windows and Linux only.");

mod oauth;
mod send;
mod userdata;
pub use oauth::{SignIn, built_in_clients, clients_from};
pub use send::collect_attachments;

const KEYRING_SERVICE: &str = "dev.emayl.EMAYL";
/// What this program was called before it was renamed; read once to carry
/// saved passwords over.
const LEGACY_KEYRING_SERVICE: &str = "dev.rustmail.Rustmail";

/// Where the program keeps its files.
#[derive(Debug, Clone)]
pub struct Paths {
    /// `accounts.toml` lives here.
    pub config: PathBuf,
    /// The mail database lives here.
    pub data: PathBuf,
}

impl Paths {
    pub fn platform_default() -> Option<Self> {
        let new = Self::from_dirs(&directories::ProjectDirs::from("dev", "emayl", "EMAYL")?);
        let old = directories::ProjectDirs::from("dev", "rustmail", "Rustmail")
            .map(|d| Self::from_dirs(&d));
        Some(match old {
            Some(old) => new.adopt_legacy(old),
            None => new,
        })
    }

    fn from_dirs(d: &directories::ProjectDirs) -> Self {
        Self {
            config: d.config_dir().to_owned(),
            data: d.data_dir().to_owned(),
        }
    }

    /// After the rename: if the old folders exist and the new ones do not, move
    /// them over (a rename, so nothing is copied or lost). If that cannot be
    /// done the old folders keep being used, so mail never seems to vanish.
    pub(crate) fn adopt_legacy(self, old: Self) -> Self {
        let pairs = [
            (old.data.clone(), self.data.clone()),
            (old.config.clone(), self.config.clone()),
        ];
        if pairs.iter().all(|(from, to)| !from.exists() || to.exists()) {
            return self;
        }
        let mut moved: Vec<(PathBuf, PathBuf)> = Vec::new();
        for (from, to) in pairs {
            if !from.exists() || to.exists() {
                continue;
            }
            let ok = to
                .parent()
                .is_none_or(|p| std::fs::create_dir_all(p).is_ok())
                && std::fs::rename(&from, &to).is_ok();
            if !ok {
                tracing::warn!("could not move the old folders; keeping the old location");
                for (f, t) in moved {
                    let _ = std::fs::rename(t, f);
                }
                return old;
            }
            moved.push((from, to));
        }
        self
    }

    pub fn under(root: &Path) -> Self {
        Self {
            config: root.join("config"),
            data: root.join("data"),
        }
    }
}

type Job = Box<dyn FnOnce() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()>>> + Send>;

/// One background thread that does all the network work of the mail engine.
///
/// The engine's futures are not required to be `Send` (they hold things like
/// open connections), so they cannot be handed to the window library's
/// thread pool. They run here instead, on a single-threaded runtime, which is
/// plenty for waiting on servers. A connection created here also stays valid
/// here for as long as it is kept.
struct Worker {
    tx: tokio::sync::mpsc::UnboundedSender<Job>,
}

impl Worker {
    fn start() -> Self {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Job>();
        let spawned = std::thread::Builder::new()
            .name("emayl-mail".into())
            .spawn(move || {
                let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    tracing::error!("could not start the mail worker runtime");
                    return;
                };
                let local = tokio::task::LocalSet::new();
                rt.block_on(local.run_until(async move {
                    while let Some(job) = rx.recv().await {
                        tokio::task::spawn_local(job());
                    }
                }));
            });
        if spawned.is_err() {
            tracing::error!("could not start the mail worker thread");
        }
        Self { tx }
    }

    fn run<R, F>(
        &self,
        make: impl FnOnce() -> F + Send + 'static,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Option<R>> + Send>>
    where
        R: Send + 'static,
        F: std::future::Future<Output = R> + 'static,
    {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let job: Job = Box::new(move || {
            Box::pin(async move {
                let r = make().await;
                let _ = done_tx.send(r);
            })
        });
        let sent = self.tx.send(job).is_ok();
        Box::pin(async move {
            if !sent {
                return None;
            }
            done_rx.await.ok()
        })
    }
}

type SessionSlot = Arc<tokio::sync::Mutex<Option<ImapSession>>>;

pub struct Services {
    /// Keeps other copies of the program away from these files; released on exit.
    _instance: mailcore::instance::InstanceLock,
    pub paths: Paths,
    pub store: Arc<Store>,
    /// Set if the database had to be rebuilt at start-up.
    pub recovery: Recovery,
    registry: Mutex<AccountRegistry>,
    pub secrets: Arc<dyn SecretStore>,
    tls: Arc<mailcore::net::TlsConfig>,
    tokens: TokenManager<HttpTransport>,
    clients: OAuthClients,
    /// One reusable connection per account for opening messages.
    fetchers: Mutex<HashMap<String, SessionSlot>>,
    worker: Worker,
    /// Messages waiting to be sent (or recently sent), kept on disk.
    outbox: Arc<Outbox>,
    /// What the Outbox screen shows, with a counter that changes whenever it does.
    outbox_view: Mutex<(u64, Vec<OutboxItem>)>,
    /// Messages being written, saved as the person types.
    drafts: Arc<Drafts>,
    /// Blocked senders and filter rules (their own small file, not the mail database).
    rulebook: mailcore::rules::RuleBook,
    /// Saved reply texts.
    templatebook: mailcore::templates::TemplateBook,
    /// Serializes changes to the two files above.
    userdata_lock: Mutex<()>,
    /// Wakes the sending loop (a message was queued or retried).
    send_wake: Arc<tokio::sync::Notify>,
    /// Stops a website sign-in that is waiting for the browser.
    sign_in_cancel: Mutex<Option<Arc<tokio::sync::Notify>>>,
}

impl std::fmt::Debug for Services {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Services")
            .field("paths", &self.paths)
            .finish_non_exhaustive()
    }
}

fn timeouts() -> Timeouts {
    Timeouts::default()
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Services {
    /// Opens everything with the real password store of the operating system.
    pub fn open(paths: Paths) -> Result<Self> {
        #[cfg(any(windows, target_os = "linux"))]
        let secrets: Arc<dyn SecretStore> = Arc::new(mailcore::secrets::MigratingStore::new(
            Arc::new(mailcore::secrets::KeyringStore::new(KEYRING_SERVICE)),
            Arc::new(mailcore::secrets::KeyringStore::new(LEGACY_KEYRING_SERVICE)),
        ));
        #[cfg(not(any(windows, target_os = "linux")))]
        let secrets: Arc<dyn SecretStore> = Arc::new(mailcore::secrets::MemorySecretStore::new());
        Self::open_with(paths, secrets)
    }

    pub fn open_with(paths: Paths, secrets: Arc<dyn SecretStore>) -> Result<Self> {
        // First of all: only one copy of the program may use these files.
        let instance = mailcore::instance::InstanceLock::acquire(&paths.data)?;
        let registry = AccountRegistry::open(paths.config.join("accounts.toml"))?;
        let (store, recovery) = Store::open_or_recover(&paths.data)?;
        let tls = client_config()?;
        let transport = HttpTransport::new().map_err(|e| Error::HttpClient(e.to_string()))?;
        // Mail is only a cache: whatever belongs to an account that no longer exists
        // (a crash while removing it, or a last sync write that came after) is deleted.
        let known: Vec<&str> = registry.accounts().iter().map(|a| a.id.as_str()).collect();
        let _ = store.retain_accounts(&known);
        let outbox = Outbox::open(&paths.data.join("outbox"))?;
        let drafts = Drafts::open(&paths.data.join("drafts"))?;
        let rulebook = mailcore::rules::RuleBook::open(&paths.data);
        store.set_rules(rulebook.load().unwrap_or_default());
        let templatebook = mailcore::templates::TemplateBook::open(&paths.data);
        let first_view = outbox.list().map(|l| l.items).unwrap_or_default();
        Ok(Self {
            _instance: instance,
            outbox: Arc::new(outbox),
            outbox_view: Mutex::new((1, first_view)),
            drafts: Arc::new(drafts),
            rulebook,
            templatebook,
            userdata_lock: Mutex::new(()),
            send_wake: Arc::new(tokio::sync::Notify::new()),
            sign_in_cancel: Mutex::new(None),
            paths,
            store: Arc::new(store),
            recovery,
            registry: Mutex::new(registry),
            tokens: TokenManager::new(secrets.clone(), transport),
            secrets,
            tls,
            // Sign-in through the provider's website needs the app owner's
            // registration with that provider; it is given when the program is
            // built (see MAINTAINING.md, section F) and is empty otherwise.
            clients: built_in_clients(),
            fetchers: Mutex::new(HashMap::new()),
            worker: Worker::start(),
        })
    }

    pub fn env(&self) -> Env<'_, HttpTransport> {
        Env {
            secrets: &self.secrets,
            tokens: &self.tokens,
            clients: &self.clients,
            tls: &self.tls,
            timeouts: timeouts(),
        }
    }

    pub fn accounts(&self) -> Vec<Account> {
        lock(&self.registry).accounts().to_vec()
    }

    pub fn account(&self, id: &str) -> Option<Account> {
        let id = AccountId::parse(id).ok()?;
        lock(&self.registry).get(&id).cloned()
    }

    /// Whether this provider's website sign-in is available in this build.
    pub fn oauth_available(&self, provider: mailcore::account::OAuthProvider) -> bool {
        self.clients.get(provider).is_some()
    }

    /// Saves the password and the account. If saving the account fails the
    /// password is removed again, so nothing is left behind.
    pub fn add_account(&self, account: Account, password: &SecretString) -> Result<()> {
        self.secrets
            .set(&account.id, SecretKind::Password, password)?;
        let added = lock(&self.registry).add(account.clone());
        if let Err(e) = added {
            let _ = self.secrets.delete(&account.id, SecretKind::Password);
            return Err(e);
        }
        Ok(())
    }

    pub fn set_password(&self, id: &AccountId, password: &SecretString) -> Result<()> {
        self.secrets.set(id, SecretKind::Password, password)
    }

    /// Forgets an account: its entry, its password, and everything stored for it.
    /// Every step is tried even if an earlier one failed, so a problem in one place
    /// never leaves the other copies of the person's data behind; the first error is reported.
    pub fn remove_account(&self, id: &AccountId) -> Result<()> {
        let removed = lock(&self.registry).remove(id)?;
        let secrets = purge_account(self.secrets.as_ref(), &removed.id);
        lock(&self.fetchers).remove(removed.id.as_str());
        let cache = self.store.delete_account(removed.id.as_str());
        let queued = self.outbox.forget_account(&removed.id).map(|_| ());
        let drafts = self.drafts.delete_account(&removed.id).map(|_| ());
        let rules = {
            let _g = lock(&self.userdata_lock);
            self.rulebook
                .remove_account(removed.id.as_str())
                .map(|left| {
                    self.store.set_rules(left);
                })
        };
        self.refresh_outbox();
        secrets.and(cache).and(queued).and(drafts).and(rules)
    }

    /// Tries both servers with the password already stored for `account`.
    async fn check(&self, account: &Account) -> Diagnosis {
        diagnose(&self.env(), account).await
    }

    /// Starts the sync loop for an account. Returns the handle that controls it and
    /// a future that completes when the loop has stopped.
    pub fn start_sync(
        self: &Arc<Self>,
        account: Account,
    ) -> (
        SyncHandle,
        impl std::future::Future<Output = ()> + Send + use<>,
    ) {
        let (handle, ctl) = syncer::sync_channel();
        let me = Arc::clone(self);
        let done = self.worker.run(move || async move {
            let env = me.env();
            syncer::run_account_loop(
                &env,
                &account,
                Arc::clone(&me.store),
                SyncOptions::default(),
                ctl,
            )
            .await;
        });
        (handle, async move {
            let _ = done.await;
        })
    }

    /// Checks both servers of an account (runs on the mail worker).
    pub fn check_account(
        self: &Arc<Self>,
        account: Account,
    ) -> impl std::future::Future<Output = Option<Diagnosis>> + Send + use<> {
        let me = Arc::clone(self);
        self.worker
            .run(move || async move { me.check(&account).await })
    }

    /// Downloads (if needed) and renders a message (runs on the mail worker).
    pub fn read_message(
        self: &Arc<Self>,
        message_id: i64,
        prefer_plain: bool,
    ) -> impl std::future::Future<Output = std::result::Result<RenderedMessage, String>> + Send + use<>
    {
        let me = Arc::clone(self);
        let job = self.worker.run(move || async move {
            me.open_message(message_id, prefer_plain)
                .await
                .map_err(describe_sync_error)
        });
        async move {
            job.await.unwrap_or_else(|| {
                Err("The mail engine stopped unexpectedly. Please restart the program.".to_owned())
            })
        }
    }

    /// Looks up server settings for an address (built-in list, then the provider's
    /// own published settings). Runs on the mail worker.
    pub fn discover(
        self: &Arc<Self>,
        email: mailcore::email::EmailAddress,
    ) -> impl std::future::Future<
        Output = std::result::Result<mailcore::provider::ResolvedConfig, String>,
    > + Send
    + use<> {
        let job = self.worker.run(move || async move {
            let fetcher = mailcore::autoconfig::HttpFetcher::new().map_err(|e| e.to_string())?;
            mailcore::autoconfig::discover(&fetcher, &email)
                .await
                .map_err(|e| e.to_string())
        });
        async move {
            job.await.unwrap_or_else(|| {
                Err("The mail engine stopped unexpectedly. Please restart the program.".to_owned())
            })
        }
    }

    /// Downloads another page of older messages for a folder (runs on the mail worker).
    pub fn load_older(
        self: &Arc<Self>,
        folder_id: i64,
    ) -> impl std::future::Future<Output = std::result::Result<usize, String>> + Send + use<> {
        let me = Arc::clone(self);
        let job = self.worker.run(move || async move {
            let store = Arc::clone(&me.store);
            let row = tokio::task::spawn_blocking(move || store.folder(folder_id))
                .await
                .map_err(|e| e.to_string())?
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "This folder no longer exists.".to_owned())?;
            let account = me
                .account(&row.account)
                .ok_or_else(|| "This account no longer exists.".to_owned())?;
            let mut session = connect_account(
                &account,
                &me.secrets,
                &me.tokens,
                &me.clients,
                &me.tls,
                timeouts(),
            )
            .await
            .map_err(|e| format!("{e} {}", e.advice()))?;
            let r = syncer::load_older(
                &mut session,
                &me.store,
                folder_id,
                500,
                &SyncOptions::default(),
            )
            .await;
            session.logout().await;
            r.map_err(describe_sync_error)
        });
        async move {
            job.await.unwrap_or_else(|| {
                Err("The mail engine stopped unexpectedly. Please restart the program.".to_owned())
            })
        }
    }

    fn slot(&self, account: &str) -> SessionSlot {
        Arc::clone(lock(&self.fetchers).entry(account.to_owned()).or_default())
    }

    /// The raw message, downloaded now if it is not stored yet.
    pub(crate) async fn raw_message(
        &self,
        message_id: i64,
    ) -> std::result::Result<Vec<u8>, SyncError> {
        let store = Arc::clone(&self.store);
        let msg = tokio::task::spawn_blocking(move || store.message(message_id))
            .await
            .map_err(|e| SyncError::Store(Error::Database(e.to_string())))??
            .ok_or(SyncError::Gone)?;
        // Already stored: no connection needed.
        let s2 = Arc::clone(&self.store);
        if let Some(raw) = tokio::task::spawn_blocking(move || s2.body(message_id))
            .await
            .map_err(|e| SyncError::Store(Error::Database(e.to_string())))??
        {
            return Ok(raw);
        }
        let account = self.account(&msg.account).ok_or(SyncError::Gone)?;
        let slot = self.slot(&msg.account);
        let mut guard = slot.lock().await;
        let opts = SyncOptions::default();
        // A connection that has been idle may have been closed by the server:
        // on a connection-level failure, one fresh connection is tried.
        for attempt in 0..2 {
            let mut session = match guard.take() {
                Some(s) => s,
                None => {
                    connect_account(
                        &account,
                        &self.secrets,
                        &self.tokens,
                        &self.clients,
                        &self.tls,
                        timeouts(),
                    )
                    .await?
                }
            };
            match syncer::fetch_body(&mut session, &self.store, message_id, &opts).await {
                Ok(raw) => {
                    *guard = Some(session);
                    return Ok(raw);
                }
                Err(SyncError::Server(e)) if attempt == 0 && e.is_transient() => {}
                Err(e @ (SyncError::Gone | SyncError::TooLarge(_) | SyncError::FolderChanged)) => {
                    *guard = Some(session);
                    return Err(e);
                }
                Err(e) => return Err(e),
            }
        }
        Err(SyncError::Gone)
    }

    /// Downloads the newest messages of the account's Inbox in the background, so that
    /// opening one is instant. Limits: a few messages, none large, and a total budget.
    /// Each message is fetched under the same lock a person's own "open" uses, so an open
    /// waits for at most one small download. Returns how many were downloaded.
    pub fn prefetch(
        self: &Arc<Self>,
        account: String,
    ) -> impl std::future::Future<Output = usize> + Send + use<> {
        let me = Arc::clone(self);
        let job = self
            .worker
            .run(move || async move { me.prefetch_inbox(&account).await });
        async move { job.await.unwrap_or(0) }
    }

    async fn prefetch_inbox(&self, account: &str) -> usize {
        let store = Arc::clone(&self.store);
        let acct = account.to_owned();
        let ids = tokio::task::spawn_blocking(move || syncer::prefetch_plan(&store, &acct))
            .await
            .unwrap_or_default();
        let mut done = 0;
        for id in ids {
            match self.raw_message(id).await {
                Ok(_) => done += 1,
                // Gone or too large: just this one. Anything else (offline, refused
                // sign-in): stop quietly; the person's own "open" will explain it.
                Err(SyncError::Gone | SyncError::TooLarge(_) | SyncError::FolderChanged) => {}
                Err(_) => break,
            }
        }
        done
    }

    /// Downloads (if needed) and renders a message for reading.
    async fn open_message(
        &self,
        message_id: i64,
        prefer_plain: bool,
    ) -> std::result::Result<RenderedMessage, SyncError> {
        let raw = self.raw_message(message_id).await?;
        tokio::task::spawn_blocking(move || render::render(&raw, &RenderOptions { prefer_plain }))
            .await
            .map_err(|e| SyncError::Store(Error::Database(e.to_string())))?
            .map_err(|e| {
                SyncError::Store(Error::NotFound(format!(
                    "this message cannot be shown: {e}"
                )))
            })
    }
}

/// A sentence a person can act on.
pub fn describe_sync_error(e: SyncError) -> String {
    match e {
        SyncError::Server(c) => format!("{} {}", c, c.advice()),
        SyncError::Store(e) => e.to_string(),
        SyncError::TooLarge(_) => "This message is too large to open here.".to_owned(),
        SyncError::Gone => "This message is no longer on the server.".to_owned(),
        SyncError::FolderChanged => {
            "The folder changed on the server. Refresh and try again.".to_owned()
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod rename_tests {
    use super::*;

    fn temp(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("emayl-rename-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).expect("temp dir");
        d
    }

    #[test]
    fn old_folders_are_moved_to_the_new_names_once() {
        let root = temp("move");
        let old = Paths::under(&root.join("old"));
        let new = Paths::under(&root.join("new"));
        std::fs::create_dir_all(&old.data).unwrap();
        std::fs::create_dir_all(&old.config).unwrap();
        std::fs::write(old.data.join("mail.db"), b"mail").unwrap();
        std::fs::write(old.config.join("accounts.toml"), b"a").unwrap();
        let got = new.clone().adopt_legacy(old.clone());
        assert_eq!(got.data, new.data);
        assert_eq!(std::fs::read(new.data.join("mail.db")).unwrap(), b"mail");
        assert_eq!(
            std::fs::read(new.config.join("accounts.toml")).unwrap(),
            b"a"
        );
        assert!(!old.data.exists() && !old.config.exists());
        // A second start changes nothing.
        let again = new.clone().adopt_legacy(old);
        assert_eq!(again.data, new.data);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn existing_new_folders_are_never_overwritten() {
        let root = temp("keep");
        let old = Paths::under(&root.join("old"));
        let new = Paths::under(&root.join("new"));
        for p in [&old.data, &old.config, &new.data, &new.config] {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(old.data.join("x"), b"old").unwrap();
        std::fs::write(new.data.join("x"), b"new").unwrap();
        let got = new.clone().adopt_legacy(old.clone());
        assert_eq!(got.data, new.data);
        assert_eq!(std::fs::read(new.data.join("x")).unwrap(), b"new");
        assert_eq!(std::fs::read(old.data.join("x")).unwrap(), b"old");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_fresh_install_just_uses_the_new_folders() {
        let root = temp("fresh");
        let new = Paths::under(&root.join("new"));
        let old = Paths::under(&root.join("old"));
        assert_eq!(new.clone().adopt_legacy(old).data, new.data);
        let _ = std::fs::remove_dir_all(&root);
    }
}
