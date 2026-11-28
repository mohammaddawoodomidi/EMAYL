//! Account model. Contains no secrets: passwords and OAuth tokens live only in
//! the OS keyring (see [`crate::secrets`]).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::email::{EmailAddress, normalize_hostname};
use crate::{Error, Result};

const MAX_TEXT_CHARS: usize = 256;

/// Opaque, canonical (lowercase hyphenated) UUID. Safe to use in keyring entry
/// names and file names.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AccountId(String);

impl AccountId {
    pub fn generate() -> Self {
        Self(Uuid::new_v4().to_string())
    }

    pub fn parse(s: &str) -> Result<Self> {
        Uuid::parse_str(s)
            .map(|u| Self(u.to_string()))
            .map_err(|_| Error::InvalidField {
                field: "account id",
                reason: "not a valid UUID",
            })
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Transport security. There is intentionally **no** plaintext variant, so an
/// unencrypted connection cannot be represented, configured or loaded from disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Security {
    /// TLS from the first byte (IMAPS 993, SMTPS 465).
    Tls,
    /// Upgrade with STARTTLS; the client must refuse to continue if the upgrade fails.
    StartTls,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
    pub security: Security,
}

impl ServerConfig {
    /// Builds a config, normalising and validating the host name.
    pub fn new(host: &str, port: u16, security: Security) -> Result<Self> {
        let host = normalize_hostname(host).map_err(|reason| Error::InvalidField {
            field: "server host",
            reason,
        })?;
        let cfg = Self {
            host,
            port,
            security,
        };
        cfg.validate("server")?;
        Ok(cfg)
    }

    pub fn validate(&self, field: &'static str) -> Result<()> {
        let canonical = normalize_hostname(&self.host)
            .map_err(|reason| Error::InvalidField { field, reason })?;
        if canonical != self.host {
            return Err(Error::InvalidField {
                field,
                reason: "host must be lowercase ASCII (punycode for international names)",
            });
        }
        if self.port == 0 {
            return Err(Error::InvalidField {
                field,
                reason: "port must not be 0",
            });
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OAuthProvider {
    Google,
    Microsoft,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum AuthMethod {
    /// Password or app password, stored in the OS keyring.
    Password,
    /// OAuth2 (XOAUTH2). The refresh token is stored in the OS keyring.
    #[serde(rename = "oauth2")]
    OAuth2 { provider: OAuthProvider },
}

/// Server endpoints plus the login names for each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerSettings {
    pub imap: ServerConfig,
    pub imap_username: String,
    pub smtp: ServerConfig,
    pub smtp_username: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    pub id: AccountId,
    pub display_name: String,
    pub email: EmailAddress,
    pub imap: ServerConfig,
    pub imap_username: String,
    pub smtp: ServerConfig,
    pub smtp_username: String,
    pub auth: AuthMethod,
}

impl Account {
    pub fn new(
        display_name: &str,
        email: EmailAddress,
        settings: ServerSettings,
        auth: AuthMethod,
    ) -> Result<Self> {
        let account = Self {
            id: AccountId::generate(),
            display_name: display_name.trim().to_owned(),
            email,
            imap: settings.imap,
            imap_username: settings.imap_username,
            smtp: settings.smtp,
            smtp_username: settings.smtp_username,
            auth,
        };
        account.validate()?;
        Ok(account)
    }

    /// Re-validates every field. Called on construction and again for every
    /// account read back from disk, because the file is not trusted.
    pub fn validate(&self) -> Result<()> {
        if AccountId::parse(self.id.as_str())? != self.id {
            return Err(Error::InvalidField {
                field: "account id",
                reason: "not in canonical form",
            });
        }
        validate_text(&self.display_name, "display name", true)?;
        validate_text(&self.imap_username, "IMAP username", false)?;
        validate_text(&self.smtp_username, "SMTP username", false)?;
        self.imap.validate("IMAP server")?;
        self.smtp.validate("SMTP server")?;
        Ok(())
    }

    /// Non-fatal configuration smells (port/security mismatches are the most
    /// common manual-setup mistake).
    pub fn warnings(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        match (self.imap.port, self.imap.security) {
            (993, Security::StartTls) => {
                out.push("IMAP port 993 normally expects TLS from the start, not STARTTLS");
            }
            (143, Security::Tls) => {
                out.push("IMAP port 143 normally expects STARTTLS, not TLS from the start");
            }
            _ => {}
        }
        match (self.smtp.port, self.smtp.security) {
            (465, Security::StartTls) => {
                out.push("SMTP port 465 normally expects TLS from the start, not STARTTLS");
            }
            (587, Security::Tls) => {
                out.push("SMTP port 587 normally expects STARTTLS, not TLS from the start");
            }
            _ => {}
        }
        out
    }
}

/// Rejects empty (unless allowed), over-long, or control-character-bearing text.
/// Control characters (CR, LF, NUL, ESC...) enable header, protocol and terminal
/// injection, so they are refused outright.
pub(crate) fn validate_text(value: &str, field: &'static str, allow_empty: bool) -> Result<()> {
    if value.is_empty() && !allow_empty {
        return Err(Error::InvalidField {
            field,
            reason: "must not be empty",
        });
    }
    if value.chars().count() > MAX_TEXT_CHARS {
        return Err(Error::InvalidField {
            field,
            reason: "is too long",
        });
    }
    if value.chars().any(char::is_control) {
        return Err(Error::InvalidField {
            field,
            reason: "contains control characters",
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> ServerSettings {
        ServerSettings {
            imap: ServerConfig::new("imap.example.com", 993, Security::Tls).unwrap(),
            imap_username: "me@example.com".into(),
            smtp: ServerConfig::new("smtp.example.com", 587, Security::StartTls).unwrap(),
            smtp_username: "me@example.com".into(),
        }
    }

    fn account() -> Account {
        Account::new(
            "Me",
            EmailAddress::parse("me@example.com").unwrap(),
            settings(),
            AuthMethod::Password,
        )
        .unwrap()
    }

    #[test]
    fn builds_and_validates() {
        let a = account();
        assert!(a.validate().is_ok());
        assert!(a.warnings().is_empty());
    }

    #[test]
    fn rejects_control_characters_in_text_fields() {
        let mut s = settings();
        s.imap_username = "me\r\nA1 LOGOUT".into();
        let r = Account::new(
            "Me",
            EmailAddress::parse("me@example.com").unwrap(),
            s,
            AuthMethod::Password,
        );
        assert!(r.is_err());

        let r = Account::new(
            "Me\r\nBcc: evil@example.com",
            EmailAddress::parse("me@example.com").unwrap(),
            settings(),
            AuthMethod::Password,
        );
        assert!(r.is_err());
    }

    #[test]
    fn rejects_empty_username_and_zero_port() {
        let mut s = settings();
        s.smtp_username.clear();
        assert!(
            Account::new(
                "",
                EmailAddress::parse("me@example.com").unwrap(),
                s,
                AuthMethod::Password
            )
            .is_err()
        );
        assert!(ServerConfig::new("imap.example.com", 0, Security::Tls).is_err());
    }

    #[test]
    fn server_host_is_normalised() {
        let c = ServerConfig::new("IMAP.Example.COM.", 993, Security::Tls).unwrap();
        assert_eq!(c.host, "imap.example.com");
    }

    #[test]
    fn warns_on_port_security_mismatch() {
        let mut a = account();
        a.imap.security = Security::StartTls;
        a.smtp.security = Security::Tls;
        assert_eq!(a.warnings().len(), 2);
    }

    #[test]
    fn account_id_must_be_canonical() {
        let mut a = account();
        a.id = AccountId(a.id.as_str().to_uppercase());
        assert!(a.validate().is_err());
        a.id = AccountId("not-a-uuid".into());
        assert!(a.validate().is_err());
    }

    #[test]
    fn toml_round_trip_including_auth_variants() {
        #[derive(Serialize, Deserialize, PartialEq, Debug)]
        struct Wrap {
            accounts: Vec<Account>,
        }
        let mut oauth = account();
        oauth.id = AccountId::generate();
        oauth.auth = AuthMethod::OAuth2 {
            provider: OAuthProvider::Google,
        };
        let w = Wrap {
            accounts: vec![account(), oauth],
        };
        let text = toml::to_string_pretty(&w).unwrap();
        let back: Wrap = toml::from_str(&text).unwrap();
        assert_eq!(w, back);
    }

    #[test]
    fn tampered_email_in_toml_is_rejected_at_parse_time() {
        let text = r#"
            id = "00000000-0000-4000-8000-000000000000"
            display_name = "x"
            email = "not an address"
            imap_username = "u"
            smtp_username = "u"
            auth = { kind = "password" }
            imap = { host = "imap.example.com", port = 993, security = "tls" }
            smtp = { host = "smtp.example.com", port = 465, security = "tls" }
        "#;
        assert!(toml::from_str::<Account>(text).is_err());
    }

    #[test]
    fn plaintext_security_cannot_be_loaded() {
        let text = r#"host = "imap.example.com"
port = 143
security = "plain""#;
        assert!(toml::from_str::<ServerConfig>(text).is_err());
    }
}
