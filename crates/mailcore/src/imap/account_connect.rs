//! Connecting an [`Account`]: fetches its secret, signs in, and (for OAuth)
//! refreshes a rejected access token exactly once before giving up.

use std::sync::Arc;

use rustls::ClientConfig;
use secrecy::SecretString;

use super::session::{ConnectParams, ImapSession, connect};
use crate::account::{Account, AuthMethod};
use crate::net::{ConnError, Credentials, FailureKind, Stage, Timeouts};
use crate::oauth::{OAuthClients, OAuthError, TokenManager, TokenTransport};
use crate::secrets::{SecretKind, SecretStore};

pub fn oauth_to_conn(e: OAuthError) -> ConnError {
    let (kind, detail) = match &e {
        OAuthError::ReauthRequired
        | OAuthError::NoRefreshToken
        | OAuthError::Denied(_)
        | OAuthError::Timeout => (FailureKind::AuthTokenRejected, e.to_string()),
        OAuthError::ClientRejected(_) | OAuthError::BadConfig(_) => {
            (FailureKind::AuthMethodUnsupported, e.to_string())
        }
        OAuthError::Transient(_) => (FailureKind::Io, e.to_string()),
        OAuthError::SecretStore(_) => (FailureKind::SecretStore, e.to_string()),
        _ => (FailureKind::Protocol, e.to_string()),
    };
    ConnError::new(Stage::Login, kind, detail)
}

pub async fn connect_account<T: TokenTransport>(
    account: &Account,
    secrets: &Arc<dyn SecretStore>,
    tokens: &TokenManager<T>,
    clients: &OAuthClients,
    tls: &Arc<ClientConfig>,
    timeouts: Timeouts,
) -> Result<ImapSession, ConnError> {
    let params = ConnectParams {
        server: &account.imap,
        username: &account.imap_username,
        timeouts,
    };
    with_credentials(
        account,
        secrets,
        tokens,
        clients,
        async |creds: &Credentials<'_>| connect(&params, creds, tls).await,
    )
    .await
}

/// Looks up the account's secret (password, or a fresh OAuth access token) and
/// runs `attempt` with it. If the server rejects an OAuth token, the token is
/// refreshed once and `attempt` runs again. Shared by IMAP and SMTP.
pub(crate) async fn with_credentials<T: TokenTransport, R>(
    account: &Account,
    secrets: &Arc<dyn SecretStore>,
    tokens: &TokenManager<T>,
    clients: &OAuthClients,
    attempt: impl AsyncFn(&Credentials<'_>) -> Result<R, ConnError>,
) -> Result<R, ConnError> {
    match account.auth {
        AuthMethod::Password => {
            let (id, store) = (account.id.clone(), secrets.clone());
            let pw: Option<SecretString> =
                tokio::task::spawn_blocking(move || store.get(&id, SecretKind::Password))
                    .await
                    .map_err(|e| {
                        ConnError::new(Stage::Login, FailureKind::SecretStore, e.to_string())
                    })?
                    .map_err(|e| {
                        ConnError::new(Stage::Login, FailureKind::SecretStore, e.to_string())
                    })?;
            let Some(pw) = pw else {
                return Err(ConnError::new(
                    Stage::Login,
                    FailureKind::AuthRejected,
                    "no saved password for this account",
                ));
            };
            attempt(&Credentials::Password(&pw)).await
        }
        AuthMethod::OAuth2 { provider } => {
            let Some(client) = clients.get(provider) else {
                return Err(ConnError::new(
                    Stage::Login,
                    FailureKind::AuthMethodUnsupported,
                    "this app has no sign-in registration for that provider",
                ));
            };
            let token = tokens
                .access_token(&account.id, provider, client)
                .await
                .map_err(oauth_to_conn)?;
            match attempt(&Credentials::AccessToken(&token)).await {
                Err(e) if e.kind == FailureKind::AuthTokenRejected => {
                    // Cached token may have been revoked or expired early: refresh once.
                    let fresh = tokens
                        .force_refresh(&account.id, provider, client)
                        .await
                        .map_err(oauth_to_conn)?;
                    attempt(&Credentials::AccessToken(&fresh)).await
                }
                other => other,
            }
        }
    }
}
