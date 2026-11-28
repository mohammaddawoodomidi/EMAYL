//! Signing in through the provider's own website (Google, Microsoft).
//!
//! The program never sees the person's password for these providers: the
//! browser does the sign-in and hands back a one-time code, which is traded for
//! tokens. The long-lived token goes to the operating system's password store.

use std::sync::Arc;
use std::time::Duration;

use mailcore::account::{AccountId, OAuthProvider};
use mailcore::oauth::{self, HttpTransport, OAuthClient, OAuthClients, OAuthError};
use secrecy::SecretString;

use super::{Services, lock};

/// How long the browser has to come back before the attempt is given up.
const SIGN_IN_LIMIT: Duration = Duration::from_secs(300);

/// How a website sign-in ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignIn {
    /// The tokens are saved in the password store.
    Done,
    /// The person pressed Cancel.
    Cancelled,
    /// Something went wrong; the text says what to do.
    Failed(String),
}

fn describe(e: &OAuthError) -> String {
    format!("{} (Details: {e})", e.advice())
}

fn client(id: Option<&str>, secret: Option<&str>, tenant: Option<&str>) -> Option<OAuthClient> {
    let id = id?.trim();
    if id.is_empty() {
        return None;
    }
    fn keep(s: Option<&str>) -> Option<&str> {
        s.map(str::trim).filter(|s| !s.is_empty())
    }
    Some(OAuthClient {
        client_id: id.to_owned(),
        client_secret: keep(secret).map(|s| SecretString::from(s.to_owned())),
        tenant: keep(tenant).map(str::to_owned),
    })
}

/// The registrations to use, from the values the program was built with.
/// An empty value means "not registered": that provider's website sign-in stays off.
pub fn clients_from(
    google_id: Option<&str>,
    google_secret: Option<&str>,
    microsoft_id: Option<&str>,
    microsoft_tenant: Option<&str>,
) -> OAuthClients {
    OAuthClients {
        google: client(google_id, google_secret, None),
        microsoft: client(microsoft_id, None, microsoft_tenant),
    }
}

/// The registrations compiled into this copy of the program (none by default).
/// They are public identifiers, not secrets of the person using the program.
pub fn built_in_clients() -> OAuthClients {
    clients_from(
        option_env!("EMAYL_GOOGLE_CLIENT_ID"),
        option_env!("EMAYL_GOOGLE_CLIENT_SECRET"),
        option_env!("EMAYL_MICROSOFT_CLIENT_ID"),
        option_env!("EMAYL_MICROSOFT_TENANT"),
    )
}

impl Services {
    /// Replaces the provider registrations (used by tests).
    #[must_use]
    pub fn with_oauth_clients(mut self, clients: OAuthClients) -> Self {
        self.clients = clients;
        self
    }

    /// Opens the provider's sign-in page in the browser and waits for the answer
    /// (runs on the mail worker). On success the tokens are saved for `id`.
    pub fn sign_in_through_website(
        self: &Arc<Self>,
        id: AccountId,
        provider: OAuthProvider,
        login_hint: String,
    ) -> impl std::future::Future<Output = SignIn> + Send + use<> {
        let cancel = Arc::new(tokio::sync::Notify::new());
        *lock(&self.sign_in_cancel) = Some(Arc::clone(&cancel));
        let me = Arc::clone(self);
        let job = self
            .worker
            .run(move || async move { me.run_sign_in(&id, provider, &login_hint, &cancel).await });
        async move {
            job.await.unwrap_or_else(|| {
                SignIn::Failed(
                    "The mail engine stopped unexpectedly. Please restart the program.".to_owned(),
                )
            })
        }
    }

    /// Stops a sign-in that is waiting for the browser.
    pub fn cancel_sign_in(&self) {
        if let Some(c) = lock(&self.sign_in_cancel).take() {
            c.notify_one();
        }
    }

    async fn run_sign_in(
        &self,
        id: &AccountId,
        provider: OAuthProvider,
        login_hint: &str,
        cancel: &tokio::sync::Notify,
    ) -> SignIn {
        let Some(client) = self.clients.get(provider).cloned() else {
            return SignIn::Failed(
                "Sign-in through this provider's website is not set up in this copy of the program."
                    .to_owned(),
            );
        };
        let pending = match oauth::begin(provider, &client, login_hint).await {
            Ok(p) => p,
            Err(e) => return SignIn::Failed(describe(&e)),
        };
        let url = pending.auth_url().to_owned();
        match tokio::task::spawn_blocking(move || crate::util::open_in_browser(&url)).await {
            Ok(Ok(())) => {}
            Ok(Err(m)) => return SignIn::Failed(m),
            Err(_) => return SignIn::Failed("Could not open your web browser.".to_owned()),
        }
        let transport = match HttpTransport::new() {
            Ok(t) => t,
            Err(e) => return SignIn::Failed(describe(&e)),
        };
        let tokens = tokio::select! {
            r = pending.complete(&transport, SIGN_IN_LIMIT) => r,
            () = cancel.notified() => return SignIn::Cancelled,
        };
        match tokens {
            Err(e) => SignIn::Failed(describe(&e)),
            Ok(t) => match self.tokens.save_initial(id, t).await {
                Ok(()) => SignIn::Done,
                Err(e) => SignIn::Failed(describe(&e)),
            },
        }
    }

    /// Saves an account that signs in through a website. Its tokens were saved
    /// by the sign-in; if the account cannot be saved they are removed again.
    pub fn add_website_account(&self, account: mailcore::account::Account) -> mailcore::Result<()> {
        let added = lock(&self.registry).add(account.clone());
        if let Err(e) = added {
            let _ = mailcore::secrets::purge_account(self.secrets.as_ref(), &account.id);
            return Err(e);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use secrecy::ExposeSecret;

    #[test]
    fn empty_or_missing_ids_leave_a_provider_off() {
        let c = clients_from(None, None, Some("  "), None);
        assert!(c.google.is_none() && c.microsoft.is_none());
        let c = clients_from(Some(""), Some("secret"), None, Some("common"));
        assert!(c.google.is_none() && c.microsoft.is_none());
    }

    #[test]
    fn registrations_are_trimmed_and_kept_apart() {
        let c = clients_from(
            Some(" g-id "),
            Some(" g-secret "),
            Some("m-id"),
            Some("consumers"),
        );
        let g = c.google.expect("google");
        assert_eq!(g.client_id, "g-id");
        assert_eq!(g.client_secret.expect("secret").expose_secret(), "g-secret");
        assert!(g.tenant.is_none());
        let m = c.microsoft.expect("microsoft");
        assert_eq!(m.client_id, "m-id");
        assert_eq!(m.tenant.as_deref(), Some("consumers"));
        assert!(m.client_secret.is_none());
    }

    #[test]
    fn blank_secret_and_tenant_are_ignored() {
        let c = clients_from(Some("g"), Some("  "), Some("m"), Some(""));
        assert!(c.google.expect("g").client_secret.is_none());
        assert!(c.microsoft.expect("m").tenant.is_none());
    }

    #[test]
    fn this_build_has_no_registrations_unless_given_at_build_time() {
        // Guards against committing real identifiers by accident: CI builds that
        // do provide them set the variables, local test runs do not.
        if option_env!("EMAYL_GOOGLE_CLIENT_ID").is_none() {
            assert!(built_in_clients().google.is_none());
        }
        if option_env!("EMAYL_MICROSOFT_CLIENT_ID").is_none() {
            assert!(built_in_clients().microsoft.is_none());
        }
    }

    #[test]
    fn error_text_has_advice_first() {
        let t = describe(&OAuthError::Timeout);
        assert!(t.starts_with("The browser sign-in was not completed in time"));
        assert!(t.contains("Details:"));
    }
}
