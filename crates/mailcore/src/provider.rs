//! Built-in provider presets and the shared "resolved configuration" type.
//!
//! Server names and ports below were compared against each provider's own help
//! page on 2026-11-13 (see `CHECKED_ON`). Domain *aliases* that are not named on
//! those pages are listed in `unchecked_aliases` and flagged to the user.
//! Providers whose official page could not be read (AOL) have no preset on
//! purpose: they fall through to autoconfig or manual setup.

use crate::account::{Account, AuthMethod, OAuthProvider, Security, ServerConfig, ServerSettings};
use crate::email::EmailAddress;
use crate::{Error, Result};

pub const CHECKED_ON: &str = "2026-11-13";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthOption {
    OAuth2(OAuthProvider),
    /// The provider insists on a generated app-specific password.
    AppPassword,
    Password,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsernameStyle {
    FullAddress,
    LocalPart,
}

impl UsernameStyle {
    pub fn apply(self, email: &EmailAddress) -> String {
        match self {
            Self::FullAddress => email.to_string(),
            Self::LocalPart => email.local_part().to_owned(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    BuiltInPreset,
    IspAutoconfig,
    IspWellKnown,
    ThunderbirdIspDb,
}

impl ConfigSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::BuiltInPreset => "built-in preset",
            Self::IspAutoconfig => "provider autoconfig server",
            Self::IspWellKnown => "provider .well-known autoconfig",
            Self::ThunderbirdIspDb => "Thunderbird ISP database",
        }
    }
}

/// The outcome of looking up server settings for an address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedConfig {
    pub source: ConfigSource,
    pub provider: Option<String>,
    pub settings: ServerSettings,
    pub auth_options: Vec<AuthOption>,
    pub notes: Vec<String>,
}

impl ResolvedConfig {
    pub fn allows(&self, auth: &AuthMethod) -> bool {
        self.auth_options.iter().any(|opt| match (opt, auth) {
            (AuthOption::OAuth2(a), AuthMethod::OAuth2 { provider: b }) => a == b,
            (AuthOption::AppPassword | AuthOption::Password, AuthMethod::Password) => true,
            _ => false,
        })
    }

    pub fn into_account(
        self,
        display_name: &str,
        email: EmailAddress,
        auth: AuthMethod,
    ) -> Result<Account> {
        if !self.allows(&auth) {
            return Err(Error::InvalidField {
                field: "authentication method",
                reason: "not offered by this provider",
            });
        }
        Account::new(display_name, email, self.settings, auth)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ServerSpec {
    pub host: &'static str,
    pub port: u16,
    pub security: Security,
}

#[derive(Debug, Clone, Copy)]
pub struct ProviderPreset {
    pub id: &'static str,
    pub name: &'static str,
    pub domains: &'static [&'static str],
    /// Subset of `domains` that no checked provider page named explicitly.
    pub unchecked_aliases: &'static [&'static str],
    pub imap: ServerSpec,
    pub smtp: ServerSpec,
    pub imap_user: UsernameStyle,
    pub smtp_user: UsernameStyle,
    pub auth: &'static [AuthOption],
    pub notes: &'static str,
}

impl ProviderPreset {
    pub fn resolve(&self, email: &EmailAddress) -> ResolvedConfig {
        let to_cfg = |s: ServerSpec| ServerConfig {
            host: s.host.to_owned(),
            port: s.port,
            security: s.security,
        };
        let mut notes = vec![
            self.notes.to_owned(),
            format!(
                "Server names for {} were compared with the provider's help page on {CHECKED_ON}.",
                self.name
            ),
        ];
        if self.unchecked_aliases.contains(&email.domain()) {
            notes.push(format!(
                "The domain {} is a recalled alias of {} and was not individually checked.",
                email.domain(),
                self.name
            ));
        }
        ResolvedConfig {
            source: ConfigSource::BuiltInPreset,
            provider: Some(self.name.to_owned()),
            settings: ServerSettings {
                imap: to_cfg(self.imap),
                imap_username: self.imap_user.apply(email),
                smtp: to_cfg(self.smtp),
                smtp_username: self.smtp_user.apply(email),
            },
            auth_options: self.auth.to_vec(),
            notes,
        }
    }
}

pub const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        id: "gmail",
        name: "Gmail",
        domains: &["gmail.com", "googlemail.com"],
        unchecked_aliases: &["googlemail.com"],
        imap: ServerSpec {
            host: "imap.gmail.com",
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerSpec {
            host: "smtp.gmail.com",
            port: 465,
            security: Security::Tls,
        },
        imap_user: UsernameStyle::FullAddress,
        smtp_user: UsernameStyle::FullAddress,
        auth: &[
            AuthOption::OAuth2(OAuthProvider::Google),
            AuthOption::AppPassword,
        ],
        notes: "Google Workspace accounts must use OAuth since 1 May 2025 (Google Workspace admin help). \
                An app password only works if 2-Step Verification is on.",
    },
    ProviderPreset {
        id: "outlook",
        name: "Outlook.com",
        domains: &["outlook.com", "hotmail.com", "live.com", "msn.com"],
        unchecked_aliases: &["hotmail.com", "live.com", "msn.com"],
        imap: ServerSpec {
            host: "outlook.office365.com",
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerSpec {
            host: "smtp-mail.outlook.com",
            port: 587,
            security: Security::StartTls,
        },
        imap_user: UsernameStyle::FullAddress,
        smtp_user: UsernameStyle::FullAddress,
        auth: &[AuthOption::OAuth2(OAuthProvider::Microsoft)],
        notes: "Outlook.com requires OAuth2 (modern auth). IMAP access is off by default and must be \
                enabled in Outlook.com settings. Microsoft 365 work/school accounts are not covered by this preset.",
    },
    ProviderPreset {
        id: "yahoo",
        name: "Yahoo Mail",
        domains: &["yahoo.com", "ymail.com"],
        unchecked_aliases: &["ymail.com"],
        imap: ServerSpec {
            host: "imap.mail.yahoo.com",
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerSpec {
            host: "smtp.mail.yahoo.com",
            port: 465,
            security: Security::Tls,
        },
        imap_user: UsernameStyle::FullAddress,
        smtp_user: UsernameStyle::FullAddress,
        auth: &[AuthOption::AppPassword],
        notes: "Yahoo asks for an app password instead of your regular password.",
    },
    ProviderPreset {
        id: "icloud",
        name: "iCloud Mail",
        domains: &["icloud.com", "me.com", "mac.com"],
        unchecked_aliases: &["me.com", "mac.com"],
        imap: ServerSpec {
            host: "imap.mail.me.com",
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerSpec {
            host: "smtp.mail.me.com",
            port: 587,
            security: Security::StartTls,
        },
        imap_user: UsernameStyle::LocalPart,
        smtp_user: UsernameStyle::FullAddress,
        auth: &[AuthOption::AppPassword],
        notes: "Requires an app-specific password. Apple says the IMAP username is normally the part \
                before the @; if login fails, try the full address.",
    },
    ProviderPreset {
        id: "fastmail",
        name: "Fastmail",
        domains: &["fastmail.com", "fastmail.fm"],
        unchecked_aliases: &["fastmail.fm"],
        imap: ServerSpec {
            host: "imap.fastmail.com",
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerSpec {
            host: "smtp.fastmail.com",
            port: 465,
            security: Security::Tls,
        },
        imap_user: UsernameStyle::FullAddress,
        smtp_user: UsernameStyle::FullAddress,
        auth: &[AuthOption::AppPassword],
        notes: "Requires an app password; Fastmail's basic plans do not include IMAP/SMTP access.",
    },
    ProviderPreset {
        id: "zoho",
        name: "Zoho Mail (.com data centre)",
        domains: &["zohomail.com", "zoho.com"],
        unchecked_aliases: &["zoho.com"],
        imap: ServerSpec {
            host: "imap.zoho.com",
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerSpec {
            host: "smtp.zoho.com",
            port: 465,
            security: Security::Tls,
        },
        imap_user: UsernameStyle::FullAddress,
        smtp_user: UsernameStyle::FullAddress,
        auth: &[AuthOption::Password],
        notes: "Covers personal/free accounts on the .com data centre only. Paid organisation accounts use \
                imappro.zoho.com / smtppro.zoho.com and other regions use other hosts: use manual setup. \
                An app-specific password is required when two-factor authentication is on.",
    },
    ProviderPreset {
        id: "gmx",
        name: "GMX (gmx.com)",
        domains: &["gmx.com"],
        unchecked_aliases: &[],
        imap: ServerSpec {
            host: "imap.gmx.com",
            port: 993,
            security: Security::Tls,
        },
        smtp: ServerSpec {
            host: "mail.gmx.com",
            port: 587,
            security: Security::StartTls,
        },
        imap_user: UsernameStyle::FullAddress,
        smtp_user: UsernameStyle::FullAddress,
        auth: &[AuthOption::Password],
        notes: "IMAP must be enabled in GMX settings (POP3 & IMAP). \
                This preset covers gmx.com only, not other GMX domains.",
    },
];

/// Case-insensitive lookup by (ASCII) email domain.
pub fn find_preset(domain: &str) -> Option<&'static ProviderPreset> {
    PRESETS
        .iter()
        .find(|p| p.domains.iter().any(|d| d.eq_ignore_ascii_case(domain)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_preset_passes_validation() {
        for p in PRESETS {
            let email = EmailAddress::parse(&format!("someone@{}", p.domains[0])).unwrap();
            let r = p.resolve(&email);
            r.settings.imap.validate("imap").unwrap();
            r.settings.smtp.validate("smtp").unwrap();
            assert!(!r.auth_options.is_empty(), "{} has no auth option", p.id);
            for alias in p.unchecked_aliases {
                assert!(p.domains.contains(alias), "{}: alias not in domains", p.id);
            }
        }
    }

    #[test]
    fn domains_are_unique_and_lowercase() {
        let mut seen = std::collections::HashSet::new();
        for p in PRESETS {
            for d in p.domains {
                assert_eq!(*d, d.to_ascii_lowercase());
                assert!(seen.insert(*d), "duplicate domain {d}");
            }
        }
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(find_preset("GMAIL.com").unwrap().id, "gmail");
        assert!(find_preset("example.com").is_none());
    }

    #[test]
    fn icloud_uses_local_part_for_imap_only() {
        let email = EmailAddress::parse("johnappleseed@icloud.com").unwrap();
        let r = find_preset("icloud.com").unwrap().resolve(&email);
        assert_eq!(r.settings.imap_username, "johnappleseed");
        assert_eq!(r.settings.smtp_username, "johnappleseed@icloud.com");
    }

    #[test]
    fn alias_note_is_added_for_unchecked_domains() {
        let email = EmailAddress::parse("a@hotmail.com").unwrap();
        let r = find_preset("hotmail.com").unwrap().resolve(&email);
        assert!(
            r.notes
                .iter()
                .any(|n| n.contains("not individually checked"))
        );
        let email = EmailAddress::parse("a@outlook.com").unwrap();
        let r = find_preset("outlook.com").unwrap().resolve(&email);
        assert!(
            !r.notes
                .iter()
                .any(|n| n.contains("not individually checked"))
        );
    }

    #[test]
    fn outlook_is_oauth_only_and_rejects_password_auth() {
        let email = EmailAddress::parse("a@outlook.com").unwrap();
        let r = find_preset("outlook.com").unwrap().resolve(&email);
        assert!(!r.allows(&AuthMethod::Password));
        assert!(r.allows(&AuthMethod::OAuth2 {
            provider: OAuthProvider::Microsoft
        }));
        assert!(
            r.clone()
                .into_account("A", email.clone(), AuthMethod::Password)
                .is_err()
        );
        assert!(
            r.into_account(
                "A",
                email,
                AuthMethod::OAuth2 {
                    provider: OAuthProvider::Microsoft
                }
            )
            .is_ok()
        );
    }
}
