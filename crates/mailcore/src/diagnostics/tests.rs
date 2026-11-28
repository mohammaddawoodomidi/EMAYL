use std::sync::Arc;
use std::time::Duration;

use secrecy::SecretString;

use super::*;
use crate::account::{AccountId, AuthMethod, ServerConfig};
use crate::email::EmailAddress;
use crate::imap::testserver::{self as imap_srv, CertFlaw, make_pki};
use crate::net::{FailureKind, Timeouts, client_config_with_extra_roots};
use crate::oauth::{HttpReply, OAuthClients, OAuthError, TokenManager};
use crate::secrets::{MemorySecretStore, SecretKind, SecretStore};
use crate::smtp::testserver::{self as smtp_srv};

struct NoNet;
impl TokenTransport for NoNet {
    async fn post_form(&self, _u: &str, _b: String) -> Result<HttpReply, OAuthError> {
        Err(OAuthError::Transient("offline".into()))
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

fn cfg(port: u16, security: Security) -> ServerConfig {
    ServerConfig {
        host: "localhost".into(),
        port,
        security,
    }
}

fn account(imap_port: u16, smtp_port: u16) -> Account {
    Account {
        id: AccountId::generate(),
        display_name: "Alice".into(),
        email: EmailAddress::parse("alice@example.com").unwrap(),
        imap: cfg(imap_port, Security::Tls),
        imap_username: "alice".into(),
        smtp: cfg(smtp_port, Security::Tls),
        smtp_username: "alice".into(),
        auth: AuthMethod::Password,
    }
}

async fn run(
    flaw: CertFlaw,
    password: &str,
    imap_port: Option<u16>,
    smtp_port: Option<u16>,
) -> Diagnosis {
    let pki = make_pki(flaw);
    let i = imap_srv::start(imap_srv::Behavior::default(), &pki).await;
    let s = smtp_srv::start(smtp_srv::Behavior::default(), &pki).await;
    let a = account(imap_port.unwrap_or(i.port), smtp_port.unwrap_or(s.port));
    let store: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    store
        .set(
            &a.id,
            SecretKind::Password,
            &SecretString::from(password.to_owned()),
        )
        .unwrap();
    let tokens = TokenManager::new(store.clone(), NoNet);
    let clients = OAuthClients::default();
    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();
    let env = Env {
        secrets: &store,
        tokens: &tokens,
        clients: &clients,
        tls: &tls,
        timeouts: timeouts(),
    };
    diagnose(&env, &a).await
}

fn failed_step(s: &ServerReport) -> Option<&Step> {
    s.steps.iter().find(|x| x.status == StepStatus::Failed)
}

#[tokio::test]
async fn healthy_account_reports_everything_ok() {
    let d = run(CertFlaw::None, "s3cret", None, None).await;
    assert!(d.all_ok(), "{}", d.render());
    assert!(
        d.servers
            .iter()
            .all(|s| s.steps.iter().all(|x| x.status == StepStatus::Ok) && s.advice.is_none())
    );
    assert!(d.servers[1].notes.iter().any(|n| n.contains("10 MB")));
    assert!(d.render().contains("everything works"));
}

#[tokio::test]
async fn wrong_password_stops_at_signing_in_with_earlier_steps_ok() {
    let d = run(CertFlaw::None, "nope", None, None).await;
    assert!(!d.all_ok());
    for s in &d.servers {
        let f = failed_step(s).unwrap();
        assert_eq!(f.label, "Signing in");
        let before: Vec<_> = s
            .steps
            .iter()
            .take_while(|x| x.status == StepStatus::Ok)
            .collect();
        assert!(before.len() >= 3);
        assert!(!s.transient, "a wrong password does not fix itself");
        assert!(
            s.advice
                .as_ref()
                .unwrap()
                .to_lowercase()
                .contains("password")
        );
    }
    assert!(
        !d.render().contains("nope"),
        "the report must never contain the password"
    );
}

#[tokio::test]
async fn certificate_problems_stop_at_the_secure_connection_step() {
    for flaw in [CertFlaw::Expired, CertFlaw::WrongName] {
        let d = run(flaw, "s3cret", None, None).await;
        for s in &d.servers {
            let f = failed_step(s).unwrap();
            assert_eq!(f.label, "Secure connection and certificate");
            assert_eq!(s.steps.last().unwrap().status, StepStatus::NotReached);
            assert!(!s.transient);
        }
    }
}

#[tokio::test]
async fn closed_port_is_reported_at_connecting_and_is_transient() {
    // Port 9 (discard) is not listening on a normal machine.
    let d = run(CertFlaw::None, "s3cret", Some(9), None).await;
    let imap = &d.servers[0];
    assert!(!imap.ok);
    assert_eq!(failed_step(imap).unwrap().label, "Connecting to the server");
    assert!(imap.transient);
    assert!(d.servers[1].ok, "the other server is still checked");
}

#[tokio::test]
async fn config_warnings_are_included() {
    let mut a = account(993, 587);
    a.imap.security = Security::StartTls;
    let w = a.warnings();
    assert_eq!(w.len(), 2);
    let d = Diagnosis {
        account: a.email.to_string(),
        servers: vec![],
        warnings: w.into_iter().map(str::to_owned).collect(),
    };
    assert!(d.render().contains("Warning: IMAP port 993"));
}

#[test]
fn starttls_sequence_orders_steps_and_late_failures_get_their_own_step() {
    let e = ConnError::new(Stage::Session, FailureKind::Protocol, "boom");
    let r = report(
        "x",
        "h",
        143,
        Security::StartTls,
        Err(e),
        Duration::from_millis(5),
    );
    let labels: Vec<_> = r.steps.iter().map(|s| s.label).collect();
    assert_eq!(labels.first(), Some(&"Finding the server"));
    assert!(labels.contains(&"Switching to a secure connection"));
    assert_eq!(labels.last(), Some(&"Using the connection"));
    assert!(
        r.steps[..r.steps.len() - 1]
            .iter()
            .all(|s| s.status == StepStatus::Ok)
    );
    assert_eq!(r.steps.last().unwrap().status, StepStatus::Failed);
}
