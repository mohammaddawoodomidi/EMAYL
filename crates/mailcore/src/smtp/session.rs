//! One authenticated SMTP submission connection.
//!
//! Same security rules as the IMAP side: implicit TLS (port 465 style) or
//! STARTTLS that fails closed, capabilities re-read inside TLS, credentials only
//! inside TLS, time limits everywhere. In addition:
//! * message bodies are normalised to CRLF and dot-stuffed (no bare LF/CR, which
//!   "SMTP smuggling" attacks depend on);
//! * the server's SIZE limit is honoured before anything is uploaded;
//! * every recipient is judged separately, so a partly accepted message is
//!   reported exactly instead of being retried to everyone.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rustls::ClientConfig;
use secrecy::ExposeSecret;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;

use super::reply::{EnhancedStatus, Reply, ReplyClass, explain, parse_line};
use crate::account::{Security, ServerConfig};
use crate::compose::crlf;
use crate::email::EmailAddress;
use crate::imap::session::{plain_blob, xoauth2_blob};
use crate::net::{self, ConnError, Credentials, FailureKind, Stage, Timeouts};

type Stream = TlsStream<TcpStream>;

const MAX_LINE: usize = 4096;
const MAX_REPLY_LINES: usize = 100;
/// Waiting for the server's verdict after the message body has been sent.
const DATA_END_WAIT: Duration = Duration::from_secs(180);
const CHUNK: usize = 64 * 1024;

#[derive(Debug)]
pub struct SmtpParams<'a> {
    pub server: &'a ServerConfig,
    pub username: &'a str,
    /// Name announced in EHLO. Defaults to "localhost" so the computer's real
    /// host name is not leaked.
    pub ehlo_name: &'a str,
    pub timeouts: Timeouts,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SmtpCaps {
    /// Largest message in bytes the server says it takes (0 or absent = unknown).
    pub size: Option<u64>,
    pub auth: HashSet<String>,
    pub starttls: bool,
    pub enhanced_codes: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    MailFrom,
    Recipients,
    Data,
    DataEnd,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rejection {
    pub address: String,
    pub code: u16,
    pub enhanced: Option<String>,
    pub text: String,
    /// `false` = temporary (4xx), worth retrying later.
    pub permanent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub phase: Phase,
    pub code: u16,
    pub enhanced: Option<EnhancedStatus>,
    pub text: String,
    pub permanent: bool,
    /// Per-recipient detail when `phase == Recipients`.
    pub rejections: Vec<Rejection>,
}

impl Refusal {
    pub fn advice(&self) -> &'static str {
        explain(self.code, self.enhanced)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendError {
    Conn(ConnError),
    /// The message is larger than the server's advertised limit; nothing was sent.
    TooLarge {
        limit: u64,
    },
    Refused(Refusal),
}

impl SendError {
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Conn(e) => e.is_transient(),
            Self::TooLarge { .. } => false,
            Self::Refused(r) => !r.permanent,
        }
    }
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Conn(e) => e.fmt(f),
            Self::TooLarge { limit } => {
                write!(f, "message exceeds the server limit of {limit} bytes")
            }
            Self::Refused(r) => write!(f, "server refused ({} {})", r.code, r.text),
        }
    }
}

impl std::error::Error for SendError {}

impl From<ConnError> for SendError {
    fn from(e: ConnError) -> Self {
        Self::Conn(e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SendReport {
    pub accepted: Vec<EmailAddress>,
    /// Recipients the server declined while accepting the message for the others.
    pub rejected: Vec<Rejection>,
    /// The server's final reply (often carries its queue id).
    pub server_reply: String,
}

pub struct SmtpSession<S = Stream> {
    io: BufReader<S>,
    caps: SmtpCaps,
    limit: Duration,
}

impl<S> std::fmt::Debug for SmtpSession<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmtpSession")
            .field("caps", &self.caps)
            .finish_non_exhaustive()
    }
}

fn io_err(stage: Stage, e: &std::io::Error) -> ConnError {
    use std::io::ErrorKind as K;
    let kind = match e.kind() {
        K::UnexpectedEof | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => {
            FailureKind::ServerClosed
        }
        K::TimedOut => FailureKind::Timeout,
        _ => FailureKind::Io,
    };
    ConnError::new(stage, kind, e.to_string())
}

fn timeout_err(stage: Stage, what: &str) -> ConnError {
    ConnError::new(
        stage,
        FailureKind::Timeout,
        format!("{what}: no answer from the server in time"),
    )
}

/// Converts line endings to CRLF and doubles leading dots (RFC 5321 §4.5.2),
/// ending the body with CRLF; the caller appends the `.` terminator line.
pub fn prepare_data(body: &[u8]) -> Vec<u8> {
    let normal = crlf(body);
    let mut out = Vec::with_capacity(normal.len() + 16);
    let mut at_line_start = true;
    for &b in &normal {
        if at_line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        at_line_start = b == b'\n';
    }
    if !out.ends_with(b"\r\n") {
        out.extend_from_slice(b"\r\n");
    }
    out
}

impl<S: AsyncRead + AsyncWrite + Unpin> SmtpSession<S> {
    fn wrap(stream: S, limit: Duration) -> Self {
        Self {
            io: BufReader::new(stream),
            caps: SmtpCaps::default(),
            limit,
        }
    }

    async fn read_line(&mut self, stage: Stage, wait: Duration) -> Result<String, ConnError> {
        let mut buf = Vec::new();
        let n = {
            let mut limited = (&mut self.io).take(MAX_LINE as u64);
            match tokio::time::timeout(wait, limited.read_until(b'\n', &mut buf)).await {
                Err(_) => return Err(timeout_err(stage, "reading reply")),
                Ok(Err(e)) => return Err(io_err(stage, &e)),
                Ok(Ok(n)) => n,
            }
        };
        if n == 0 {
            return Err(ConnError::new(
                stage,
                FailureKind::ServerClosed,
                "server closed the connection",
            ));
        }
        if buf.last() != Some(&b'\n') {
            return Err(ConnError::new(
                stage,
                FailureKind::Protocol,
                "reply line too long",
            ));
        }
        Ok(String::from_utf8_lossy(&buf)
            .trim_end_matches(['\r', '\n'])
            .to_owned())
    }

    async fn read_reply(&mut self, stage: Stage, wait: Duration) -> Result<Reply, ConnError> {
        let mut lines = Vec::new();
        let mut code = None;
        for _ in 0..MAX_REPLY_LINES {
            let line = self.read_line(stage, wait).await?;
            let Some((c, more, text)) = parse_line(&line) else {
                return Err(ConnError::new(
                    stage,
                    FailureKind::Protocol,
                    "this does not look like an SMTP server",
                ));
            };
            if code.is_some_and(|prev| prev != c) {
                return Err(ConnError::new(
                    stage,
                    FailureKind::Protocol,
                    "inconsistent reply codes",
                ));
            }
            code = Some(c);
            lines.push(text.to_owned());
            if !more {
                return Ok(Reply { code: c, lines });
            }
        }
        Err(ConnError::new(
            stage,
            FailureKind::Protocol,
            "reply is too long",
        ))
    }

    async fn write_all(&mut self, stage: Stage, bytes: &[u8]) -> Result<(), ConnError> {
        let limit = self.limit;
        let w = async {
            self.io.get_mut().write_all(bytes).await?;
            self.io.get_mut().flush().await
        };
        match tokio::time::timeout(limit, w).await {
            Err(_) => Err(timeout_err(stage, "sending")),
            Ok(Err(e)) => Err(io_err(stage, &e)),
            Ok(Ok(())) => Ok(()),
        }
    }

    async fn command(&mut self, stage: Stage, line: &str) -> Result<Reply, ConnError> {
        self.write_all(stage, format!("{line}\r\n").as_bytes())
            .await?;
        let limit = self.limit;
        self.read_reply(stage, limit).await
    }

    async fn greeting(&mut self) -> Result<(), ConnError> {
        let limit = self.limit;
        let r = self.read_reply(Stage::Greeting, limit).await?;
        match r.code {
            220 => Ok(()),
            421 | 554 => Err(ConnError::new(
                Stage::Greeting,
                FailureKind::ServerClosed,
                format!("server refused us: {} {}", r.code, r.text()),
            )),
            _ => Err(ConnError::new(
                Stage::Greeting,
                FailureKind::Protocol,
                "unexpected greeting",
            )),
        }
    }

    async fn ehlo(&mut self, stage: Stage, name: &str) -> Result<SmtpCaps, ConnError> {
        let r = self.command(stage, &format!("EHLO {name}")).await?;
        if r.code != 250 {
            return Err(ConnError::new(
                stage,
                FailureKind::Protocol,
                format!("EHLO refused: {} {}", r.code, r.text()),
            ));
        }
        let mut caps = SmtpCaps::default();
        for line in r.lines.iter().skip(1) {
            let upper = line.to_ascii_uppercase();
            let mut words = upper.split_whitespace();
            match words.next() {
                Some("STARTTLS") => caps.starttls = true,
                Some("ENHANCEDSTATUSCODES") => caps.enhanced_codes = true,
                Some("SIZE") => {
                    caps.size = words.next().and_then(|n| n.parse().ok()).filter(|n| *n > 0)
                }
                Some("AUTH") => caps.auth.extend(words.map(str::to_owned)),
                // Older servers write "AUTH=PLAIN LOGIN".
                Some(w) if w.starts_with("AUTH=") => {
                    caps.auth.insert(w["AUTH=".len()..].to_owned());
                    caps.auth.extend(words.map(str::to_owned));
                }
                _ => {}
            }
        }
        Ok(caps)
    }

    fn into_clean_stream(self, stage: Stage) -> Result<S, ConnError> {
        if !self.io.buffer().is_empty() {
            return Err(ConnError::new(
                stage,
                FailureKind::Protocol,
                "server sent unexpected data; connection dropped for safety",
            ));
        }
        Ok(self.io.into_inner())
    }

    fn auth_failure(stage: Stage, oauth: bool, r: &Reply) -> ConnError {
        let kind = match r.code {
            421 | 454 => FailureKind::Io,
            _ if oauth => FailureKind::AuthTokenRejected,
            _ => FailureKind::AuthRejected,
        };
        ConnError::new(stage, kind, format!("server said {} {}", r.code, r.text()))
    }

    async fn authenticate(&mut self, user: &str, creds: &Credentials<'_>) -> Result<(), ConnError> {
        let st = Stage::Login;
        let has = |m: &str| self.caps.auth.contains(m);
        match creds {
            Credentials::AccessToken(tok) => {
                if !has("XOAUTH2") {
                    return Err(ConnError::new(
                        st,
                        FailureKind::AuthMethodUnsupported,
                        "server does not offer XOAUTH2",
                    ));
                }
                let blob = xoauth2_blob(user, tok.expose_secret());
                let line =
                    zeroize::Zeroizing::new(format!("AUTH XOAUTH2 {}", STANDARD.encode(&*blob)));
                let r = self.command(st, &line).await?;
                match r.code {
                    235 => Ok(()),
                    334 => {
                        // Error details arrive as a challenge; the protocol wants an empty reply.
                        let fin = self.command(st, "").await?;
                        Err(Self::auth_failure(st, true, &fin))
                    }
                    _ => Err(Self::auth_failure(st, true, &r)),
                }
            }
            Credentials::Password(pw) => {
                if has("PLAIN") {
                    let blob = plain_blob(user, pw.expose_secret());
                    let line =
                        zeroize::Zeroizing::new(format!("AUTH PLAIN {}", STANDARD.encode(&*blob)));
                    let r = self.command(st, &line).await?;
                    if r.code == 235 {
                        Ok(())
                    } else {
                        Err(Self::auth_failure(st, false, &r))
                    }
                } else if has("LOGIN") {
                    let r = self.command(st, "AUTH LOGIN").await?;
                    if r.code != 334 {
                        return Err(Self::auth_failure(st, false, &r));
                    }
                    let r = self.command(st, &STANDARD.encode(user)).await?;
                    if r.code != 334 {
                        return Err(Self::auth_failure(st, false, &r));
                    }
                    let line = zeroize::Zeroizing::new(STANDARD.encode(pw.expose_secret()));
                    let r = self.command(st, &line).await?;
                    if r.code == 235 {
                        Ok(())
                    } else {
                        Err(Self::auth_failure(st, false, &r))
                    }
                } else {
                    Err(ConnError::new(
                        st,
                        FailureKind::AuthMethodUnsupported,
                        "server offers no password sign-in",
                    ))
                }
            }
        }
    }

    pub fn caps(&self) -> &SmtpCaps {
        &self.caps
    }

    /// Forgets any half-finished transaction (used between messages).
    pub async fn reset(&mut self) -> Result<(), ConnError> {
        let r = self.command(Stage::Session, "RSET").await?;
        if r.is_positive() {
            Ok(())
        } else {
            Err(ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                "RSET refused",
            ))
        }
    }

    /// Sends one message to the given envelope recipients.
    pub async fn send(
        &mut self,
        from: &EmailAddress,
        recipients: &[EmailAddress],
        message: &[u8],
    ) -> Result<SendReport, SendError> {
        if recipients.is_empty() {
            return Err(SendError::Refused(Refusal {
                phase: Phase::Recipients,
                code: 0,
                enhanced: None,
                text: "no recipients".into(),
                permanent: true,
                rejections: Vec::new(),
            }));
        }
        let body = prepare_data(message);
        if let Some(limit) = self.caps.size
            && body.len() as u64 > limit
        {
            return Err(SendError::TooLarge { limit });
        }

        let size_param = if self.caps.size.is_some() {
            format!(" SIZE={}", body.len())
        } else {
            String::new()
        };
        let r = self
            .command(Stage::Session, &format!("MAIL FROM:<{from}>{size_param}"))
            .await?;
        if !r.is_positive() {
            let refusal = refusal(Phase::MailFrom, &r, Vec::new());
            self.best_effort_reset().await;
            return Err(SendError::Refused(refusal));
        }

        let mut accepted = Vec::new();
        let mut rejected = Vec::new();
        for rcpt in recipients {
            let r = self
                .command(Stage::Session, &format!("RCPT TO:<{rcpt}>"))
                .await?;
            if r.is_positive() {
                accepted.push(rcpt.clone());
            } else {
                rejected.push(Rejection {
                    address: rcpt.to_string(),
                    code: r.code,
                    enhanced: r.enhanced().map(|e| e.to_string()),
                    text: r.text(),
                    permanent: r.class() == ReplyClass::Permanent,
                });
            }
        }
        if accepted.is_empty() {
            let permanent = rejected.iter().all(|r| r.permanent);
            let first = rejected.first().cloned();
            self.best_effort_reset().await;
            return Err(SendError::Refused(Refusal {
                phase: Phase::Recipients,
                code: first.as_ref().map_or(0, |r| r.code),
                enhanced: first
                    .as_ref()
                    .and_then(|r| r.enhanced.as_deref().and_then(EnhancedStatus::parse)),
                text: first.map(|r| r.text).unwrap_or_default(),
                permanent,
                rejections: rejected,
            }));
        }

        let r = self.command(Stage::Session, "DATA").await?;
        if r.code != 354 {
            let refusal = refusal(Phase::Data, &r, Vec::new());
            self.best_effort_reset().await;
            return Err(SendError::Refused(refusal));
        }
        for chunk in body.chunks(CHUNK) {
            self.write_all(Stage::Session, chunk).await?;
        }
        self.write_all(Stage::Session, b".\r\n").await?;
        let fin = self.read_reply(Stage::Session, DATA_END_WAIT).await?;
        if !fin.is_positive() {
            return Err(SendError::Refused(refusal(Phase::DataEnd, &fin, rejected)));
        }
        Ok(SendReport {
            accepted,
            rejected,
            server_reply: fin.text(),
        })
    }

    async fn best_effort_reset(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(5), self.command(Stage::Session, "RSET"))
            .await;
    }

    pub async fn quit(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), self.command(Stage::Session, "QUIT"))
            .await;
    }
}

fn refusal(phase: Phase, r: &Reply, rejections: Vec<Rejection>) -> Refusal {
    Refusal {
        phase,
        code: r.code,
        enhanced: r.enhanced(),
        text: r.text(),
        permanent: r.class() == ReplyClass::Permanent,
        rejections,
    }
}

/// Opens, secures and authenticates a submission connection.
pub async fn connect(
    params: &SmtpParams<'_>,
    creds: &Credentials<'_>,
    tls: &Arc<ClientConfig>,
) -> Result<SmtpSession, ConnError> {
    let t = params.timeouts;
    let host = params.server.host.as_str();
    let tcp = net::open_tcp(host, params.server.port, &t).await?;
    let ehlo_name = if params.ehlo_name.is_empty() {
        "localhost"
    } else {
        params.ehlo_name
    };

    let mut session = match params.server.security {
        Security::Tls => {
            let tls_stream = net::tls_handshake(tcp, host, tls, t.tls).await?;
            let mut s = SmtpSession::wrap(tls_stream, t.command);
            s.limit = t.greeting;
            s.greeting().await?;
            s.limit = t.command;
            s
        }
        Security::StartTls => {
            let mut plain = SmtpSession::wrap(tcp, t.command);
            plain.limit = t.greeting;
            plain.greeting().await?;
            plain.limit = t.command;
            let pre = plain.ehlo(Stage::StartTls, ehlo_name).await?;
            if !pre.starttls {
                return Err(ConnError::new(
                    Stage::StartTls,
                    FailureKind::StartTlsUnsupported,
                    "server does not advertise STARTTLS",
                ));
            }
            let r = plain.command(Stage::StartTls, "STARTTLS").await?;
            if r.code != 220 {
                return Err(ConnError::new(
                    Stage::StartTls,
                    FailureKind::StartTlsUnsupported,
                    format!("STARTTLS refused: {} {}", r.code, r.text()),
                ));
            }
            let tcp = plain.into_clean_stream(Stage::StartTls)?;
            SmtpSession::wrap(net::tls_handshake(tcp, host, tls, t.tls).await?, t.command)
        }
    };

    // Anything advertised before TLS is not trusted.
    session.caps = session.ehlo(Stage::Greeting, ehlo_name).await?;
    session.authenticate(params.username, creds).await?;
    Ok(session)
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn dot_stuffing_and_line_endings() {
        assert_eq!(prepare_data(b"hello"), b"hello\r\n");
        assert_eq!(prepare_data(b"a\n.\nb\n"), b"a\r\n..\r\nb\r\n");
        assert_eq!(prepare_data(b".leading"), b"..leading\r\n");
        assert_eq!(prepare_data(b"x\r\n..y\r\n"), b"x\r\n...y\r\n");
        assert_eq!(prepare_data(b""), b"\r\n");
        // A bare-LF terminator trick ("\n.\n") must not survive as a terminator.
        let smuggle = prepare_data(b"a\n.\nMAIL FROM:<x@y>\n");
        assert!(!String::from_utf8_lossy(&smuggle).contains("\r\n.\r\nMAIL"));
        assert!(smuggle.windows(2).all(|w| w != b"\n\n"));
    }
}
