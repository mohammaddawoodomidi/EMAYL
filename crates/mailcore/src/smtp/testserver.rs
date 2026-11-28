//! In-process SMTP submission server for tests, with switches to misbehave.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

use crate::imap::testserver::Pki;

#[derive(Clone)]
pub struct Behavior {
    pub implicit_tls: bool,
    pub advertise_starttls: bool,
    pub inject_after_starttls: bool,
    pub size_limit: Option<u64>,
    pub auth_mechs: Vec<&'static str>,
    pub password: (&'static str, &'static str),
    pub token: &'static str,
    /// (address, code, text) for RCPT TO refusals.
    pub reject_rcpt: Vec<(&'static str, u16, &'static str)>,
    pub reject_mail_from: Option<(u16, &'static str)>,
    pub reject_data_start: Option<(u16, &'static str)>,
    pub data_reply: (u16, &'static str),
    pub stall_greeting: bool,
    pub greeting_421: bool,
    pub drop_after_data: bool,
}

impl Default for Behavior {
    fn default() -> Self {
        Self {
            implicit_tls: true,
            advertise_starttls: true,
            inject_after_starttls: false,
            size_limit: Some(10_000_000),
            auth_mechs: vec!["PLAIN", "LOGIN", "XOAUTH2"],
            password: ("alice", "s3cret"),
            token: "GOOD-TOKEN",
            reject_rcpt: Vec::new(),
            reject_mail_from: None,
            reject_data_start: None,
            data_reply: (250, "2.0.0 OK queued as ABC123"),
            stall_greeting: false,
            greeting_421: false,
            drop_after_data: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Received {
    pub from: String,
    pub rcpts: Vec<String>,
    pub data: Vec<u8>,
}

#[derive(Default)]
struct Shared {
    log: Vec<String>,
    received: Vec<Received>,
}

pub struct MockSmtp {
    pub port: u16,
    shared: Arc<Mutex<Shared>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for MockSmtp {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl MockSmtp {
    pub fn log(&self) -> Vec<String> {
        self.shared.lock().unwrap().log.clone()
    }
    pub fn received(&self) -> Vec<Received> {
        self.shared.lock().unwrap().received.clone()
    }
}

pub async fn start(b: Behavior, pki: &Pki) -> MockSmtp {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let shared = Arc::new(Mutex::new(Shared::default()));
    let (acceptor, sh) = (pki.acceptor(), shared.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok((tcp, _)) = listener.accept().await else {
                return;
            };
            let (b, acceptor, sh) = (b.clone(), acceptor.clone(), sh.clone());
            tokio::spawn(async move {
                let _ = connection(tcp, b, acceptor, sh).await;
            });
        }
    });
    MockSmtp { port, shared, task }
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

fn ehlo_reply(b: &Behavior, secure: bool) -> String {
    let mut lines = vec!["localhost hello".to_owned()];
    if secure {
        if let Some(s) = b.size_limit {
            lines.push(format!("SIZE {s}"));
        }
        lines.push("ENHANCEDSTATUSCODES".into());
        if !b.auth_mechs.is_empty() {
            lines.push(format!("AUTH {}", b.auth_mechs.join(" ")));
        }
    } else if b.advertise_starttls {
        lines.push("STARTTLS".into());
    }
    let n = lines.len();
    lines
        .iter()
        .enumerate()
        .map(|(i, l)| format!("250{}{l}\r\n", if i + 1 == n { ' ' } else { '-' }))
        .collect()
}

async fn greet<S: AsyncRead + AsyncWrite + Unpin>(r: &mut BufReader<S>, b: &Behavior) -> Res<bool> {
    if b.stall_greeting {
        tokio::time::sleep(Duration::from_secs(30)).await;
        return Ok(false);
    }
    if b.greeting_421 {
        send(r.get_mut(), "421 4.3.2 service shutting down\r\n").await?;
        return Ok(false);
    }
    send(r.get_mut(), "220 localhost ESMTP mock\r\n").await?;
    Ok(true)
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
        let upper = l.to_ascii_uppercase();
        if upper.starts_with("EHLO") {
            send(r.get_mut(), &ehlo_reply(&b, false)).await?;
        } else if upper == "STARTTLS" && b.advertise_starttls {
            let extra = if b.inject_after_starttls {
                "250 injected\r\n"
            } else {
                ""
            };
            send(
                r.get_mut(),
                &format!("220 2.0.0 ready to start TLS\r\n{extra}"),
            )
            .await?;
            let tls = acceptor.accept(r.into_inner()).await?;
            return session(BufReader::new(tls), &b, &sh).await;
        } else if upper == "QUIT" {
            send(r.get_mut(), "221 bye\r\n").await?;
            return Ok(());
        } else {
            send(
                r.get_mut(),
                "530 5.7.0 Must issue a STARTTLS command first\r\n",
            )
            .await?;
        }
    }
}

fn addr_in(args: &str) -> String {
    let start = args.find('<').map_or(0, |i| i + 1);
    let end = args.find('>').unwrap_or(args.len());
    args[start..end].to_owned()
}

async fn session<S: AsyncRead + AsyncWrite + Unpin>(
    mut r: BufReader<S>,
    b: &Behavior,
    sh: &Arc<Mutex<Shared>>,
) -> Res<()> {
    let mut authed = false;
    let mut from = String::new();
    let mut rcpts: Vec<String> = Vec::new();
    loop {
        let Some(l) = line(&mut r).await? else {
            return Ok(());
        };
        let shown = if l.to_ascii_uppercase().starts_with("AUTH") {
            l.split(' ').take(2).collect::<Vec<_>>().join(" ")
        } else {
            l.clone()
        };
        sh.lock().unwrap().log.push(format!("TLS< {shown}"));
        let (cmd, args) = l.split_once(' ').unwrap_or((&l, ""));
        match cmd.to_ascii_uppercase().as_str() {
            "EHLO" | "HELO" => send(r.get_mut(), &ehlo_reply(b, true)).await?,
            "AUTH" => {
                let (mech, initial) = args.split_once(' ').unwrap_or((args, ""));
                let mech = mech.to_ascii_uppercase();
                if !b.auth_mechs.iter().any(|m| m.eq_ignore_ascii_case(&mech)) {
                    send(r.get_mut(), "504 5.5.4 mechanism not supported\r\n").await?;
                    continue;
                }
                let ok = match mech.as_str() {
                    "PLAIN" => {
                        let blob = STANDARD.decode(initial).unwrap_or_default();
                        let parts: Vec<&[u8]> = blob.split(|c| *c == 0).collect();
                        parts.len() == 3
                            && parts[1] == b.password.0.as_bytes()
                            && parts[2] == b.password.1.as_bytes()
                    }
                    "LOGIN" => {
                        send(r.get_mut(), "334 VXNlcm5hbWU6\r\n").await?;
                        let u = line(&mut r).await?.unwrap_or_default();
                        send(r.get_mut(), "334 UGFzc3dvcmQ6\r\n").await?;
                        let p = line(&mut r).await?.unwrap_or_default();
                        STANDARD.decode(u).unwrap_or_default() == b.password.0.as_bytes()
                            && STANDARD.decode(p).unwrap_or_default() == b.password.1.as_bytes()
                    }
                    "XOAUTH2" => {
                        let text =
                            String::from_utf8_lossy(&STANDARD.decode(initial).unwrap_or_default())
                                .into_owned();
                        let token = text
                            .split("\u{1}auth=Bearer ")
                            .nth(1)
                            .unwrap_or("")
                            .trim_end_matches('\u{1}')
                            .to_owned();
                        sh.lock()
                            .unwrap()
                            .log
                            .push(format!("XOAUTH2 token={token}"));
                        text.starts_with("user=") && token == b.token
                    }
                    _ => false,
                };
                if ok {
                    authed = true;
                    send(r.get_mut(), "235 2.7.0 Authentication successful\r\n").await?;
                } else if mech == "XOAUTH2" {
                    let json = STANDARD.encode(r#"{"status":"401","schemes":"Bearer"}"#);
                    send(r.get_mut(), &format!("334 {json}\r\n")).await?;
                    let ack = line(&mut r).await?;
                    sh.lock().unwrap().log.push(format!(
                        "XOAUTH2-ERROR-ACK empty={}",
                        ack.as_deref() == Some("")
                    ));
                    send(r.get_mut(), "535 5.7.8 invalid token\r\n").await?;
                } else {
                    send(
                        r.get_mut(),
                        "535 5.7.8 Authentication credentials invalid\r\n",
                    )
                    .await?;
                }
            }
            _ if !authed && !cmd.eq_ignore_ascii_case("QUIT") => {
                send(r.get_mut(), "530 5.7.0 Authentication required\r\n").await?;
            }
            "MAIL" => {
                if let Some((c, t)) = b.reject_mail_from {
                    send(r.get_mut(), &format!("{c} {t}\r\n")).await?;
                    continue;
                }
                let declared: u64 = args
                    .to_ascii_uppercase()
                    .split("SIZE=")
                    .nth(1)
                    .and_then(|s| s.split_whitespace().next()?.parse().ok())
                    .unwrap_or(0);
                if b.size_limit.is_some_and(|lim| declared > lim) {
                    send(r.get_mut(), "552 5.3.4 message size exceeds limit\r\n").await?;
                    continue;
                }
                from = addr_in(args);
                rcpts.clear();
                send(r.get_mut(), "250 2.1.0 sender ok\r\n").await?;
            }
            "RCPT" => {
                let a = addr_in(args);
                if let Some((_, c, t)) = b
                    .reject_rcpt
                    .iter()
                    .find(|(x, _, _)| x.eq_ignore_ascii_case(&a))
                {
                    send(r.get_mut(), &format!("{c} {t}\r\n")).await?;
                } else {
                    rcpts.push(a);
                    send(r.get_mut(), "250 2.1.5 recipient ok\r\n").await?;
                }
            }
            "DATA" => {
                if let Some((c, t)) = b.reject_data_start {
                    send(r.get_mut(), &format!("{c} {t}\r\n")).await?;
                    continue;
                }
                send(r.get_mut(), "354 end data with <CR><LF>.<CR><LF>\r\n").await?;
                let mut data = Vec::new();
                loop {
                    let Some(l) = line(&mut r).await? else {
                        return Ok(());
                    };
                    if l == "." {
                        break;
                    }
                    let l = l.strip_prefix('.').map_or(l.clone(), str::to_owned);
                    data.extend_from_slice(l.as_bytes());
                    data.extend_from_slice(b"\r\n");
                }
                sh.lock().unwrap().received.push(Received {
                    from: from.clone(),
                    rcpts: std::mem::take(&mut rcpts),
                    data,
                });
                if b.drop_after_data {
                    return Ok(());
                }
                send(
                    r.get_mut(),
                    &format!("{} {}\r\n", b.data_reply.0, b.data_reply.1),
                )
                .await?;
            }
            "RSET" => {
                rcpts.clear();
                send(r.get_mut(), "250 2.0.0 reset\r\n").await?;
            }
            "NOOP" => send(r.get_mut(), "250 2.0.0 ok\r\n").await?,
            "QUIT" => {
                send(r.get_mut(), "221 2.0.0 bye\r\n").await?;
                return Ok(());
            }
            _ => send(r.get_mut(), "502 5.5.2 command not recognized\r\n").await?,
        }
    }
}
