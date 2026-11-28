//! What the Settings screen shows and the report people can copy for support.
//!
//! The report contains the program version, system, folder paths, server names
//! and how each account is doing. It never contains passwords, tokens, message
//! text or email addresses.

use std::fmt::Write as _;

use mailcore::account::AuthMethod;

use crate::view::status_line;
use crate::wizard::provider_label;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountFacts {
    pub incoming: String,
    pub outgoing: String,
    pub sign_in: String,
    pub status: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Facts {
    pub version: &'static str,
    pub system: String,
    pub sqlite: String,
    pub data_folder: String,
    pub settings_folder: String,
    pub google_sign_in: bool,
    pub microsoft_sign_in: bool,
    pub waiting: usize,
    pub problems: usize,
    pub drafts: usize,
    pub accounts: Vec<AccountFacts>,
}

pub fn system() -> String {
    format!("{} {}", std::env::consts::OS, std::env::consts::ARCH)
}

pub fn sign_in_text(auth: AuthMethod) -> String {
    match auth {
        AuthMethod::Password => "password".to_owned(),
        AuthMethod::OAuth2 { provider } => {
            format!("website sign-in ({})", provider_label(provider))
        }
    }
}

impl crate::app::App {
    /// Collects what the screen and the report show.
    pub fn facts(&self, sqlite: Option<&str>) -> Facts {
        let (waiting, problems) = crate::compose_flow::outbox_counts(&self.outbox);
        let (paths, google, microsoft) = match &self.svc {
            Some(s) => (
                (
                    s.paths.data.display().to_string(),
                    s.paths.config.display().to_string(),
                ),
                s.oauth_available(mailcore::account::OAuthProvider::Google),
                s.oauth_available(mailcore::account::OAuthProvider::Microsoft),
            ),
            None => ((String::new(), String::new()), false, false),
        };
        Facts {
            version: VERSION,
            system: system(),
            sqlite: sqlite.unwrap_or("unknown").to_owned(),
            data_folder: paths.0,
            settings_folder: paths.1,
            google_sign_in: google,
            microsoft_sign_in: microsoft,
            waiting,
            problems,
            drafts: self.drafts.len(),
            accounts: self
                .accounts
                .iter()
                .map(|a| AccountFacts {
                    incoming: format!("{}:{}", a.account.imap.host, a.account.imap.port),
                    outgoing: format!("{}:{}", a.account.smtp.host, a.account.smtp.port),
                    sign_in: sign_in_text(a.account.auth),
                    status: status_text(&a.status),
                })
                .collect(),
        }
    }
}

fn status_text(s: &mailcore::syncer::SyncStatus) -> String {
    use mailcore::syncer::SyncStatus as S;
    let (line, _) = status_line(s);
    match s {
        S::Offline { advice, .. } | S::NeedsAttention { advice, .. } => {
            format!("{line} - {}", crate::util::clip(advice, 200))
        }
        _ => line,
    }
}

/// Plain text for support: easy to read, safe to paste into a message or a bug report.
pub fn report(f: &Facts) -> String {
    let yes_no = |b: bool| {
        if b {
            "available"
        } else {
            "not set up in this build"
        }
    };
    let mut out = String::new();
    let _ = writeln!(out, "EMAYL {}", f.version);
    let _ = writeln!(out, "System: {}", f.system);
    let _ = writeln!(out, "Drawing: {}", crate::startup::drawing());
    let _ = writeln!(out, "SQLite: {}", f.sqlite);
    let _ = writeln!(out, "Mail folder: {}", f.data_folder);
    let _ = writeln!(out, "Settings folder: {}", f.settings_folder);
    let _ = writeln!(out, "Google website sign-in: {}", yes_no(f.google_sign_in));
    let _ = writeln!(
        out,
        "Microsoft website sign-in: {}",
        yes_no(f.microsoft_sign_in)
    );
    let _ = writeln!(
        out,
        "Outbox: {} waiting, {} need attention; {} unfinished draft(s)",
        f.waiting, f.problems, f.drafts
    );
    let _ = writeln!(out, "Accounts: {}", f.accounts.len());
    for (i, a) in f.accounts.iter().enumerate() {
        let _ = writeln!(
            out,
            "  {}. incoming {} / outgoing {} / {} / {}",
            i + 1,
            a.incoming,
            a.outgoing,
            a.sign_in,
            a.status
        );
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use mailcore::account::OAuthProvider;

    fn facts() -> Facts {
        Facts {
            version: "1.2.3",
            system: "linux x86_64".into(),
            sqlite: "3.50.0".into(),
            data_folder: "/home/a/.local/share/EMAYL".into(),
            settings_folder: "/home/a/.config/EMAYL".into(),
            google_sign_in: false,
            microsoft_sign_in: true,
            waiting: 1,
            problems: 2,
            drafts: 3,
            accounts: vec![AccountFacts {
                incoming: "imap.example.com:993".into(),
                outgoing: "smtp.example.com:465".into(),
                sign_in: sign_in_text(AuthMethod::OAuth2 {
                    provider: OAuthProvider::Microsoft,
                }),
                status: "Up to date".into(),
            }],
        }
    }

    #[test]
    fn report_lists_what_support_needs() {
        let r = report(&facts());
        for needle in [
            "EMAYL 1.2.3",
            "SQLite: 3.50.0",
            "Google website sign-in: not set up in this build",
            "Microsoft website sign-in: available",
            "1 waiting, 2 need attention; 3 unfinished draft(s)",
            "imap.example.com:993",
            "website sign-in (Microsoft)",
        ] {
            assert!(r.contains(needle), "missing {needle:?} in\n{r}");
        }
    }

    #[test]
    fn report_has_no_addresses_or_secrets_by_construction() {
        // `Facts` has no field that could hold an address or a secret; this pins
        // the report against sneaking one in through the account lines.
        let r = report(&facts());
        assert!(!r.contains('@'), "{r}");
    }

    #[test]
    fn sign_in_text_names_the_method() {
        assert_eq!(sign_in_text(AuthMethod::Password), "password");
        assert_eq!(
            sign_in_text(AuthMethod::OAuth2 {
                provider: OAuthProvider::Google
            }),
            "website sign-in (Google)"
        );
    }
}
