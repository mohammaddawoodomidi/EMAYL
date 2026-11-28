use std::time::Duration;

use secrecy::SecretString;

use super::*;
use crate::account::{AccountId, AuthMethod, Security, ServerConfig};
use crate::email::EmailAddress;
use crate::imap::testserver::{self as srv, Behavior, CertFlaw, MockServer, Pki, make_pki};
use crate::net::{Timeouts, client_config_with_extra_roots};
use crate::oauth::{HttpReply, OAuthClients, OAuthError, TokenManager};
use crate::secrets::{MemorySecretStore, SecretKind, SecretStore};
use crate::store::{ListScope, SearchQuery, SearchScope, SearchSort};

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

struct World {
    _pki: Pki,
    secrets: Arc<dyn SecretStore>,
    tokens: TokenManager<NoNet>,
    clients: OAuthClients,
    tls: Arc<rustls::ClientConfig>,
    server: MockServer,
    account: Account,
    store: Arc<Store>,
}

async fn world_with(behavior: Behavior, password: &str) -> World {
    let pki = make_pki(CertFlaw::None);
    let server = srv::start(behavior, &pki).await;
    let secrets: Arc<dyn SecretStore> = Arc::new(MemorySecretStore::new());
    let account = Account {
        id: AccountId::generate(),
        display_name: "Alice".into(),
        email: EmailAddress::parse("alice@example.com").unwrap(),
        imap: ServerConfig {
            host: "localhost".into(),
            port: server.port,
            security: Security::Tls,
        },
        imap_username: "alice".into(),
        smtp: ServerConfig {
            host: "localhost".into(),
            port: 1,
            security: Security::Tls,
        },
        smtp_username: "alice".into(),
        auth: AuthMethod::Password,
    };
    secrets
        .set(
            &account.id,
            SecretKind::Password,
            &SecretString::from(password.to_owned()),
        )
        .unwrap();
    let tls = client_config_with_extra_roots(vec![pki.ca.clone()]).unwrap();
    World {
        _pki: pki,
        tokens: TokenManager::new(secrets.clone(), NoNet),
        secrets,
        clients: OAuthClients::default(),
        tls,
        server,
        account,
        store: Arc::new(Store::open_in_memory().unwrap()),
    }
}

async fn world() -> World {
    world_with(Behavior::default(), "s3cret").await
}

impl World {
    fn env(&self) -> Env<'_, NoNet> {
        Env {
            secrets: &self.secrets,
            tokens: &self.tokens,
            clients: &self.clients,
            tls: &self.tls,
            timeouts: timeouts(),
        }
    }
    async fn sync(&self) -> SyncReport {
        sync_account(
            &self.env(),
            &self.account,
            &self.store,
            &SyncOptions::default(),
        )
        .await
        .unwrap()
    }
    fn acct(&self) -> &str {
        self.account.id.as_str()
    }
    fn inbox(&self) -> FolderRow {
        self.store
            .folders(self.acct())
            .unwrap()
            .into_iter()
            .find(|f| f.role == FolderRole::Inbox)
            .unwrap()
    }
    fn inbox_uids(&self) -> Vec<u32> {
        self.store.uids_in_folder(self.inbox().id).unwrap()
    }
    async fn connect(&self) -> ImapSession {
        connect_account(
            &self.account,
            &self.secrets,
            &self.tokens,
            &self.clients,
            &self.tls,
            timeouts(),
        )
        .await
        .unwrap()
    }
}

#[tokio::test]
async fn first_sync_downloads_headers_and_lists_folders() {
    let w = world().await;
    let rep = w.sync().await;
    let inbox = w.inbox();
    assert_eq!((inbox.total, inbox.unread), (3, 2));
    assert_eq!(w.inbox_uids(), vec![101, 102, 103]);
    assert!(rep.new_messages >= 3);
    assert!(inbox.last_synced.is_some() && inbox.uid_validity == Some(7));
    // A folder the server cannot open is reported, the rest still synced.
    let names: Vec<String> = w
        .store
        .folders(w.acct())
        .unwrap()
        .into_iter()
        .map(|f| f.display_name)
        .collect();
    assert!(names.iter().any(|n| n == "INBOX"), "{names:?}");
    assert!(rep.folders_synced >= 2, "{rep:?}");
    let hits = w
        .store
        .search(
            &SearchQuery::parse("hello"),
            &SearchScope::Account(w.acct().into()),
            SearchSort::Newest,
            10,
        )
        .unwrap();
    assert!(hits.len() >= 3);
    let list = w
        .store
        .list_messages(&ListScope::Folder(inbox.id), false, None, 10)
        .unwrap();
    assert_eq!(list[0].from_name, "Bob");
    assert!(list.iter().find(|m| m.uid == 101).unwrap().flags & crate::store::FLAG_SEEN != 0);
}

#[tokio::test]
async fn second_sync_without_changes_changes_nothing() {
    let w = world().await;
    w.sync().await;
    let rep = w.sync().await;
    assert_eq!(
        (rep.new_messages, rep.removed_messages, rep.flag_changes),
        (0, 0, 0),
        "{rep:?}"
    );
    assert_eq!(w.inbox_uids().len(), 3);
}

#[tokio::test]
async fn new_mail_flags_and_deletions_are_picked_up() {
    let w = world().await;
    w.sync().await;
    let uid = w.server.add_message("Fresh news", "text");
    w.server.set_flag(103, "\\Flagged", true);
    w.server.set_flag(102, "\\Seen", true);
    w.server.remove_uid(101);
    let rep = w.sync().await;
    assert!(
        rep.new_messages >= 1 && rep.removed_messages >= 1 && rep.flag_changes >= 2,
        "{rep:?}"
    );
    assert_eq!(w.inbox_uids(), vec![102, 103, uid]);
    let m103 = w.store.message_by_uid(w.inbox().id, 103).unwrap().unwrap();
    assert!(m103.is_flagged());
    assert!(
        !w.store
            .message_by_uid(w.inbox().id, 102)
            .unwrap()
            .unwrap()
            .is_unread()
    );
    assert_eq!(
        w.store
            .search(
                &SearchQuery::parse("fresh"),
                &SearchScope::Everywhere,
                SearchSort::Newest,
                10
            )
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn renumbered_folder_is_rebuilt_not_mixed() {
    let w = world().await;
    w.sync().await;
    w.server.remove_uid(102);
    w.server.renumber(99);
    let rep = w.sync().await;
    assert!(rep.rebuilt_folders.iter().any(|f| f == "INBOX"), "{rep:?}");
    assert_eq!(w.inbox_uids(), vec![1, 2]);
    assert_eq!(w.inbox().uid_validity, Some(99));
}

#[tokio::test]
async fn waiting_actions_are_sent_and_the_server_agrees() {
    let w = world().await;
    w.sync().await;
    let inbox = w.inbox().id;
    let m102 = w.store.message_by_uid(inbox, 102).unwrap().unwrap();
    let m101 = w.store.message_by_uid(inbox, 101).unwrap().unwrap();
    set_read(&w.store, &m102, true).unwrap();
    set_starred(&w.store, &m102, true).unwrap();
    move_to_folder(
        &w.store,
        &m101,
        w.store
            .folders(w.acct())
            .unwrap()
            .iter()
            .find(|f| f.role == FolderRole::Sent)
            .unwrap()
            .id,
    )
    .unwrap();
    assert_eq!(w.store.pending_count(w.acct()).unwrap(), 3);
    // Locally it already looks done.
    assert!(
        !w.store
            .message_by_uid(inbox, 102)
            .unwrap()
            .unwrap()
            .is_unread()
    );
    assert_eq!(w.inbox_uids(), vec![102, 103]);
    let rep = w.sync().await;
    assert_eq!(rep.actions_sent, 3, "{rep:?}");
    assert_eq!(w.store.pending_count(w.acct()).unwrap(), 0);
    let flags = w.server.flags_of(102);
    assert!(
        flags.contains(&"\\Seen".to_owned()) && flags.contains(&"\\Flagged".to_owned()),
        "{flags:?}"
    );
    assert!(!w.server.uids().contains(&101));
    // The sync did not bring the moved message back, and kept our flags.
    assert_eq!(w.inbox_uids(), vec![102, 103]);
    assert!(
        w.store
            .message_by_uid(inbox, 102)
            .unwrap()
            .unwrap()
            .is_flagged()
    );
}

#[tokio::test]
async fn a_local_change_wins_over_a_stale_server_flag_until_sent() {
    let w = world().await;
    w.sync().await;
    let inbox = w.inbox().id;
    let m = w.store.message_by_uid(inbox, 102).unwrap().unwrap();
    set_read(&w.store, &m, true).unwrap();
    // The server changes some other flag in the meantime; our pending change must survive.
    w.server.set_flag(102, "\\Answered", true);
    w.sync().await;
    let now = w.store.message_by_uid(inbox, 102).unwrap().unwrap();
    assert!(!now.is_unread());
    assert!(w.server.flags_of(102).contains(&"\\Seen".to_owned()));
}

#[tokio::test]
async fn delete_goes_to_trash_or_is_refused_without_one() {
    let w = world().await;
    w.sync().await;
    let m = w.store.message_by_uid(w.inbox().id, 102).unwrap().unwrap();
    // The mock server has no Trash folder.
    let err = delete(&w.store, &m).unwrap_err();
    assert!(matches!(err, Error::NotFound(_)));
    assert!(archive(&w.store, &m).is_err());
    assert!(mark_spam(&w.store, &m).is_err());
    assert_eq!(w.inbox_uids().len(), 3, "a refused action changes nothing");
    delete_permanently(&w.store, &m).unwrap();
    assert_eq!(w.inbox_uids().len(), 2);
    let rep = w.sync().await;
    assert!(rep.actions_sent >= 1);
    assert!(!w.server.uids().contains(&102));
}

#[tokio::test]
async fn body_is_downloaded_on_demand_cached_and_searchable() {
    let w = world().await;
    w.sync().await;
    let inbox = w.inbox().id;
    let m = w.store.message_by_uid(inbox, 101).unwrap().unwrap();
    assert!(!m.has_body);
    let mut s = w.connect().await;
    let raw = fetch_body(&mut s, &w.store, m.id, &SyncOptions::default())
        .await
        .unwrap();
    assert!(String::from_utf8_lossy(&raw).contains("Body of message 101"));
    let again = w.store.message(m.id).unwrap().unwrap();
    assert!(again.has_body);
    assert!(
        again.preview.contains("Body of message 101"),
        "{:?}",
        again.preview
    );
    // Second call is served from the cache: no new FETCH reaches the server.
    let before = w
        .server
        .log()
        .iter()
        .filter(|l| l.contains("FETCH"))
        .count();
    let raw2 = fetch_body(&mut s, &w.store, m.id, &SyncOptions::default())
        .await
        .unwrap();
    assert_eq!(raw, raw2);
    assert_eq!(
        w.server
            .log()
            .iter()
            .filter(|l| l.contains("FETCH"))
            .count(),
        before
    );
    let hits = w
        .store
        .search(
            &SearchQuery::parse("\"message 101\""),
            &SearchScope::Folder(inbox),
            SearchSort::Newest,
            10,
        )
        .unwrap();
    assert_eq!(hits.len(), 1);
    s.logout().await;
}

#[tokio::test]
async fn body_of_a_message_gone_from_the_server_is_reported_and_forgotten() {
    let w = world().await;
    w.sync().await;
    let inbox = w.inbox().id;
    let m = w.store.message_by_uid(inbox, 103).unwrap().unwrap();
    w.server.remove_uid(103);
    let mut s = w.connect().await;
    let r = fetch_body(&mut s, &w.store, m.id, &SyncOptions::default()).await;
    assert!(matches!(r, Err(SyncError::Gone)), "{r:?}");
    assert!(w.store.message(m.id).unwrap().is_none());
    s.logout().await;
}

#[tokio::test]
async fn too_large_messages_are_refused_before_download() {
    let w = world().await;
    w.sync().await;
    let m = w.store.message_by_uid(w.inbox().id, 101).unwrap().unwrap();
    let mut s = w.connect().await;
    let opts = SyncOptions {
        max_message_bytes: 10,
        ..SyncOptions::default()
    };
    let before = w
        .server
        .log()
        .iter()
        .filter(|l| l.contains("BODY.PEEK[]"))
        .count();
    let r = fetch_body(&mut s, &w.store, m.id, &opts).await;
    assert!(matches!(r, Err(SyncError::TooLarge(10))), "{r:?}");
    assert_eq!(
        w.server
            .log()
            .iter()
            .filter(|l| l.contains("BODY.PEEK[]"))
            .count(),
        before
    );
    s.logout().await;
}

#[tokio::test]
async fn first_download_is_limited_and_older_mail_loads_on_request() {
    let w = world().await;
    let opts = SyncOptions {
        initial_window: 2,
        ..SyncOptions::default()
    };
    sync_account(&w.env(), &w.account, &w.store, &opts)
        .await
        .unwrap();
    assert_eq!(w.inbox_uids(), vec![102, 103]);
    assert_eq!(w.inbox().floor_uid, 102);
    // Syncing again must not fetch the older message by itself.
    sync_account(&w.env(), &w.account, &w.store, &opts)
        .await
        .unwrap();
    assert_eq!(w.inbox_uids(), vec![102, 103]);
    // New mail still arrives.
    let uid = w.server.add_message("Newer", "x");
    sync_account(&w.env(), &w.account, &w.store, &opts)
        .await
        .unwrap();
    assert_eq!(w.inbox_uids(), vec![102, 103, uid]);
    let mut s = w.connect().await;
    let n = load_older(&mut s, &w.store, w.inbox().id, 10, &opts)
        .await
        .unwrap();
    assert_eq!(n, 1);
    assert_eq!(w.inbox_uids(), vec![101, 102, 103, uid]);
    assert_eq!(w.inbox().floor_uid, 1);
    assert_eq!(
        load_older(&mut s, &w.store, w.inbox().id, 10, &opts)
            .await
            .unwrap(),
        0
    );
    s.logout().await;
}

#[tokio::test]
async fn wrong_password_is_a_problem_for_the_person_not_a_retry() {
    let w = world_with(Behavior::default(), "wrong").await;
    let e = sync_account(&w.env(), &w.account, &w.store, &SyncOptions::default())
        .await
        .unwrap_err();
    match e {
        SyncError::Server(c) => {
            assert_eq!(c.kind, FailureKind::AuthRejected);
            assert!(!c.is_transient());
        }
        other => panic!("{other:?}"),
    }
    assert!(w.store.folders(w.acct()).unwrap().is_empty());
}

#[tokio::test]
async fn the_loop_syncs_notices_new_mail_by_itself_and_stops() {
    let w = world().await;
    let (handle, ctl) = sync_channel();
    let opts = SyncOptions {
        idle_timeout: Duration::from_secs(30),
        poll_interval: Duration::from_millis(200),
        ..SyncOptions::default()
    };
    let env = w.env();
    let runner = run_account_loop(&env, &w.account, w.store.clone(), opts, ctl);
    let driver = async {
        let wait_for = async |pred: &dyn Fn() -> bool| {
            for _ in 0..200 {
                if pred() {
                    return true;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            false
        };
        assert!(
            wait_for(&|| matches!(handle.status(), SyncStatus::Online { .. })).await,
            "{:?}",
            handle.status()
        );
        assert_eq!(w.inbox_uids().len(), 3);
        // The mock server does not push; a wake-up (what "refresh" does) interrupts IDLE and re-syncs.
        w.server.add_message("While you were away", "x");
        handle.wake();
        assert!(
            wait_for(&|| w.inbox_uids().len() == 4).await,
            "{:?}",
            handle.status()
        );
        // Queue an action while idle; waking sends it.
        let m = w.store.message_by_uid(w.inbox().id, 102).unwrap().unwrap();
        set_read(&w.store, &m, true).unwrap();
        handle.wake();
        assert!(wait_for(&|| w.server.flags_of(102).contains(&"\\Seen".to_owned())).await);
        // One connection served all of it.
        assert!(
            w.server.connections() <= 2,
            "connections: {}",
            w.server.connections()
        );
        handle.stop();
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(runner, driver)
    })
    .await
    .expect("loop must stop when asked");
    assert_eq!(handle.status(), SyncStatus::Stopped);
}

#[tokio::test]
async fn the_loop_waits_for_the_person_after_a_rejected_password() {
    let w = world_with(Behavior::default(), "wrong").await;
    let (handle, ctl) = sync_channel();
    let env = w.env();
    let runner = run_account_loop(
        &env,
        &w.account,
        w.store.clone(),
        SyncOptions::default(),
        ctl,
    );
    let driver = async {
        for _ in 0..200 {
            if matches!(handle.status(), SyncStatus::NeedsAttention { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            matches!(handle.status(), SyncStatus::NeedsAttention { .. }),
            "{:?}",
            handle.status()
        );
        let tries = w.server.connections();
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            w.server.connections(),
            tries,
            "must not retry a bad password by itself"
        );
        // The person fixes the password and presses retry.
        w.secrets
            .set(
                &w.account.id,
                SecretKind::Password,
                &SecretString::from("s3cret".to_owned()),
            )
            .unwrap();
        handle.wake();
        for _ in 0..200 {
            if matches!(handle.status(), SyncStatus::Online { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(
            matches!(handle.status(), SyncStatus::Online { .. }),
            "{:?}",
            handle.status()
        );
        handle.stop();
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(runner, driver)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn a_dropped_handle_stops_the_loop() {
    let w = world().await;
    let (handle, ctl) = sync_channel();
    let env = w.env();
    let runner = run_account_loop(
        &env,
        &w.account,
        w.store.clone(),
        SyncOptions::default(),
        ctl,
    );
    let driver = async {
        for _ in 0..200 {
            if matches!(handle.status(), SyncStatus::Online { .. }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        handle.stop();
    };
    tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(runner, driver)
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn the_inbox_is_downloaded_first_and_progress_is_reported() {
    let w = world().await;
    for i in 0..60 {
        w.server.add_message(&format!("Bulk {i}"), "x");
    }
    let opts = SyncOptions {
        initial_window: 40,
        other_window: 10,
        batch_size: 15,
        ..SyncOptions::default()
    };
    let mut seen: Vec<SyncProgress> = Vec::new();
    let mut s = w.connect().await;
    let mut sink = |p: &SyncProgress| seen.push(p.clone());
    sync_session_reporting(&mut s, w.acct(), &w.store, &opts, SyncScope::All, &mut sink)
        .await
        .unwrap();
    s.logout().await;

    // The inbox is the first folder named, and it is named before any other.
    let named: Vec<&str> = seen
        .iter()
        .map(|p| p.folder.as_str())
        .filter(|f| !f.is_empty())
        .collect();
    assert_eq!(named.first().copied(), Some("INBOX"));
    let last_inbox = named.iter().rposition(|f| *f == "INBOX").unwrap();
    let first_other = named.iter().position(|f| *f != "INBOX").unwrap();
    assert!(last_inbox < first_other, "inbox work is not interleaved");

    // Counting up inside the inbox, in batches of 15, towards 40.
    let mut inbox_done: Vec<usize> = seen
        .iter()
        .filter(|p| p.folder == "INBOX" && p.messages_total == 40)
        .map(|p| p.messages_done)
        .collect();
    inbox_done.dedup();
    assert_eq!(inbox_done, vec![0, 15, 30, 40]);

    // The totals match what is stored: 40 newest in the inbox, 10 in the others.
    assert_eq!(w.inbox_uids().len(), 40);
    let sent = w
        .store
        .folders(w.acct())
        .unwrap()
        .into_iter()
        .find(|f| f.role == FolderRole::Sent)
        .unwrap();
    assert_eq!(w.store.uids_in_folder(sent.id).unwrap().len(), 10);
    let end = seen.last().unwrap();
    assert_eq!(end.folders_done, end.folders_total);
    assert_eq!(end.stored_total, 50);
    // The older mail is still reachable.
    assert!(w.inbox().floor_uid > 1);
}

#[tokio::test]
async fn a_status_watcher_sees_the_download_advance_before_it_ends() {
    let w = world().await;
    for i in 0..30 {
        w.server.add_message(&format!("Bulk {i}"), "x");
    }
    let (handle, ctl) = sync_channel();
    let opts = SyncOptions {
        batch_size: 5,
        ..SyncOptions::default()
    };
    let mut rx = handle.subscribe();
    let store = Arc::clone(&w.store);
    let account = w.account.clone();
    let watcher = tokio::spawn(async move {
        let mut best = 0;
        loop {
            if rx.changed().await.is_err() {
                break;
            }
            match rx.borrow().clone() {
                SyncStatus::Syncing(p) => best = best.max(p.stored_total),
                SyncStatus::Online { .. } => break,
                _ => {}
            }
        }
        best
    });
    let run = async {
        let env = w.env();
        run_account_loop(&env, &account, store, opts, ctl).await;
    };
    let stop = async {
        let best = tokio::time::timeout(Duration::from_secs(20), watcher)
            .await
            .expect("the first pass finished")
            .unwrap();
        handle.stop();
        best
    };
    let (_, best) = tokio::join!(run, stop);
    // The watcher saw (at least some of) the 33 stored messages being counted.
    assert!(best > 0);
    assert_eq!(w.inbox_uids().len(), 33);
}

#[tokio::test]
async fn rules_are_applied_to_new_inbox_mail_but_not_to_the_first_download() {
    use crate::rules::{Rule, RuleAction, RuleField};
    let w = world().await;
    let rules = vec![
        Rule::new(None, RuleField::Subject, "promo", RuleAction::Star).unwrap(),
        Rule::new(None, RuleField::Subject, "promo", RuleAction::MarkRead).unwrap(),
    ];
    w.store.set_rules(rules);
    let first = w.sync().await;
    assert_eq!(
        first.filtered, 0,
        "old mail is left alone until 'apply now'"
    );
    let uid = w.server.add_message("Big promo today", "x");
    let other = w.server.add_message("Hello", "y");
    let rep = w.sync().await;
    assert_eq!(rep.filtered, 1);
    let inbox = w.inbox().id;
    let m = w.store.message_by_uid(inbox, uid).unwrap().unwrap();
    assert!(m.is_flagged() && !m.is_unread());
    let o = w.store.message_by_uid(inbox, other).unwrap().unwrap();
    assert!(!o.is_flagged() && o.is_unread());
    // The server hears about it on the next pass.
    w.sync().await;
    let f = w.server.flags_of(uid);
    assert!(f.contains(&"\\Flagged".to_owned()) && f.contains(&"\\Seen".to_owned()));
}

#[tokio::test]
async fn apply_now_runs_rules_over_the_inbox_and_a_missing_junk_folder_stops_nothing() {
    use crate::rules::{Rule, RuleAction, RuleField};
    let w = world().await;
    w.sync().await;
    assert_eq!(
        apply_rules_to_inboxes(&w.store).unwrap(),
        0,
        "no rules: no change"
    );
    w.store.set_rules(vec![
        // The mock server has no Junk folder, so this part cannot be done...
        Rule::new(None, RuleField::From, "@example.com", RuleAction::Junk).unwrap(),
        // ...but this one still must be.
        Rule::new(None, RuleField::From, "@example.com", RuleAction::Star).unwrap(),
    ]);
    let n = apply_rules_to_inboxes(&w.store).unwrap();
    assert_eq!(n, w.inbox_uids().len());
    assert_eq!(w.inbox_uids().len(), 3, "nothing was lost");
    // Running it again changes nothing more (everything is starred already).
    assert_eq!(apply_rules_to_inboxes(&w.store).unwrap(), 0);
}

#[test]
fn junk_rules_move_mail_to_the_junk_folder() {
    use crate::imap::Folder;
    use crate::rules::{Rule, RuleAction, RuleField};
    let s = Store::open_in_memory().unwrap();
    let mk = |raw: &str, role| Folder {
        raw_name: raw.into(),
        display_name: raw.into(),
        delimiter: Some("/".into()),
        role,
        selectable: true,
    };
    let f = s
        .upsert_folders(
            "a1",
            &[mk("INBOX", FolderRole::Inbox), mk("Spam", FolderRole::Junk)],
            true,
        )
        .unwrap();
    let inbox = f.iter().find(|f| f.role == FolderRole::Inbox).unwrap().id;
    let new = |uid: u32, from: &str| NewMessage {
        uid,
        message_id: Some(format!("m{uid}@x")),
        subject: "hi".into(),
        from_name: "X".into(),
        from_addr: from.into(),
        to: "me@example.org".into(),
        date: 1_700_000_000,
        ..NewMessage::default()
    };
    s.insert_messages(
        inbox,
        &[new(1, "bad@spam.example"), new(2, "good@friend.example")],
    )
    .unwrap();
    s.set_rules(vec![
        Rule::new(None, RuleField::From, "@spam.example", RuleAction::Junk).unwrap(),
    ]);
    assert_eq!(apply_rules_to_uids(&s, inbox, &[1, 2]), 1);
    assert_eq!(s.uids_in_folder(inbox).unwrap(), vec![2]);
    let ops = s.pending_for_folder(inbox).unwrap();
    assert_eq!(ops.len(), 1);
    assert_eq!(
        (ops[0].kind, ops[0].arg.as_str(), ops[0].uid),
        (OpKind::Move, "Spam", 1)
    );
}

#[tokio::test]
async fn prefetch_plan_covers_new_mail_and_empties_once_downloaded() {
    let w = world().await;
    // Nothing is known before the first sync.
    assert!(prefetch_plan(&w.store, w.acct()).is_empty());
    w.sync().await;
    let plan = prefetch_plan(&w.store, w.acct());
    assert_eq!(
        plan.len(),
        w.inbox_uids().len(),
        "every new message is planned"
    );
    // Another account's mail is never planned.
    assert!(prefetch_plan(&w.store, "someone-else").is_empty());
    // Downloading the planned messages through the same path a person's "open" uses.
    let mut s = w.connect().await;
    for id in &plan {
        let raw = fetch_body(&mut s, &w.store, *id, &SyncOptions::default())
            .await
            .unwrap();
        assert!(!raw.is_empty());
    }
    assert!(
        prefetch_plan(&w.store, w.acct()).is_empty(),
        "nothing left to fetch"
    );
    for id in plan {
        assert!(w.store.message(id).unwrap().unwrap().has_body);
    }
}

#[test]
fn prefetch_plan_respects_count_size_and_budget() {
    let store = Store::open_in_memory().unwrap();
    let f = store
        .upsert_folders(
            "acct",
            &[crate::imap::Folder {
                raw_name: "INBOX".into(),
                display_name: "INBOX".into(),
                delimiter: Some("/".into()),
                role: FolderRole::Inbox,
                selectable: true,
            }],
            true,
        )
        .unwrap();
    let inbox = f[0].id;
    // 40 messages of 500 KB each: the count cap (20) and the byte budget (8 MB = 16) both bite;
    // the budget is stricter.
    let msgs: Vec<_> = (1..=40u32)
        .map(|uid| crate::store::NewMessage {
            uid,
            message_id: Some(format!("m{uid}@x")),
            subject: format!("s{uid}"),
            from_addr: "a@example.com".into(),
            date: i64::from(uid),
            size: Some(500 * 1024),
            ..crate::store::NewMessage::default()
        })
        .collect();
    store.insert_messages(inbox, &msgs).unwrap();
    let plan = prefetch_plan(&store, "acct");
    assert_eq!(plan.len(), 16);
    let newest = store.message_by_uid(inbox, 40).unwrap().unwrap().id;
    assert_eq!(plan[0], newest, "newest first");
}
