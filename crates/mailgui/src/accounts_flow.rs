//! The steps for adding and removing accounts.

use iced::{Subscription, Task, keyboard};
use mailcore::account::{Account, AccountId, AuthMethod};
use mailcore::secrets::purge_account;
use secrecy::SecretString;

use crate::app::{App, Dialog, Message, Nav, Screen};
use crate::services::SignIn;
use crate::wizard::{ManualFields, Outcome, Step, provider_label};

/// A sign-in through a provider's website that is waiting for the browser.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SigningIn {
    pub account: AccountId,
    /// True when an existing account signs in again (not the add-account steps).
    pub existing: bool,
}

fn friendly_secret_error(e: &mailcore::Error) -> String {
    format!(
        "Your computer's password store did not accept the password ({e}). On Linux, make sure a password manager such as GNOME Keyring or KWallet is installed and unlocked, then try again."
    )
}

impl App {
    pub(crate) fn wiz_continue(&mut self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        if self.wizard.is_busy() {
            return Task::none();
        }
        let email = match self.wizard.parsed_email() {
            Ok(e) => e,
            Err(msg) => {
                self.wizard.outcome = Outcome::Problem(msg);
                return Task::none();
            }
        };
        if self.accounts.iter().any(|a| {
            a.account
                .email
                .to_string()
                .eq_ignore_ascii_case(&email.to_string())
        }) {
            self.wizard.outcome = Outcome::Problem("This address has already been added.".into());
            return Task::none();
        }
        self.wizard.outcome = Outcome::Busy("Looking up the settings for your provider…");
        Task::perform(svc.discover(email), |r| Message::WizFound(r.map(Box::new)))
    }

    pub(crate) fn wiz_found(&mut self, r: Result<Box<mailcore::provider::ResolvedConfig>, String>) {
        match r {
            Ok(cfg) => {
                self.wizard.found = Some(*cfg);
                self.wizard.step = Step::Password;
                self.wizard.outcome = Outcome::Idle;
            }
            Err(_) => {
                let domain = self
                    .wizard
                    .email
                    .rsplit('@')
                    .next()
                    .unwrap_or("")
                    .trim()
                    .to_ascii_lowercase();
                self.wizard.found = None;
                self.wizard.manual = ManualFields {
                    imap_host: format!("imap.{domain}"),
                    smtp_host: format!("smtp.{domain}"),
                    ..ManualFields::default()
                };
                self.wizard.step = Step::Manual;
                self.wizard.outcome = Outcome::Problem(
                    "We could not find the settings for this address automatically. Your provider's help pages list the \"IMAP\" and \"SMTP\" server names; the guesses below are often right.".into(),
                );
            }
        }
    }

    pub(crate) fn wiz_submit(&mut self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        if self.wizard.is_busy() {
            return Task::none();
        }
        if self.wizard.step == Step::Password && self.wizard.needs_website_sign_in() {
            self.wizard.outcome = Outcome::Problem(
                "This provider only allows signing in through its own website. That kind of sign-in is not available in this version yet.".into(),
            );
            return Task::none();
        }
        if self.wizard.password_to_use().is_empty() {
            self.wizard.outcome =
                Outcome::Problem("Please type the password for this account.".into());
            return Task::none();
        }
        let account = match self.wizard.build_account() {
            Ok(a) => a,
            Err(msg) => {
                self.wizard.outcome = Outcome::Problem(msg);
                return Task::none();
            }
        };
        self.wizard.username_retried = false;
        let secret = SecretString::from(self.wizard.password_to_use());
        if let Err(e) = svc.set_password(&account.id, &secret) {
            self.wizard.outcome = Outcome::Problem(friendly_secret_error(&e));
            return Task::none();
        }
        self.wizard.outcome = Outcome::Busy("Checking your settings and password…");
        let shown = account.clone();
        Task::perform(svc.check_account(account), move |d| {
            Message::WizChecked(Box::new(shown.clone()), d.map(Box::new))
        })
    }

    pub(crate) fn wiz_checked(
        &mut self,
        account: mailcore::account::Account,
        d: Option<Box<mailcore::diagnostics::Diagnosis>>,
    ) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let forget = |svc: &crate::services::Services| {
            let _ = purge_account(svc.secrets.as_ref(), &account.id);
        };
        let Some(d) = d else {
            forget(&svc);
            self.wizard.outcome =
                Outcome::Problem("The check stopped before it finished. Please try again.".into());
            return Task::none();
        };
        if !d.all_ok() {
            if let Some(again) = self.wizard.retry_with_full_address(&account, &d) {
                self.wizard.outcome =
                    Outcome::Busy("Trying the full email address as the user name…");
                let shown = again.clone();
                return Task::perform(svc.check_account(again), move |d| {
                    Message::WizChecked(Box::new(shown.clone()), d.map(Box::new))
                });
            }
            forget(&svc);
            self.wizard.outcome = Outcome::CheckFailed(d);
            return Task::none();
        }
        let saved = match account.auth {
            AuthMethod::Password => {
                let secret = SecretString::from(self.wizard.password_to_use());
                svc.add_account(account.clone(), &secret)
            }
            AuthMethod::OAuth2 { .. } => svc.add_website_account(account.clone()),
        };
        match saved {
            Ok(()) => {
                self.wizard = crate::wizard::Wizard::default();
                self.screen = Screen::Mailbox;
                self.nav = Nav::AllInboxes;
                self.threads.clear();
                self.list_scroll = 0.0;
                self.toast("Account added. Your mail is being downloaded…", false);
                let start = self.start_account_public(account);
                Task::batch([start, self.reload_all()])
            }
            Err(e) => {
                forget(&svc);
                self.wizard.outcome = Outcome::Problem(e.to_string());
                Task::none()
            }
        }
    }

    /// "Sign in with Google/Microsoft" on the password step.
    pub(crate) fn wiz_web_sign_in(&mut self) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        if self.wizard.is_busy() || self.signing_in.is_some() {
            return Task::none();
        }
        let Some(provider) = self
            .wizard
            .web_provider()
            .filter(|p| svc.oauth_available(*p))
        else {
            self.wizard.outcome = Outcome::Problem(
                "Sign-in through this provider's website is not set up in this copy of the program."
                    .into(),
            );
            return Task::none();
        };
        let account = match self.wizard.build_website_account(provider) {
            Ok(a) => a,
            Err(msg) => {
                self.wizard.outcome = Outcome::Problem(msg);
                return Task::none();
            }
        };
        self.wizard.outcome =
            Outcome::Busy("Waiting for you to finish signing in, in your web browser…");
        self.signing_in = Some(SigningIn {
            account: account.id.clone(),
            existing: false,
        });
        let hint = account.email.to_string();
        let id = account.id.clone();
        Task::perform(svc.sign_in_through_website(id, provider, hint), move |r| {
            Message::WizWebSignedIn(Box::new(account.clone()), r)
        })
    }

    pub(crate) fn wiz_web_signed_in(&mut self, account: Account, result: SignIn) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        let current = self
            .signing_in
            .as_ref()
            .is_some_and(|s| !s.existing && s.account == account.id);
        if !current {
            // Cancelled or replaced while the browser was open: leave nothing behind.
            if result == SignIn::Done {
                let _ = purge_account(svc.secrets.as_ref(), &account.id);
            }
            return Task::none();
        }
        self.signing_in = None;
        match result {
            SignIn::Done => {
                self.wizard.outcome = Outcome::Busy("Checking your settings…");
                let shown = account.clone();
                Task::perform(svc.check_account(account), move |d| {
                    Message::WizChecked(Box::new(shown.clone()), d.map(Box::new))
                })
            }
            SignIn::Cancelled => {
                self.wizard.outcome = Outcome::Idle;
                Task::none()
            }
            SignIn::Failed(m) => {
                self.wizard.outcome = Outcome::Problem(m);
                Task::none()
            }
        }
    }

    /// Stops a sign-in that is waiting for the browser.
    pub(crate) fn stop_sign_in(&mut self) {
        if self.signing_in.take().is_some() {
            if let Some(svc) = &self.svc {
                svc.cancel_sign_in();
            }
            if matches!(self.wizard.outcome, Outcome::Busy(_)) {
                self.wizard.outcome = Outcome::Idle;
            }
        }
    }

    /// An account that uses a website sign-in signs in again (expired or revoked).
    pub(crate) fn resign_in(&mut self, id: &str) -> Task<Message> {
        let Some(svc) = self.svc.clone() else {
            return Task::none();
        };
        if self.signing_in.is_some() {
            self.toast("Another sign-in is already waiting for your browser.", true);
            return Task::none();
        }
        let Some(account) = svc.account(id) else {
            return Task::none();
        };
        let AuthMethod::OAuth2 { provider } = account.auth else {
            return Task::none();
        };
        if !svc.oauth_available(provider) {
            self.dialog = Some(Dialog::Info {
                title: "Sign-in is not available".into(),
                text: format!(
                    "This copy of the program was built without its {} sign-in registration, so it cannot sign in again. Install the official release, or ask the person who gave you this program.",
                    provider_label(provider)
                ),
            });
            return Task::none();
        }
        self.signing_in = Some(SigningIn {
            account: account.id.clone(),
            existing: true,
        });
        let hint = account.email.to_string();
        let id = id.to_owned();
        Task::perform(
            svc.sign_in_through_website(account.id.clone(), provider, hint),
            move |r| Message::ResignedIn(id.clone(), r),
        )
    }

    pub(crate) fn resigned_in(&mut self, id: &str, result: SignIn) {
        let current = self
            .signing_in
            .as_ref()
            .is_some_and(|s| s.existing && s.account.as_str() == id);
        if !current {
            return;
        }
        self.signing_in = None;
        match result {
            SignIn::Done => {
                if let Some(a) = self.account_state(id) {
                    a.handle.wake();
                }
                self.toast("Signed in again. Trying to reach your mail…", false);
            }
            SignIn::Cancelled => {}
            SignIn::Failed(text) => {
                self.dialog = Some(Dialog::Info {
                    title: "Sign-in did not finish".into(),
                    text,
                });
            }
        }
    }

    pub(crate) fn remove_account(&mut self, id: &str) -> Task<Message> {
        self.dialog = None;
        let (Some(svc), Ok(account_id)) = (self.svc.clone(), AccountId::parse(id)) else {
            return Task::none();
        };
        if let Some(pos) = self
            .accounts
            .iter()
            .position(|a| a.account.id.as_str() == id)
        {
            self.accounts[pos].handle.stop();
            self.accounts.remove(pos);
        }
        match svc.remove_account(&account_id) {
            Ok(()) => self.toast("Account removed from this computer.", false),
            Err(e) => self.toast(
                format!("The account was removed, but cleaning up failed: {e}"),
                true,
            ),
        }
        self.folders.remove(id);
        if matches!(&self.nav, Nav::Folder { account, .. } if account == id) {
            self.nav = Nav::AllInboxes;
        }
        self.reader = None;
        if self.accounts.is_empty() {
            self.screen = Screen::AddAccount { first: true };
            self.wizard = crate::wizard::Wizard::default();
        }
        Task::batch([self.list_to_top(), self.reload_all()])
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(std::time::Duration::from_secs(2)).map(|_| Message::Tick),
            iced::system::theme_changes().map(Message::SystemTheme),
            iced::window::resize_events().map(|(_, size)| Message::WindowResized(size)),
            iced::event::listen_with(|event, _status, _window| match event {
                iced::Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    ..
                }) => Some(Message::Escape),
                iced::Event::Window(iced::window::Event::FileDropped(path)) => Some(
                    Message::Compose(crate::compose_flow::ComposeMsg::Dropped(path)),
                ),
                _ => None,
            }),
        ])
    }
}
