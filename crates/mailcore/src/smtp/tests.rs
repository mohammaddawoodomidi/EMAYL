//! End-to-end SMTP client tests against the in-process mock server.

use std::time::Duration;

use secrecy::SecretString;

use super::session::{Phase, SendError, SmtpParams, SmtpSession, connect};
use super::testserver::{Behavior, MockSmtp, start};
use crate::account::{Security, ServerConfig};
use crate::compose::{Mailbox, OutgoingMessage, compose};
use crate::email::EmailAddress;
use crate::imap::testserver::{CertFlaw, Pki, make_pki};
use crate::net::{
    ConnError, Credentials, FailureKind, Stage, Timeouts, client_config_with_extra_roots,
};

fn timeouts() -> Timeouts {
    Timeouts {
        dns: Duration::from_secs(5),
        connect: Duration::from_secs(5),
        tls: Duration::from_secs(5),
        greeting: Duration::from_secs(5),
        command: Duration::from_secs(5),
    }
}

fn addr(s: &str) -> EmailAddress {
    EmailAddress::parse(s).unwrap()
}

fn pw(s: &str) -> SecretString {
    SecretString::from(s.to_owned())
}

async fn open_with(
    srv: &MockSmtp,
    pki: &Pki,
    security: Security,
    creds: &Credentials<'_>,
    t: Timeouts,
) -> Result<SmtpSession, ConnError> {
    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();
    let server = ServerConfig {
        host: "localhost".into(),
        port: srv.port,
        security,
    };
    connect(
        &SmtpParams {
            server: &server,
            username: "alice",
            ehlo_name: "",
            timeouts: t,
        },
        creds,
        &tls,
    )
    .await
}

async fn ready(b: Behavior) -> (MockSmtp, SmtpSession) {
    let pki = make_pki(CertFlaw::None);
    let srv = start(b, &pki).await;
    let p = pw("s3cret");
    let s = open_with(
        &srv,
        &pki,
        Security::Tls,
        &Credentials::Password(&p),
        timeouts(),
    )
    .await
    .unwrap();
    (srv, s)
}

fn message(text: &str) -> crate::compose::ComposedMessage {
    let mb = |n: &str, a: &str| Mailbox {
        name: n.into(),
        address: addr(a),
    };
    compose(
        &OutgoingMessage {
            from: Some(mb("Alice", "alice@example.com")),
            to: vec![mb("Bob", "bob@example.org")],
            subject: "Test".into(),
            text: text.into(),
            ..OutgoingMessage::default()
        },
        0,
    )
    .unwrap()
}

// ---- sending ---------------------------------------------------------------

#[tokio::test]
async fn sends_a_message_and_the_server_receives_it_intact() {
    let (srv, mut s) = ready(Behavior::default()).await;
    let m = message("Hello Bob\n");
    let report = s.send(&m.from, &m.recipients, &m.bytes).await.unwrap();
    assert_eq!(report.accepted, vec![addr("bob@example.org")]);
    assert!(report.rejected.is_empty());
    assert!(
        report.server_reply.contains("queued as ABC123"),
        "{report:?}"
    );
    let got = srv.received();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].from, "alice@example.com");
    assert_eq!(got[0].rcpts, vec!["bob@example.org"]);
    assert_eq!(got[0].data, m.bytes);
    s.quit().await;
}

#[tokio::test]
async fn lines_starting_with_a_dot_survive_dot_stuffing() {
    let (srv, mut s) = ready(Behavior::default()).await;
    let raw = b"Subject: dots\r\n\r\n.\r\n..two\r\n.start\r\nend\r\n";
    s.send(&addr("alice@example.com"), &[addr("bob@example.org")], raw)
        .await
        .unwrap();
    assert_eq!(srv.received()[0].data, raw);
}

#[tokio::test]
async fn smtp_smuggling_attempt_stays_one_message() {
    let (srv, mut s) = ready(Behavior::default()).await;
    let evil = b"Subject: x\n\nhi\n.\nMAIL FROM:<evil@example.net>\nRCPT TO:<victim@example.org>\nDATA\n\nspoof\n";
    s.send(&addr("alice@example.com"), &[addr("bob@example.org")], evil)
        .await
        .unwrap();
    assert_eq!(
        srv.received().len(),
        1,
        "the embedded commands must be message text"
    );
    let mails = srv.log().iter().filter(|l| l.contains("MAIL FROM")).count();
    assert_eq!(mails, 1, "{:?}", srv.log());
    assert!(
        String::from_utf8_lossy(&srv.received()[0].data).contains("MAIL FROM:<evil@example.net>")
    );
}

#[tokio::test]
async fn one_connection_can_send_several_messages() {
    let (srv, mut s) = ready(Behavior::default()).await;
    for i in 0..3 {
        let m = message(&format!("message {i}\n"));
        s.send(&m.from, &m.recipients, &m.bytes).await.unwrap();
        s.reset().await.unwrap();
    }
    assert_eq!(srv.received().len(), 3);
}

#[tokio::test]
async fn large_message_streams_in_chunks() {
    let (srv, mut s) = ready(Behavior::default()).await;
    let body = "0123456789abcdef\n".repeat(30_000); // ~510 KB
    let m = message(&body);
    s.send(&m.from, &m.recipients, &m.bytes).await.unwrap();
    assert_eq!(srv.received()[0].data, m.bytes);
}

// ---- per-recipient results -------------------------------------------------

fn two_recipients() -> Vec<EmailAddress> {
    vec![addr("good@example.org"), addr("gone@example.org")]
}

#[tokio::test]
async fn partly_accepted_message_reports_each_recipient() {
    let (srv, mut s) = ready(Behavior {
        reject_rcpt: vec![("gone@example.org", 550, "5.1.1 No such user")],
        ..Behavior::default()
    })
    .await;
    let m = message("hi\n");
    let r = s.send(&m.from, &two_recipients(), &m.bytes).await.unwrap();
    assert_eq!(r.accepted, vec![addr("good@example.org")]);
    assert_eq!(r.rejected.len(), 1);
    assert_eq!(r.rejected[0].address, "gone@example.org");
    assert!(r.rejected[0].permanent);
    assert_eq!(r.rejected[0].enhanced.as_deref(), Some("5.1.1"));
    assert_eq!(srv.received()[0].rcpts, vec!["good@example.org"]);
}

#[tokio::test]
async fn temporary_recipient_refusal_is_marked_retryable() {
    let (_srv, mut s) = ready(Behavior {
        reject_rcpt: vec![("gone@example.org", 450, "4.2.0 greylisted, try later")],
        ..Behavior::default()
    })
    .await;
    let m = message("hi\n");
    let r = s.send(&m.from, &two_recipients(), &m.bytes).await.unwrap();
    assert!(!r.rejected[0].permanent);
}

#[tokio::test]
async fn all_recipients_permanently_refused_is_a_permanent_failure_with_advice() {
    let (srv, mut s) = ready(Behavior {
        reject_rcpt: vec![("gone@example.org", 550, "5.1.1 No such user")],
        ..Behavior::default()
    })
    .await;
    let m = message("hi\n");
    let e = s
        .send(&m.from, &[addr("gone@example.org")], &m.bytes)
        .await
        .unwrap_err();
    let SendError::Refused(r) = &e else {
        panic!("{e:?}")
    };
    assert_eq!(r.phase, Phase::Recipients);
    assert!(r.permanent && !e.is_transient());
    assert!(r.advice().contains("does not exist"));
    assert!(srv.received().is_empty());
    assert!(
        srv.log().iter().any(|l| l.ends_with("RSET")),
        "transaction must be reset"
    );
}

#[tokio::test]
async fn all_recipients_temporarily_refused_is_transient() {
    let (_srv, mut s) = ready(Behavior {
        reject_rcpt: vec![("gone@example.org", 451, "4.7.1 try later")],
        ..Behavior::default()
    })
    .await;
    let m = message("hi\n");
    let e = s
        .send(&m.from, &[addr("gone@example.org")], &m.bytes)
        .await
        .unwrap_err();
    assert!(e.is_transient());
}

#[tokio::test]
async fn sender_refused_is_reported_by_phase() {
    let (_srv, mut s) = ready(Behavior {
        reject_mail_from: Some((554, "5.7.1 sender blocked")),
        ..Behavior::default()
    })
    .await;
    let m = message("hi\n");
    let e = s.send(&m.from, &m.recipients, &m.bytes).await.unwrap_err();
    let SendError::Refused(r) = e else { panic!() };
    assert_eq!((r.phase, r.permanent), (Phase::MailFrom, true));
    assert!(r.advice().contains("policy") || r.advice().contains("blocked"));
}

#[tokio::test]
async fn data_phase_verdicts_map_to_transient_or_permanent() {
    for (code, text, transient) in [
        (451, "4.3.0 local error", true),
        (554, "5.7.1 spam suspected", false),
    ] {
        let (srv, mut s) = ready(Behavior {
            data_reply: (code, text),
            ..Behavior::default()
        })
        .await;
        let m = message("hi\n");
        let e = s.send(&m.from, &m.recipients, &m.bytes).await.unwrap_err();
        let SendError::Refused(r) = &e else { panic!() };
        assert_eq!(r.phase, Phase::DataEnd);
        assert_eq!(e.is_transient(), transient, "{code}");
        drop(srv);
    }
}

#[tokio::test]
async fn refused_data_command_is_reported() {
    let (_srv, mut s) = ready(Behavior {
        reject_data_start: Some((451, "4.3.2 try later")),
        ..Behavior::default()
    })
    .await;
    let m = message("hi\n");
    let e = s.send(&m.from, &m.recipients, &m.bytes).await.unwrap_err();
    assert!(matches!(&e, SendError::Refused(r) if r.phase == Phase::Data) && e.is_transient());
}

#[tokio::test]
async fn message_over_the_server_limit_is_not_uploaded() {
    let (srv, mut s) = ready(Behavior {
        size_limit: Some(2_000),
        ..Behavior::default()
    })
    .await;
    let m = message(&"x".repeat(5_000));
    let e = s.send(&m.from, &m.recipients, &m.bytes).await.unwrap_err();
    assert_eq!(e, SendError::TooLarge { limit: 2_000 });
    assert!(!e.is_transient());
    assert!(
        !srv.log().iter().any(|l| l.contains("MAIL FROM")),
        "nothing may be sent"
    );
}

#[tokio::test]
async fn connection_lost_after_data_is_a_transient_connection_error() {
    let (srv, mut s) = ready(Behavior {
        drop_after_data: true,
        ..Behavior::default()
    })
    .await;
    let m = message("hi\n");
    let e = s.send(&m.from, &m.recipients, &m.bytes).await.unwrap_err();
    assert!(
        matches!(&e, SendError::Conn(c) if c.kind == FailureKind::ServerClosed),
        "{e:?}"
    );
    assert!(e.is_transient());
    assert_eq!(
        srv.received().len(),
        1,
        "server did get it: retrying may duplicate (documented at-least-once)"
    );
}

// ---- security & authentication --------------------------------------------

#[tokio::test]
async fn starttls_upgrades_before_credentials() {
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
    let m = message("hi\n");
    s.send(&m.from, &m.recipients, &m.bytes).await.unwrap();
    let log = srv.log();
    let up = log.iter().position(|l| l == "PLAIN< STARTTLS").unwrap();
    let auth = log.iter().position(|l| l.contains("AUTH")).unwrap();
    assert!(up < auth && log[..up].iter().all(|l| l.starts_with("PLAIN<")));
    assert!(
        srv.log()
            .iter()
            .filter(|l| l.starts_with("TLS< EHLO"))
            .count()
            == 1,
        "EHLO repeated inside TLS"
    );
}

#[tokio::test]
async fn missing_starttls_fails_closed_without_credentials() {
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
    assert!(
        !srv.log().iter().any(|l| l.contains("AUTH")),
        "{:?}",
        srv.log()
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

#[tokio::test]
async fn wrong_password_is_permanent() {
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
}

#[tokio::test]
async fn login_mechanism_is_used_when_plain_is_not_offered() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(
        Behavior {
            auth_mechs: vec!["LOGIN"],
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let p = pw("s3cret");
    assert!(
        open_with(
            &srv,
            &pki,
            Security::Tls,
            &Credentials::Password(&p),
            timeouts()
        )
        .await
        .is_ok()
    );
    let none = start(
        Behavior {
            auth_mechs: vec![],
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let e = open_with(
        &none,
        &pki,
        Security::Tls,
        &Credentials::Password(&p),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::AuthMethodUnsupported);
}

#[tokio::test]
async fn xoauth2_success_and_error_challenge_handshake() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(Behavior::default(), &pki).await;
    let good = pw("GOOD-TOKEN");
    assert!(
        open_with(
            &srv,
            &pki,
            Security::Tls,
            &Credentials::AccessToken(&good),
            timeouts()
        )
        .await
        .is_ok()
    );
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
    assert!(
        srv.log()
            .iter()
            .any(|l| l == "XOAUTH2-ERROR-ACK empty=true"),
        "{:?}",
        srv.log()
    );

    let plain_only = start(
        Behavior {
            auth_mechs: vec!["PLAIN"],
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let e = open_with(
        &plain_only,
        &pki,
        Security::Tls,
        &Credentials::AccessToken(&good),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::AuthMethodUnsupported);
}

#[tokio::test]
async fn silent_or_busy_servers_are_transient_connection_errors() {
    let pki = make_pki(CertFlaw::None);
    let p = pw("s3cret");
    let silent = start(
        Behavior {
            stall_greeting: true,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let mut t = timeouts();
    t.greeting = Duration::from_millis(300);
    let e = open_with(&silent, &pki, Security::Tls, &Credentials::Password(&p), t)
        .await
        .unwrap_err();
    assert_eq!((e.stage, e.kind), (Stage::Greeting, FailureKind::Timeout));

    let busy = start(
        Behavior {
            greeting_421: true,
            ..Behavior::default()
        },
        &pki,
    )
    .await;
    let e = open_with(
        &busy,
        &pki,
        Security::Tls,
        &Credentials::Password(&p),
        timeouts(),
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::ServerClosed);
    assert!(e.is_transient());
}

#[tokio::test]
async fn untrusted_certificate_is_refused_for_smtp_too() {
    let pki = make_pki(CertFlaw::None);
    let srv = start(Behavior::default(), &pki).await;
    let tls = crate::net::client_config().unwrap(); // OS roots only: the test CA is unknown
    let server = ServerConfig {
        host: "localhost".into(),
        port: srv.port,
        security: Security::Tls,
    };
    let p = pw("s3cret");
    let e = connect(
        &SmtpParams {
            server: &server,
            username: "alice",
            ehlo_name: "",
            timeouts: timeouts(),
        },
        &Credentials::Password(&p),
        &tls,
    )
    .await
    .unwrap_err();
    assert_eq!(e.kind, FailureKind::CertificateUntrusted);
}
