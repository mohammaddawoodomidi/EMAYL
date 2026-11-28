//! A small in-process IMAP server (TLS or STARTTLS) for tests. It implements
//! just enough of the protocol to exercise the client, plus switches that make
//! it misbehave on purpose.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    date_time_ymd,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

pub struct Pki {
    pub ca: CertificateDer<'static>,
    acceptor: TlsAcceptor,
}

impl Pki {
    pub fn acceptor(&self) -> TlsAcceptor {
        self.acceptor.clone()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CertFlaw {
    None,
    Expired,
    WrongName,
}

pub fn make_pki(flaw: CertFlaw) -> Pki {
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params
        .distinguished_name
        .push(DnType::CommonName, "Test CA");
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    let ca_cert = ca_params.self_signed(&ca_key).unwrap();
    let issuer = Issuer::new(ca_params, ca_key);

    let name = if flaw == CertFlaw::WrongName {
        "other.example"
    } else {
        "localhost"
    };
    let leaf_key = KeyPair::generate().unwrap();
    let mut leaf = CertificateParams::new(vec![name.to_owned()]).unwrap();
    if flaw == CertFlaw::Expired {
        leaf.not_before = date_time_ymd(2000, 1, 1);
        leaf.not_after = date_time_ymd(2001, 1, 1);
    }
    let leaf_cert = leaf.signed_by(&leaf_key, &issuer).unwrap();

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![leaf_cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der())),
        )
        .unwrap();
    Pki {
        ca: ca_cert.der().clone(),
        acceptor: TlsAcceptor::from(Arc::new(cfg)),
    }
}

#[derive(Clone)]
pub struct Behavior {
    pub implicit_tls: bool,
    pub advertise_starttls: bool,
    pub inject_after_starttls: bool,
    pub password: Option<(&'static str, &'static str)>,
    pub token: Option<&'static str>,
    pub offer_xoauth2: bool,
    pub offer_plain: bool,
    pub extra_caps: Vec<&'static str>,
    pub stall_greeting: bool,
    pub bye_greeting: bool,
    pub drop_on_fetch: bool,
    /// Pads every full-message FETCH answer to this many bytes (a hostile or broken server).
    pub huge_body: Option<usize>,
    pub hang_on_select: bool,
    pub no_uidvalidity: bool,
    pub idle_push_after: Option<Duration>,
    /// If set, APPEND is refused with this tagged reply (e.g. `NO [OVERQUOTA] full`).
    pub append_reply: Option<&'static str>,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            implicit_tls: true,
            advertise_starttls: true,
            inject_after_starttls: false,
            password: Some(("alice", "s3cret")),
            token: Some("GOOD-TOKEN"),
            offer_xoauth2: true,
            offer_plain: true,
            extra_caps: vec![
                "IDLE",
                "CONDSTORE",
                "MOVE",
                "UIDPLUS",
                "SPECIAL-USE",
                "LIST-EXTENDED",
                "ID",
            ],
            stall_greeting: false,
            bye_greeting: false,
            drop_on_fetch: false,
            huge_body: None,
            hang_on_select: false,
            no_uidvalidity: false,
            idle_push_after: None,
            append_reply: None,
        }
    }
}

#[derive(Clone)]
struct Msg {
    uid: u32,
    flags: BTreeSet<String>,
    modseq: u64,
    header: String,
    body: String,
}

struct Shared {
    uid_validity: u32,
    next_uid: u32,
    appended: Vec<(String, String, Vec<u8>)>,
    msgs: Vec<Msg>,
    log: Vec<String>,
    connections: usize,
}

pub struct MockServer {
    pub port: u16,
    shared: Arc<Mutex<Shared>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockServer {
    pub fn log(&self) -> Vec<String> {
        self.shared.lock().unwrap().log.clone()
    }
    /// Messages stored with APPEND: (folder, flags, bytes).
    pub fn appended(&self) -> Vec<(String, String, Vec<u8>)> {
        self.shared.lock().unwrap().appended.clone()
    }
    /// Adds a message as if new mail arrived; returns its UID.
    pub fn add_message(&self, subject: &str, body: &str) -> u32 {
        let mut g = self.shared.lock().unwrap();
        let uid = g.next_uid;
        g.next_uid += 1;
        let modseq = g.msgs.iter().map(|m| m.modseq).max().unwrap_or(0) + 1;
        g.msgs.push(Msg {
            uid,
            flags: BTreeSet::new(),
            modseq,
            header: format!(
                "From: Bob <bob@example.com>\r\nTo: alice@example.com\r\nSubject: {subject}\r\n\
                 Message-ID: <m{uid}@example.com>\r\nDate: Wed, 18 Nov 2026 09:00:00 +0000\r\n\r\n"
            ),
            body: body.to_owned(),
        });
        uid
    }
    /// Removes a message as if another client had deleted it.
    pub fn remove_uid(&self, uid: u32) {
        self.shared.lock().unwrap().msgs.retain(|m| m.uid != uid);
    }
    /// Changes a flag as if another client had.
    pub fn set_flag(&self, uid: u32, flag: &str, on: bool) {
        let mut g = self.shared.lock().unwrap();
        let top = g.msgs.iter().map(|m| m.modseq).max().unwrap_or(0);
        if let Some(m) = g.msgs.iter_mut().find(|m| m.uid == uid) {
            if on {
                m.flags.insert(flag.to_owned());
            } else {
                m.flags.remove(flag);
            }
            m.modseq = top + 1;
        }
    }
    /// Simulates the server renumbering its mailbox: new UIDVALIDITY, new UIDs.
    pub fn renumber(&self, new_validity: u32) {
        let mut g = self.shared.lock().unwrap();
        g.uid_validity = new_validity;
        let mut next = 1;
        for m in &mut g.msgs {
            m.uid = next;
            next += 1;
        }
        g.next_uid = next;
    }
    pub fn uids(&self) -> Vec<u32> {
        self.shared
            .lock()
            .unwrap()
            .msgs
            .iter()
            .map(|m| m.uid)
            .collect()
    }
    pub fn flags_of(&self, uid: u32) -> Vec<String> {
        let g = self.shared.lock().unwrap();
        g.msgs
            .iter()
            .find(|m| m.uid == uid)
            .map(|m| m.flags.iter().cloned().collect())
            .unwrap_or_default()
    }
    pub fn connections(&self) -> usize {
        self.shared.lock().unwrap().connections
    }
}

fn initial_messages() -> Vec<Msg> {
    (0..3u32)
        .map(|i| {
            let uid = 101 + i;
            let mut flags = BTreeSet::new();
            if i == 0 {
                flags.insert("\\Seen".to_owned());
            }
            Msg {
                uid,
                flags,
                modseq: 10 + u64::from(i),
                header: format!(
                    "From: Bob <bob@example.com>\r\nTo: alice@example.com\r\nSubject: Hello {uid}\r\n\
                     Message-ID: <m{uid}@example.com>\r\nDate: Tue, 17 Nov 2026 10:00:00 +0000\r\n\r\n"
                ),
                body: format!("Body of message {uid}\r\n"),
            }
        })
        .collect()
}

pub async fn start(b: Behavior, pki: &Pki) -> MockServer {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Arc::new(Mutex::new(Shared {
        uid_validity: 7,
        next_uid: 104,
        appended: Vec::new(),
        msgs: initial_messages(),
        log: Vec::new(),
        connections: 0,
    }));
    let acceptor = pki.acceptor.clone();
    let sh = shared.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            sh.lock().unwrap().connections += 1;
            let (b, acceptor, sh) = (b.clone(), acceptor.clone(), sh.clone());
            tokio::spawn(async move {
                let _ = connection(tcp, b, acceptor, sh).await;
            });
        }
    });
    MockServer { port, shared, task }
}

type Res<T> = std::io::Result<T>;

async fn send<W: AsyncWrite + Unpin>(w: &mut W, text: &str) -> Res<()> {
    w.write_all(text.as_bytes()).await?;
    w.flush().await
}

async fn line<S: AsyncRead + Unpin>(r: &mut BufReader<S>) -> Res<Option<String>> {
    let mut s = String::new();
    if r.read_line(&mut s).await? == 0 {
        return Ok(None);
    }
    Ok(Some(s.trim_end_matches(['\r', '\n']).to_owned()))
}

fn caps_line(b: &Behavior, secure: bool) -> String {
    let mut c = String::from("* CAPABILITY IMAP4rev1");
    if secure {
        if b.offer_plain {
            c.push_str(" AUTH=PLAIN");
        }
        if b.offer_xoauth2 {
            c.push_str(" AUTH=XOAUTH2");
        }
        for x in &b.extra_caps {
            c.push(' ');
            c.push_str(x);
        }
    } else {
        if b.advertise_starttls {
            c.push_str(" STARTTLS");
        }
        c.push_str(" LOGINDISABLED");
    }
    c
}

async fn connection(
    tcp: TcpStream,
    b: Behavior,
    acceptor: TlsAcceptor,
    sh: Arc<Mutex<Shared>>,
) -> Res<()> {
    if b.implicit_tls {
        let tls = acceptor.accept(tcp).await?;
        let mut r = BufReader::new(tls);
        if !greet(&mut r, &b).await? {
            return Ok(());
        }
        return session(r, &b, &sh).await;
    }
    let mut r = BufReader::new(tcp);
    if !greet(&mut r, &b).await? {
        return Ok(());
    }
    loop {
        let Some(l) = line(&mut r).await? else {
            return Ok(());
        };
        sh.lock().unwrap().log.push(format!("PLAIN< {l}"));
        let (tag, cmd) = l.split_once(' ').unwrap_or((&l, ""));
        match cmd.to_ascii_uppercase().as_str() {
            "CAPABILITY" => {
                send(
                    r.get_mut(),
                    &format!("{}\r\n{tag} OK done\r\n", caps_line(&b, false)),
                )
                .await?
            }
            "STARTTLS" if b.advertise_starttls => {
                let extra = if b.inject_after_starttls {
                    "* OK injected\r\n"
                } else {
                    ""
                };
                send(r.get_mut(), &format!("{tag} OK begin TLS\r\n{extra}")).await?;
                let tcp = r.into_inner();
                let tls = acceptor.accept(tcp).await?;
                return session(BufReader::new(tls), &b, &sh).await;
            }
            "LOGOUT" => {
                send(r.get_mut(), &format!("* BYE\r\n{tag} OK bye\r\n")).await?;
                return Ok(());
            }
            _ => {
                send(
                    r.get_mut(),
                    &format!("{tag} NO [PRIVACYREQUIRED] encryption first\r\n"),
                )
                .await?
            }
        }
    }
}

/// Sends the greeting; `false` means the connection should just end.
async fn greet<S: AsyncRead + AsyncWrite + Unpin>(r: &mut BufReader<S>, b: &Behavior) -> Res<bool> {
    if b.stall_greeting {
        tokio::time::sleep(Duration::from_secs(30)).await;
        return Ok(false);
    }
    if b.bye_greeting {
        send(r.get_mut(), "* BYE too many connections\r\n").await?;
        return Ok(false);
    }
    send(r.get_mut(), "* OK mock IMAP ready\r\n").await?;
    Ok(true)
}

fn quoted(args: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut inq = false;
    let mut esc = false;
    for ch in args.chars() {
        match (inq, esc, ch) {
            (true, true, c) => {
                cur.push(c);
                esc = false;
            }
            (true, false, '\\') => esc = true,
            (true, false, '"') => {
                inq = false;
                out.push(std::mem::take(&mut cur));
            }
            (true, false, c) => cur.push(c),
            (false, _, '"') => inq = true,
            _ => {}
        }
    }
    out
}

fn parse_uid_set(set: &str, msgs: &[Msg]) -> Vec<u32> {
    let max = msgs.iter().map(|m| m.uid).max().unwrap_or(0);
    let mut out = Vec::new();
    for part in set.split(',') {
        let (a, b) = part.split_once(':').unwrap_or((part, part));
        let f = |s: &str| {
            if s == "*" {
                max
            } else {
                s.parse().unwrap_or(0)
            }
        };
        let (lo, hi) = (f(a).min(f(b)), f(a).max(f(b)));
        out.extend(msgs.iter().map(|m| m.uid).filter(|u| *u >= lo && *u <= hi));
    }
    out
}

fn flags_text(m: &Msg) -> String {
    m.flags.iter().cloned().collect::<Vec<_>>().join(" ")
}

#[allow(clippy::too_many_lines)]
async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    mut r: BufReader<S>,
    b: &Behavior,
    sh: &Arc<Mutex<Shared>>,
) -> Res<()> {
    let mut authed = false;
    loop {
        let Some(l) = line(&mut r).await? else {
            return Ok(());
        };
        let log_line = if l.to_ascii_uppercase().contains(" LOGIN ") {
            "LOGIN <hidden>".to_owned()
        } else {
            l.clone()
        };
        sh.lock().unwrap().log.push(format!("TLS< {log_line}"));
        let (tag, rest) = l.split_once(' ').unwrap_or((&l, ""));
        let (cmd, args) = rest.split_once(' ').unwrap_or((rest, ""));
        let cmd_u = cmd.to_ascii_uppercase();
        let w = r.get_mut();

        match cmd_u.as_str() {
            "CAPABILITY" => {
                send(w, &format!("{}\r\n{tag} OK done\r\n", caps_line(b, true))).await?
            }
            "AUTHENTICATE" => {
                let mech = args.split(' ').next().unwrap_or("").to_ascii_uppercase();
                send(w, "+ \r\n").await?;
                let Some(resp) = line(&mut r).await? else {
                    return Ok(());
                };
                let blob = STANDARD.decode(resp.trim()).unwrap_or_default();
                let ok = match mech.as_str() {
                    "PLAIN" if b.offer_plain => {
                        let parts: Vec<&[u8]> = blob.split(|c| *c == 0).collect();
                        parts.len() == 3
                            && b.password.is_some_and(|(u, p)| {
                                parts[1] == u.as_bytes() && parts[2] == p.as_bytes()
                            })
                    }
                    "XOAUTH2" if b.offer_xoauth2 => {
                        let text = String::from_utf8_lossy(&blob).into_owned();
                        let token = text
                            .split("\u{1}auth=Bearer ")
                            .nth(1)
                            .map(|t| t.trim_end_matches('\u{1}').to_owned());
                        sh.lock().unwrap().log.push(format!(
                            "XOAUTH2 token={}",
                            token.clone().unwrap_or_default()
                        ));
                        text.starts_with("user=") && token.as_deref() == b.token
                    }
                    _ => false,
                };
                let w = r.get_mut();
                if ok {
                    authed = true;
                    send(w, &format!("{tag} OK authenticated\r\n")).await?;
                } else if mech == "XOAUTH2" {
                    let json =
                        STANDARD.encode(r#"{"status":"401","schemes":"Bearer","scope":"x"}"#);
                    send(w, &format!("+ {json}\r\n")).await?;
                    let ack = line(&mut r).await?;
                    sh.lock().unwrap().log.push(format!(
                        "XOAUTH2-ERROR-ACK empty={}",
                        ack.as_deref() == Some("")
                    ));
                    send(
                        r.get_mut(),
                        &format!("{tag} NO [AUTHENTICATIONFAILED] invalid token\r\n"),
                    )
                    .await?;
                } else {
                    send(
                        w,
                        &format!("{tag} NO [AUTHENTICATIONFAILED] invalid credentials\r\n"),
                    )
                    .await?;
                }
            }
            _ if !authed => send(w, &format!("{tag} NO not authenticated\r\n")).await?,
            "ID" => send(w, &format!("* ID NIL\r\n{tag} OK done\r\n")).await?,
            "NOOP" => send(w, &format!("{tag} OK noop\r\n")).await?,
            "LIST" => {
                let mut out = String::new();
                for (attrs, name) in [
                    ("\\HasNoChildren", "INBOX"),
                    ("\\HasNoChildren \\Sent", "Sent"),
                    ("\\HasNoChildren \\Drafts", "Entw&APw-rfe"),
                    ("\\Noselect \\HasChildren", "[Gmail]"),
                    ("\\NonExistent", "Ghost"),
                ] {
                    out.push_str(&format!("* LIST ({attrs}) \"/\" \"{name}\"\r\n"));
                }
                send(w, &format!("{out}{tag} OK LIST done\r\n")).await?;
            }
            "SELECT" => {
                if b.hang_on_select {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    return Ok(());
                }
                let name = quoted(args).into_iter().next().unwrap_or_default();
                if name != "INBOX" && name != "Sent" {
                    send(w, &format!("{tag} NO no such mailbox\r\n")).await?;
                    continue;
                }
                let (n, modseq, validity_no, uidnext) = {
                    let g = sh.lock().unwrap();
                    (
                        g.msgs.len(),
                        g.msgs.iter().map(|m| m.modseq).max().unwrap_or(1),
                        g.uid_validity,
                        g.next_uid,
                    )
                };
                let validity = if b.no_uidvalidity {
                    String::new()
                } else {
                    format!("* OK [UIDVALIDITY {validity_no}] ok\r\n")
                };
                let ms = if b.extra_caps.contains(&"CONDSTORE") {
                    format!("* OK [HIGHESTMODSEQ {modseq}] ok\r\n")
                } else {
                    String::new()
                };
                send(
                    w,
                    &format!(
                        "* {n} EXISTS\r\n* 0 RECENT\r\n{validity}* OK [UIDNEXT {uidnext}] ok\r\n\
                         * FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)\r\n{ms}{tag} OK [READ-WRITE] SELECT done\r\n"
                    ),
                )
                .await?;
            }
            "APPEND" => {
                if let Some(reply) = b.append_reply {
                    send(w, &format!("{tag} {reply}\r\n")).await?;
                    continue;
                }
                let n: usize = args
                    .rsplit_once('{')
                    .and_then(|(_, t)| t.trim_end_matches('}').parse().ok())
                    .unwrap_or(0);
                let folder = quoted(args).into_iter().next().unwrap_or_default();
                let flags = args
                    .split_once('(')
                    .and_then(|(_, t)| t.split_once(')'))
                    .map(|(f, _)| f.to_owned())
                    .unwrap_or_default();
                send(w, "+ ready\r\n").await?;
                let mut body = vec![0u8; n];
                r.read_exact(&mut body).await?;
                let mut crlf = [0u8; 2];
                r.read_exact(&mut crlf).await?;
                {
                    let mut g = sh.lock().unwrap();
                    let uid = g.msgs.iter().map(|m| m.uid).max().unwrap_or(100) + 1;
                    g.msgs.push(Msg {
                        uid,
                        flags: BTreeSet::new(),
                        modseq: 99,
                        header: String::from_utf8_lossy(&body).into_owned(),
                        body: String::new(),
                    });
                    g.appended.push((folder, flags, body));
                }
                send(r.get_mut(), &format!("{tag} OK APPEND completed\r\n")).await?;
            }
            "UID" => {
                let (sub, rest) = args.split_once(' ').unwrap_or((args, ""));
                let (set, tail) = rest.split_once(' ').unwrap_or((rest, ""));
                match sub.to_ascii_uppercase().as_str() {
                    "SEARCH" => {
                        // Only `ALL` and `HEADER Message-ID "<value>"` are understood.
                        let wanted = if set.eq_ignore_ascii_case("HEADER") {
                            quoted(tail).into_iter().next()
                        } else {
                            None
                        };
                        let uids: Vec<String> = sh
                            .lock()
                            .unwrap()
                            .msgs
                            .iter()
                            .filter(|m| {
                                wanted
                                    .as_ref()
                                    .is_none_or(|w| m.header.contains(w.as_str()))
                            })
                            .map(|m| m.uid.to_string())
                            .collect();
                        send(
                            w,
                            &format!("* SEARCH {}\r\n{tag} OK done\r\n", uids.join(" ")),
                        )
                        .await?;
                    }
                    "FETCH" => {
                        if b.drop_on_fetch {
                            return Ok(());
                        }
                        let since = tail
                            .split("CHANGEDSINCE ")
                            .nth(1)
                            .and_then(|s| s.trim_end_matches(')').trim().parse::<u64>().ok());
                        let want_header = tail.to_ascii_uppercase().contains("HEADER");
                        let want_full = !want_header
                            && (tail.contains("BODY.PEEK[]") || tail.contains("BODY[]"));
                        let msgs = sh.lock().unwrap().msgs.clone();
                        let mut out = String::new();
                        for (i, m) in msgs.iter().enumerate() {
                            if !parse_uid_set(set, &msgs).contains(&m.uid)
                                || since.is_some_and(|s| m.modseq <= s)
                            {
                                continue;
                            }
                            let seq = i + 1;
                            if want_full {
                                let mut full = format!("{}{}", m.header, m.body);
                                if let Some(n) = b.huge_body {
                                    full.push_str(&"x".repeat(n));
                                }
                                out.push_str(&format!(
                                    "* {seq} FETCH (UID {} FLAGS ({}) BODY[] {{{}}}\r\n{})\r\n",
                                    m.uid,
                                    flags_text(m),
                                    full.len(),
                                    full
                                ));
                            } else if want_header {
                                out.push_str(&format!(
                                    "* {seq} FETCH (UID {} FLAGS ({}) RFC822.SIZE {} INTERNALDATE \"17-Nov-2026 10:00:00 +0000\" BODY[HEADER] {{{}}}\r\n{})\r\n",
                                    m.uid, flags_text(m), m.header.len() + 500, m.header.len(), m.header
                                ));
                            } else {
                                out.push_str(&format!(
                                    "* {seq} FETCH (UID {} FLAGS ({}) MODSEQ ({}))\r\n",
                                    m.uid,
                                    flags_text(m),
                                    m.modseq
                                ));
                            }
                        }
                        send(w, &format!("{out}{tag} OK FETCH done\r\n")).await?;
                    }
                    "STORE" => {
                        let (op, flag) = {
                            let mut it = tail.split(' ');
                            (
                                it.next().unwrap_or("").to_owned(),
                                it.next().unwrap_or("").trim_matches(['(', ')']).to_owned(),
                            )
                        };
                        {
                            let mut g = sh.lock().unwrap();
                            let uids = parse_uid_set(set, &g.msgs);
                            let top = g.msgs.iter().map(|m| m.modseq).max().unwrap_or(0);
                            for m in g.msgs.iter_mut().filter(|m| uids.contains(&m.uid)) {
                                if op.starts_with('+') {
                                    m.flags.insert(flag.clone());
                                } else {
                                    m.flags.remove(&flag);
                                }
                                m.modseq = top + 1;
                            }
                        }
                        send(w, &format!("{tag} OK STORE done\r\n")).await?;
                    }
                    "MOVE" | "COPY" => {
                        {
                            let mut g = sh.lock().unwrap();
                            let uids = parse_uid_set(set, &g.msgs);
                            if sub.eq_ignore_ascii_case("MOVE") {
                                g.msgs.retain(|m| !uids.contains(&m.uid));
                            }
                        }
                        send(w, &format!("{tag} OK done\r\n")).await?;
                    }
                    "EXPUNGE" => {
                        {
                            let mut g = sh.lock().unwrap();
                            let uids = parse_uid_set(set, &g.msgs);
                            g.msgs.retain(|m| {
                                !(uids.contains(&m.uid) && m.flags.contains("\\Deleted"))
                            });
                        }
                        send(w, &format!("{tag} OK expunged\r\n")).await?;
                    }
                    _ => send(w, &format!("{tag} BAD unknown UID command\r\n")).await?,
                }
            }
            "IDLE" => {
                send(w, "+ idling\r\n").await?;
                let push = b.idle_push_after;
                let waited = async {
                    if let Some(d) = push {
                        tokio::time::sleep(d).await;
                        send(r.get_mut(), "* 4 EXISTS\r\n").await?;
                    }
                    line(&mut r).await
                };
                // Wait for DONE (pushing first if configured).
                let done = waited.await?;
                if done.as_deref().map(str::to_ascii_uppercase).as_deref() != Some("DONE") {
                    // A push happened; the client's DONE is still to come.
                    let _ = line(&mut r).await?;
                }
                send(r.get_mut(), &format!("{tag} OK IDLE terminated\r\n")).await?;
            }
            "LOGOUT" => {
                send(w, &format!("* BYE\r\n{tag} OK bye\r\n")).await?;
                return Ok(());
            }
            _ => send(w, &format!("{tag} BAD unknown command\r\n")).await?,
        }
    }
}
