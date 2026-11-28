//! OAuth 2.0 sign-in for Gmail and Microsoft mail (authorization code + PKCE on a
//! loopback redirect, RFC 7636 / RFC 8252) and access-token management.
//!
//! * The app owner supplies their own client IDs ([`OAuthClient`]); none are
//!   borrowed from other projects.
//! * The browser redirect is received on `127.0.0.1` (and `::1` when available)
//!   on a random port. The request's `Host` header and the `state` value are
//!   checked, and the page served back echoes nothing from the request.
//! * Refresh tokens live only in the OS keyring. Microsoft rotates refresh
//!   tokens, so a new one returned by the server always replaces the stored one.
//! * Access tokens live in memory only and are refreshed one-at-a-time per
//!   account, so two parallel IMAP/SMTP connections never race to refresh.

use std::collections::HashMap;
use std::future::Future;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use zeroize::Zeroizing;

use crate::account::{AccountId, OAuthProvider};
use crate::secrets::{SecretKind, SecretStore};

/// Largest token-endpoint reply accepted.
const MAX_REPLY: usize = 64 * 1024;
/// Refresh this long before the access token really expires.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum OAuthError {
    #[error("sign-in was cancelled or denied ({0})")]
    Denied(String),
    #[error("sign-in timed out")]
    Timeout,
    #[error("the saved sign-in is no longer valid")]
    ReauthRequired,
    #[error("the provider rejected this app's registration: {0}")]
    ClientRejected(String),
    #[error("temporary problem reaching the sign-in service: {0}")]
    Transient(String),
    #[error("the provider refused the request: {0}")]
    Rejected(String),
    #[error("the provider did not grant long-term access (no refresh token)")]
    NoRefreshToken,
    #[error("unexpected reply from the sign-in service: {0}")]
    Protocol(String),
    #[error("could not listen for the browser redirect: {0}")]
    Listener(String),
    #[error("secure secret store failed: {0}")]
    SecretStore(String),
    #[error("invalid OAuth configuration: {0}")]
    BadConfig(&'static str),
}

impl OAuthError {
    pub fn is_transient(&self) -> bool {
        matches!(self, Self::Transient(_))
    }

    pub fn advice(&self) -> &'static str {
        match self {
            Self::Denied(_) => "Sign-in was not completed. Try again and choose Allow.",
            Self::Timeout => "The browser sign-in was not completed in time. Please try again.",
            Self::ReauthRequired => {
                "This account's sign-in has expired or was revoked. Please sign in again."
            }
            Self::ClientRejected(_) => {
                "The provider does not accept this app's sign-in registration. This is a problem \
                 with the app setup, not your password; use an app password for this account \
                 instead if your provider offers one."
            }
            Self::Transient(_) => {
                "The sign-in service could not be reached. We will try again shortly."
            }
            Self::Rejected(_) | Self::Protocol(_) => {
                "The provider's sign-in service gave an unexpected answer. Try again later."
            }
            Self::NoRefreshToken => {
                "The provider did not allow ongoing access. Remove this app from your account's \
                 connected apps and sign in again."
            }
            Self::Listener(_) => {
                "Could not receive the browser's answer on this computer. A firewall or security \
                 program may be blocking local connections."
            }
            Self::SecretStore(_) => {
                "Your computer's password vault is not available, so sign-in cannot be saved \
                 safely. Unlock or start it and try again."
            }
            Self::BadConfig(_) => "This provider's sign-in is not set up in this copy of the app.",
        }
    }
}

type Result<T> = std::result::Result<T, OAuthError>;

/// The app owner's registration with a provider.
#[derive(Clone)]
pub struct OAuthClient {
    pub client_id: String,
    /// Google desktop clients have a secret that is not confidential; public
    /// clients (Microsoft) have none.
    pub client_secret: Option<SecretString>,
    /// Microsoft only: `common`, `consumers`, `organizations`, a domain or a GUID.
    pub tenant: Option<String>,
}

impl std::fmt::Debug for OAuthClient {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OAuthClient")
            .field("client_id", &self.client_id)
            .field(
                "client_secret",
                &self.client_secret.as_ref().map(|_| "<redacted>"),
            )
            .field("tenant", &self.tenant)
            .finish()
    }
}

/// The app owner's registrations, one per provider that is supported.
#[derive(Debug, Clone, Default)]
pub struct OAuthClients {
    pub google: Option<OAuthClient>,
    pub microsoft: Option<OAuthClient>,
}

impl OAuthClients {
    pub fn get(&self, provider: OAuthProvider) -> Option<&OAuthClient> {
        match provider {
            OAuthProvider::Google => self.google.as_ref(),
            OAuthProvider::Microsoft => self.microsoft.as_ref(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    pub auth_url: String,
    pub token_url: String,
    pub scope: String,
    /// Host name used in the redirect URI.
    pub redirect_host: &'static str,
}

fn valid_tenant(t: &str) -> bool {
    !t.is_empty()
        && t.len() <= 64
        && t.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.')
}

pub fn endpoints(provider: OAuthProvider, client: &OAuthClient) -> Result<Endpoints> {
    if client.client_id.trim().is_empty() || client.client_id.chars().any(char::is_control) {
        return Err(OAuthError::BadConfig("client id is empty or invalid"));
    }
    Ok(match provider {
        OAuthProvider::Google => Endpoints {
            auth_url: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token_url: "https://oauth2.googleapis.com/token".into(),
            scope: "https://mail.google.com/".into(),
            redirect_host: "127.0.0.1",
        },
        OAuthProvider::Microsoft => {
            let tenant = client.tenant.as_deref().unwrap_or("common");
            if !valid_tenant(tenant) {
                return Err(OAuthError::BadConfig("tenant contains invalid characters"));
            }
            Endpoints {
                auth_url: format!(
                    "https://login.microsoftonline.com/{tenant}/oauth2/v2.0/authorize"
                ),
                token_url: format!("https://login.microsoftonline.com/{tenant}/oauth2/v2.0/token"),
                scope: "https://outlook.office.com/IMAP.AccessAsUser.All \
                        https://outlook.office.com/SMTP.Send offline_access"
                    .into(),
                redirect_host: "localhost",
            }
        }
    })
}

/// What a successful exchange or refresh yields.
pub struct TokenSet {
    pub access_token: SecretString,
    pub refresh_token: Option<SecretString>,
    pub expires_in: Duration,
}

impl std::fmt::Debug for TokenSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenSet")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// HTTP transport (injectable so tests can fake the token endpoint)
// ---------------------------------------------------------------------------

pub struct HttpReply {
    pub status: u16,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for HttpReply {
    // The body can contain tokens, so only its size is shown.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpReply")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .finish()
    }
}

pub trait TokenTransport: Send + Sync {
    fn post_form(&self, url: &str, body: String) -> impl Future<Output = Result<HttpReply>> + Send;
}

/// Production transport: HTTPS only, no redirects, hard timeouts, capped reply.
#[derive(Debug)]
pub struct HttpTransport {
    client: reqwest::Client,
}

impl HttpTransport {
    pub fn new() -> Result<Self> {
        let tls = crate::net::http_tls_config().map_err(|e| OAuthError::Protocol(e.to_string()))?;
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(tls)
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .user_agent(concat!("emayl/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| OAuthError::Protocol(format!("could not build HTTP client: {e}")))?;
        Ok(Self { client })
    }
}

impl TokenTransport for HttpTransport {
    async fn post_form(&self, url: &str, body: String) -> Result<HttpReply> {
        let resp = self
            .client
            .post(url)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|e| OAuthError::Transient(crate::autoconfig::describe_request_error(e)))?;
        let status = resp.status().as_u16();
        if resp.content_length().is_some_and(|n| n > MAX_REPLY as u64) {
            return Err(OAuthError::Protocol("reply too large".into()));
        }
        let bytes = resp
            .bytes()
            .await
            .map_err(|e| OAuthError::Transient(crate::autoconfig::describe_request_error(e)))?;
        if bytes.len() > MAX_REPLY {
            return Err(OAuthError::Protocol("reply too large".into()));
        }
        Ok(HttpReply {
            status,
            body: bytes.to_vec(),
        })
    }
}

// ---------------------------------------------------------------------------
// Token endpoint calls
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TokenReply {
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

fn form(pairs: &[(&str, &str)]) -> String {
    let mut s = url::form_urlencoded::Serializer::new(String::new());
    for (k, v) in pairs {
        s.append_pair(k, v);
    }
    s.finish()
}

/// Keeps server-supplied text short and free of control characters, text-direction
/// overrides and zero-width characters (it is shown to the person in a message).
fn tidy(text: &str) -> String {
    crate::textsafe::clean_line(text, 200)
}

fn parse_token_reply(reply: &HttpReply) -> Result<TokenSet> {
    let parsed: std::result::Result<TokenReply, _> = serde_json::from_slice(&reply.body);
    if reply.status >= 500 || reply.status == 429 {
        return Err(OAuthError::Transient(format!("HTTP {}", reply.status)));
    }
    let parsed = parsed
        .map_err(|_| OAuthError::Protocol(format!("HTTP {} with unreadable body", reply.status)))?;
    if let Some(code) = parsed.error.as_deref() {
        let desc = tidy(parsed.error_description.as_deref().unwrap_or(""));
        return Err(match code {
            "invalid_grant" => OAuthError::ReauthRequired,
            "invalid_client" | "unauthorized_client" | "invalid_request" | "invalid_scope" => {
                OAuthError::ClientRejected(format!("{} {desc}", tidy(code)))
            }
            "temporarily_unavailable" | "server_error" => OAuthError::Transient(tidy(code)),
            other => OAuthError::Rejected(format!("{} {desc}", tidy(other))),
        });
    }
    if reply.status != 200 {
        return Err(OAuthError::Rejected(format!("HTTP {}", reply.status)));
    }
    let access = parsed
        .access_token
        .filter(|t| !t.is_empty())
        .ok_or_else(|| OAuthError::Protocol("reply has no access token".into()))?;
    if let Some(tt) = parsed.token_type.as_deref()
        && !tt.eq_ignore_ascii_case("bearer")
    {
        return Err(OAuthError::Protocol(format!(
            "unsupported token type {}",
            tidy(tt)
        )));
    }
    let expires = parsed.expires_in.unwrap_or(3600).min(24 * 3600);
    Ok(TokenSet {
        access_token: SecretString::from(access),
        refresh_token: parsed
            .refresh_token
            .filter(|t| !t.is_empty())
            .map(SecretString::from),
        expires_in: Duration::from_secs(expires),
    })
}

pub async fn refresh<T: TokenTransport>(
    transport: &T,
    provider: OAuthProvider,
    client: &OAuthClient,
    refresh_token: &SecretString,
) -> Result<TokenSet> {
    let ep = endpoints(provider, client)?;
    let secret = client.client_secret.as_ref();
    let mut pairs = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token.expose_secret()),
        ("client_id", client.client_id.as_str()),
    ];
    if let Some(s) = secret {
        pairs.push(("client_secret", s.expose_secret()));
    }
    if provider == OAuthProvider::Microsoft {
        pairs.push(("scope", ep.scope.as_str()));
    }
    let body = Zeroizing::new(form(&pairs));
    let reply = transport.post_form(&ep.token_url, (*body).clone()).await?;
    parse_token_reply(&reply)
}

// ---------------------------------------------------------------------------
// PKCE + loopback authorization
// ---------------------------------------------------------------------------

fn random_urlsafe(bytes: usize) -> Result<String> {
    let mut buf = Zeroizing::new(vec![0u8; bytes]);
    getrandom::fill(&mut buf)
        .map_err(|e| OAuthError::Listener(format!("no secure randomness: {e}")))?;
    Ok(URL_SAFE_NO_PAD.encode(&*buf))
}

pub(crate) fn pkce_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// A sign-in that has been prepared but not finished. Open [`Self::auth_url`]
/// in the user's browser, then call [`Self::complete`].
pub struct PendingAuth {
    auth_url: String,
    state: String,
    verifier: Zeroizing<String>,
    redirect_uri: String,
    port: u16,
    v4: TcpListener,
    v6: Option<TcpListener>,
    provider: OAuthProvider,
    client: OAuthClient,
    endpoints: Endpoints,
}

impl std::fmt::Debug for PendingAuth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PendingAuth")
            .field("provider", &self.provider)
            .field("port", &self.port)
            .finish_non_exhaustive()
    }
}

impl PendingAuth {
    pub fn auth_url(&self) -> &str {
        &self.auth_url
    }
}

/// Prepares a browser sign-in: binds the loopback listener and builds the URL.
pub async fn begin(
    provider: OAuthProvider,
    client: &OAuthClient,
    login_hint: &str,
) -> Result<PendingAuth> {
    let ep = endpoints(provider, client)?;
    let v4 = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .map_err(|e| OAuthError::Listener(e.to_string()))?;
    let port = v4
        .local_addr()
        .map_err(|e| OAuthError::Listener(e.to_string()))?
        .port();
    // "localhost" may resolve to ::1 first, so listen there too when possible.
    let v6 = TcpListener::bind(SocketAddr::from((Ipv6Addr::LOCALHOST, port)))
        .await
        .ok();

    let verifier = Zeroizing::new(random_urlsafe(32)?);
    let state = random_urlsafe(16)?;
    let redirect_uri = format!("http://{}:{port}/", ep.redirect_host);

    let challenge = pkce_challenge(&verifier);
    let mut pairs: Vec<(&str, &str)> = vec![
        ("response_type", "code"),
        ("client_id", client.client_id.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("scope", ep.scope.as_str()),
        ("code_challenge", challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state.as_str()),
    ];
    if !login_hint.is_empty() && !login_hint.chars().any(char::is_control) {
        pairs.push(("login_hint", login_hint));
    }
    if provider == OAuthProvider::Google {
        // Without these Google may omit the refresh token on repeat sign-ins.
        pairs.push(("access_type", "offline"));
        pairs.push(("prompt", "consent"));
    }
    let auth_url = format!("{}?{}", ep.auth_url, form(&pairs));

    Ok(PendingAuth {
        auth_url,
        state,
        verifier,
        redirect_uri,
        port,
        v4,
        v6,
        provider,
        client: client.clone(),
        endpoints: ep,
    })
}

enum Callback {
    Code(String),
    Denied(String),
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Signed in</title>\
<body style=\"font-family:system-ui,sans-serif;text-align:center;margin-top:20vh\">\
<h2>You can close this tab</h2><p>Return to the mail app to continue.</p></body>";

const FAIL_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Not signed in</title>\
<body style=\"font-family:system-ui,sans-serif;text-align:center;margin-top:20vh\">\
<h2>Sign-in was not completed</h2><p>Return to the mail app and try again.</p></body>";

async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &str) {
    let msg = format!(
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\
         Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nX-Content-Type-Options: nosniff\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.write_all(msg.as_bytes())).await;
    let _ = tokio::time::timeout(Duration::from_secs(2), stream.shutdown()).await;
}

/// Reads one HTTP request head (at most 8 KiB, 5 s).
async fn read_head(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buf = Vec::with_capacity(1024);
    let read = async {
        let mut chunk = [0u8; 1024];
        loop {
            let n = stream.read(&mut chunk).await.ok()?;
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                return Some(());
            }
            if buf.len() > 8192 {
                return None;
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(5), read)
        .await
        .ok()??;
    String::from_utf8(buf).ok()
}

/// Judges one browser request. `Err(status)` means "answer with this and keep waiting".
fn judge_request(
    head: &str,
    port: u16,
    expected_state: &str,
) -> std::result::Result<Callback, &'static str> {
    let mut lines = head.split("\r\n");
    let request_line = lines.next().ok_or("400 Bad Request")?;
    let mut parts = request_line.split(' ');
    let (method, target, version) = (parts.next(), parts.next(), parts.next());
    if method != Some("GET") || !version.is_some_and(|v| v.starts_with("HTTP/1.")) {
        return Err("405 Method Not Allowed");
    }
    let target = target.ok_or("400 Bad Request")?;

    // DNS-rebinding defence: only our own loopback names are acceptable.
    let host = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim().to_ascii_lowercase())
        .ok_or("400 Bad Request")?;
    let ok_hosts = [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("[::1]:{port}"),
    ];
    if !ok_hosts.contains(&host) {
        return Err("400 Bad Request");
    }

    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    if path != "/" {
        return Err("404 Not Found");
    }
    let (mut code, mut state, mut error) = (None, None, None);
    for (k, v) in url::form_urlencoded::parse(query.as_bytes()) {
        match k.as_ref() {
            "code" => code = Some(v.into_owned()),
            "state" => state = Some(v.into_owned()),
            "error" => error = Some(v.into_owned()),
            _ => {}
        }
    }
    // Anything without the right state is not ours (stray tab, local attacker).
    if !state
        .as_deref()
        .is_some_and(|s| constant_time_eq(s, expected_state))
    {
        return Err("400 Bad Request");
    }
    if let Some(e) = error {
        return Ok(Callback::Denied(tidy(&e)));
    }
    match code {
        Some(c) if !c.is_empty() && c.len() <= 4096 => Ok(Callback::Code(c)),
        _ => Err("400 Bad Request"),
    }
}

impl PendingAuth {
    async fn accept(&self) -> std::io::Result<tokio::net::TcpStream> {
        match &self.v6 {
            Some(v6) => tokio::select! {
                r = self.v4.accept() => r.map(|(s, _)| s),
                r = v6.accept() => r.map(|(s, _)| s),
            },
            None => self.v4.accept().await.map(|(s, _)| s),
        }
    }

    async fn wait_for_callback(&self) -> Result<Callback> {
        loop {
            let mut stream = self
                .accept()
                .await
                .map_err(|e| OAuthError::Listener(e.to_string()))?;
            let Some(head) = read_head(&mut stream).await else {
                respond(&mut stream, "400 Bad Request", FAIL_PAGE).await;
                continue;
            };
            match judge_request(&head, self.port, &self.state) {
                Ok(cb) => {
                    let page = if matches!(cb, Callback::Code(_)) {
                        DONE_PAGE
                    } else {
                        FAIL_PAGE
                    };
                    respond(&mut stream, "200 OK", page).await;
                    return Ok(cb);
                }
                Err(status) => respond(&mut stream, status, FAIL_PAGE).await,
            }
        }
    }

    /// Waits (up to `limit`) for the browser to come back, then trades the code
    /// for tokens. A refresh token is required.
    pub async fn complete<T: TokenTransport>(
        self,
        transport: &T,
        limit: Duration,
    ) -> Result<TokenSet> {
        let cb = tokio::time::timeout(limit, self.wait_for_callback())
            .await
            .map_err(|_| OAuthError::Timeout)??;
        let code = match cb {
            Callback::Code(c) => c,
            Callback::Denied(e) => return Err(OAuthError::Denied(e)),
        };
        let mut pairs = vec![
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", self.redirect_uri.as_str()),
            ("client_id", self.client.client_id.as_str()),
            ("code_verifier", self.verifier.as_str()),
        ];
        if let Some(s) = self.client.client_secret.as_ref() {
            pairs.push(("client_secret", s.expose_secret()));
        }
        if self.provider == OAuthProvider::Microsoft {
            pairs.push(("scope", self.endpoints.scope.as_str()));
        }
        let body = form(&pairs);
        let reply = transport.post_form(&self.endpoints.token_url, body).await?;
        let tokens = parse_token_reply(&reply)?;
        if tokens.refresh_token.is_none() {
            return Err(OAuthError::NoRefreshToken);
        }
        Ok(tokens)
    }
}

// ---------------------------------------------------------------------------
// Token manager
// ---------------------------------------------------------------------------

struct Cached {
    token: SecretString,
    expires_at: Instant,
}

type Slot = Arc<tokio::sync::Mutex<Option<Cached>>>;

impl<T: TokenTransport> std::fmt::Debug for TokenManager<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TokenManager").finish_non_exhaustive()
    }
}

/// Hands out valid access tokens, refreshing when needed.
pub struct TokenManager<T: TokenTransport> {
    store: Arc<dyn SecretStore>,
    transport: T,
    slots: Mutex<HashMap<AccountId, Slot>>,
}

impl<T: TokenTransport> TokenManager<T> {
    pub fn new(store: Arc<dyn SecretStore>, transport: T) -> Self {
        Self {
            store,
            transport,
            slots: Mutex::new(HashMap::new()),
        }
    }

    fn slot(&self, id: &AccountId) -> Slot {
        let mut map = self.slots.lock().unwrap_or_else(|e| e.into_inner());
        map.entry(id.clone()).or_default().clone()
    }

    async fn store_call<R: Send + 'static>(
        &self,
        f: impl FnOnce(&dyn SecretStore) -> crate::Result<R> + Send + 'static,
    ) -> Result<R> {
        let store = self.store.clone();
        tokio::task::spawn_blocking(move || f(store.as_ref()))
            .await
            .map_err(|e| OAuthError::SecretStore(e.to_string()))?
            .map_err(|e| OAuthError::SecretStore(e.to_string()))
    }

    /// Saves the result of a first sign-in: refresh token to the keyring,
    /// access token to memory.
    pub async fn save_initial(&self, id: &AccountId, tokens: TokenSet) -> Result<()> {
        let Some(refresh) = tokens.refresh_token else {
            return Err(OAuthError::NoRefreshToken);
        };
        let account = id.clone();
        self.store_call(move |s| s.set(&account, SecretKind::OAuthRefreshToken, &refresh))
            .await?;
        let slot = self.slot(id);
        *slot.lock().await = Some(Cached {
            token: tokens.access_token,
            expires_at: Instant::now() + tokens.expires_in,
        });
        Ok(())
    }

    /// A usable access token, from memory if still fresh, else refreshed.
    pub async fn access_token(
        &self,
        id: &AccountId,
        provider: OAuthProvider,
        client: &OAuthClient,
    ) -> Result<SecretString> {
        self.get(id, provider, client, false).await
    }

    /// Discards the cached token and refreshes now. Call after a server
    /// rejected the current token (revoked, clock skew).
    pub async fn force_refresh(
        &self,
        id: &AccountId,
        provider: OAuthProvider,
        client: &OAuthClient,
    ) -> Result<SecretString> {
        self.get(id, provider, client, true).await
    }

    async fn get(
        &self,
        id: &AccountId,
        provider: OAuthProvider,
        client: &OAuthClient,
        force: bool,
    ) -> Result<SecretString> {
        // One refresh at a time per account: later callers wait, then reuse the result.
        let slot = self.slot(id);
        let mut guard = slot.lock().await;
        if !force
            && let Some(c) = guard.as_ref()
            && c.expires_at > Instant::now() + EXPIRY_MARGIN
        {
            return Ok(SecretString::from(c.token.expose_secret().to_owned()));
        }
        *guard = None;

        let account = id.clone();
        let stored = self
            .store_call(move |s| s.get(&account, SecretKind::OAuthRefreshToken))
            .await?;
        let Some(refresh_token) = stored else {
            return Err(OAuthError::ReauthRequired);
        };

        let tokens = refresh(&self.transport, provider, client, &refresh_token).await?;

        // Rotation: a new refresh token replaces the old one *before* we use the access token.
        if let Some(new) = tokens.refresh_token.as_ref()
            && new.expose_secret() != refresh_token.expose_secret()
        {
            let (account, new) = (
                id.clone(),
                SecretString::from(new.expose_secret().to_owned()),
            );
            self.store_call(move |s| s.set(&account, SecretKind::OAuthRefreshToken, &new))
                .await?;
        }
        let out = SecretString::from(tokens.access_token.expose_secret().to_owned());
        *guard = Some(Cached {
            token: tokens.access_token,
            expires_at: Instant::now() + tokens.expires_in,
        });
        Ok(out)
    }
}

#[cfg(test)]
mod tests;
