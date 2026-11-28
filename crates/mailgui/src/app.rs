//! The application state and everything that changes it.
//!
//! `update` never blocks: database work and network work run as background
//! tasks and report back with a message. The view (see `view/`) only reads state.

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use std::time::Duration;

use iced::{Size, Task};
use mailcore::account::{Account, AccountId};
use mailcore::diagnostics::Diagnosis;
use mailcore::drafts::DraftSummary;
use mailcore::imap::FolderRole;
use mailcore::outbox::OutboxItem;
use mailcore::provider::ResolvedConfig;
use mailcore::render::{Link, RenderedMessage};
use mailcore::store::{
    FolderRow, ListScope, MessageRow, Recovery, SearchHit, SearchQuery, SearchScope, SearchSort,
    ThreadRow,
};
use mailcore::syncer::{self, Deleted, SyncHandle, SyncStatus};
use secrecy::SecretString;

use crate::accounts_flow::SigningIn;
use crate::compose_flow::{ComposeMsg, ComposeState, OutboxMsg, SendWarning};
use crate::prefs::{self, Prefs};
use crate::services::{Paths, Services, SignIn};
use crate::util;
use crate::wizard::{Outcome, SecurityChoice, Step, Wizard};

const PAGE: usize = 100;

/// Identifies the conversation list's scroll area.
pub const LIST_SCROLL: &str = "mail-list";
const SEARCH_DELAY: Duration = Duration::from_millis(220);
const TOAST_SECS: i64 = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Nav {
    AllInboxes,
    Folder { account: String, folder_id: i64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    ToggleRead,
    /// Used by the bulk bar, where "toggle" would be unclear.
    MarkRead,
    MarkUnread,
    /// Back from Junk to the Inbox.
    NotSpam,
    ToggleStar,
    Archive,
    Delete,
    Spam,
}

#[derive(Debug, Clone)]
pub enum BodyState {
    Loading,
    Ready(Box<RenderedMessage>),
    Failed(String),
}

#[derive(Debug, Clone)]
pub struct Reader {
    pub thread_key: String,
    pub messages: Vec<MessageRow>,
    pub expanded: BTreeSet<i64>,
    /// Messages whose quoted earlier messages are unfolded.
    pub quotes_open: BTreeSet<i64>,
    /// Messages whose details (privacy, signature checks) are shown.
    pub details_open: BTreeSet<i64>,
    /// Which messages the toolbar acts on: those in this folder (or any inbox).
    pub scope: ListScope,
}

#[derive(Debug, Clone)]
pub enum Dialog {
    Link(Link),
    RemoveAccount(String),
    ChangePassword {
        account: String,
        input: String,
    },
    Report {
        title: String,
        ok: bool,
        text: String,
    },
    Info {
        title: String,
        text: String,
    },
    /// There is no Trash folder: deleting would be permanent.
    PermanentDelete(Vec<(i64, u32)>),
    /// Something looks unintended about the message that is about to go out.
    ConfirmSend(Vec<SendWarning>),
    /// Throw away the message being written.
    DiscardDraft,
    /// Saving a file that can run programs.
    SaveRisky {
        message: i64,
        index: usize,
        name: String,
        disguised: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Mailbox,
    /// The add-account steps; `first` is true when no account exists yet.
    AddAccount {
        first: bool,
    },
    Accounts,
    /// Writing a message.
    Compose,
    /// Messages waiting to be sent, failed ones, and unfinished drafts.
    Outbox,
    /// Version, folders, privacy notes and the support report.
    Settings,
    /// Blocked senders and filter rules.
    Filters,
    /// Saved reply texts.
    Templates,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub text: String,
    pub bad: bool,
    pub until: i64,
}

/// A message that was sent a moment ago and can still be taken back.
#[derive(Debug)]
pub struct UndoSend {
    pub outbox_id: String,
    pub subject: String,
    /// When the message is really sent (seconds since the epoch).
    pub until: i64,
    /// What was typed, so "Undo" can open it again.
    pub compose: Box<crate::compose_flow::ComposeState>,
    /// "Undo" was pressed and is being carried out.
    pub working: bool,
}

#[derive(Debug)]
pub struct AccountState {
    pub account: Account,
    pub handle: SyncHandle,
    pub status: SyncStatus,
    /// What the last refresh of the lists was based on.
    seen: (i64, u64),
    /// Messages stored by the running pass when the lists were last refreshed.
    progress_seen: usize,
}

#[derive(Debug)]
pub struct App {
    pub svc: Option<Arc<Services>>,
    pub fatal: Option<String>,
    pub screen: Screen,
    pub accounts: Vec<AccountState>,
    pub folders: HashMap<String, Vec<FolderRow>>,
    pub nav: Nav,
    pub threads: Vec<ThreadRow>,
    pub limit: usize,
    pub list_loading: bool,
    pub search: String,
    search_seq: u64,
    pub hits: Option<Vec<SearchHit>>,
    pub search_everywhere: bool,
    pub reader: Option<Reader>,
    pub undo: Option<UndoSend>,
    pub rules: Vec<mailcore::rules::Rule>,
    pub rules_problem: Option<String>,
    pub rules_note: Option<String>,
    pub rule_form: crate::userdata_flow::RuleForm,
    pub templates: Vec<mailcore::templates::Template>,
    pub template_form: crate::userdata_flow::TemplateForm,
    /// Show only conversations with unread mail.
    pub unread_only: bool,
    /// How far the conversation list is scrolled and how tall it is, in logical pixels; only
    /// used to decide which rows to build.
    pub list_scroll: f32,
    pub list_height: f32,
    /// Message text arrived in the background: refresh the list's previews at the next tick.
    list_stale: bool,
    /// True while the list shows tick boxes for choosing several conversations.
    pub selecting: bool,
    /// Thread keys ticked in the list.
    pub selected: BTreeSet<String>,
    pub bodies: HashMap<i64, BodyState>,
    pub wizard: Wizard,
    pub dialog: Option<Dialog>,
    pub toast: Option<Toast>,
    pub busy_older: bool,
    pub now: i64,
    pub tz: i32,
    pub compose: Option<ComposeState>,
    pub outbox: Vec<OutboxItem>,
    pub(crate) outbox_gen: u64,
    pub(crate) outbox_primed: bool,
    pub drafts: Vec<DraftSummary>,
    /// A website sign-in that is waiting for the browser.
    pub signing_in: Option<SigningIn>,
    /// The SQLite version, asked for when Settings opens.
    pub sqlite: Option<String>,
    /// What is remembered between runs (see `prefs`), as last written.
    pub(crate) prefs: Prefs,
    /// The folder that was open last time, until the folder list is known.
    restore_view: Option<(String, i64)>,
    /// The window's size as last reported.
    window_size: Option<Size>,
    window_dirty: bool,
    prefs_dirty: bool,
    /// The unread count drawn into the window icon right now.
    badge_shown: usize,
    /// What the operating system says: dark or not.
    system_dark: bool,
    /// Set once the window has been checked against the screen size.
    screen_checked: bool,
    ticks: u32,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    WindowResized(Size),
    WindowState(bool),
    ScreenKnown(Option<Size>),
    SyncEnded(String),
    // navigation
    Go(Nav),
    FoldersLoaded(Vec<(String, Vec<FolderRow>)>),
    ThreadsLoaded(Nav, usize, Vec<ThreadRow>),
    ShowMore,
    LoadOlder(i64),
    OlderLoaded(Result<usize, String>),
    RefreshNow,
    // reading
    OpenThread(String, ListScope),
    ThreadLoaded(String, Vec<MessageRow>),
    BodyLoaded(i64, Result<Box<RenderedMessage>, String>),
    /// The background download of the newest messages finished.
    Prefetched(usize),
    ToggleExpand(i64),
    /// Fold or unfold the quoted earlier messages inside a message.
    ToggleQuotes(i64),
    /// Show or hide the notes about a message (what was kept out, signature checks).
    ToggleDetails(i64),
    /// Back from the message to the list (narrow windows).
    CloseReader,
    Act(Action),
    ActionDone(Result<String, String>),
    Data(crate::userdata_flow::DataMsg),
    SetUnreadOnly(bool),
    /// The list was scrolled: offset and height of what is visible.
    ListScrolled(f32, f32),
    // choosing several conversations
    SelectMode(bool),
    ToggleSelect(String),
    SelectAll,
    SelectNone,
    Bulk(Action),
    BulkDone(Result<String, String>),
    AskPermanent(Vec<(i64, u32)>),
    ConfirmPermanentDelete,
    CopyText(String),
    LinkClicked(Link),
    OpenLink(String),
    // search
    SearchChanged(String),
    SearchRun(u64),
    SearchDone(u64, Vec<SearchHit>),
    ClearSearch,
    SearchEverywhere(bool),
    // accounts
    ShowAddAccount,
    ShowAccounts,
    BackToMailbox,
    /// The operating system's light or dark setting (at start and when it changes).
    SystemTheme(iced::theme::Mode),
    SetTheme(crate::theme::ThemeChoice),
    SetAccent(crate::theme::Accent),
    SetScale(u16),
    SetCompact(bool),
    SetPreviews(bool),
    SetSideBySide(bool),
    SetListWidth(prefs::ListWidth),
    SetSendDelay(u8),
    /// Leave Settings, Accounts, Outbox or Write and return to the mail list as it was.
    ShowMailbox,
    WizName(String),
    WizEmail(String),
    WizPassword(String),
    WizToggleShow,
    WizContinue,
    WizFound(Result<Box<ResolvedConfig>, String>),
    WizManual,
    WizBack,
    WizManualImapHost(String),
    WizManualImapPort(String),
    WizManualImapSecurity(SecurityChoice),
    WizManualSmtpHost(String),
    WizManualSmtpPort(String),
    WizManualSmtpSecurity(SecurityChoice),
    WizManualUser(String),
    WizSubmit,
    WizChecked(Box<Account>, Option<Box<Diagnosis>>),
    CheckAccount(String),
    AccountChecked(String, Option<Box<Diagnosis>>),
    AskRemoveAccount(String),
    RemoveAccount(String),
    AskChangePassword(String),
    PasswordInput(String),
    SavePassword,
    RetryAccount(String),
    WizWebSignIn,
    WizWebSignedIn(Box<Account>, SignIn),
    WizCancelSignIn,
    ResignIn(String),
    ResignedIn(String, SignIn),
    // settings
    ShowSettings,
    SqliteKnown(Option<String>),
    // attachments of received mail
    SaveAttachment(i64, usize),
    SaveConfirmed(i64, usize),
    AttachmentSaved(Result<String, String>),
    // writing and sending
    Compose(ComposeMsg),
    Outbox(OutboxMsg),
    // dialogs
    CloseDialog,
    Escape,
}

pub async fn blocking<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    tokio::task::spawn_blocking(f).await.ok()
}

pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(0))
}

fn scope_of(nav: &Nav) -> ListScope {
    match nav {
        Nav::AllInboxes => ListScope::AllInboxes,
        Nav::Folder { folder_id, .. } => ListScope::Folder(*folder_id),
    }
}

/// Below this width (in interface units) three columns leave the reading pane too narrow, so
/// only one of "list" and "message" is shown at a time.
pub const NARROW_BELOW: f32 = 1040.0;

impl App {
    /// Seconds a sent message waits so it can be taken back (0 = off).
    pub fn send_delay(&self) -> u8 {
        self.prefs.send_delay
    }

    /// True when the list and the message are shown one at a time: because the person chose
    /// that, or because the window is too narrow for both side by side (a wider list needs a
    /// wider window).
    pub fn narrow(&self) -> bool {
        if !self.prefs.look.side_by_side {
            return true;
        }
        let w = self
            .window_size
            .unwrap_or_else(|| self.prefs.window_size())
            .width;
        let wider_list = self.prefs.look.list_width.px() - prefs::ListWidth::Normal.px();
        w / (f32::from(self.prefs.look.scale) / 100.0) < NARROW_BELOW + wider_list
    }

    /// Starts the program: opens the stores, begins syncing every account.
    pub fn boot() -> (Self, Task<Message>) {
        let opened = Paths::platform_default()
            .ok_or_else(|| {
                "This computer has no place for program files (no home folder found).".to_owned()
            })
            .and_then(|p| Services::open(p).map_err(|e| e.to_string()));
        match opened {
            Ok(svc) => Self::new(Arc::new(svc)),
            Err(e) => (Self::fatal(e), Task::none()),
        }
    }

    fn blank(svc: Option<Arc<Services>>) -> Self {
        Self {
            svc,
            fatal: None,
            screen: Screen::Mailbox,
            accounts: Vec::new(),
            folders: HashMap::new(),
            nav: Nav::AllInboxes,
            threads: Vec::new(),
            limit: PAGE,
            list_loading: false,
            search: String::new(),
            search_seq: 0,
            hits: None,
            search_everywhere: false,
            reader: None,
            selecting: false,
            selected: BTreeSet::new(),
            undo: None,
            rules: Vec::new(),
            rules_problem: None,
            rules_note: None,
            rule_form: crate::userdata_flow::RuleForm::default(),
            templates: Vec::new(),
            template_form: crate::userdata_flow::TemplateForm::default(),
            unread_only: false,
            list_scroll: 0.0,
            list_height: 0.0,
            list_stale: false,
            bodies: HashMap::new(),
            wizard: Wizard::default(),
            dialog: None,
            toast: None,
            busy_older: false,
            now: unix_now(),
            tz: util::local_offset_secs(),
            compose: None,
            outbox: Vec::new(),
            outbox_gen: 0,
            outbox_primed: false,
            drafts: Vec::new(),
            signing_in: None,
            sqlite: None,
            prefs: Prefs::default(),
            restore_view: None,
            window_size: None,
            window_dirty: false,
            prefs_dirty: false,
            badge_shown: 0,
            system_dark: false,
            screen_checked: false,
            ticks: 0,
        }
    }

    pub fn fatal(text: String) -> Self {
        let mut a = Self::blank(None);
        a.fatal = Some(text);
        a
    }

    /// The colours for the current choice of theme and accent.
    pub fn colors(&self) -> crate::theme::Colors {
        use crate::theme::ThemeChoice;
        let dark = match self.prefs.look.theme {
            ThemeChoice::Light => false,
            ThemeChoice::Dark => true,
            ThemeChoice::System => self.system_dark,
        };
        crate::theme::Colors::build(dark, self.prefs.look.accent)
    }

    fn change_look(&mut self, f: impl FnOnce(&mut prefs::Look)) {
        f(&mut self.prefs.look);
        self.prefs_dirty = true;
    }

    pub fn new(svc: Arc<Services>) -> (Self, Task<Message>) {
        let mut app = Self::blank(Some(Arc::clone(&svc)));
        app.prefs = Prefs::load(&svc.paths.data);
        app.restore_view = app.prefs.view.clone();
        app.search_everywhere = app.prefs.search_everywhere;
        if matches!(svc.recovery, Recovery::Recreated { .. }) {
            app.dialog = Some(Dialog::Info {
                title: "Your mail was reloaded".into(),
                text: "The saved copy of your mail on this computer was damaged, so it was set aside and a fresh one is being downloaded from your mail provider. Nothing on the server was changed.".into(),
            });
        }
        let mut tasks = Vec::new();
        for account in svc.accounts() {
            tasks.push(app.start_account(account));
        }
        if app.accounts.is_empty() {
            app.screen = Screen::AddAccount { first: true };
        }
        tasks.push(app.reload_all());
        tasks.push(iced::system::theme().map(Message::SystemTheme));
        svc.start_sender();
        app.sync_outbox_view();
        tasks.push(app.list_drafts());
        (app, Task::batch(tasks))
    }

    pub fn start_account_public(&mut self, account: Account) -> Task<Message> {
        self.start_account(account)
    }

    fn start_account(&mut self, account: Account) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let id = account.id.as_str().to_owned();
        let (handle, fut) = svc.start_sync(account.clone());
        self.accounts.push(AccountState {
            account,
            handle,
            status: SyncStatus::Starting,
            seen: (0, 0),
            progress_seen: 0,
        });
        Task::perform(fut, move |()| Message::SyncEnded(id.clone()))
    }

    // ----- loading -------------------------------------------------------------

    fn load_folders(&self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let ids: Vec<String> = self
            .accounts
            .iter()
            .map(|a| a.account.id.as_str().to_owned())
            .collect();
        Task::perform(
            blocking(move || {
                ids.into_iter()
                    .map(|id| (id.clone(), svc.store.folders(&id).unwrap_or_default()))
                    .collect::<Vec<_>>()
            }),
            |r| Message::FoldersLoaded(r.unwrap_or_default()),
        )
    }

    /// Scrolls the conversation list back to the top. Used whenever what the list shows is
    /// replaced (another folder, a search, a filter), so the rows that are built always match
    /// where the scroll area really is.
    pub(crate) fn list_to_top(&mut self) -> Task<Message> {
        self.list_scroll = 0.0;
        iced::widget::operation::snap_to(
            iced::widget::Id::new(LIST_SCROLL),
            iced::widget::scrollable::RelativeOffset::START,
        )
    }

    fn load_threads(&mut self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let nav = self.nav.clone();
        let limit = self.limit;
        let unread_only = self.unread_only;
        self.list_loading = true;
        Task::perform(
            blocking(move || {
                svc.store
                    .list_threads(&scope_of(&nav), unread_only, None, limit)
                    .unwrap_or_default()
                    .into_iter()
                    .collect::<Vec<_>>()
                    .pipe(|t| (nav, limit, t))
            }),
            |r| match r {
                Some((nav, limit, t)) => Message::ThreadsLoaded(nav, limit, t),
                None => Message::ThreadsLoaded(Nav::AllInboxes, 0, Vec::new()),
            },
        )
    }

    fn load_thread(&self, key: String) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let k = key.clone();
        Task::perform(
            blocking(move || svc.store.thread_messages(&key).unwrap_or_default()),
            move |r| Message::ThreadLoaded(k.clone(), r.unwrap_or_default()),
        )
    }

    pub fn reload_all(&mut self) -> Task<Message> {
        let mut tasks = vec![self.load_folders(), self.load_threads()];
        if let Some(r) = &self.reader {
            tasks.push(self.load_thread(r.thread_key.clone()));
        }
        if !self.search.trim().is_empty() {
            tasks.push(self.run_search());
        }
        Task::batch(tasks)
    }

    fn load_body(&mut self, id: i64) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        if self.bodies.contains_key(&id) {
            return Task::none();
        }
        self.bodies.insert(id, BodyState::Loading);
        Task::perform(svc.read_message(id, false), move |r| {
            Message::BodyLoaded(id, r.map(Box::new))
        })
    }

    fn run_search(&mut self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let text = self.search.clone();
        let scope = if self.search_everywhere {
            SearchScope::Everywhere
        } else {
            match &self.nav {
                Nav::AllInboxes => SearchScope::Everywhere,
                Nav::Folder { account, .. } => SearchScope::Account(account.clone()),
            }
        };
        let seq = self.search_seq;
        Task::perform(
            blocking(move || {
                svc.store
                    .search(&SearchQuery::parse(&text), &scope, SearchSort::Newest, 100)
                    .unwrap_or_default()
            }),
            move |r| Message::SearchDone(seq, r.unwrap_or_default()),
        )
    }

    // ----- helpers for the views --------------------------------------------------

    pub fn folder(&self, account: &str, folder_id: i64) -> Option<&FolderRow> {
        self.folders
            .get(account)?
            .iter()
            .find(|f| f.id == folder_id)
    }

    pub fn current_folder(&self) -> Option<&FolderRow> {
        match &self.nav {
            Nav::Folder { account, folder_id } => self.folder(account, *folder_id),
            Nav::AllInboxes => None,
        }
    }

    pub fn account_state(&self, id: &str) -> Option<&AccountState> {
        self.accounts.iter().find(|a| a.account.id.as_str() == id)
    }

    pub fn unread_inboxes(&self) -> u32 {
        self.folders
            .values()
            .flatten()
            .filter(|f| f.role == FolderRole::Inbox)
            .map(|f| f.unread)
            .sum()
    }

    /// The unread count currently shown on the window icon.
    pub fn badge_shown(&self) -> usize {
        self.badge_shown
    }

    pub fn title(&self) -> String {
        let n = self.unread_inboxes();
        if n > 0 {
            format!("({n}) EMAYL")
        } else {
            "EMAYL".to_owned()
        }
    }

    pub(crate) fn toast(&mut self, text: impl Into<String>, bad: bool) {
        self.toast = Some(Toast {
            text: text.into(),
            bad,
            until: self.now + TOAST_SECS,
        });
    }

    pub(crate) fn wake_all(&self) {
        for a in &self.accounts {
            a.handle.wake();
        }
    }

    // ----- update ---------------------------------------------------------------

    pub fn update(&mut self, message: Message) -> Task<Message> {
        let task = self.apply(message);
        // Leaving the add-account steps while the browser is open ends that sign-in.
        if self.signing_in.as_ref().is_some_and(|s| !s.existing)
            && !matches!(self.screen, Screen::AddAccount { .. })
        {
            self.stop_sign_in();
        }
        task
    }

    fn apply(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Tick => return self.on_tick(),
            Message::WindowResized(size) => {
                self.window_size = Some(size);
                self.window_dirty = true;
            }
            Message::WindowState(maximized) => {
                self.prefs.maximized = maximized;
                if !maximized && let Some(size) = self.window_size {
                    self.prefs.size = Some((size.width.round() as u32, size.height.round() as u32));
                }
                self.prefs_dirty = true;
            }
            Message::ScreenKnown(screen) => return self.on_screen_known(screen),
            Message::SaveAttachment(m, i) => return self.save_attachment(m, i, false),
            Message::SaveConfirmed(m, i) => {
                self.dialog = None;
                return self.save_attachment(m, i, true);
            }
            Message::AttachmentSaved(r) => match r {
                Ok(t) => self.toast(t, false),
                Err(e) => self.toast(e, true),
            },
            Message::Compose(m) => return self.update_compose(m),
            Message::Outbox(m) => return self.update_outbox(m),
            Message::SyncEnded(_) => {}
            Message::Go(nav) => {
                self.nav = nav;
                self.limit = PAGE;
                self.screen = Screen::Mailbox;
                self.reader = None;
                self.threads.clear();
                self.selecting = false;
                self.selected.clear();
                self.search.clear();
                self.hits = None;
                return Task::batch([self.list_to_top(), self.load_threads()]);
            }
            Message::FoldersLoaded(all) => {
                self.folders = all.into_iter().collect();
                // Reopen the folder that was open last time (once, if it still exists).
                if let Some((account, folder_id)) = self.restore_view.take()
                    && matches!(self.nav, Nav::AllInboxes)
                    && self
                        .folders
                        .get(&account)
                        .is_some_and(|f| f.iter().any(|f| f.id == folder_id))
                {
                    self.nav = Nav::Folder { account, folder_id };
                    return self.load_threads();
                }
                // First start: the inbox shows up after the first sync; prefer it to "all inboxes"
                // when there is a single account.
                if matches!(self.nav, Nav::AllInboxes) && self.accounts.len() == 1 {
                    let id = self.accounts[0].account.id.as_str().to_owned();
                    if let Some(f) = self
                        .folders
                        .get(&id)
                        .and_then(|f| f.iter().find(|f| f.role == FolderRole::Inbox))
                    {
                        self.nav = Nav::Folder {
                            account: id,
                            folder_id: f.id,
                        };
                        return self.load_threads();
                    }
                }
            }
            Message::ThreadsLoaded(nav, limit, threads) => {
                if nav == self.nav && limit == self.limit {
                    self.threads = threads;
                    self.list_loading = false;
                    let present: std::collections::HashSet<&str> =
                        self.threads.iter().map(|t| t.thread_key.as_str()).collect();
                    self.selected.retain(|k| present.contains(k.as_str()));
                }
            }
            Message::ShowMore => {
                self.limit += PAGE;
                return self.load_threads();
            }
            Message::LoadOlder(folder_id) => {
                let Some(svc) = self.svc.clone() else {
                    return Task::none();
                };
                self.busy_older = true;
                return Task::perform(svc.load_older(folder_id), Message::OlderLoaded);
            }
            Message::OlderLoaded(r) => {
                self.busy_older = false;
                match r {
                    Ok(0) => self.toast("There is no older mail on the server.", false),
                    Ok(n) => {
                        self.toast(format!("Loaded {n} older messages."), false);
                        self.limit += PAGE * 5;
                        return self.reload_all();
                    }
                    Err(e) => self.toast(e, true),
                }
            }
            Message::RefreshNow => {
                self.wake_all();
                self.toast("Checking for new mail…", false);
            }
            Message::OpenThread(key, scope) => {
                self.reader = Some(Reader {
                    thread_key: key.clone(),
                    messages: Vec::new(),
                    expanded: BTreeSet::new(),
                    quotes_open: BTreeSet::new(),
                    details_open: BTreeSet::new(),
                    scope,
                });
                return self.load_thread(key);
            }
            Message::ThreadLoaded(key, msgs) => return self.on_thread_loaded(key, msgs),
            Message::BodyLoaded(id, r) => {
                self.bodies.insert(
                    id,
                    match r {
                        Ok(m) => BodyState::Ready(m),
                        Err(e) => BodyState::Failed(e),
                    },
                );
                // Bodies are cached now: the list shows their previews. The list is
                // refreshed at the next tick, not once per message.
                self.list_stale = true;
            }
            Message::Prefetched(n) => {
                if n > 0 {
                    self.list_stale = true;
                }
            }
            Message::ToggleExpand(id) => {
                if let Some(r) = &mut self.reader
                    && !r.expanded.remove(&id)
                {
                    r.expanded.insert(id);
                    return self.load_body(id);
                }
            }
            Message::ToggleQuotes(id) => {
                if let Some(r) = &mut self.reader
                    && !r.quotes_open.remove(&id)
                {
                    r.quotes_open.insert(id);
                }
            }
            Message::ToggleDetails(id) => {
                if let Some(r) = &mut self.reader
                    && !r.details_open.remove(&id)
                {
                    r.details_open.insert(id);
                }
            }
            Message::Data(m) => return self.on_data(m),
            Message::SetUnreadOnly(on) => {
                self.unread_only = on;
                self.selected.clear();
                return Task::batch([self.list_to_top(), self.load_threads()]);
            }
            Message::ListScrolled(offset, height) => {
                self.list_scroll = offset;
                self.list_height = height;
            }
            Message::CloseReader => self.reader = None,
            Message::Act(action) => return self.on_action(action),
            Message::SelectMode(on) => {
                self.selecting = on;
                self.selected.clear();
                if on {
                    self.reader = None;
                }
            }
            Message::ToggleSelect(key) => {
                if !self.selected.remove(&key) {
                    self.selected.insert(key);
                }
            }
            Message::SelectAll => {
                self.selected = self.threads.iter().map(|t| t.thread_key.clone()).collect();
            }
            Message::SelectNone => self.selected.clear(),
            Message::Bulk(action) => return self.on_bulk(action),
            Message::BulkDone(r) => {
                if r.is_ok() {
                    self.selected.clear();
                    self.selecting = false;
                }
                return self.update(Message::ActionDone(r));
            }
            Message::ActionDone(r) => {
                match r {
                    Ok(text) => self.toast(text, false),
                    Err(e) => self.toast(e, true),
                }
                self.wake_all();
                return self.reload_all();
            }
            Message::AskPermanent(t) => self.dialog = Some(Dialog::PermanentDelete(t)),
            Message::ConfirmPermanentDelete => {
                let Some(Dialog::PermanentDelete(targets)) = self.dialog.take() else {
                    return Task::none();
                };
                let Some(svc) = self.svc.clone() else {
                    return Task::none();
                };
                self.reader = None;
                self.selected.clear();
                self.selecting = false;
                return Task::perform(
                    blocking(move || {
                        for (folder_id, uid) in targets {
                            let m = svc.store.message_by_uid(folder_id, uid).ok().flatten();
                            if let Some(m) = m {
                                syncer::delete_permanently(&svc.store, &m)
                                    .map_err(|e| e.to_string())?;
                            }
                        }
                        Ok("Deleted for good.".to_owned())
                    }),
                    |r: Option<Result<String, String>>| {
                        Message::ActionDone(
                            r.unwrap_or_else(|| Err("The action did not finish.".into())),
                        )
                    },
                );
            }
            Message::CopyText(t) => {
                self.toast("Copied.", false);
                return iced::clipboard::write(t);
            }
            Message::LinkClicked(link) => self.dialog = Some(Dialog::Link(link)),
            Message::OpenLink(url) => {
                self.dialog = None;
                return Task::perform(blocking(move || util::open_in_browser(&url)), |r| match r {
                    Some(Ok(())) => Message::CloseDialog,
                    Some(Err(e)) => Message::ActionDone(Err(e)),
                    None => Message::CloseDialog,
                });
            }
            Message::SearchChanged(text) => {
                self.selecting = false;
                self.selected.clear();
                self.search = text;
                self.search_seq += 1;
                if self.search.trim().is_empty() {
                    self.hits = None;
                    return self.list_to_top();
                }
                let seq = self.search_seq;
                // The timer is created when the task runs, not here, so this also works
                // outside a Tokio runtime context.
                return Task::perform(
                    async { tokio::time::sleep(SEARCH_DELAY).await },
                    move |()| Message::SearchRun(seq),
                );
            }
            Message::SearchRun(seq) => {
                if seq == self.search_seq && !self.search.trim().is_empty() {
                    return self.run_search();
                }
            }
            Message::SearchDone(seq, hits) => {
                if seq == self.search_seq && !self.search.trim().is_empty() {
                    self.hits = Some(hits);
                    return self.list_to_top();
                }
            }
            Message::ClearSearch => {
                self.search.clear();
                self.search_seq += 1;
                self.hits = None;
                return self.list_to_top();
            }
            Message::SearchEverywhere(on) => {
                self.search_everywhere = on;
                self.search_seq += 1;
                if !self.search.trim().is_empty() {
                    return self.run_search();
                }
            }
            Message::ShowAddAccount => {
                self.stop_sign_in();
                self.wizard = Wizard::default();
                self.screen = Screen::AddAccount {
                    first: self.accounts.is_empty(),
                };
            }
            Message::ShowAccounts => self.screen = Screen::Accounts,
            Message::ShowMailbox => self.screen = Screen::Mailbox,
            Message::SystemTheme(mode) => self.system_dark = mode == iced::theme::Mode::Dark,
            Message::SetTheme(t) => self.change_look(|l| l.theme = t),
            Message::SetAccent(x) => self.change_look(|l| l.accent = x),
            Message::SetScale(s) => {
                if prefs::SCALES.contains(&s) {
                    self.change_look(|l| l.scale = s);
                }
            }
            Message::SetCompact(on) => self.change_look(|l| l.compact = on),
            Message::SetPreviews(on) => self.change_look(|l| l.previews = on),
            Message::SetSideBySide(on) => self.change_look(|l| l.side_by_side = on),
            Message::SetListWidth(w) => self.change_look(|l| l.list_width = w),
            Message::SetSendDelay(s) => {
                if crate::prefs::SEND_DELAYS.contains(&s) {
                    self.prefs.send_delay = s;
                    self.prefs_dirty = true;
                }
            }
            Message::BackToMailbox => {
                self.stop_sign_in();
                self.wizard = Wizard::default();
                self.screen = Screen::Mailbox;
            }
            Message::WizName(s) => self.wizard.name = s,
            Message::WizEmail(s) => {
                self.wizard.email = s;
                if matches!(self.wizard.outcome, Outcome::Problem(_)) {
                    self.wizard.outcome = Outcome::Idle;
                }
            }
            Message::WizPassword(s) => self.wizard.password = s,
            Message::WizToggleShow => self.wizard.show_password = !self.wizard.show_password,
            Message::WizContinue => return self.wiz_continue(),
            Message::WizFound(r) => self.wiz_found(r),
            Message::WizManual => {
                if self.wizard.is_busy() {
                    return Task::none();
                }
                self.wizard.step = Step::Manual;
                self.wizard.outcome = Outcome::Idle;
                if let Some(f) = &self.wizard.found {
                    self.wizard.manual = crate::wizard::ManualFields::from_settings(&f.settings);
                }
            }
            Message::WizBack => {
                self.wizard.step = Step::Email;
                self.wizard.outcome = Outcome::Idle;
                self.wizard.clear_password();
            }
            Message::WizManualImapHost(s) => self.wizard.manual.imap_host = s,
            Message::WizManualImapPort(s) => self.wizard.manual.imap_port = s,
            Message::WizManualImapSecurity(s) => self.wizard.manual.imap_security = s,
            Message::WizManualSmtpHost(s) => self.wizard.manual.smtp_host = s,
            Message::WizManualSmtpPort(s) => self.wizard.manual.smtp_port = s,
            Message::WizManualSmtpSecurity(s) => self.wizard.manual.smtp_security = s,
            Message::WizManualUser(s) => self.wizard.manual.username = s,
            Message::WizSubmit => return self.wiz_submit(),
            Message::WizChecked(account, diagnosis) => {
                return self.wiz_checked(*account, diagnosis);
            }
            Message::CheckAccount(id) => {
                let (Some(svc), Some(account)) = (
                    self.svc.clone(),
                    self.svc.as_ref().and_then(|s| s.account(&id)),
                ) else {
                    return Task::none();
                };
                self.toast("Checking the connection…", false);
                return Task::perform(svc.check_account(account), move |d| {
                    Message::AccountChecked(id.clone(), d.map(Box::new))
                });
            }
            Message::AccountChecked(id, d) => {
                let email = self
                    .account_state(&id)
                    .map(|a| a.account.email.to_string())
                    .unwrap_or_default();
                self.toast = None;
                self.dialog = Some(match d {
                    Some(d) => Dialog::Report {
                        title: format!("Connection check for {email}"),
                        ok: d.all_ok(),
                        text: d.render(),
                    },
                    None => Dialog::Info {
                        title: "Check stopped".into(),
                        text: "The check could not run.".into(),
                    },
                });
            }
            Message::AskRemoveAccount(id) => self.dialog = Some(Dialog::RemoveAccount(id)),
            Message::RemoveAccount(id) => return self.remove_account(&id),
            Message::AskChangePassword(id) => {
                self.dialog = Some(Dialog::ChangePassword {
                    account: id,
                    input: String::new(),
                })
            }
            Message::PasswordInput(s) => {
                if let Some(Dialog::ChangePassword { input, .. }) = &mut self.dialog {
                    *input = s;
                }
            }
            Message::SavePassword => {
                if let Some(Dialog::ChangePassword { account, mut input }) = self.dialog.take() {
                    let secret = SecretString::from(std::mem::take(&mut input));
                    zeroize::Zeroize::zeroize(&mut input);
                    if let (Some(svc), Ok(id)) = (&self.svc, AccountId::parse(&account)) {
                        match svc.set_password(&id, &secret) {
                            Ok(()) => {
                                if let Some(a) = self.account_state(&account) {
                                    a.handle.wake();
                                }
                                self.toast("Password saved. Trying again…", false);
                            }
                            Err(e) => self.toast(e.to_string(), true),
                        }
                    }
                }
            }
            Message::WizWebSignIn => return self.wiz_web_sign_in(),
            Message::WizWebSignedIn(account, result) => {
                return self.wiz_web_signed_in(*account, result);
            }
            Message::WizCancelSignIn => self.stop_sign_in(),
            Message::ResignIn(id) => return self.resign_in(&id),
            Message::ResignedIn(id, result) => self.resigned_in(&id, result),
            Message::ShowSettings => {
                self.screen = Screen::Settings;
                if let Some(svc) = self.svc.clone() {
                    return Task::perform(blocking(move || svc.store.sqlite_version().ok()), |v| {
                        Message::SqliteKnown(v.flatten())
                    });
                }
            }
            Message::SqliteKnown(v) => self.sqlite = v,
            Message::RetryAccount(id) => {
                if let Some(a) = self.account_state(&id) {
                    a.handle.wake();
                }
            }
            Message::CloseDialog => self.dialog = None,
            Message::Escape => {
                if self.dialog.is_some() {
                    self.dialog = None;
                } else if self.selecting {
                    self.selecting = false;
                    self.selected.clear();
                } else if self.narrow() && self.reader.is_some() && self.screen == Screen::Mailbox {
                    self.reader = None;
                } else if !self.search.is_empty() {
                    self.search.clear();
                    self.search_seq += 1;
                    self.hits = None;
                    return self.list_to_top();
                } else if matches!(
                    self.screen,
                    Screen::Accounts | Screen::Settings | Screen::AddAccount { first: false }
                ) {
                    self.screen = Screen::Mailbox;
                } else if matches!(self.screen, Screen::Filters | Screen::Templates) {
                    self.screen = Screen::Settings;
                }
            }
        }
        Task::none()
    }

    fn on_tick(&mut self) -> Task<Message> {
        crate::startup::still_alive();
        self.now = unix_now();
        if self.toast.as_ref().is_some_and(|t| t.until <= self.now) {
            self.toast = None;
        }
        if self
            .undo
            .as_ref()
            .is_some_and(|u| !u.working && u.until < self.now)
        {
            self.undo = None;
        }
        let mut changed = false;
        let mut prefetch = Vec::new();
        for a in &mut self.accounts {
            let status = a.handle.status();
            let sig = match &status {
                SyncStatus::Online {
                    last_sync,
                    new_messages_total,
                    ..
                } => (*last_sync, *new_messages_total),
                _ => a.seen,
            };
            if sig != a.seen {
                a.seen = sig;
                changed = true;
                if matches!(status, SyncStatus::Online { .. }) {
                    prefetch.push(a.account.id.as_str().to_owned());
                }
            }
            // Mail that arrives during the first download is shown as it comes,
            // not only when the whole pass ends.
            let stored = match &status {
                SyncStatus::Syncing(p) => p.stored_total,
                _ => 0,
            };
            if stored != a.progress_seen {
                a.progress_seen = stored;
                changed |= stored > 0;
            }
            a.status = status;
        }
        self.sync_outbox_view();
        let save = self.autosave();
        let remember = self.remember_tick();
        let mut tasks = vec![save, remember];
        if let Some(svc) = &self.svc {
            for id in prefetch {
                tasks.push(Task::perform(svc.prefetch(id), Message::Prefetched));
            }
        }
        if changed {
            tasks.push(self.reload_all());
            self.list_stale = false;
        } else if self.list_stale {
            self.list_stale = false;
            tasks.push(self.load_threads());
        }
        Task::batch(tasks)
    }

    /// Notices changes worth remembering and writes them, at most every couple of
    /// seconds. Also checks, early on, that the window fits the screen.
    fn remember_tick(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        self.ticks = self.ticks.saturating_add(1);
        if !self.screen_checked && self.ticks <= 5 {
            tasks.push(
                iced::window::oldest()
                    .and_then(iced::window::monitor_size)
                    .map(Message::ScreenKnown),
            );
        }
        let view = match &self.nav {
            Nav::AllInboxes => None,
            Nav::Folder { account, folder_id } => Some((account.clone(), *folder_id)),
        };
        if view != self.prefs.view || self.search_everywhere != self.prefs.search_everywhere {
            self.prefs.view = view;
            self.prefs.search_everywhere = self.search_everywhere;
            self.prefs_dirty = true;
        }
        if self.window_dirty {
            self.window_dirty = false;
            tasks.push(
                iced::window::oldest()
                    .and_then(iced::window::is_maximized)
                    .map(Message::WindowState),
            );
        }
        let unread = self.unread_inboxes() as usize;
        if unread != self.badge_shown {
            self.badge_shown = unread;
            let rgba =
                crate::badge::with_badge(crate::WINDOW_ICON, crate::WINDOW_ICON_SIDE, unread);
            // A badge that cannot be drawn must never get in the way of the mail.
            if let Ok(icon) = iced::window::icon::from_rgba(
                rgba,
                crate::WINDOW_ICON_SIDE,
                crate::WINDOW_ICON_SIDE,
            ) {
                tasks.push(
                    iced::window::oldest()
                        .and_then(move |id| iced::window::set_icon(id, icon.clone())),
                );
            }
            crate::badge::launcher(unread);
        }
        if self.prefs_dirty {
            self.prefs_dirty = false;
            if let Some(svc) = &self.svc {
                self.prefs.save(&svc.paths.data);
            }
        }
        Task::batch(tasks)
    }

    fn on_screen_known(&mut self, screen: Option<Size>) -> Task<Message> {
        let Some(screen) = screen else {
            return Task::none();
        };
        self.screen_checked = true;
        if self.prefs.maximized {
            return Task::none();
        }
        let current = self.window_size.unwrap_or_else(|| self.prefs.window_size());
        match prefs::fit_to_screen(current, screen) {
            Some(fitted) => {
                self.window_size = Some(fitted);
                iced::window::oldest().and_then(move |id| iced::window::resize(id, fitted))
            }
            None => Task::none(),
        }
    }

    fn on_thread_loaded(&mut self, key: String, msgs: Vec<MessageRow>) -> Task<Message> {
        let Some(r) = &mut self.reader else {
            return Task::none();
        };
        if r.thread_key != key {
            return Task::none();
        }
        let first_load = r.messages.is_empty();
        r.messages = msgs;
        let mut tasks = Vec::new();
        if first_load {
            // Open the newest message and any that are unread (at most five).
            let mut open: Vec<i64> = r
                .messages
                .iter()
                .filter(|m| m.is_unread())
                .map(|m| m.id)
                .collect();
            if let Some(last) = r.messages.last()
                && !open.contains(&last.id)
            {
                open.push(last.id);
            }
            let skip = open.len().saturating_sub(5);
            r.expanded = open.into_iter().skip(skip).collect();
            let ids: Vec<i64> = r.expanded.iter().copied().collect();
            // Reading marks it read.
            let unread: Vec<MessageRow> = r
                .messages
                .iter()
                .filter(|m| m.is_unread() && in_scope(&r.scope, m, &self.folders))
                .cloned()
                .collect();
            for id in ids {
                tasks.push(self.load_body(id));
            }
            if !unread.is_empty()
                && let Some(svc) = self.svc.clone()
            {
                tasks.push(Task::perform(
                    blocking(move || {
                        for m in &unread {
                            let _ = syncer::set_read(&svc.store, m, true);
                        }
                        Ok("".to_owned())
                    }),
                    |r: Option<Result<String, String>>| {
                        Message::ActionDone(r.unwrap_or(Ok(String::new())))
                    },
                ));
            }
        }
        Task::batch(tasks)
    }

    /// Runs one action over every ticked conversation (only the messages that are in the
    /// open folder, or in an inbox when "All inboxes" is open).
    fn on_bulk(&mut self, action: Action) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        if self.selected.is_empty() {
            return Task::none();
        }
        let keys: Vec<String> = self.selected.iter().cloned().collect();
        let scope = scope_of(&self.nav);
        let folders = self.folders.clone();
        Task::perform(
            blocking(move || bulk_apply(&svc, &scope, &folders, &keys, action)),
            |r| match r {
                Some(Ok(ActionOutcome::Done(text))) => Message::BulkDone(Ok(text)),
                Some(Ok(ActionOutcome::NeedsPermanentConfirm(t))) => Message::AskPermanent(t),
                Some(Err(e)) => Message::BulkDone(Err(e)),
                None => Message::BulkDone(Err("The action did not finish.".into())),
            },
        )
    }

    pub(crate) fn on_action(&mut self, action: Action) -> Task<Message> {
        let (Some(svc), Some(reader)) = (self.svc.clone(), self.reader.clone()) else {
            return Task::none();
        };
        let targets: Vec<MessageRow> = reader
            .messages
            .iter()
            .filter(|m| in_scope(&reader.scope, m, &self.folders))
            .cloned()
            .collect();
        if targets.is_empty() {
            return Task::none();
        }
        let closes = matches!(
            action,
            Action::Archive | Action::Delete | Action::Spam | Action::NotSpam
        );
        if closes {
            self.reader = None;
        }
        Task::perform(
            blocking(move || run_action(&svc, action, &targets)),
            |r| match r {
                Some(Ok(ActionOutcome::Done(text))) => Message::ActionDone(Ok(text)),
                Some(Ok(ActionOutcome::NeedsPermanentConfirm(t))) => Message::AskPermanent(t),
                Some(Err(e)) => Message::ActionDone(Err(e)),
                None => Message::ActionDone(Err("The action did not finish.".into())),
            },
        )
    }
}

/// Runs `action` over the messages of the given conversations that are in `scope`.
pub fn bulk_apply(
    svc: &Services,
    scope: &ListScope,
    folders: &HashMap<String, Vec<FolderRow>>,
    keys: &[String],
    action: Action,
) -> Result<ActionOutcome, String> {
    let mut targets = Vec::new();
    for k in keys {
        let msgs = svc.store.thread_messages(k).map_err(|e| e.to_string())?;
        targets.extend(msgs.into_iter().filter(|m| in_scope(scope, m, folders)));
    }
    if targets.is_empty() {
        return Ok(ActionOutcome::Done("Nothing to change.".into()));
    }
    match run_action(svc, action, &targets)? {
        ActionOutcome::Done(text) => Ok(ActionOutcome::Done(format!(
            "{} ({} {})",
            text.trim_end_matches('.'),
            keys.len(),
            if keys.len() == 1 {
                "conversation"
            } else {
                "conversations"
            }
        ))),
        other => Ok(other),
    }
}

fn in_scope(scope: &ListScope, m: &MessageRow, folders: &HashMap<String, Vec<FolderRow>>) -> bool {
    match scope {
        ListScope::Folder(id) => m.folder_id == *id,
        ListScope::AllInboxes => folders
            .get(&m.account)
            .and_then(|f| f.iter().find(|f| f.id == m.folder_id))
            .is_some_and(|f| f.role == FolderRole::Inbox),
    }
}

#[derive(Debug)]
pub enum ActionOutcome {
    Done(String),
    NeedsPermanentConfirm(Vec<(i64, u32)>),
}

fn run_action(
    svc: &Services,
    action: Action,
    targets: &[MessageRow],
) -> Result<ActionOutcome, String> {
    let store = &svc.store;
    let e = |e: mailcore::Error| match e {
        mailcore::Error::NotFound(what) => {
            format!("This account has no {what}, so that could not be done.")
        }
        other => other.to_string(),
    };
    match action {
        Action::ToggleRead => {
            let make_read = targets.iter().any(MessageRow::is_unread);
            for m in targets {
                syncer::set_read(store, m, make_read).map_err(e)?;
            }
            Ok(ActionOutcome::Done(if make_read {
                "Marked as read.".into()
            } else {
                "Marked as unread.".into()
            }))
        }
        Action::NotSpam => {
            for m in targets {
                syncer::mark_not_spam(store, m).map_err(e)?;
            }
            Ok(ActionOutcome::Done("Moved back to the Inbox.".into()))
        }
        Action::MarkRead | Action::MarkUnread => {
            let read = matches!(action, Action::MarkRead);
            for m in targets {
                syncer::set_read(store, m, read).map_err(e)?;
            }
            Ok(ActionOutcome::Done(if read {
                "Marked as read.".into()
            } else {
                "Marked as unread.".into()
            }))
        }
        Action::ToggleStar => {
            let on = !targets.iter().all(MessageRow::is_flagged);
            for m in targets {
                syncer::set_starred(store, m, on).map_err(e)?;
            }
            Ok(ActionOutcome::Done(if on {
                "Starred.".into()
            } else {
                "Star removed.".into()
            }))
        }
        Action::Archive => {
            for m in targets {
                syncer::archive(store, m).map_err(e)?;
            }
            Ok(ActionOutcome::Done("Archived.".into()))
        }
        Action::Spam => {
            for m in targets {
                syncer::mark_spam(store, m).map_err(e)?;
            }
            Ok(ActionOutcome::Done("Moved to Junk.".into()))
        }
        Action::Delete => {
            let mut permanent = false;
            for m in targets {
                match syncer::delete(store, m) {
                    Ok(Deleted::MovedToTrash) => {}
                    Ok(Deleted::Permanently) => permanent = true,
                    Err(mailcore::Error::NotFound(_)) => {
                        return Ok(ActionOutcome::NeedsPermanentConfirm(
                            targets.iter().map(|m| (m.folder_id, m.uid)).collect(),
                        ));
                    }
                    Err(err) => return Err(err.to_string()),
                }
            }
            Ok(ActionOutcome::Done(if permanent {
                "Deleted for good.".into()
            } else {
                "Moved to Trash.".into()
            }))
        }
    }
}

trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}
impl<T> Pipe for T {}
