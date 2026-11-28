//! One authenticated IMAP connection.
//!
//! Security rules implemented here:
//! * Implicit TLS, or STARTTLS that **fails closed**: if the server does not
//!   offer STARTTLS, or answers anything but OK, nothing sensitive is sent.
//! * Capabilities learned before TLS are discarded; they are asked again after.
//! * Bytes that arrive between "STARTTLS OK" and the handshake are treated as an
//!   injection attempt and abort the connection.
//! * Passwords go out only inside TLS, via SASL PLAIN (UTF-8 safe) when offered,
//!   else LOGIN; OAuth uses XOAUTH2.
//! * Every network wait has a time limit; server-supplied sizes are capped.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_imap::imap_proto::NameAttribute;
use async_imap::{Authenticator, Client, Session};
use futures_util::TryStreamExt;
use rustls::ClientConfig;
use secrecy::ExposeSecret;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::client::TlsStream;
use zeroize::Zeroizing;

use super::folder::{Folder, FolderRole, ListedFolder, MAX_FOLDERS, assign_roles};
use super::sync::RemoteFolderState;
use crate::account::{Security, ServerConfig};
use crate::net::{self, ConnError, FailureKind, Stage, Timeouts};

type Stream = net::Capped<TlsStream<TcpStream>>;
type ImapResult<T> = Result<T, ConnError>;

/// Longest response line accepted before TLS/login.
const MAX_LINE: usize = 8192;
/// Cap on messages accepted from one FETCH/SEARCH.
const MAX_ITEMS: usize = 200_000;
/// Most data one command's answer may contain: 200 header blocks of up to
/// `MAX_HEADER_BYTES` each is the largest honest case, with room to spare.
const MAX_REPLY_BYTES: u64 = 48 * 1024 * 1024;
/// Extra room on top of a message's own size for the protocol text around it.
const REPLY_OVERHEAD: u64 = 1024 * 1024;
/// Header bytes kept per message (the rest is dropped).
pub const MAX_HEADER_BYTES: usize = 128 * 1024;
/// RFC 2177: servers may drop an IDLE client after 30 minutes of silence.
pub const MAX_IDLE: Duration = Duration::from_secs(28 * 60);

#[derive(Debug)]
pub struct ConnectParams<'a> {
    pub server: &'a ServerConfig,
    pub username: &'a str,
    pub timeouts: Timeouts,
}

pub use crate::net::Credentials;

/// What the server can do (from its post-login CAPABILITY list).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Features {
    pub idle: bool,
    pub condstore: bool,
    pub move_cmd: bool,
    pub uidplus: bool,
    pub special_use: bool,
    pub list_extended: bool,
    pub id: bool,
}

impl Features {
    fn from_tokens(t: &HashSet<String>) -> Self {
        let has = |n: &str| t.contains(n);
        Self {
            idle: has("IDLE"),
            condstore: has("CONDSTORE") || has("QRESYNC"),
            move_cmd: has("MOVE"),
            uidplus: has("UIDPLUS"),
            special_use: has("SPECIAL-USE"),
            list_extended: has("LIST-EXTENDED"),
            id: has("ID"),
        }
    }
}

/// Message flags in a form the rest of the app can use.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageFlags {
    pub seen: bool,
    pub answered: bool,
    pub flagged: bool,
    pub deleted: bool,
    pub draft: bool,
    /// User keywords (at most 32, each at most 64 printable characters).
    pub keywords: Vec<String>,
}

impl MessageFlags {
    fn from_flags<'a>(flags: impl Iterator<Item = async_imap::types::Flag<'a>>) -> Self {
        use async_imap::types::Flag;
        let mut out = Self::default();
        for f in flags {
            match f {
                Flag::Seen => out.seen = true,
                Flag::Answered => out.answered = true,
                Flag::Flagged => out.flagged = true,
                Flag::Deleted => out.deleted = true,
                Flag::Draft => out.draft = true,
                Flag::Custom(k) => {
                    let k: String = k.chars().filter(|c| !c.is_control()).take(64).collect();
                    if !k.is_empty() && out.keywords.len() < 32 {
                        out.keywords.push(k);
                    }
                }
                _ => {}
            }
        }
        out
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageSummary {
    pub uid: u32,
    pub flags: MessageFlags,
    pub size: Option<u32>,
    /// Server's arrival time, seconds since the Unix epoch.
    pub internal_date: Option<i64>,
    /// Raw RFC 5322 header block (parsed in the MIME layer).
    pub header: Vec<u8>,
    /// True if the header exceeded [`MAX_HEADER_BYTES`] and was cut.
    pub header_truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagUpdate {
    pub uid: u32,
    pub flags: MessageFlags,
    pub modseq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagChange {
    Add,
    Remove,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleOutcome {
    /// The server reported a change (new mail, flag change, expunge).
    Changed,
    /// Nothing happened before the timeout; re-issue IDLE.
    Timeout,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoveOutcome {
    Moved,
    /// Server lacks MOVE and UIDPLUS: the copy exists and the original is only
    /// flagged \Deleted, never expunged blindly (that could remove other mail).
    CopiedOriginalFlagged,
}

// ---------------------------------------------------------------------------
// error mapping
// ---------------------------------------------------------------------------

fn classify_io(stage: Stage, e: &std::io::Error) -> ConnError {
    use std::io::ErrorKind as K;
    let kind = match e.kind() {
        K::UnexpectedEof | K::ConnectionReset | K::ConnectionAborted | K::BrokenPipe => {
            FailureKind::ServerClosed
        }
        K::TimedOut => FailureKind::Timeout,
        // Our own limit on a reply's size (see `net::ReplyCap`).
        K::InvalidData => FailureKind::Protocol,
        _ => FailureKind::Io,
    };
    ConnError::new(stage, kind, e.to_string())
}

fn map_imap(stage: Stage, oauth: bool, e: async_imap::error::Error) -> ConnError {
    use async_imap::error::Error as E;
    match e {
        E::Io(io) => classify_io(stage, &io),
        E::ConnectionLost => ConnError::new(stage, FailureKind::ServerClosed, "connection lost"),
        E::No(msg) if stage == Stage::Login => {
            // RFC 5530 response codes, as recalled: UNAVAILABLE is a temporary condition.
            let kind = if msg.to_ascii_uppercase().contains("[UNAVAILABLE]") {
                FailureKind::Io
            } else if oauth {
                FailureKind::AuthTokenRejected
            } else {
                FailureKind::AuthRejected
            };
            ConnError::new(stage, kind, format!("server said NO: {}", tidy(&msg)))
        }
        E::No(msg) => ConnError::new(
            stage,
            FailureKind::Protocol,
            format!("server said NO: {}", tidy(&msg)),
        ),
        E::Bad(msg) => ConnError::new(
            stage,
            FailureKind::Protocol,
            format!("server said BAD: {}", tidy(&msg)),
        ),
        other => ConnError::new(stage, FailureKind::Protocol, tidy(&other.to_string())),
    }
}

/// Server text is untrusted: no control characters, bounded length.
fn tidy(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).take(200).collect()
}

async fn guard<R>(
    stage: Stage,
    limit: Duration,
    what: &str,
    fut: impl Future<Output = ImapResult<R>>,
) -> ImapResult<R> {
    match tokio::time::timeout(limit, fut).await {
        Ok(r) => r,
        Err(_) => Err(ConnError::new(
            stage,
            FailureKind::Timeout,
            format!("{what} took longer than {}s", limit.as_secs()),
        )),
    }
}

// ---------------------------------------------------------------------------
// raw pre-login conversation (greeting, CAPABILITY, STARTTLS)
// ---------------------------------------------------------------------------

struct Raw<S> {
    io: BufReader<S>,
    tag: u32,
    limit: Duration,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Raw<S> {
    fn new(stream: S, limit: Duration) -> Self {
        Self {
            io: BufReader::new(stream),
            tag: 0,
            limit,
        }
    }

    async fn read_line(&mut self, stage: Stage) -> ImapResult<String> {
        let mut buf = Vec::new();
        let n = {
            let mut limited = (&mut self.io).take(MAX_LINE as u64);
            match tokio::time::timeout(self.limit, limited.read_until(b'\n', &mut buf)).await {
                Err(_) => {
                    return Err(ConnError::new(
                        stage,
                        FailureKind::Timeout,
                        "server stopped answering",
                    ));
                }
                Ok(Err(e)) => return Err(classify_io(stage, &e)),
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
                "response line too long",
            ));
        }
        let text = String::from_utf8_lossy(&buf);
        Ok(text.trim_end_matches(['\r', '\n']).to_owned())
    }

    async fn greeting(&mut self) -> ImapResult<()> {
        let line = self.read_line(Stage::Greeting).await?;
        let upper = line.to_ascii_uppercase();
        if upper.starts_with("* OK") {
            Ok(())
        } else if upper.starts_with("* BYE") {
            Err(ConnError::new(
                Stage::Greeting,
                FailureKind::ServerClosed,
                format!("server refused us: {}", tidy(&line)),
            ))
        } else if upper.starts_with("* PREAUTH") {
            // Skipping authentication is not something we negotiate.
            Err(ConnError::new(
                Stage::Greeting,
                FailureKind::Protocol,
                "unexpected PREAUTH greeting",
            ))
        } else {
            Err(ConnError::new(
                Stage::Greeting,
                FailureKind::Protocol,
                "this does not look like an IMAP server",
            ))
        }
    }

    /// Sends one command and returns the untagged lines up to the tagged reply,
    /// which must be OK.
    async fn command(&mut self, stage: Stage, cmd: &str) -> ImapResult<Vec<String>> {
        self.tag += 1;
        let tag = format!("r{}", self.tag);
        let line = format!("{tag} {cmd}\r\n");
        let write = async {
            self.io.get_mut().write_all(line.as_bytes()).await?;
            self.io.get_mut().flush().await
        };
        match tokio::time::timeout(self.limit, write).await {
            Err(_) => {
                return Err(ConnError::new(
                    stage,
                    FailureKind::Timeout,
                    "could not send command",
                ));
            }
            Ok(Err(e)) => return Err(classify_io(stage, &e)),
            Ok(Ok(())) => {}
        }
        let mut untagged = Vec::new();
        for _ in 0..128 {
            let reply = self.read_line(stage).await?;
            if let Some(rest) = reply.strip_prefix(&format!("{tag} ")) {
                let status = rest.split(' ').next().unwrap_or("").to_ascii_uppercase();
                return if status == "OK" {
                    Ok(untagged)
                } else {
                    Err(ConnError::new(
                        stage,
                        FailureKind::Protocol,
                        format!("{cmd} was refused: {}", tidy(rest)),
                    ))
                };
            }
            if reply.starts_with("* ") {
                untagged.push(reply);
            } else {
                return Err(ConnError::new(
                    stage,
                    FailureKind::Protocol,
                    "unexpected reply",
                ));
            }
        }
        Err(ConnError::new(
            stage,
            FailureKind::Protocol,
            "too many reply lines",
        ))
    }

    async fn capabilities(&mut self) -> ImapResult<HashSet<String>> {
        let lines = self.command(Stage::Greeting, "CAPABILITY").await?;
        let mut out = HashSet::new();
        for l in lines {
            let upper = l.to_ascii_uppercase();
            if let Some(rest) = upper.strip_prefix("* CAPABILITY ") {
                out.extend(rest.split_whitespace().map(str::to_owned));
            }
        }
        if out.is_empty() {
            return Err(ConnError::new(
                Stage::Greeting,
                FailureKind::Protocol,
                "server did not list its capabilities",
            ));
        }
        Ok(out)
    }

    /// Gives the stream back. Refuses if the server sent anything unasked.
    fn into_clean_stream(self, stage: Stage) -> ImapResult<S> {
        if !self.io.buffer().is_empty() {
            return Err(ConnError::new(
                stage,
                FailureKind::Protocol,
                "server sent unexpected data; connection dropped for safety",
            ));
        }
        Ok(self.io.into_inner())
    }
}

// ---------------------------------------------------------------------------
// authenticators
// ---------------------------------------------------------------------------

struct Plain {
    blob: Option<Zeroizing<Vec<u8>>>,
}

impl Authenticator for Plain {
    type Response = Zeroizing<Vec<u8>>;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.blob.take().unwrap_or_default()
    }
}

/// XOAUTH2: the first challenge gets the token blob; if the server then sends an
/// error challenge (base64 JSON), the protocol wants an empty reply, after which
/// the server answers NO.
struct XOAuth2 {
    blob: Option<Zeroizing<Vec<u8>>>,
}

impl Authenticator for XOAuth2 {
    type Response = Zeroizing<Vec<u8>>;
    fn process(&mut self, _challenge: &[u8]) -> Self::Response {
        self.blob.take().unwrap_or_default()
    }
}

pub(crate) fn xoauth2_blob(user: &str, token: &str) -> Zeroizing<Vec<u8>> {
    let mut v = Zeroizing::new(Vec::with_capacity(user.len() + token.len() + 24));
    v.extend_from_slice(b"user=");
    v.extend_from_slice(user.as_bytes());
    v.extend_from_slice(b"\x01auth=Bearer ");
    v.extend_from_slice(token.as_bytes());
    v.extend_from_slice(b"\x01\x01");
    v
}

pub(crate) fn plain_blob(user: &str, pass: &str) -> Zeroizing<Vec<u8>> {
    let mut v = Zeroizing::new(Vec::with_capacity(user.len() + pass.len() + 2));
    v.push(0);
    v.extend_from_slice(user.as_bytes());
    v.push(0);
    v.extend_from_slice(pass.as_bytes());
    v
}

// ---------------------------------------------------------------------------
// the session
// ---------------------------------------------------------------------------

pub struct ImapSession {
    inner: Session<Stream>,
    features: Features,
    limit: Duration,
    cap: Arc<net::ReplyCap>,
}

impl std::fmt::Debug for ImapSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImapSession")
            .field("features", &self.features)
            .finish_non_exhaustive()
    }
}

/// Opens, secures and authenticates a connection.
pub async fn connect(
    params: &ConnectParams<'_>,
    creds: &Credentials<'_>,
    tls: &Arc<ClientConfig>,
) -> ImapResult<ImapSession> {
    let t = params.timeouts;
    let host = params.server.host.as_str();
    let tcp = net::open_tcp(host, params.server.port, &t).await?;

    let mut raw = match params.server.security {
        Security::Tls => {
            let stream = net::tls_handshake(tcp, host, tls, t.tls).await?;
            let mut raw = Raw::new(stream, t.greeting);
            raw.greeting().await?;
            raw
        }
        Security::StartTls => {
            let mut plain = Raw::new(tcp, t.greeting);
            plain.greeting().await?;
            let pre = plain.capabilities().await?;
            if !pre.contains("STARTTLS") {
                return Err(ConnError::new(
                    Stage::StartTls,
                    FailureKind::StartTlsUnsupported,
                    "server does not advertise STARTTLS",
                ));
            }
            plain
                .command(Stage::StartTls, "STARTTLS")
                .await
                .map_err(|e| {
                    // A refusal here means "no encryption available": never carry on.
                    if e.kind == FailureKind::Protocol {
                        ConnError::new(Stage::StartTls, FailureKind::StartTlsUnsupported, e.detail)
                    } else {
                        e
                    }
                })?;
            let tcp = plain.into_clean_stream(Stage::StartTls)?;
            Raw::new(net::tls_handshake(tcp, host, tls, t.tls).await?, t.greeting)
        }
    };

    // Whatever was advertised before TLS is not trusted; ask again inside TLS.
    let tokens = raw.capabilities().await?;
    let stream = raw.into_clean_stream(Stage::Greeting)?;
    let cap = net::ReplyCap::new(MAX_REPLY_BYTES);
    let client = Client::new(net::Capped::new(stream, Arc::clone(&cap)));

    let session = login(client, params.username, creds, &tokens, t.command).await?;
    let mut me = ImapSession {
        inner: session,
        features: Features::default(),
        limit: t.command,
        cap,
    };
    me.refresh_features().await?;
    me.announce_client().await;
    Ok(me)
}

async fn login(
    client: Client<Stream>,
    user: &str,
    creds: &Credentials<'_>,
    tokens: &HashSet<String>,
    limit: Duration,
) -> ImapResult<Session<Stream>> {
    let fut = async {
        match creds {
            Credentials::AccessToken(tok) => {
                if !tokens.contains("AUTH=XOAUTH2") {
                    return Err(ConnError::new(
                        Stage::Login,
                        FailureKind::AuthMethodUnsupported,
                        "server does not offer XOAUTH2",
                    ));
                }
                let auth = XOAuth2 {
                    blob: Some(xoauth2_blob(user, tok.expose_secret())),
                };
                client
                    .authenticate("XOAUTH2", auth)
                    .await
                    .map_err(|(e, _)| map_imap(Stage::Login, true, e))
            }
            Credentials::Password(pw) => {
                if tokens.contains("AUTH=PLAIN") {
                    let auth = Plain {
                        blob: Some(plain_blob(user, pw.expose_secret())),
                    };
                    client
                        .authenticate("PLAIN", auth)
                        .await
                        .map_err(|(e, _)| map_imap(Stage::Login, false, e))
                } else if !tokens.contains("LOGINDISABLED") {
                    client
                        .login(user, pw.expose_secret())
                        .await
                        .map_err(|(e, _)| map_imap(Stage::Login, false, e))
                } else {
                    Err(ConnError::new(
                        Stage::Login,
                        FailureKind::AuthMethodUnsupported,
                        "server offers no password sign-in",
                    ))
                }
            }
        }
    };
    guard(Stage::Login, limit, "signing in", fut).await
}

impl ImapSession {
    pub fn features(&self) -> Features {
        self.features
    }

    async fn refresh_features(&mut self) -> ImapResult<()> {
        let limit = self.limit;
        let caps = guard(Stage::Session, limit, "CAPABILITY", async {
            self.inner
                .capabilities()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await?;
        let tokens: HashSet<String> = [
            "IDLE",
            "CONDSTORE",
            "QRESYNC",
            "MOVE",
            "UIDPLUS",
            "SPECIAL-USE",
            "LIST-EXTENDED",
            "ID",
        ]
        .iter()
        .filter(|c| caps.has_str(c))
        .map(|c| (*c).to_owned())
        .collect();
        self.features = Features::from_tokens(&tokens);
        Ok(())
    }

    /// Some providers insist on an ID command before SELECT; it costs one round
    /// trip and only names this app. Failure is ignored.
    async fn announce_client(&mut self) {
        if !self.features.id {
            return;
        }
        let limit = self.limit;
        let _ = tokio::time::timeout(limit, self.inner.id([("name", Some("EMAYL"))])).await;
    }

    pub async fn list_folders(&mut self) -> ImapResult<Vec<Folder>> {
        let limit = self.limit;
        let extended = self.features.special_use && self.features.list_extended;
        let listed = guard(Stage::Session, limit, "LIST", async {
            // With LIST-EXTENDED the server is asked explicitly for special-use
            // attributes; otherwise they are whatever a plain LIST carries.
            let pattern = if extended {
                "\"*\" RETURN (SPECIAL-USE)"
            } else {
                "\"*\""
            };
            let stream = self
                .inner
                .list(Some(""), Some(pattern))
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?;
            let mut out: Vec<ListedFolder> = Vec::new();
            futures_util::pin_mut!(stream);
            while let Some(name) = stream
                .try_next()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?
            {
                if out.len() >= MAX_FOLDERS {
                    break;
                }
                let mut f = ListedFolder {
                    raw_name: name.name().to_owned(),
                    delimiter: name.delimiter().map(str::to_owned),
                    attr_role: None,
                    noselect: false,
                    nonexistent: false,
                };
                for a in name.attributes() {
                    match a {
                        NameAttribute::NoSelect => f.noselect = true,
                        NameAttribute::All => f.attr_role = Some(FolderRole::All),
                        NameAttribute::Archive => f.attr_role = Some(FolderRole::Archive),
                        NameAttribute::Drafts => f.attr_role = Some(FolderRole::Drafts),
                        NameAttribute::Flagged => f.attr_role = Some(FolderRole::Flagged),
                        NameAttribute::Junk => f.attr_role = Some(FolderRole::Junk),
                        NameAttribute::Sent => f.attr_role = Some(FolderRole::Sent),
                        NameAttribute::Trash => f.attr_role = Some(FolderRole::Trash),
                        NameAttribute::Extension(x) if x.eq_ignore_ascii_case("\\NonExistent") => {
                            f.nonexistent = true;
                        }
                        _ => {}
                    }
                }
                out.push(f);
            }
            Ok(out)
        })
        .await?;
        Ok(assign_roles(listed))
    }

    /// Selects a folder by its **raw** name.
    pub async fn select(&mut self, raw_name: &str) -> ImapResult<RemoteFolderState> {
        let limit = self.limit;
        let condstore = self.features.condstore;
        let mb = guard(Stage::Session, limit, "SELECT", async {
            let r = if condstore {
                self.inner.select_condstore(raw_name).await
            } else {
                self.inner.select(raw_name).await
            };
            r.map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await?;
        let missing = |what: &str| {
            ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                format!("server did not report {what}; this folder cannot be synced safely"),
            )
        };
        Ok(RemoteFolderState {
            uid_validity: mb.uid_validity.ok_or_else(|| missing("UIDVALIDITY"))?,
            uid_next: mb.uid_next.ok_or_else(|| missing("UIDNEXT"))?,
            exists: mb.exists,
            highest_modseq: mb.highest_modseq,
        })
    }

    pub async fn uid_search_all(&mut self) -> ImapResult<Vec<u32>> {
        let limit = self.limit;
        let set = guard(Stage::Session, limit, "SEARCH", async {
            self.inner
                .uid_search("ALL")
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await?;
        if set.len() > MAX_ITEMS {
            return Err(ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                "folder is implausibly large",
            ));
        }
        let mut v: Vec<u32> = set.into_iter().collect();
        v.sort_unstable();
        Ok(v)
    }

    /// Downloads flags, size, arrival date and the header block of the given UIDs.
    pub async fn fetch_headers(&mut self, uid_set: &str) -> ImapResult<Vec<MessageSummary>> {
        check_uid_set(uid_set)?;
        let limit = self.limit;
        guard(Stage::Session, limit, "FETCH", async {
            let stream = self
                .inner
                .uid_fetch(
                    uid_set,
                    "(UID FLAGS RFC822.SIZE INTERNALDATE BODY.PEEK[HEADER])",
                )
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?;
            futures_util::pin_mut!(stream);
            let mut out = Vec::new();
            while let Some(f) = stream
                .try_next()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?
            {
                if out.len() >= MAX_ITEMS {
                    break;
                }
                let Some(uid) = f.uid else { continue };
                let header = f.header().unwrap_or_default();
                let truncated = header.len() > MAX_HEADER_BYTES;
                out.push(MessageSummary {
                    uid,
                    flags: MessageFlags::from_flags(f.flags()),
                    size: f.size,
                    internal_date: f.internal_date().map(|d| d.timestamp()),
                    header: header[..header.len().min(MAX_HEADER_BYTES)].to_vec(),
                    header_truncated: truncated,
                });
            }
            Ok(out)
        })
        .await
    }

    /// Flags of every message, or only those changed after `since` (CONDSTORE).
    /// Do not call on an empty folder.
    pub async fn fetch_flags(&mut self, since: Option<u64>) -> ImapResult<Vec<FlagUpdate>> {
        let limit = self.limit;
        let query = match since {
            Some(m) if self.features.condstore => format!("(UID FLAGS) (CHANGEDSINCE {m})"),
            _ => "(UID FLAGS)".to_owned(),
        };
        guard(Stage::Session, limit, "FETCH FLAGS", async {
            let stream = self
                .inner
                .uid_fetch("1:*", &query)
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?;
            futures_util::pin_mut!(stream);
            let mut out = Vec::new();
            while let Some(f) = stream
                .try_next()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?
            {
                if out.len() >= MAX_ITEMS {
                    break;
                }
                if let Some(uid) = f.uid {
                    out.push(FlagUpdate {
                        uid,
                        flags: MessageFlags::from_flags(f.flags()),
                        modseq: f.modseq,
                    });
                }
            }
            Ok(out)
        })
        .await
    }

    /// Adds or removes a system flag (`"\\Seen"`, `"\\Flagged"`, `"\\Deleted"`...).
    /// UIDs of messages in the selected folder whose Message-ID header matches.
    /// `message_id` is without angle brackets and limited to a safe character set.
    pub async fn uid_search_message_id(&mut self, message_id: &str) -> ImapResult<Vec<u32>> {
        let ok = !message_id.is_empty()
            && message_id.len() <= 200
            && message_id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-@+=%".contains(c));
        if !ok {
            return Err(ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                "unsafe Message-ID",
            ));
        }
        let query = format!("HEADER Message-ID \"<{message_id}>\"");
        let limit = self.limit;
        let set = guard(Stage::Session, limit, "SEARCH", async {
            self.inner
                .uid_search(&query)
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await?;
        let mut v: Vec<u32> = set.into_iter().collect();
        v.sort_unstable();
        Ok(v)
    }

    /// Stores a message in a folder (`APPEND`). Used to keep a copy of a sent
    /// message. The time allowed grows with the size of the message.
    pub async fn append(
        &mut self,
        raw_folder: &str,
        flags: &[&str],
        message: &[u8],
    ) -> ImapResult<()> {
        if flags
            .iter()
            .any(|f| !matches!(*f, "\\Seen" | "\\Answered" | "\\Flagged" | "\\Draft"))
        {
            return Err(ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                "unsupported flag",
            ));
        }
        let flag_text = format!("({})", flags.join(" "));
        let limit = self.limit + Duration::from_secs(message.len() as u64 / 50_000);
        guard(Stage::Session, limit, "APPEND", async {
            self.inner
                .append(raw_folder, Some(&flag_text), None, message)
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await
    }

    pub async fn change_flag(
        &mut self,
        uid_set: &str,
        flag: &str,
        change: FlagChange,
    ) -> ImapResult<()> {
        check_uid_set(uid_set)?;
        if !matches!(
            flag,
            "\\Seen" | "\\Answered" | "\\Flagged" | "\\Deleted" | "\\Draft"
        ) {
            return Err(ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                "unsupported flag",
            ));
        }
        let sign = if change == FlagChange::Add { '+' } else { '-' };
        let query = format!("{sign}FLAGS.SILENT ({flag})");
        let limit = self.limit;
        guard(Stage::Session, limit, "STORE", async {
            let stream = self
                .inner
                .uid_store(uid_set, &query)
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?;
            futures_util::pin_mut!(stream);
            while stream
                .try_next()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?
                .is_some()
            {}
            Ok(())
        })
        .await
    }

    /// Moves messages to another folder (raw name).
    pub async fn move_to(&mut self, uid_set: &str, dest_raw: &str) -> ImapResult<MoveOutcome> {
        check_uid_set(uid_set)?;
        let limit = self.limit;
        let f = self.features;
        if f.move_cmd {
            guard(Stage::Session, limit, "MOVE", async {
                self.inner
                    .uid_mv(uid_set, dest_raw)
                    .await
                    .map_err(|e| map_imap(Stage::Session, false, e))
            })
            .await?;
            return Ok(MoveOutcome::Moved);
        }
        guard(Stage::Session, limit, "COPY", async {
            self.inner
                .uid_copy(uid_set, dest_raw)
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await?;
        self.change_flag(uid_set, "\\Deleted", FlagChange::Add)
            .await?;
        if f.uidplus {
            guard(Stage::Session, limit, "EXPUNGE", async {
                let s = self
                    .inner
                    .uid_expunge(uid_set)
                    .await
                    .map_err(|e| map_imap(Stage::Session, false, e))?;
                futures_util::pin_mut!(s);
                while s
                    .try_next()
                    .await
                    .map_err(|e| map_imap(Stage::Session, false, e))?
                    .is_some()
                {}
                Ok(())
            })
            .await?;
            Ok(MoveOutcome::Moved)
        } else {
            Ok(MoveOutcome::CopiedOriginalFlagged)
        }
    }

    /// Downloads one whole message without marking it read (`BODY.PEEK[]`).
    /// `max_bytes` is checked after receipt; callers should also check the
    /// size reported in the message list first, because the library reads the
    /// whole message into memory.
    pub async fn fetch_message(
        &mut self,
        uid: u32,
        max_bytes: usize,
    ) -> ImapResult<Option<Vec<u8>>> {
        if uid == 0 {
            return Err(ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                "invalid UID",
            ));
        }
        let limit = self.limit;
        let set = uid.to_string();
        // This one answer may be as large as the message we agreed to take, and no more.
        self.cap
            .set_limit((max_bytes as u64).saturating_add(REPLY_OVERHEAD));
        let got = guard(Stage::Session, limit, "FETCH", async {
            let stream = self
                .inner
                .uid_fetch(&set, "(UID BODY.PEEK[])")
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?;
            futures_util::pin_mut!(stream);
            let mut found = None;
            while let Some(f) = stream
                .try_next()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?
            {
                if f.uid == Some(uid)
                    && let Some(b) = f.body()
                {
                    found = Some(b.to_vec());
                }
            }
            Ok(found)
        })
        .await;
        self.cap.set_limit(MAX_REPLY_BYTES);
        let got = got?;
        match got {
            Some(b) if b.len() > max_bytes => Err(ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                "message larger than the allowed size",
            )),
            other => Ok(other),
        }
    }

    /// Permanently removes the given messages (`UID EXPUNGE`; needs UIDPLUS so
    /// that other messages marked deleted by other programs are left alone).
    /// Returns false, doing nothing, if the server lacks UIDPLUS.
    pub async fn expunge_uids(&mut self, uid_set: &str) -> ImapResult<bool> {
        check_uid_set(uid_set)?;
        if !self.features.uidplus {
            return Ok(false);
        }
        self.change_flag(uid_set, "\\Deleted", FlagChange::Add)
            .await?;
        let limit = self.limit;
        guard(Stage::Session, limit, "EXPUNGE", async {
            let s = self
                .inner
                .uid_expunge(uid_set)
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?;
            futures_util::pin_mut!(s);
            while s
                .try_next()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))?
                .is_some()
            {}
            Ok(())
        })
        .await?;
        Ok(true)
    }

    /// Keeps the connection alive and notices server-side changes.
    pub async fn noop(&mut self) -> ImapResult<()> {
        let limit = self.limit;
        guard(Stage::Session, limit, "NOOP", async {
            self.inner
                .noop()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await
    }

    /// Waits for a server push (IDLE) for at most `max` (capped at 28 minutes).
    /// Consumes and returns the session because IDLE owns the connection.
    pub async fn idle(self, max: Duration) -> ImapResult<(Self, IdleOutcome)> {
        self.idle_until(max, std::future::pending()).await
    }

    /// Like [`Self::idle`], but also ends (reporting `Changed`, meaning "go and
    /// look") as soon as `interrupt` completes. The connection stays usable.
    pub async fn idle_until(
        self,
        max: Duration,
        interrupt: impl Future<Output = ()>,
    ) -> ImapResult<(Self, IdleOutcome)> {
        let Self {
            inner,
            features,
            limit,
            cap,
        } = self;
        if !features.idle {
            return Err(ConnError::new(
                Stage::Session,
                FailureKind::Protocol,
                "server does not support IDLE",
            ));
        }
        let max = max.clamp(Duration::from_secs(1), MAX_IDLE);
        let mut handle = inner.idle();
        guard(Stage::Session, limit, "IDLE", async {
            handle
                .init()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await?;

        let outcome = {
            // The stop source must stay alive: dropping it cancels the wait.
            let (wait, stop) = handle.wait_with_timeout(max);
            tokio::pin!(wait);
            tokio::pin!(interrupt);
            // The library's own timer fires at `max`; this outer one only guards against a hang.
            let first = {
                let guarded = tokio::time::timeout(max + Duration::from_secs(10), &mut wait);
                tokio::pin!(guarded);
                tokio::select! {
                    r = &mut guarded => Some(r),
                    () = &mut interrupt => None,
                }
            };
            match first {
                None => {
                    drop(stop);
                    match wait.await {
                        Err(e) => return Err(map_imap(Stage::Session, false, e)),
                        Ok(_) => IdleOutcome::Changed,
                    }
                }
                Some(Err(_)) => IdleOutcome::Timeout,
                Some(Ok(Err(e))) => return Err(map_imap(Stage::Session, false, e)),
                Some(Ok(Ok(async_imap::extensions::idle::IdleResponse::Timeout))) => {
                    IdleOutcome::Timeout
                }
                Some(Ok(Ok(async_imap::extensions::idle::IdleResponse::NewData(_)))) => {
                    IdleOutcome::Changed
                }
                Some(Ok(Ok(async_imap::extensions::idle::IdleResponse::ManualInterrupt))) => {
                    return Err(ConnError::new(
                        Stage::Session,
                        FailureKind::ServerClosed,
                        "IDLE ended unexpectedly",
                    ));
                }
            }
        };
        let inner = guard(Stage::Session, limit, "ending IDLE", async {
            handle
                .done()
                .await
                .map_err(|e| map_imap(Stage::Session, false, e))
        })
        .await?;
        Ok((
            Self {
                inner,
                features,
                limit,
                cap,
            },
            outcome,
        ))
    }

    /// Polite sign-off; errors are irrelevant at this point.
    pub async fn logout(mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(3), self.inner.logout()).await;
    }
}

/// Only digits, `,`, `:` and `*` are allowed in a UID set (blocks command injection).
fn check_uid_set(set: &str) -> ImapResult<()> {
    let ok = !set.is_empty()
        && set.len() <= 8192
        && set
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b',' | b':' | b'*'));
    if ok {
        Ok(())
    } else {
        Err(ConnError::new(
            Stage::Session,
            FailureKind::Protocol,
            "invalid UID set",
        ))
    }
}

#[cfg(test)]
mod unit_tests {
    use super::*;

    #[test]
    fn xoauth2_blob_matches_documented_layout() {
        let b = xoauth2_blob("someuser@example.com", "ya29.vF9dft4q");
        assert_eq!(
            &b[..],
            b"user=someuser@example.com\x01auth=Bearer ya29.vF9dft4q\x01\x01".as_slice()
        );
    }

    #[test]
    fn plain_blob_layout_and_utf8() {
        assert_eq!(&plain_blob("u", "p")[..], b"\0u\0p".as_slice());
        assert_eq!(&plain_blob("jörg", "pä")[..], "\0jörg\0pä".as_bytes());
    }

    #[test]
    fn uid_set_validation() {
        assert!(check_uid_set("1:5,9,12:*").is_ok());
        for bad in ["", "1 2", "1;2", "1\r\nA1 LOGOUT", "a", "1:5 (FLAGS)"] {
            assert!(check_uid_set(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn flags_are_sanitised() {
        use async_imap::types::Flag;
        let f = MessageFlags::from_flags(
            vec![
                Flag::Seen,
                Flag::Flagged,
                Flag::Custom("$Label1".into()),
                Flag::Custom(format!("x\u{7}{}", "y".repeat(100)).into()),
            ]
            .into_iter(),
        );
        assert!(f.seen && f.flagged && !f.deleted);
        assert_eq!(f.keywords.len(), 2);
        assert!(f.keywords[1].len() <= 64 && !f.keywords[1].contains('\u{7}'));
    }
}
