//! Network plumbing shared by IMAP and SMTP: staged errors, DNS, TCP and TLS.
//!
//! Every failure is tagged with the **stage** it happened in (DNS, TCP connect,
//! TLS, greeting, STARTTLS, login, session) and a **kind**, so the UI and the
//! diagnostics screen can say something useful instead of "connection failed".
//!
//! TLS policy: rustls with the `ring` provider, TLS 1.2 or newer, certificates
//! checked by the operating system's verifier (`rustls-platform-verifier`).
//! There is no switch that turns certificate checking off.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::{CertificateDer, ServerName};
use rustls::{CertificateError, ClientConfig};
use rustls_platform_verifier::Verifier;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use crate::{Error, Result};

/// Where in the connection sequence a failure happened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Dns,
    Connect,
    Tls,
    Greeting,
    StartTls,
    Login,
    Session,
}

impl Stage {
    pub fn label(self) -> &'static str {
        match self {
            Self::Dns => "finding the server",
            Self::Connect => "connecting to the server",
            Self::Tls => "securing the connection",
            Self::Greeting => "waiting for the server to greet us",
            Self::StartTls => "upgrading to a secure connection",
            Self::Login => "signing in",
            Self::Session => "talking to the server",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Timeout,
    Dns,
    Refused,
    Unreachable,
    CertificateUntrusted,
    CertificateExpired,
    CertificateNotYetValid,
    CertificateNameMismatch,
    TlsProtocol,
    StartTlsUnsupported,
    AuthRejected,
    /// OAuth: the access token was refused (expired, revoked). Refresh and retry.
    AuthTokenRejected,
    AuthMethodUnsupported,
    /// The OS password vault could not be read (locked, missing, denied).
    SecretStore,
    ServerClosed,
    Protocol,
    Io,
}

/// A connection or session failure with its stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnError {
    pub stage: Stage,
    pub kind: FailureKind,
    /// Technical detail for logs and the "show details" disclosure. Never
    /// contains passwords or tokens.
    pub detail: String,
}

impl ConnError {
    pub fn new(stage: Stage, kind: FailureKind, detail: impl Into<String>) -> Self {
        Self {
            stage,
            kind,
            detail: detail.into(),
        }
    }

    /// Whether trying again later could plausibly succeed. Certificate and
    /// credential problems never fix themselves, so they are not retried.
    pub fn is_transient(&self) -> bool {
        matches!(
            self.kind,
            FailureKind::Timeout
                | FailureKind::Dns
                | FailureKind::Refused
                | FailureKind::Unreachable
                | FailureKind::ServerClosed
                | FailureKind::Io
        )
    }

    /// Plain-language explanation and next step, for non-technical users.
    pub fn advice(&self) -> &'static str {
        match self.kind {
            FailureKind::Timeout => {
                "The server did not answer in time. Check your internet connection; if it works \
                 for other sites, the mail server may be busy or blocked by a firewall. \
                 We will keep retrying."
            }
            FailureKind::Dns => {
                "The server's address could not be found. Check that the server name is spelled \
                 correctly and that you are online."
            }
            FailureKind::Refused => {
                "The server refused the connection. Check the port number, or try again later \
                 in case the provider is having an outage."
            }
            FailureKind::Unreachable => {
                "There is no network route to the server. Check your internet connection or VPN."
            }
            FailureKind::CertificateUntrusted => {
                "The server's security certificate is not trusted by this computer. This can \
                 mean a wrong server name, a company or public Wi-Fi that intercepts traffic, \
                 or an attack. For your safety the connection was stopped."
            }
            FailureKind::CertificateExpired => {
                "The server's security certificate has expired. Ask your provider to renew it, \
                 and check that your computer's date is correct. The connection was stopped."
            }
            FailureKind::CertificateNotYetValid => {
                "The server's certificate is not valid yet, which usually means this computer's \
                 date or time is wrong. Fix the clock and try again."
            }
            FailureKind::CertificateNameMismatch => {
                "The certificate belongs to a different server name than the one configured. \
                 Check the server name in the account settings. The connection was stopped."
            }
            FailureKind::TlsProtocol => {
                "The secure connection could not be set up. The server may only support old, \
                 insecure encryption, or the port may not be a secure mail port."
            }
            FailureKind::StartTlsUnsupported => {
                "The server does not offer encryption on this port, so nothing was sent. Try the \
                 provider's secure port (usually 993 for incoming and 465 or 587 for outgoing)."
            }
            FailureKind::AuthRejected => {
                "The server rejected the sign-in. Check your password. Many providers (Gmail, \
                 Yahoo, iCloud) require an app password or a sign-in through the browser instead \
                 of your normal password."
            }
            FailureKind::AuthTokenRejected => {
                "The saved sign-in has expired or was revoked. Please sign in to this account \
                 again."
            }
            FailureKind::AuthMethodUnsupported => {
                "This server does not offer the sign-in method this account needs. Check the \
                 account's sign-in type in its settings."
            }
            FailureKind::SecretStore => {
                "Your computer's password vault is not available, so the saved password cannot \
                 be read. Unlock or start it (for example GNOME Keyring or KWallet) and try again."
            }
            FailureKind::ServerClosed => {
                "The server closed the connection unexpectedly. We will try again shortly."
            }
            FailureKind::Protocol => {
                "The server sent something unexpected. If this keeps happening, check the server \
                 name and port, or contact your provider."
            }
            FailureKind::Io => {
                "A network error interrupted the connection. We will try again shortly."
            }
        }
    }
}

impl std::fmt::Display for ConnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "problem {}: {}", self.stage.label(), self.detail)
    }
}

impl std::error::Error for ConnError {}

/// What to sign in with. Borrowed so secrets stay in one place and get zeroised there.
#[derive(Debug)]
pub enum Credentials<'a> {
    Password(&'a secrecy::SecretString),
    /// OAuth2 access token (never the refresh token).
    AccessToken(&'a secrecy::SecretString),
}

/// Per-stage time limits. Nothing in this crate waits on the network without one.
#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    pub dns: Duration,
    pub connect: Duration,
    pub tls: Duration,
    pub greeting: Duration,
    pub command: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            dns: Duration::from_secs(10),
            connect: Duration::from_secs(10),
            tls: Duration::from_secs(15),
            greeting: Duration::from_secs(15),
            command: Duration::from_secs(60),
        }
    }
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The TLS configuration type (so other crates need not depend on rustls directly).
pub type TlsConfig = ClientConfig;

/// TLS client configuration: TLS 1.2+, OS certificate verification.
pub fn client_config() -> Result<Arc<ClientConfig>> {
    build_config(Vec::new())
}

/// The same configuration by value, for HTTP clients (reqwest takes ownership).
pub fn http_tls_config() -> Result<ClientConfig> {
    Ok((*build_config(Vec::new())?).clone())
}

/// Like [`client_config`] but additionally trusts the given roots. Used by tests
/// (a throw-away CA); the app itself calls [`client_config`].
pub fn client_config_with_extra_roots(
    roots: Vec<CertificateDer<'static>>,
) -> Result<Arc<ClientConfig>> {
    build_config(roots)
}

fn build_config(extra_roots: Vec<CertificateDer<'static>>) -> Result<Arc<ClientConfig>> {
    let provider = provider();
    let verifier = if extra_roots.is_empty() {
        Verifier::new(provider.clone())
    } else {
        Verifier::new_with_extra_roots(extra_roots, provider.clone())
    }
    .map_err(|e| {
        Error::Tls(format!(
            "could not initialise certificate verification: {e}"
        ))
    })?;

    let config = ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])
        .map_err(|e| Error::Tls(e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Ok(Arc::new(config))
}

/// Resolves a host name. Returns addresses interleaved IPv6/IPv4 (RFC 8305 style)
/// so a broken IPv6 route does not stall the connection.
pub async fn resolve(
    host: &str,
    port: u16,
    limit: Duration,
) -> std::result::Result<Vec<SocketAddr>, ConnError> {
    let lookup = tokio::time::timeout(limit, tokio::net::lookup_host((host, port))).await;
    let addrs: Vec<SocketAddr> = match lookup {
        Err(_) => {
            return Err(ConnError::new(
                Stage::Dns,
                FailureKind::Timeout,
                format!("looking up {host} took longer than {}s", limit.as_secs()),
            ));
        }
        Ok(Err(e)) => {
            return Err(ConnError::new(
                Stage::Dns,
                FailureKind::Dns,
                format!("could not look up {host}: {e}"),
            ));
        }
        Ok(Ok(it)) => it.collect(),
    };
    if addrs.is_empty() {
        return Err(ConnError::new(
            Stage::Dns,
            FailureKind::Dns,
            format!("{host} has no addresses"),
        ));
    }
    Ok(interleave(addrs))
}

/// Alternates address families, starting with IPv6 when present.
pub fn interleave(addrs: Vec<SocketAddr>) -> Vec<SocketAddr> {
    let (v6, v4): (Vec<_>, Vec<_>) = addrs.into_iter().partition(SocketAddr::is_ipv6);
    let mut out = Vec::with_capacity(v6.len() + v4.len());
    let (mut a, mut b) = (v6.into_iter(), v4.into_iter());
    loop {
        match (a.next(), b.next()) {
            (None, None) => break,
            (x, y) => {
                out.extend(x);
                out.extend(y);
            }
        }
    }
    out
}

/// Connects to the first address that answers, within `limit` overall.
pub async fn connect_tcp(
    addrs: &[SocketAddr],
    limit: Duration,
) -> std::result::Result<TcpStream, ConnError> {
    let deadline = tokio::time::Instant::now() + limit;
    let mut last: Option<ConnError> = None;
    for addr in addrs {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        // Give each address a fair share but never beyond the overall deadline.
        let per_try = remaining.min(Duration::from_secs(5).max(remaining / 2));
        match tokio::time::timeout(per_try, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                tune_socket(&stream);
                return Ok(stream);
            }
            Ok(Err(e)) => last = Some(classify_connect_error(&e, addr)),
            Err(_) => {
                last = Some(ConnError::new(
                    Stage::Connect,
                    FailureKind::Timeout,
                    format!("{addr} did not answer"),
                ));
            }
        }
    }
    Err(last.unwrap_or_else(|| {
        ConnError::new(
            Stage::Connect,
            FailureKind::Timeout,
            format!("no server answered within {}s", limit.as_secs()),
        )
    }))
}

fn classify_connect_error(e: &std::io::Error, addr: &SocketAddr) -> ConnError {
    use std::io::ErrorKind as K;
    let kind = match e.kind() {
        K::ConnectionRefused => FailureKind::Refused,
        K::NetworkUnreachable | K::HostUnreachable | K::NetworkDown => FailureKind::Unreachable,
        K::TimedOut => FailureKind::Timeout,
        _ => FailureKind::Io,
    };
    ConnError::new(Stage::Connect, kind, format!("{addr}: {e}"))
}

/// Best-effort socket tuning; failure to tune is not a connection failure.
fn tune_socket(stream: &TcpStream) {
    let _ = stream.set_nodelay(true);
    let keepalive = socket2::TcpKeepalive::new()
        .with_time(Duration::from_secs(60))
        .with_interval(Duration::from_secs(30));
    let _ = socket2::SockRef::from(stream).set_tcp_keepalive(&keepalive);
}

/// Resolves and connects in one step.
pub async fn open_tcp(
    host: &str,
    port: u16,
    t: &Timeouts,
) -> std::result::Result<TcpStream, ConnError> {
    let addrs = resolve(host, port, t.dns).await?;
    connect_tcp(&addrs, t.connect).await
}

/// How much a server may send in answer to one command.
///
/// The IMAP library keeps a whole announced message in memory. Without a limit a hostile
/// (or broken) server could announce a multi-gigabyte message and make the program run out
/// of memory, which ends it. This counts what arrives after each command we send and fails
/// the connection once the allowance is used up.
#[derive(Debug)]
pub struct ReplyCap {
    limit: std::sync::atomic::AtomicU64,
    used: std::sync::atomic::AtomicU64,
}

impl ReplyCap {
    pub fn new(limit: u64) -> Arc<Self> {
        Arc::new(Self {
            limit: std::sync::atomic::AtomicU64::new(limit),
            used: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Changes the allowance for the next replies (the count restarts with the next command).
    pub fn set_limit(&self, limit: u64) {
        self.limit
            .store(limit, std::sync::atomic::Ordering::Relaxed);
    }
}

/// A connection whose incoming data is counted against a [`ReplyCap`].
pub struct Capped<S> {
    inner: S,
    cap: Arc<ReplyCap>,
}

impl<S> std::fmt::Debug for Capped<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Capped").finish_non_exhaustive()
    }
}

impl<S> Capped<S> {
    pub fn new(inner: S, cap: Arc<ReplyCap>) -> Self {
        Self { inner, cap }
    }
}

impl<S: tokio::io::AsyncRead + Unpin> tokio::io::AsyncRead for Capped<S> {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::sync::atomic::Ordering::Relaxed;
        let before = buf.filled().len();
        let polled = std::pin::Pin::new(&mut self.inner).poll_read(cx, buf);
        if let std::task::Poll::Ready(Ok(())) = &polled {
            let got = (buf.filled().len() - before) as u64;
            let used = self.cap.used.fetch_add(got, Relaxed).saturating_add(got);
            if used > self.cap.limit.load(Relaxed) {
                return std::task::Poll::Ready(Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "the server sent more data than allowed for one answer",
                )));
            }
        }
        polled
    }
}

impl<S: tokio::io::AsyncWrite + Unpin> tokio::io::AsyncWrite for Capped<S> {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        // Our own command starts a new answer.
        self.cap.used.store(0, std::sync::atomic::Ordering::Relaxed);
        std::pin::Pin::new(&mut self.inner).poll_write(cx, data)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Runs the TLS handshake and verifies the certificate against `host`.
pub async fn tls_handshake(
    tcp: TcpStream,
    host: &str,
    config: &Arc<ClientConfig>,
    limit: Duration,
) -> std::result::Result<TlsStream<TcpStream>, ConnError> {
    let name = ServerName::try_from(host.to_owned()).map_err(|_| {
        ConnError::new(
            Stage::Tls,
            FailureKind::CertificateNameMismatch,
            format!("{host:?} is not a valid server name"),
        )
    })?;
    let connector = TlsConnector::from(config.clone());
    match tokio::time::timeout(limit, connector.connect(name, tcp)).await {
        Err(_) => Err(ConnError::new(
            Stage::Tls,
            FailureKind::Timeout,
            format!("secure handshake took longer than {}s", limit.as_secs()),
        )),
        Ok(Err(e)) => Err(classify_tls_error(&e)),
        Ok(Ok(stream)) => Ok(stream),
    }
}

fn classify_tls_error(e: &std::io::Error) -> ConnError {
    let rustls_err = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>());
    let kind = match rustls_err {
        Some(rustls::Error::InvalidCertificate(c)) => match c {
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                FailureKind::CertificateExpired
            }
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                FailureKind::CertificateNotYetValid
            }
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => {
                FailureKind::CertificateNameMismatch
            }
            _ => FailureKind::CertificateUntrusted,
        },
        Some(rustls::Error::NoCertificatesPresented) => FailureKind::CertificateUntrusted,
        Some(_) => FailureKind::TlsProtocol,
        None => match e.kind() {
            std::io::ErrorKind::UnexpectedEof
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::BrokenPipe => FailureKind::ServerClosed,
            std::io::ErrorKind::TimedOut => FailureKind::Timeout,
            _ => FailureKind::Io,
        },
    };
    ConnError::new(Stage::Tls, kind, e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn interleave_alternates_families_starting_with_v6() {
        let out = interleave(vec![
            addr("1.1.1.1:1"),
            addr("2.2.2.2:1"),
            addr("[::1]:1"),
            addr("[::2]:1"),
            addr("[::3]:1"),
        ]);
        let fam: Vec<bool> = out.iter().map(SocketAddr::is_ipv6).collect();
        assert_eq!(fam, vec![true, false, true, false, true]);
        assert_eq!(out.len(), 5);
    }

    #[test]
    fn transient_vs_permanent() {
        let t = |k| ConnError::new(Stage::Connect, k, "x").is_transient();
        assert!(t(FailureKind::Timeout));
        assert!(t(FailureKind::Refused));
        assert!(t(FailureKind::ServerClosed));
        assert!(!t(FailureKind::CertificateUntrusted));
        assert!(!t(FailureKind::AuthRejected));
        assert!(!t(FailureKind::StartTlsUnsupported));
    }

    #[test]
    fn every_kind_has_advice() {
        use FailureKind::*;
        for k in [
            Timeout,
            Dns,
            Refused,
            Unreachable,
            CertificateUntrusted,
            CertificateExpired,
            CertificateNotYetValid,
            CertificateNameMismatch,
            TlsProtocol,
            StartTlsUnsupported,
            AuthRejected,
            AuthTokenRejected,
            AuthMethodUnsupported,
            SecretStore,
            ServerClosed,
            Protocol,
            Io,
        ] {
            let advice = ConnError::new(Stage::Session, k, "d").advice();
            assert!(advice.len() > 30, "{k:?}");
        }
    }

    #[test]
    fn client_config_builds() {
        assert!(client_config().is_ok());
    }

    #[tokio::test]
    async fn connect_refused_is_classified() {
        // Bind then drop to obtain a port that is certainly closed.
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let a = l.local_addr().unwrap();
        drop(l);
        // Linux answers "refused" at once. On Windows a closed local port is not refused
        // straight away (a CI run used its whole 2 s limit and reported a timeout), so
        // there is more time here and a timeout is accepted on Windows only. What matters
        // is that a dead port ends in a Connect-stage error instead of hanging.
        let err = connect_tcp(&[a], Duration::from_secs(15))
            .await
            .unwrap_err();
        assert_eq!(err.stage, Stage::Connect);
        let ok = matches!(err.kind, FailureKind::Refused | FailureKind::Io)
            || (cfg!(windows) && err.kind == FailureKind::Timeout);
        assert!(ok, "{err:?}");
    }

    #[tokio::test]
    async fn resolve_literal_ip_works_without_dns() {
        let out = resolve("127.0.0.1", 993, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(out, vec![addr("127.0.0.1:993")]);
    }

    #[tokio::test]
    async fn reply_cap_counts_per_command_and_cuts_off_the_excess() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let cap = ReplyCap::new(100);
        let mut c = Capped::new(client, Arc::clone(&cap));
        // 60 bytes arrive: fine. Another 60 in the same answer: over the limit.
        server.write_all(&[b'a'; 60]).await.unwrap();
        let mut buf = [0u8; 60];
        c.read_exact(&mut buf).await.unwrap();
        server.write_all(&[b'b'; 60]).await.unwrap();
        let err = c.read_exact(&mut buf).await.unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("more data than allowed"));
    }

    #[tokio::test]
    async fn sending_a_new_command_restarts_the_count() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (client, mut server) = tokio::io::duplex(64 * 1024);
        let cap = ReplyCap::new(100);
        let mut c = Capped::new(client, Arc::clone(&cap));
        let mut buf = [0u8; 80];
        for _ in 0..5 {
            c.write_all(b"A1 NOOP\r\n").await.unwrap();
            server.write_all(&[b'x'; 80]).await.unwrap();
            c.read_exact(&mut buf).await.unwrap();
        }
        // The allowance can be raised for one answer (a message we agreed to take).
        cap.set_limit(10_000);
        c.write_all(b"A2 FETCH\r\n").await.unwrap();
        server.write_all(&[b'y'; 5_000]).await.unwrap();
        let mut big = vec![0u8; 5_000];
        c.read_exact(&mut big).await.unwrap();
    }
}
