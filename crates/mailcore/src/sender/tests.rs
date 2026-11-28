use std::path::PathBuf;
use std::time::Duration;

use secrecy::SecretString;

use super::*;
use crate::account::{AccountId, AuthMethod, Security, ServerConfig};
use crate::compose::{Mailbox, OutgoingMessage, compose};
use crate::fsutil::new_id;
use crate::imap::testserver::{self as imap_srv, CertFlaw, Pki, make_pki};
use crate::net::client_config_with_extra_roots;
use crate::oauth::{HttpReply, OAuthError};
use crate::secrets::{MemorySecretStore, SecretKind};
use crate::smtp::testserver::{self as smtp_srv};

struct NoNet;
impl TokenTransport for NoNet {
    async fn post_form(&self, _u: &str, _b: String) -> std::result::Result<HttpReply, OAuthError> {
        Err(OAuthError::Transient("no network in tests".into()))
    }
}

fn timeouts() -> Timeouts {
    let s = Duration::from_secs(5);
    Timeouts {
        dns: s,
        connect: s,
        tls: s,
        greeting: s,
        command: s,
    }
}

fn cfg(port: u16) -> ServerConfig {
    ServerConfig {
        host: "localhost".into(),
        port,
        security: Security::Tls,
    }
}

fn account(smtp_port: u16, imap_port: u16) -> Account {
    Account {
        id: AccountId::generate(),
        display_name: "Alice".into(),
        email: EmailAddress::parse("alice@example.com").unwrap(),
        imap: cfg(imap_port),
        imap_username: "alice".into(),
        smtp: cfg(smtp_port),
        smtp_username: "alice".into(),
        auth: AuthMethod::Password,
    }
}

fn composed(to: &str) -> crate::compose::ComposedMessage {
    let mb = |a: &str| Mailbox {
        name: String::new(),
        address: EmailAddress::parse(a).unwrap(),
    };
    compose(
        &OutgoingMessage {
            from: Some(mb("alice@example.com")),
            to: vec![mb(to)],
            subject: "Hi".into(),
            text: "hello\n".into(),
            ..OutgoingMessage::default()
        },
        0,
    )
    .unwrap()
}

struct World {
    pki: Pki,
    store: Arc<dyn SecretStore>,
    tokens: TokenManager<NoNet>,
    clients: OAuthClients,
    tls: Arc<ClientConfig>,
    dir: PathBuf,
    outbox: Outbox,
}

fn world(password: &str) -> World {
    let pki = make_pki(CertFlaw::None);
    let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();
    let dir = std::env::temp_dir().join(format!("emayl-sender-{}", new_id()));
    let outbox = Outbox::open(&dir).unwrap();
    let w = World {
        pki,
        store: store.clone(),
        tokens: TokenManager::new(store, NoNet),
        clients: OAuthClients::default(),
        tls,
        dir,
        outbox,
    };
    let _ = password;
    w
}

impl World {
    fn env(&self) -> Env<'_, NoNet> {
        Env {
            secrets: &self.store,
            tokens: &self.tokens,
            clients: &self.clients,
            tls: &self.tls,
            timeouts: timeouts(),
        }
    }
    fn set_password(&self, a: &Account, pw: &str) {
        self.store
            .set(
                &a.id,
                SecretKind::Password,
                &SecretString::from(pw.to_owned()),
            )
            .unwrap();
    }
    fn queue(&self, a: &Account, to: &str, copy: bool) -> OutboxItem {
        self.outbox
            .enqueue(&a.id, &composed(to), 1000, copy)
            .unwrap()
    }
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[tokio::test]
async fn delivers_all_waiting_messages_over_one_connection() {
    let w = world("s3cret");
    let smtp = smtp_srv::start(smtp_srv::Behavior::default(), &w.pki).await;
    let a = account(smtp.port, 1);
    w.set_password(&a, "s3cret");
    let i1 = w.queue(&a, "bob@example.org", false);
    let i2 = w.queue(&a, "carol@example.org", false);
    let done = process_account(&w.env(), &a, &w.outbox, 2000)
        .await
        .unwrap();
    assert_eq!(done.len(), 2);
    assert!(done.iter().all(|i| i.state == OutboxState::Sent));
    let got = smtp.received();
    assert_eq!(got.len(), 2);
    let mut to: Vec<_> = got.iter().map(|g| g.rcpts[0].clone()).collect();
    to.sort();
    assert_eq!(to, vec!["bob@example.org", "carol@example.org"]);
    assert_eq!(
        smtp.log().iter().filter(|l| l.contains("AUTH")).count(),
        1,
        "one login for both"
    );
    assert!(w.outbox.message_bytes(&i1.id).is_err() && w.outbox.message_bytes(&i2.id).is_err());
}

#[tokio::test]
async fn nothing_due_means_no_connection() {
    let w = world("s3cret");
    let smtp = smtp_srv::start(smtp_srv::Behavior::default(), &w.pki).await;
    let a = account(smtp.port, 1);
    w.set_password(&a, "s3cret");
    assert!(
        process_account(&w.env(), &a, &w.outbox, 2000)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(smtp.log().is_empty());
}

#[tokio::test]
async fn wrong_password_parks_every_message_and_blames_the_account() {
    let w = world("x");
    let smtp = smtp_srv::start(smtp_srv::Behavior::default(), &w.pki).await;
    let a = account(smtp.port, 1);
    w.set_password(&a, "wrong");
    w.queue(&a, "bob@example.org", false);
    w.queue(&a, "carol@example.org", false);
    let done = process_account(&w.env(), &a, &w.outbox, 2000)
        .await
        .unwrap();
    assert!(
        done.iter()
            .all(|i| i.state == OutboxState::Failed
                && i.last_error.as_ref().unwrap().account_problem)
    );
    assert!(smtp.received().is_empty());
    // After the user fixes the password the messages go out.
    w.set_password(&a, "s3cret");
    assert_eq!(w.outbox.retry_account(&a.id, 3000).unwrap(), 2);
    let again = process_account(&w.env(), &a, &w.outbox, 3000)
        .await
        .unwrap();
    assert!(again.iter().all(|i| i.state == OutboxState::Sent));
}

#[tokio::test]
async fn missing_password_is_reported_not_a_crash() {
    let w = world("x");
    let smtp = smtp_srv::start(smtp_srv::Behavior::default(), &w.pki).await;
    let a = account(smtp.port, 1);
    w.queue(&a, "bob@example.org", false);
    let done = process_account(&w.env(), &a, &w.outbox, 2000)
        .await
        .unwrap();
    assert_eq!(done[0].state, OutboxState::Failed);
}

#[tokio::test]
async fn one_bad_recipient_does_not_block_the_good_one() {
    let w = world("s3cret");
    let b = smtp_srv::Behavior {
        reject_rcpt: vec![("gone@example.org", 550, "5.1.1 no such user")],
        ..Default::default()
    };
    let smtp = smtp_srv::start(b, &w.pki).await;
    let a = account(smtp.port, 1);
    w.set_password(&a, "s3cret");
    let mb = |x: &str| Mailbox {
        name: String::new(),
        address: EmailAddress::parse(x).unwrap(),
    };
    let msg = compose(
        &OutgoingMessage {
            from: Some(mb("alice@example.com")),
            to: vec![mb("good@example.org"), mb("gone@example.org")],
            subject: "Two".into(),
            text: "x\n".into(),
            ..OutgoingMessage::default()
        },
        0,
    )
    .unwrap();
    w.outbox.enqueue(&a.id, &msg, 1000, false).unwrap();
    let done = process_account(&w.env(), &a, &w.outbox, 2000)
        .await
        .unwrap();
    assert_eq!(done[0].state, OutboxState::Sent);
    assert!(done[0].has_failed_recipients());
    assert_eq!(smtp.received()[0].rcpts, vec!["good@example.org"]);
}

#[tokio::test]
async fn connection_lost_after_data_defers_that_message_and_leaves_the_rest_queued() {
    let w = world("s3cret");
    let b = smtp_srv::Behavior {
        drop_after_data: true,
        ..Default::default()
    };
    let smtp = smtp_srv::start(b, &w.pki).await;
    let a = account(smtp.port, 1);
    w.set_password(&a, "s3cret");
    let first = w
        .outbox
        .enqueue(&a.id, &composed("bob@example.org"), 1000, false)
        .unwrap();
    let second = w
        .outbox
        .enqueue(&a.id, &composed("carol@example.org"), 1001, false)
        .unwrap();
    let done = process_account(&w.env(), &a, &w.outbox, 2000)
        .await
        .unwrap();
    assert_eq!(done.len(), 1, "stopped after the connection died");
    assert_eq!(done[0].id, first.id);
    assert_eq!(
        done[0].state,
        OutboxState::Deferred,
        "will be retried (a duplicate is possible: documented)"
    );
    let untouched = w.outbox.get(&second.id).unwrap();
    assert_eq!(
        (untouched.state, untouched.attempts),
        (OutboxState::Queued, 0)
    );
}

#[tokio::test]
async fn messages_of_a_removed_account_are_parked_visibly() {
    let w = world("s3cret");
    let ghost = AccountId::generate();
    let item = w
        .outbox
        .enqueue(&ghost, &composed("bob@example.org"), 1000, false)
        .unwrap();
    let done = process_due(&w.env(), &[], &w.outbox, 2000).await.unwrap();
    assert_eq!(done.len(), 1);
    assert_eq!(done[0].id, item.id);
    assert_eq!(done[0].state, OutboxState::Failed);
}

#[tokio::test]
async fn unreadable_stored_message_fails_cleanly() {
    let w = world("s3cret");
    let smtp = smtp_srv::start(smtp_srv::Behavior::default(), &w.pki).await;
    let a = account(smtp.port, 1);
    w.set_password(&a, "s3cret");
    let item = w.queue(&a, "bob@example.org", false);
    std::fs::remove_file(w.dir.join(format!("{}.eml", item.id))).unwrap();
    let done = process_account(&w.env(), &a, &w.outbox, 2000)
        .await
        .unwrap();
    assert_eq!(done[0].state, OutboxState::Failed);
    assert!(smtp.received().is_empty());
}

#[test]
fn gmail_is_not_given_a_second_copy() {
    let mk = |host: &str| {
        let mut a = account(1, 1);
        a.smtp.host = host.into();
        a
    };
    assert!(!wants_sent_copy(&mk("smtp.gmail.com")));
    assert!(!wants_sent_copy(&mk("SMTP.GMAIL.COM")));
    assert!(wants_sent_copy(&mk("smtp.example.com")));
    assert!(wants_sent_copy(&mk("smtp.office365.com")));
}

#[test]
fn sent_copy_verdicts() {
    let transient = Err(ConnError::new(Stage::Connect, FailureKind::Timeout, "slow"));
    let refused = Err(ConnError::new(
        Stage::Session,
        FailureKind::Protocol,
        "NO over quota",
    ));
    assert_eq!(
        decide_sent_copy(&Ok(Stored::Appended), 0, 10),
        SentCopy::Saved
    );
    assert_eq!(
        decide_sent_copy(&Ok(Stored::AlreadyThere), 0, 10),
        SentCopy::Saved
    );
    assert_eq!(
        decide_sent_copy(&Ok(Stored::NoSentFolder), 0, 10),
        SentCopy::GaveUp
    );
    assert_eq!(decide_sent_copy(&transient, 0, 10), SentCopy::Pending);
    assert_eq!(
        decide_sent_copy(&transient, 0, SENT_COPY_GIVE_UP_SECS + 1),
        SentCopy::GaveUp
    );
    assert_eq!(decide_sent_copy(&refused, 0, 10), SentCopy::GaveUp);
}

async fn imap_world(b: imap_srv::Behavior) -> (World, imap_srv::MockServer, Account) {
    let w = world("s3cret");
    let srv = imap_srv::start(b, &w.pki).await;
    let a = account(1, srv.port);
    w.set_password(&a, "s3cret");
    (w, srv, a)
}

#[tokio::test]
async fn sent_copy_is_appended_once_when_missing() {
    let (w, srv, a) = imap_world(imap_srv::Behavior::default()).await;
    let item = w.queue(&a, "bob@example.org", true);
    w.outbox.begin_attempt(&item.id).unwrap();
    let r = AttemptResult::Delivered {
        accepted: vec!["bob@example.org".into()],
        rejected: vec![],
        server_reply: "ok".into(),
    };
    w.outbox.finish_attempt(&item.id, &r, 2000, 0.0).unwrap();
    assert_eq!(w.outbox.get(&item.id).unwrap().sent_copy, SentCopy::Pending);

    assert_eq!(
        save_sent_copies(&w.env(), &a, &w.outbox, 2100)
            .await
            .unwrap(),
        1
    );
    let appended = srv.appended();
    assert_eq!(appended.len(), 1);
    assert_eq!(appended[0].0, "Sent");
    assert_eq!(appended[0].1, "\\Seen");
    assert!(String::from_utf8_lossy(&appended[0].2).contains(&item.message_id));
    assert_eq!(w.outbox.get(&item.id).unwrap().sent_copy, SentCopy::Saved);
    assert!(
        w.outbox.message_bytes(&item.id).is_err(),
        "message file removed afterwards"
    );
    // A second pass has nothing to do.
    assert_eq!(
        save_sent_copies(&w.env(), &a, &w.outbox, 2200)
            .await
            .unwrap(),
        0
    );
    assert_eq!(srv.appended().len(), 1);
}

#[tokio::test]
async fn no_second_copy_when_the_server_already_has_one() {
    let (w, srv, a) = imap_world(imap_srv::Behavior::default()).await;
    let mut s =
        crate::imap::connect_account(&a, &w.store, &w.tokens, &w.clients, &w.tls, timeouts())
            .await
            .unwrap();
    // The mock's seed mail m101@example.com stands in for "server already saved it".
    assert_eq!(
        ensure_in_sent(&mut s, "m101@example.com", b"x")
            .await
            .unwrap(),
        Stored::AlreadyThere
    );
    assert!(srv.appended().is_empty());
    assert_eq!(
        ensure_in_sent(
            &mut s,
            "fresh@example.com",
            b"Message-ID: <fresh@example.com>\r\n\r\nbody"
        )
        .await
        .unwrap(),
        Stored::Appended
    );
    assert_eq!(srv.appended().len(), 1);
}

#[tokio::test]
async fn unsafe_message_ids_are_not_sent_to_the_server() {
    let (w, srv, a) = imap_world(imap_srv::Behavior::default()).await;
    let mut s =
        crate::imap::connect_account(&a, &w.store, &w.tokens, &w.clients, &w.tls, timeouts())
            .await
            .unwrap();
    s.select("Sent").await.unwrap();
    for bad in ["", "a\" b", "x\r\nA1 DELETE INBOX", "a<b>"] {
        assert!(s.uid_search_message_id(bad).await.is_err(), "{bad:?}");
    }
    assert!(!srv.log().iter().any(|l| l.contains("DELETE")));
}

#[tokio::test]
async fn refused_append_gives_up_but_a_down_server_keeps_trying() {
    let b = imap_srv::Behavior {
        append_reply: Some("NO [OVERQUOTA] mailbox full"),
        ..Default::default()
    };
    let (w, _srv, a) = imap_world(b).await;
    let item = w.queue(&a, "bob@example.org", true);
    w.outbox.begin_attempt(&item.id).unwrap();
    let r = AttemptResult::Delivered {
        accepted: vec!["bob@example.org".into()],
        rejected: vec![],
        server_reply: "ok".into(),
    };
    w.outbox.finish_attempt(&item.id, &r, 2000, 0.0).unwrap();
    assert_eq!(
        save_sent_copies(&w.env(), &a, &w.outbox, 2100)
            .await
            .unwrap(),
        0
    );
    let after = w.outbox.get(&item.id).unwrap();
    assert_eq!(
        after.sent_copy,
        SentCopy::GaveUp,
        "a server that says no is not retried forever"
    );

    // Nobody listening on the port: the copy stays owed (until 24 h pass).
    let (w2, srv2, mut a2) = imap_world(imap_srv::Behavior::default()).await;
    let it2 = w2.queue(&a2, "bob@example.org", true);
    w2.outbox.begin_attempt(&it2.id).unwrap();
    w2.outbox.finish_attempt(&it2.id, &r, 2000, 0.0).unwrap();
    drop(srv2);
    a2.imap.port = 9; // discard port: refused
    assert_eq!(
        save_sent_copies(&w2.env(), &a2, &w2.outbox, 2100)
            .await
            .unwrap(),
        0
    );
    assert_eq!(w2.outbox.get(&it2.id).unwrap().sent_copy, SentCopy::Pending);
    assert_eq!(
        save_sent_copies(
            &w2.env(),
            &a2,
            &w2.outbox,
            2000 + SENT_COPY_GIVE_UP_SECS + 5
        )
        .await
        .unwrap(),
        0
    );
    assert_eq!(w2.outbox.get(&it2.id).unwrap().sent_copy, SentCopy::GaveUp);
}
