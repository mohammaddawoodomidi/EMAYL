//! End-to-end tests of the IMAP client against the in-process mock server.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use secrecy::SecretString;

use super::testserver::{Behavior, CertFlaw, MockServer, Pki, make_pki, start};
use super::*;
use crate::account::{Account, AccountId, AuthMethod, OAuthProvider, Security, ServerConfig};
use crate::email::EmailAddress;
use crate::net::{
    ConnError, FailureKind, Stage, Timeouts, client_config, client_config_with_extra_roots,
};
use crate::oauth::{
    HttpReply, OAuthClient, OAuthClients, OAuthError, TokenManager, TokenSet, TokenTransport,
};
use crate::secrets::{MemorySecretStore, SecretKind, SecretStore};

const ARRIVAL: i64 = 1_794_909_600; // 2026-11-17 10:00:00 UTC

fn cfg(port: u16, security: Security) -> ServerConfig {
    ServerConfig {
        host: "localhost".into(),
        port,
        security,
    }
}

fn timeouts() -> Timeouts {
    Timeouts {
        dns: Duration::from_secs(5),
        connect: Duration::from_secs(5),
        tls: Duration::from_secs(5),
        greeting: Duration::from_secs(5),
        command: Duration::from_secs(5),
    }
}

fn pw(s: &str) -> SecretString {
    SecretString::from(s.to_owned())
}

async fn open_with(
    srv: &MockServer,
    pki: &Pki,
    security: Security,
    creds: &Credentials<'_>,
    t: Timeouts,
) -> Result<ImapSession, ConnError> {
    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();
    let server = cfg(srv.port, security);
    connect(
        &ConnectParams {
            server: &server,
            username: "alice",
            timeouts: t,
        },
        creds,
        &tls,
    )
    .await
}

async fn open(srv: &MockServer, pki: &Pki) -> Result<ImapSession, ConnError> {
    let p = pw("s3cret");
    open_with(
        srv,
        pki,
        Security::Tls,
        &Credentials::Password(&p),
        timeouts(),
    )
    .await
}

async fn ready(b: Behavior) -> (MockServer, Pki, ImapSession) {
    let pki = make_pki(CertFlaw::None);
    let srv = start(b, &pki).await;
    let mut s = open(&srv, &pki).await.unwrap();
    s.select("INBOX").await.unwrap();
    (srv, pki, s)
}

// ---- happy path -----------------------------------------------------------

#[tokio::test]
async fn full_session_over_implicit_tls() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(Behavior::default(), &pki).await;
    let mut s = open(&srv, &pki).await.unwrap();

    let f = s.features();
    assert!(f.idle && f.condstore && f.move_cmd && f.uidplus && f.special_use && f.list_extended);

    let folders = s.list_folders().await.unwrap();
    let names: Vec<(&str, FolderRole, bool)> = folders
        .iter()
        .map(|f| (f.display_name.as_str(), f.role, f.selectable))
        .collect();
    assert_eq!(
        names,
        vec![
            ("INBOX", FolderRole::Inbox, true),
            ("Entwürfe", FolderRole::Drafts, true),
            ("Sent", FolderRole::Sent, true),
            ("[Gmail]", FolderRole::Other, false),
        ]
    );
    assert_eq!(folders[1].raw_name, "Entw&APw-rfe");

    let st = s.select("INBOX").await.unwrap();
    assert_eq!((st.uid_validity, st.uid_next, st.exists), (7, 104, 3));
    assert_eq!(st.highest_modseq, Some(12));

    let msgs = s.fetch_headers("101:103").await.unwrap();
    assert_eq!(msgs.len(), 3);
    let m = msgs.iter().find(|m| m.uid == 101).unwrap();
    assert!(m.flags.seen && !m.flags.flagged);
    assert!(String::from_utf8_lossy(&m.header).contains("Subject: Hello 101"));
    assert!(m.size.is_some_and(|n| n > 0));
    assert_eq!(m.internal_date, Some(ARRIVAL));
    assert!(!m.header_truncated);

    assert_eq!(s.uid_search_all().await.unwrap(), vec![101, 102, 103]);

    s.change_flag("102", "\\Seen", FlagChange::Add)
        .await
        .unwrap();
    let changed = s.fetch_flags(Some(12)).await.unwrap();
    assert_eq!(changed.len(), 1, "only the message changed after modseq 12");
    assert_eq!(changed[0].uid, 102);
    assert!(changed[0].flags.seen);
    assert_eq!(s.fetch_flags(None).await.unwrap().len(), 3);

    s.noop().await.unwrap();
    s.logout().await;
}

#[tokio::test]
async fn select_of_unknown_folder_is_a_clean_error() {
    let (_srv, _pki, mut s) = ready(Behavior::default()).await;
    let e = s.select("Nope").await.unwrap_err();
    assert_eq!(e.kind, FailureKind::Protocol);
}

#[tokio::test]
async fn malicious_uid_sets_are_refused_before_sending() {
    let (srv, _pki, mut s) = ready(Behavior::default()).await;
    let before = srv.log().len();
    assert!(s.fetch_headers("1\r\nA1 LOGOUT").await.is_err());
    assert!(
        s.change_flag("1:3", "\\Seen) (\\Deleted", FlagChange::Add)
            .await
            .is_err()
    );
    assert!(s.move_to("1 2", "Sent").await.is_err());
    assert_eq!(srv.log().len(), before, "nothing may reach the wire");
}

// ---- authentication -------------------------------------------------------

#[tokio::test]
async fn wrong_password_is_a_permanent_auth_failure() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(Behavior::default(), &pki).await;
    let bad = pw("nope");
    let e = open_with(
        &srv,
        &pki,
        Security::Tls,
        &Credentials::Password(&bad),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!((e.stage, e.kind), (Stage::Login, FailureKind::AuthRejected));
    assert!(!e.is_transient());
    assert!(e.advice().contains("app password"));
}

#[tokio::test]
async fn xoauth2_success_and_rejected_token_follows_the_error_protocol() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(Behavior::default(), &pki).await;

    let good = pw("GOOD-TOKEN");
    let s = open_with(
        &srv,
        &pki,
        Security::Tls,
        &Credentials::AccessToken(&good),
        timeouts(),
    )
    .await;
    assert!(s.is_ok());

    let stale = pw("STALE");
    let e = open_with(
        &srv,
        &pki,
        Security::Tls,
        &Credentials::AccessToken(&stale),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::AuthTokenRejected);
    let log = srv.log().join("\n");
    assert!(
        log.contains("XOAUTH2-ERROR-ACK empty=true"),
        "client must answer the error challenge with an empty line:\n{log}"
    );
}

#[tokio::test]
async fn xoauth2_needs_server_support() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            offer_xoauth2: false,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let t = pw("GOOD-TOKEN");
    let e = open_with(
        &srv,
        &pki,
        Security::Tls,
        &Credentials::AccessToken(&t),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::AuthMethodUnsupported);
}

#[tokio::test]
async fn password_login_falls_back_to_login_command_when_plain_is_absent() {
    // Inside TLS with no AUTH=PLAIN and no LOGINDISABLED the client uses LOGIN.
    // The mock does not implement LOGIN, so sign-in fails; the log proves the
    // fallback was attempted and the failure is reported as a login problem.
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            offer_plain: false,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let p = pw("s3cret");
    let e = open_with(
        &srv,
        &pki,
        Security::Tls,
        &Credentials::Password(&p),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.stage, Stage::Login);
    assert!(srv.log().iter().any(|l| l.contains("LOGIN <hidden>")));
}

// ---- STARTTLS --------------------------------------------------------------

#[tokio::test]
async fn starttls_upgrades_before_any_credentials() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            implicit_tls: false,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let p = pw("s3cret");
    let mut s = open_with(
        &srv,
        &pki,
        Security::StartTls,
        &Credentials::Password(&p),
        timeouts(),
    )
    .await
    .unwrap();
    s.select("INBOX").await.unwrap();
    let log = srv.log();
    let starttls = log.iter().position(|l| l.contains("STARTTLS")).unwrap();
    let auth = log.iter().position(|l| l.contains("AUTHENTICATE")).unwrap();
    assert!(starttls < auth);
    assert!(log[..starttls].iter().all(|l| l.starts_with("PLAIN<")));
    assert!(
        log[auth..]
            .iter()
            .all(|l| l.starts_with("TLS<") || l.starts_with("XOAUTH2"))
    );
}

#[tokio::test]
async fn missing_starttls_fails_closed_and_sends_no_credentials() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            implicit_tls: false,
            advertise_starttls: false,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let p = pw("s3cret");
    let e = open_with(
        &srv,
        &pki,
        Security::StartTls,
        &Credentials::Password(&p),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::StartTlsUnsupported);
    assert!(!e.is_transient());
    let log = srv.log().join("\n");
    assert!(
        !log.contains("AUTHENTICATE") && !log.contains("LOGIN"),
        "{log}"
    );
}

#[tokio::test]
async fn data_injected_after_starttls_is_rejected() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            implicit_tls: false,
            inject_after_starttls: true,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let p = pw("s3cret");
    let e = open_with(
        &srv,
        &pki,
        Security::StartTls,
        &Credentials::Password(&p),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(
        (e.stage, e.kind),
        (Stage::StartTls, FailureKind::Protocol),
        "{e:?}"
    );
}

// ---- certificates ----------------------------------------------------------

async fn cert_case(flaw: CertFlaw, trust_ca: bool) -> ConnError {
    let pki = make_pki(flaw);
    let srv = start(Behavior::default(), &pki).await;
    let tls = if trust_ca {
        client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap()
    } else {
        client_config().unwrap()
    };
    let server = cfg(srv.port, Security::Tls);
    let p = pw("s3cret");
    connect(
        &ConnectParams {
            server: &server,
            username: "alice",
            timeouts: timeouts(),
        },
        &Credentials::Password(&p),
        &tls,
    )
    .await
    .unwrap_err()
}

#[tokio::test]
async fn untrusted_certificate_is_refused() {
    let e = cert_case(CertFlaw::None, false).await;
    assert_eq!(
        (e.stage, e.kind),
        (Stage::Tls, FailureKind::CertificateUntrusted),
        "{e:?}"
    );
    assert!(!e.is_transient());
}

#[tokio::test]
async fn expired_certificate_is_refused() {
    let e = cert_case(CertFlaw::Expired, true).await;
    assert_eq!(
        (e.stage, e.kind),
        (Stage::Tls, FailureKind::CertificateExpired),
        "{e:?}"
    );
}

#[tokio::test]
async fn certificate_for_another_name_is_refused() {
    let e = cert_case(CertFlaw::WrongName, true).await;
    assert_eq!(
        (e.stage, e.kind),
        (Stage::Tls, FailureKind::CertificateNameMismatch),
        "{e:?}"
    );
}

// ---- hostile or broken servers --------------------------------------------

#[tokio::test]
async fn silent_server_times_out_and_is_transient() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            stall_greeting: true,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let mut t = timeouts();
    t.greeting = Duration::from_millis(300);
    let p = pw("s3cret");
    let e = open_with(&srv, &pki, Security::Tls, &Credentials::Password(&p), t)
        .await
        .unwrap_err();
    assert_eq!((e.stage, e.kind), (Stage::Greeting, FailureKind::Timeout));
    assert!(e.is_transient());
}

#[tokio::test]
async fn bye_greeting_is_reported_as_server_closed() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            bye_greeting: true,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let e = open(&srv, &pki).await.unwrap_err();
    assert_eq!(e.kind, FailureKind::ServerClosed);
    assert!(e.is_transient());
}

#[tokio::test]
async fn connection_dropped_mid_fetch_is_a_transient_error() {
    let (_srv, _pki, mut s) = ready(Behavior {
        drop_on_fetch: true,
        ..Behavior::default()
    })
    .await;
    let e = s.fetch_headers("101:103").await.unwrap_err();
    assert!(e.is_transient(), "{e:?}");
}

#[tokio::test]
async fn hung_select_hits_the_command_timeout() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            hang_on_select: true,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let mut t = timeouts();
    t.command = Duration::from_millis(400);
    let p = pw("s3cret");
    let mut s = open_with(&srv, &pki, Security::Tls, &Credentials::Password(&p), t)
        .await
        .unwrap();
    let e = s.select("INBOX").await.unwrap_err();
    assert_eq!(e.kind, FailureKind::Timeout);
}

#[tokio::test]
async fn folder_without_uidvalidity_is_refused_not_guessed() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            no_uidvalidity: true,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let mut s = open(&srv, &pki).await.unwrap();
    let e = s.select("INBOX").await.unwrap_err();
    assert!(e.detail.contains("UIDVALIDITY"), "{e:?}");
}

#[tokio::test]
async fn connection_refused_reports_the_connect_stage() {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let pki = make_pki(CertFlaw::None);
    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();
    let server = cfg(port, Security::Tls);
    let p = pw("x");
    let e = connect(
        &ConnectParams {
            server: &server,
            username: "alice",
            timeouts: timeouts(),
        },
        &Credentials::Password(&p),
        &tls,
    )
    .await
    .unwrap_err();
    assert_eq!(e.stage, Stage::Connect);
    assert!(e.is_transient());
}

// ---- IDLE -------------------------------------------------------------------

#[tokio::test]
async fn idle_reports_server_push_and_connection_stays_usable() {
    let (_srv, _pki, s) = ready(Behavior {
        idle_push_after: Some(Duration::from_millis(150)),
        ..Behavior::default()
    })
    .await;
    let (mut s, outcome) = s.idle(Duration::from_secs(10)).await.unwrap();
    assert_eq!(outcome, IdleOutcome::Changed);
    s.noop().await.unwrap();
}

#[tokio::test]
async fn idle_times_out_quietly_and_can_be_repeated() {
    let (_srv, _pki, s) = ready(Behavior::default()).await;
    let (s, outcome) = s.idle(Duration::from_secs(1)).await.unwrap();
    assert_eq!(outcome, IdleOutcome::Timeout);
    let (mut s, outcome) = s.idle(Duration::from_secs(1)).await.unwrap();
    assert_eq!(outcome, IdleOutcome::Timeout);
    s.noop().await.unwrap();
}

#[tokio::test]
async fn idle_is_refused_when_the_server_lacks_it() {
    let (_srv, _pki, s) = ready(Behavior {
        extra_caps: vec!["UIDPLUS"],
        ..Behavior::default()
    })
    .await;
    assert!(!s.features().idle);
    assert!(s.idle(Duration::from_secs(1)).await.is_err());
}

// ---- moving messages --------------------------------------------------------

#[tokio::test]
async fn move_uses_the_move_command_when_available() {
    let (_srv, _pki, mut s) = ready(Behavior::default()).await;
    assert_eq!(s.move_to("101", "Sent").await.unwrap(), MoveOutcome::Moved);
    assert_eq!(s.uid_search_all().await.unwrap(), vec![102, 103]);
}

#[tokio::test]
async fn move_falls_back_to_copy_flag_and_uid_expunge() {
    let (srv, _pki, mut s) = ready(Behavior {
        extra_caps: vec!["UIDPLUS"],
        ..Behavior::default()
    })
    .await;
    assert_eq!(s.move_to("101", "Sent").await.unwrap(), MoveOutcome::Moved);
    assert_eq!(s.uid_search_all().await.unwrap(), vec![102, 103]);
    let log = srv.log().join("\n");
    assert!(log.contains("UID COPY") && log.contains("UID EXPUNGE"));
}

#[tokio::test]
async fn move_without_move_or_uidplus_never_expunges_blindly() {
    let (srv, _pki, mut s) = ready(Behavior {
        extra_caps: vec![],
        ..Behavior::default()
    })
    .await;
    assert_eq!(
        s.move_to("101", "Sent").await.unwrap(),
        MoveOutcome::CopiedOriginalFlagged
    );
    assert_eq!(s.uid_search_all().await.unwrap(), vec![101, 102, 103]);
    assert!(!srv.log().join("\n").contains("EXPUNGE"));
    let flags = s.fetch_flags(None).await.unwrap();
    assert!(flags.iter().find(|f| f.uid == 101).unwrap().flags.deleted);
}

// ---- connecting an Account -------------------------------------------------

struct Refresher(AtomicUsize);

impl TokenTransport for Refresher {
    async fn post_form(&self, _url: &str, _body: String) -> Result<HttpReply, OAuthError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(HttpReply {
            status: 200,
            body: br#"{"access_token":"GOOD-TOKEN","expires_in":3600}"#.to_vec(),
        })
    }
}

fn account(port: u16, auth: AuthMethod) -> Account {
    Account {
        id: AccountId::generate(),
        display_name: "Alice".into(),
        email: EmailAddress::parse("alice@example.com").unwrap(),
        imap: cfg(port, Security::Tls),
        imap_username: "alice".into(),
        smtp: cfg(465, Security::Tls),
        smtp_username: "alice".into(),
        auth,
    }
}

fn google_clients() -> OAuthClients {
    OAuthClients {
        google: Some(OAuthClient {
            client_id: "cid".into(),
            client_secret: None,
            tenant: None,
        }),
        microsoft: None,
    }
}

#[tokio::test]
async fn stale_oauth_token_is_refreshed_once_and_retried() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(Behavior::default(), &pki).await;
    let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    let tokens = TokenManager::new(store.clone(), Refresher(AtomicUsize::new(0)));
    let acct = account(
        srv.port,
        AuthMethod::OAuth2 {
            provider: OAuthProvider::Google,
        },
    );
    tokens
        .save_initial(
            &acct.id,
            TokenSet {
                access_token: pw("STALE"),
                refresh_token: Some(pw("RT")),
                expires_in: Duration::from_secs(3600),
            },
        )
        .await
        .unwrap();

    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();
    let s = connect_account(&acct, &store, &tokens, &google_clients(), &tls, timeouts()).await;
    assert!(s.is_ok(), "{:?}", s.err());
    assert_eq!(srv.connections(), 2, "one rejected attempt, one retry");
}

#[tokio::test]
async fn oauth_account_without_app_registration_is_unsupported() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(Behavior::default(), &pki).await;
    let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    let tokens = TokenManager::new(store.clone(), Refresher(AtomicUsize::new(0)));
    let acct = account(
        srv.port,
        AuthMethod::OAuth2 {
            provider: OAuthProvider::Google,
        },
    );
    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();
    let e = connect_account(
        &acct,
        &store,
        &tokens,
        &OAuthClients::default(),
        &tls,
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::AuthMethodUnsupported);
    assert_eq!(srv.connections(), 0);
}

#[tokio::test]
async fn password_account_reads_the_vault_and_missing_secret_is_reported() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(Behavior::default(), &pki).await;
    let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    let tokens = TokenManager::new(store.clone(), Refresher(AtomicUsize::new(0)));
    let acct = account(srv.port, AuthMethod::Password);
    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();

    let e = connect_account(
        &acct,
        &store,
        &tokens,
        &OAuthClients::default(),
        &tls,
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::AuthRejected);
    assert_eq!(
        srv.connections(),
        0,
        "no connection without a saved password"
    );

    store
        .set(&acct.id, SecretKind::Password, &pw("s3cret"))
        .unwrap();
    assert!(
        connect_account(
            &acct,
            &store,
            &tokens,
            &OAuthClients::default(),
            &tls,
            timeouts()
        )
        .await
        .is_ok()
    );
}

#[tokio::test]
async fn a_server_that_sends_far_more_than_the_message_size_is_cut_off() {
    // The server announces a 4 MiB message although we only agreed to take 1 KiB.
    let (_srv, _pki, mut s) = ready(Behavior {
        huge_body: Some(4 * 1024 * 1024),
        ..Behavior::default()
    })
    .await;
    let e = s.fetch_message(101, 1024).await.unwrap_err();
    assert_eq!(e.kind, FailureKind::Protocol, "{e:?}");
    assert!(e.detail.contains("more data than allowed"), "{e:?}");
}

#[tokio::test]
async fn a_message_within_the_agreed_size_is_still_downloaded() {
    let (_srv, _pki, mut s) = ready(Behavior::default()).await;
    let got = s.fetch_message(101, 1024 * 1024).await.unwrap();
    assert!(got.is_some_and(|b| !b.is_empty()));
}
