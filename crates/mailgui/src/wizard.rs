//! The "add an account" steps, as plain data plus the checks behind them.

use std::fmt;

use mailcore::account::{
    Account, AuthMethod, OAuthProvider, Security, ServerConfig, ServerSettings,
};
use mailcore::diagnostics::{Diagnosis, StepStatus};
use mailcore::email::EmailAddress;
use mailcore::provider::{AuthOption, ResolvedConfig};
use zeroize::Zeroize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Name and address.
    Email,
    /// Settings were found; ask for the password.
    Password,
    /// Settings were not found (or the person wants to type them).
    Manual,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityChoice {
    Tls,
    StartTls,
}

impl SecurityChoice {
    pub const ALL: [Self; 2] = [Self::Tls, Self::StartTls];

    pub fn security(self) -> Security {
        match self {
            Self::Tls => Security::Tls,
            Self::StartTls => Security::StartTls,
        }
    }
}

impl From<Security> for SecurityChoice {
    fn from(s: Security) -> Self {
        match s {
            Security::Tls => Self::Tls,
            Security::StartTls => Self::StartTls,
        }
    }
}

impl fmt::Display for SecurityChoice {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tls => "SSL/TLS (recommended)",
            Self::StartTls => "STARTTLS",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManualFields {
    pub imap_host: String,
    pub imap_port: String,
    pub imap_security: SecurityChoice,
    pub smtp_host: String,
    pub smtp_port: String,
    pub smtp_security: SecurityChoice,
    /// Empty means "the full email address".
    pub username: String,
}

impl Default for ManualFields {
    fn default() -> Self {
        Self {
            imap_host: String::new(),
            imap_port: "993".into(),
            imap_security: SecurityChoice::Tls,
            smtp_host: String::new(),
            smtp_port: "465".into(),
            smtp_security: SecurityChoice::Tls,
            username: String::new(),
        }
    }
}

impl ManualFields {
    pub fn from_settings(s: &ServerSettings) -> Self {
        Self {
            imap_host: s.imap.host.clone(),
            imap_port: s.imap.port.to_string(),
            imap_security: s.imap.security.into(),
            smtp_host: s.smtp.host.clone(),
            smtp_port: s.smtp.port.to_string(),
            smtp_security: s.smtp.security.into(),
            username: s.imap_username.clone(),
        }
    }

    /// Turns what was typed into settings, or says in plain words what is wrong.
    pub fn to_settings(&self, email: &EmailAddress) -> Result<ServerSettings, String> {
        let port = |text: &str, what: &str| -> Result<u16, String> {
            text.trim()
                .parse::<u16>()
                .ok()
                .filter(|p| *p != 0)
                .ok_or_else(|| {
                    format!(
                        "The {what} port must be a number between 1 and 65535 (for example 993)."
                    )
                })
        };
        let imap_port = port(&self.imap_port, "incoming")?;
        let smtp_port = port(&self.smtp_port, "outgoing")?;
        let imap = ServerConfig::new(&self.imap_host, imap_port, self.imap_security.security())
            .map_err(|_| "The incoming server name does not look right. It should look like imap.example.com.".to_owned())?;
        let smtp = ServerConfig::new(&self.smtp_host, smtp_port, self.smtp_security.security())
            .map_err(|_| "The outgoing server name does not look right. It should look like smtp.example.com.".to_owned())?;
        let user = if self.username.trim().is_empty() {
            email.to_string()
        } else {
            self.username.trim().to_owned()
        };
        Ok(ServerSettings {
            imap,
            imap_username: user.clone(),
            smtp,
            smtp_username: user,
        })
    }
}

#[derive(Debug, Clone)]
pub enum Outcome {
    /// Nothing is running.
    Idle,
    /// Looking up settings or checking the connection.
    Busy(&'static str),
    /// Something to show the person.
    Problem(String),
    /// The connection check failed; the report explains why.
    CheckFailed(Box<Diagnosis>),
}

#[derive(Debug, Clone)]
pub struct Wizard {
    pub step: Step,
    pub name: String,
    pub email: String,
    pub password: String,
    pub show_password: bool,
    pub found: Option<ResolvedConfig>,
    pub manual: ManualFields,
    pub outcome: Outcome,
    /// The full address has already been tried as the user name (see
    /// [`retry_with_full_address`](Wizard::retry_with_full_address)).
    pub username_retried: bool,
}

impl Default for Wizard {
    fn default() -> Self {
        Self {
            step: Step::Email,
            name: String::new(),
            email: String::new(),
            password: String::new(),
            show_password: false,
            found: None,
            manual: ManualFields::default(),
            outcome: Outcome::Idle,
            username_retried: false,
        }
    }
}

/// Characters that can ride along when a password is pasted but are never part of
/// one: spaces, tabs, line breaks and the invisible "zero width" marks.
fn is_noise(c: char) -> bool {
    c.is_whitespace() || matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}')
}

/// True for the way Google shows an app password: four groups of four letters or
/// digits ("abcd efgh ijkl mnop"). Nobody types a normal password like that.
fn looks_like_grouped_app_password(text: &str) -> bool {
    let groups: Vec<&str> = text.split(' ').collect();
    groups.len() == 4
        && groups
            .iter()
            .all(|g| g.len() == 4 && g.chars().all(|c| c.is_ascii_alphanumeric()))
}

/// What to actually send to the server for what was typed or pasted.
///
/// Providers show app passwords in groups ("abcd efgh ijkl mnop") and copying them
/// often adds a space or a line break at the end, but the server wants the
/// characters only. So the ends are always cleaned, and for providers that issue app
/// passwords every space is removed. A password for an ordinary server keeps its
/// inner spaces, because a pass phrase may really contain them.
pub(crate) fn clean_password(typed: &str, issues_app_passwords: bool) -> String {
    let trimmed = typed.trim_matches(is_noise);
    if issues_app_passwords || looks_like_grouped_app_password(trimmed) {
        trimmed.chars().filter(|c| !is_noise(*c)).collect()
    } else {
        trimmed
            .chars()
            .filter(|c| !matches!(c, '\u{200B}'..='\u{200D}' | '\u{2060}' | '\u{FEFF}'))
            .collect()
    }
}

impl Drop for Wizard {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

impl Wizard {
    pub fn is_busy(&self) -> bool {
        matches!(self.outcome, Outcome::Busy(_))
    }

    /// The address as typed, if it is a valid one.
    pub fn parsed_email(&self) -> Result<EmailAddress, String> {
        EmailAddress::parse(self.email.trim()).map_err(|_| {
            "That does not look like an email address. It should look like name@example.com."
                .to_owned()
        })
    }

    /// The name shown for the account: what was typed, else the part before the @.
    pub fn display_name(&self) -> String {
        let n = self.name.trim();
        if !n.is_empty() {
            return n.to_owned();
        }
        self.email.split('@').next().unwrap_or("").trim().to_owned()
    }

    /// The password as it will be saved and sent (see [`clean_password`]).
    pub fn password_to_use(&self) -> String {
        let app_passwords = self.found.as_ref().is_some_and(|f| {
            f.auth_options
                .iter()
                .any(|o| matches!(o, AuthOption::AppPassword))
        });
        clean_password(&self.password, app_passwords)
    }

    /// Some providers (iCloud) want only the part before the @ as the user name,
    /// some servers want the whole address. When the sign-in was rejected with the
    /// short form, try the full address once, without asking. Returns the account to
    /// check again, or `None` when there is nothing different to try.
    pub fn retry_with_full_address(
        &mut self,
        account: &Account,
        diagnosis: &Diagnosis,
    ) -> Option<Account> {
        let full = account.email.to_string();
        if self.username_retried
            || !matches!(account.auth, AuthMethod::Password)
            || account.imap_username.is_empty()
            || account.imap_username == full
        {
            return None;
        }
        let incoming = diagnosis
            .servers
            .iter()
            .find(|s| s.role == "Incoming mail (IMAP)")?;
        let rejected = !incoming.ok
            && !incoming.transient
            && incoming
                .steps
                .iter()
                .any(|s| s.label == "Signing in" && s.status == StepStatus::Failed);
        if !rejected {
            return None;
        }
        self.username_retried = true;
        let mut again = account.clone();
        again.imap_username = full;
        Some(again)
    }

    pub fn clear_password(&mut self) {
        self.password.zeroize();
        self.password.clear();
    }

    /// What the provider allows, in words, for the password step.
    pub fn password_hint(&self) -> Option<String> {
        let found = self.found.as_ref()?;
        let app_only = found
            .auth_options
            .iter()
            .any(|o| matches!(o, AuthOption::AppPassword))
            && !found
                .auth_options
                .iter()
                .any(|o| matches!(o, AuthOption::Password));
        if app_only {
            Some("This provider needs an app password, not your usual password. Create one in your account's security settings. You can paste it exactly as shown; spaces are removed for you.".to_owned())
        } else {
            None
        }
    }

    /// True when the provider only accepts a sign-in through its website.
    pub fn needs_website_sign_in(&self) -> bool {
        self.found
            .as_ref()
            .is_some_and(|f| !f.allows(&AuthMethod::Password))
    }

    /// The website sign-in this provider offers, if any.
    pub fn web_provider(&self) -> Option<OAuthProvider> {
        self.found
            .as_ref()?
            .auth_options
            .iter()
            .find_map(|o| match o {
                AuthOption::OAuth2(p) => Some(*p),
                _ => None,
            })
    }

    /// The account for a sign-in through the provider's website.
    pub fn build_website_account(&self, provider: OAuthProvider) -> Result<Account, String> {
        let email = self.parsed_email()?;
        let found = self
            .found
            .clone()
            .ok_or_else(|| "The provider's settings are missing. Please start again.".to_owned())?;
        found
            .into_account(&self.display_name(), email, AuthMethod::OAuth2 { provider })
            .map_err(|e| e.to_string())
    }

    /// Builds the account from the current step's data.
    pub fn build_account(&self) -> Result<Account, String> {
        let email = self.parsed_email()?;
        let name = self.display_name();
        match (self.step, &self.found) {
            (Step::Password, Some(found)) => found
                .clone()
                .into_account(&name, email, AuthMethod::Password)
                .map_err(|e| e.to_string()),
            _ => {
                let settings = self.manual.to_settings(&email)?;
                Account::new(&name, email, settings, AuthMethod::Password)
                    .map_err(|e| e.to_string())
            }
        }
    }
}

/// The provider's name as people know it.
pub fn provider_label(p: OAuthProvider) -> &'static str {
    match p {
        OAuthProvider::Google => "Google",
        OAuthProvider::Microsoft => "Microsoft",
    }
}

/// A sentence for the top of a failed check: the first failing server's advice.
pub fn failure_headline(d: &Diagnosis) -> String {
    d.servers
        .iter()
        .find(|s| !s.ok)
        .and_then(|s| s.advice.clone())
        .unwrap_or_else(|| "The connection check did not succeed.".to_owned())
}

/// Step-by-step help for a sign-in the provider refused, when the provider is known.
#[derive(Debug)]
pub struct SignInHelp {
    pub steps: &'static str,
    /// Button label and the web address it opens.
    pub buttons: &'static [(&'static str, &'static str)],
}

const GMAIL_HELP: SignInHelp = SignInHelp {
    steps: "Google does not accept your normal Google password here. It needs an app password:\n\
            1. Turn on 2-Step Verification for this Google account (first button). Without it, Google says the app password setting is not available for your account.\n\
            2. Open Google's app password page (second button), type any name such as EMAYL and press Create.\n\
            3. Copy the 16 letters Google shows and paste them into the password box. Spaces do not matter.\n\
            If the app password page is still not available, the account is a work or school account, uses only security keys for 2-Step Verification, or has Advanced Protection turned on. Google then allows no app passwords at all: ask the account's administrator, or add a different Gmail address. Changing your Google password cancels older app passwords, so make a new one then.",
    buttons: &[
        (
            "1. Turn on 2-Step Verification",
            "https://myaccount.google.com/signinoptions/two-step-verification",
        ),
        (
            "2. Create an app password",
            "https://myaccount.google.com/apppasswords",
        ),
    ],
};

/// Looks at a failed check and, when the password was refused by a provider that has its own
/// rules for it, returns the steps that fix it.
pub fn sign_in_help(d: &Diagnosis) -> Option<SignInHelp> {
    let refused = d.servers.iter().any(|s| {
        !s.ok
            && s.steps.iter().any(|step| {
                step.label == "Signing in"
                    && step.status == mailcore::diagnostics::StepStatus::Failed
            })
    });
    let google = d.servers.iter().any(|s| {
        let host = s.host.to_ascii_lowercase();
        host == "gmail.com"
            || host.ends_with(".gmail.com")
            || host == "googlemail.com"
            || host.ends_with(".googlemail.com")
    });
    (refused && google).then_some(GMAIL_HELP)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn email(s: &str) -> EmailAddress {
        EmailAddress::parse(s).unwrap()
    }

    fn report(host: &str, signing_in_failed: bool) -> Diagnosis {
        use mailcore::diagnostics::{ServerReport, Step, StepStatus};
        Diagnosis {
            account: "a@example.com".into(),
            servers: vec![ServerReport {
                role: "Incoming mail (IMAP)",
                host: host.into(),
                port: 993,
                security: mailcore::account::Security::Tls,
                steps: vec![Step {
                    label: "Signing in",
                    status: if signing_in_failed {
                        StepStatus::Failed
                    } else {
                        StepStatus::Ok
                    },
                    detail: None,
                }],
                ok: !signing_in_failed,
                advice: None,
                transient: false,
                elapsed: std::time::Duration::from_secs(1),
                notes: Vec::new(),
            }],
            warnings: Vec::new(),
        }
    }

    #[test]
    fn a_refused_gmail_sign_in_gets_the_app_password_steps() {
        let help = sign_in_help(&report("imap.gmail.com", true)).unwrap();
        assert!(help.steps.contains("2-Step Verification"));
        assert_eq!(help.buttons.len(), 2);
        assert_eq!(
            help.buttons[1].1,
            "https://myaccount.google.com/apppasswords"
        );
        for (_, url) in help.buttons {
            assert!(crate::util::checked_web_url(url).is_ok(), "{url}");
        }
        assert!(sign_in_help(&report("smtp.googlemail.com", true)).is_some());
    }

    #[test]
    fn other_providers_and_other_failures_get_no_gmail_steps() {
        assert!(sign_in_help(&report("imap.example.com", true)).is_none());
        assert!(sign_in_help(&report("imap.notgmail.com", true)).is_none());
        assert!(sign_in_help(&report("imap.gmail.com", false)).is_none());
    }

    #[test]
    fn manual_fields_turn_into_settings() {
        let mut m = ManualFields {
            imap_host: "IMAP.Example.com".into(),
            smtp_host: "smtp.example.com".into(),
            ..ManualFields::default()
        };
        let s = m.to_settings(&email("a@example.com")).unwrap();
        assert_eq!(s.imap.host, "imap.example.com");
        assert_eq!((s.imap.port, s.smtp.port), (993, 465));
        assert_eq!(s.imap_username, "a@example.com");
        m.username = " alice ".into();
        assert_eq!(
            m.to_settings(&email("a@example.com"))
                .unwrap()
                .smtp_username,
            "alice"
        );
    }

    #[test]
    fn bad_manual_input_gets_plain_messages() {
        let ok = ManualFields {
            imap_host: "imap.example.com".into(),
            smtp_host: "smtp.example.com".into(),
            ..ManualFields::default()
        };
        for (field, expect) in [("port", "port"), ("host", "server name")] {
            let mut m = ok.clone();
            match field {
                "port" => m.imap_port = "99999".into(),
                _ => m.smtp_host = "not a host!".into(),
            }
            let e = m.to_settings(&email("a@example.com")).unwrap_err();
            assert!(e.contains(expect), "{e}");
        }
        let mut m = ok.clone();
        m.smtp_port = "0".into();
        assert!(m.to_settings(&email("a@example.com")).is_err());
        let mut m = ok;
        m.imap_host = "127.0.0.1\n".into();
        assert!(
            m.to_settings(&email("a@example.com")).is_err() || m.imap_host.trim() == "127.0.0.1"
        );
    }

    #[test]
    fn names_and_addresses() {
        let mut w = Wizard::default();
        w.email = "  jo@example.com ".into();
        assert!(w.parsed_email().is_ok());
        assert_eq!(w.display_name(), "jo");
        w.name = " Jo Smith ".into();
        assert_eq!(w.display_name(), "Jo Smith");
        w.email = "nonsense".into();
        assert!(w.parsed_email().unwrap_err().contains("email address"));
    }

    #[test]
    fn password_is_wiped() {
        let mut w = Wizard::default();
        w.password = "hunter2".into();
        w.clear_password();
        assert!(w.password.is_empty());
    }
}

#[cfg(test)]
mod password_tests {
    use super::clean_password;

    #[test]
    fn a_pasted_app_password_loses_its_spaces_and_line_breaks() {
        assert_eq!(
            clean_password("abcd efgh ijkl mnop", true),
            "abcdefghijklmnop"
        );
        assert_eq!(
            clean_password("  abcd efgh ijkl mnop\r\n", true),
            "abcdefghijklmnop"
        );
        assert_eq!(
            clean_password("abcd\u{a0}efgh\u{200b}ijkl\tmnop", true),
            "abcdefghijklmnop"
        );
    }

    #[test]
    fn google_style_groups_are_cleaned_even_when_the_provider_is_not_known() {
        assert_eq!(
            clean_password("abcd efgh ijkl mnop", false),
            "abcdefghijklmnop"
        );
        assert_eq!(
            clean_password("ab12 EF34 gh56 ij78", false),
            "ab12EF34gh56ij78"
        );
    }

    #[test]
    fn an_ordinary_password_keeps_its_inner_spaces_but_not_stray_ends() {
        assert_eq!(
            clean_password("correct horse battery", false),
            "correct horse battery"
        );
        assert_eq!(
            clean_password(" my pass phrase \n", false),
            "my pass phrase"
        );
        assert_eq!(clean_password("pa\u{200b}ss", false), "pass");
        // Not the Google shape (wrong group sizes): left alone.
        assert_eq!(
            clean_password("abc defg hijk lmno", false),
            "abc defg hijk lmno"
        );
    }

    #[test]
    fn nothing_typed_stays_nothing() {
        assert_eq!(clean_password("", true), "");
        assert_eq!(clean_password("  \n\t ", false), "");
    }

    #[test]
    fn special_characters_are_never_touched() {
        let tricky = r#"p@ss"w\ord'%&;<>é漢"#;
        assert_eq!(clean_password(tricky, true), tricky);
        assert_eq!(clean_password(tricky, false), tricky);
    }
}

#[cfg(test)]
mod retry_tests {
    use std::time::Duration;

    use mailcore::account::{Account, AuthMethod, Security, ServerConfig, ServerSettings};
    use mailcore::diagnostics::{Diagnosis, ServerReport, Step, StepStatus};
    use mailcore::email::EmailAddress;

    use super::Wizard;

    fn account(username: &str) -> Account {
        let email =
            EmailAddress::parse("johnappleseed@icloud.com").unwrap_or_else(|_| unreachable!());
        let server =
            |h: &str, p| ServerConfig::new(h, p, Security::Tls).unwrap_or_else(|_| unreachable!());
        let settings = ServerSettings {
            imap: server("imap.example.com", 993),
            imap_username: username.to_owned(),
            smtp: server("smtp.example.com", 465),
            smtp_username: "johnappleseed@icloud.com".to_owned(),
        };
        Account::new("John", email, settings, AuthMethod::Password)
            .unwrap_or_else(|_| unreachable!())
    }

    fn report(login: StepStatus, transient: bool) -> Diagnosis {
        Diagnosis {
            account: "x".into(),
            warnings: Vec::new(),
            servers: vec![ServerReport {
                role: "Incoming mail (IMAP)",
                host: "imap.example.com".into(),
                port: 993,
                security: Security::Tls,
                steps: vec![Step {
                    label: "Signing in",
                    status: login,
                    detail: None,
                }],
                ok: login == StepStatus::Ok,
                advice: None,
                transient,
                elapsed: Duration::from_millis(5),
                notes: Vec::new(),
            }],
        }
    }

    #[test]
    fn a_rejected_short_user_name_is_retried_once_with_the_full_address() {
        let mut w = Wizard::default();
        let a = account("johnappleseed");
        let again = w.retry_with_full_address(&a, &report(StepStatus::Failed, false));
        assert_eq!(
            again.map(|x| x.imap_username),
            Some("johnappleseed@icloud.com".to_owned())
        );
        // Only once: a second rejection is shown to the person.
        assert!(
            w.retry_with_full_address(&a, &report(StepStatus::Failed, false))
                .is_none()
        );
    }

    #[test]
    fn nothing_is_retried_when_it_would_not_help() {
        let mut w = Wizard::default();
        // Already the full address.
        assert!(
            w.retry_with_full_address(
                &account("johnappleseed@icloud.com"),
                &report(StepStatus::Failed, false)
            )
            .is_none()
        );
        // The network failed, not the password.
        assert!(
            w.retry_with_full_address(&account("johnappleseed"), &report(StepStatus::Failed, true))
                .is_none()
        );
        // Signing in worked.
        assert!(
            w.retry_with_full_address(&account("johnappleseed"), &report(StepStatus::Ok, false))
                .is_none()
        );
    }
}
