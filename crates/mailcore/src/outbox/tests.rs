use super::*;
use crate::bounce::{BounceRecipient, Confidence};
use crate::compose::{Mailbox, OutgoingMessage, compose};
use crate::email::EmailAddress;
use crate::net::{FailureKind, Stage};

fn tmpdir() -> PathBuf {
    std::env::temp_dir().join(format!("emayl-outbox-{}", new_id()))
}

fn composed(to: &[&str]) -> ComposedMessage {
    let mb = |a: &str| Mailbox {
        name: String::new(),
        address: EmailAddress::parse(a).unwrap(),
    };
    compose(
        &OutgoingMessage {
            from: Some(mb("alice@example.com")),
            to: to.iter().map(|a| mb(a)).collect(),
            subject: "Hi".into(),
            text: "body\n".into(),
            ..OutgoingMessage::default()
        },
        0,
    )
    .unwrap()
}

fn setup(to: &[&str]) -> (Outbox, OutboxItem, PathBuf) {
    let dir = tmpdir();
    let ob = Outbox::open(&dir).unwrap();
    let item = ob
        .enqueue(&AccountId::generate(), &composed(to), 1000, false)
        .unwrap();
    (ob, item, dir)
}

fn rej(addr: &str, code: u16, enh: &str, permanent: bool) -> Rejection {
    Rejection {
        address: addr.into(),
        code,
        enhanced: Some(enh.into()),
        text: "server text".into(),
        permanent,
    }
}

fn attempt(ob: &Outbox, id: &str, r: &AttemptResult, now: i64) -> OutboxItem {
    ob.begin_attempt(id).unwrap();
    ob.finish_attempt(id, r, now, 1.0).unwrap()
}

fn delivered(addrs: &[&str]) -> AttemptResult {
    AttemptResult::Delivered {
        accepted: addrs.iter().map(|s| (*s).to_owned()).collect(),
        rejected: vec![],
        server_reply: "queued as X".into(),
    }
}

#[test]
fn message_is_on_disk_before_any_attempt_and_listed() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    assert_eq!(item.state, OutboxState::Queued);
    assert!(
        ob.message_bytes(&item.id)
            .unwrap()
            .starts_with(b"Message-ID:")
            || !ob.message_bytes(&item.id).unwrap().is_empty()
    );
    let l = ob.list().unwrap();
    assert_eq!(l.items.len(), 1);
    assert!(l.unreadable.is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn successful_send_marks_sent_and_deletes_the_stored_message() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let done = attempt(&ob, &item.id, &delivered(&["bob@example.org"]), 1100);
    assert_eq!(done.state, OutboxState::Sent);
    assert_eq!(done.sent_at, Some(1100));
    assert_eq!(done.attempts, 1);
    assert!(
        ob.message_bytes(&item.id).is_err(),
        "plaintext message must not linger"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn temporary_failure_defers_with_growing_backoff_then_succeeds() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let conn =
        AttemptResult::Connection(ConnError::new(Stage::Connect, FailureKind::Timeout, "slow"));
    let a = attempt(&ob, &item.id, &conn, 2000);
    assert_eq!(a.state, OutboxState::Deferred);
    let first_wait = a.next_attempt_at - 2000;
    assert_eq!(first_wait, 60, "1 minute (jitter=1.0)");
    assert!(ob.due(2000 + 59).unwrap().is_empty());
    assert_eq!(ob.due(2000 + 60).unwrap().len(), 1);
    let b = attempt(&ob, &item.id, &conn, 3000);
    assert_eq!(b.next_attempt_at - 3000, 120);
    assert!(b.last_error.as_ref().unwrap().advice.contains("try"));
    let c = attempt(&ob, &item.id, &delivered(&["bob@example.org"]), 4000);
    assert_eq!((c.state, c.attempts), (OutboxState::Sent, 3));
    assert!(c.last_error.is_none());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn backoff_is_capped_at_six_hours() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let conn =
        AttemptResult::Connection(ConnError::new(Stage::Connect, FailureKind::Refused, "down"));
    let mut now = 1000;
    let mut last = item;
    for _ in 0..20 {
        last = attempt(&ob, &last.id, &conn, now);
        now = last.next_attempt_at;
        if last.state != OutboxState::Deferred {
            break;
        }
    }
    let wait = RETRY.cap.as_secs() as i64;
    assert!(last.next_attempt_at - now <= wait);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn partial_success_never_resends_to_those_already_delivered() {
    let (ob, item, dir) = setup(&["good@example.org", "later@example.org", "gone@example.org"]);
    let r = AttemptResult::Delivered {
        accepted: vec!["good@example.org".into()],
        rejected: vec![
            rej("later@example.org", 450, "4.2.0", false),
            rej("gone@example.org", 550, "5.1.1", true),
        ],
        server_reply: "ok".into(),
    };
    let a = attempt(&ob, &item.id, &r, 2000);
    assert_eq!(
        a.state,
        OutboxState::Deferred,
        "one recipient is still pending"
    );
    assert_eq!(a.pending_recipients(), vec!["later@example.org"]);
    assert!(a.has_failed_recipients());
    let gone = a
        .recipients
        .iter()
        .find(|r| r.address == "gone@example.org")
        .unwrap();
    assert_eq!(gone.status, RecipientStatus::Failed);
    assert!(
        gone.note
            .as_ref()
            .unwrap()
            .advice
            .contains("does not exist")
    );
    // Retry: only the pending one is attempted, and its success completes the item.
    let b = attempt(&ob, &item.id, &delivered(&["later@example.org"]), 9000);
    assert_eq!(b.state, OutboxState::Sent);
    assert!(
        b.has_failed_recipients(),
        "the failed recipient stays visible"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn permanent_refusal_of_everything_fails_with_plain_advice() {
    let (ob, item, dir) = setup(&["gone@example.org"]);
    let r = AttemptResult::Refused {
        phase: Phase::Recipients,
        code: 550,
        enhanced: Some("5.1.1".into()),
        text: "No such user".into(),
        permanent: true,
        rejections: vec![rej("gone@example.org", 550, "5.1.1", true)],
    };
    let a = attempt(&ob, &item.id, &r, 2000);
    assert_eq!(a.state, OutboxState::Failed);
    assert!(
        a.last_error
            .as_ref()
            .unwrap()
            .advice
            .contains("does not exist")
    );
    assert!(ob.due(i64::MAX).unwrap().is_empty());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn data_phase_refusals_split_into_retry_and_failure() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let temp = AttemptResult::Refused {
        phase: Phase::DataEnd,
        code: 451,
        enhanced: Some("4.3.0".into()),
        text: "local error".into(),
        permanent: false,
        rejections: vec![],
    };
    assert_eq!(
        attempt(&ob, &item.id, &temp, 2000).state,
        OutboxState::Deferred
    );
    let perm = AttemptResult::Refused {
        phase: Phase::DataEnd,
        code: 554,
        enhanced: Some("5.7.1".into()),
        text: "spam".into(),
        permanent: true,
        rejections: vec![],
    };
    let a = attempt(&ob, &item.id, &perm, 3000);
    assert_eq!(a.state, OutboxState::Failed);
    assert_eq!(a.recipients[0].status, RecipientStatus::Failed);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn oversize_message_fails_without_retries() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let a = attempt(
        &ob,
        &item.id,
        &AttemptResult::TooLarge { limit: 25_000_000 },
        2000,
    );
    assert_eq!(a.state, OutboxState::Failed);
    assert!(a.last_error.unwrap().advice.contains("25 MB"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn account_problems_park_the_message_until_the_account_is_fixed() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let auth = AttemptResult::Connection(ConnError::new(
        Stage::Login,
        FailureKind::AuthRejected,
        "bad password",
    ));
    let a = attempt(&ob, &item.id, &auth, 2000);
    assert_eq!(a.state, OutboxState::Failed);
    assert!(a.last_error.as_ref().unwrap().account_problem);
    assert_eq!(
        a.recipients[0].status,
        RecipientStatus::Pending,
        "recipients are not blamed"
    );
    assert!(ob.due(i64::MAX).unwrap().is_empty());
    // A different account's fix does not release it...
    assert_eq!(ob.retry_account(&AccountId::generate(), 3000).unwrap(), 0);
    // ...its own does.
    assert_eq!(ob.retry_account(&a.account, 3000).unwrap(), 1);
    let q = ob.get(&item.id).unwrap();
    assert_eq!(
        (q.state, q.attempts, q.next_attempt_at),
        (OutboxState::Queued, 0, 3000)
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn gives_up_after_three_days_and_can_be_retried_manually() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let conn =
        AttemptResult::Connection(ConnError::new(Stage::Connect, FailureKind::Timeout, "slow"));
    let a = attempt(&ob, &item.id, &conn, 1000 + GIVE_UP_AFTER_SECS + 1);
    assert_eq!(a.state, OutboxState::Failed);
    assert!(a.last_error.as_ref().unwrap().advice.contains("3 days"));
    let r = ob.retry_now(&item.id, 500_000).unwrap();
    assert_eq!(
        (r.state, r.queued_at, r.attempts),
        (OutboxState::Queued, 500_000, 0)
    );
    assert_eq!(r.recipients[0].status, RecipientStatus::Pending);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn too_many_attempts_also_gives_up() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let conn =
        AttemptResult::Connection(ConnError::new(Stage::Connect, FailureKind::Timeout, "slow"));
    let mut it = ob.get(&item.id).unwrap();
    it.attempts = MAX_ATTEMPTS - 1;
    ob.save(&it).unwrap();
    let a = attempt(&ob, &item.id, &conn, 1100);
    assert_eq!(a.state, OutboxState::Failed);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn crash_during_sending_is_recovered_on_open() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    ob.begin_attempt(&item.id).unwrap();
    drop(ob); // "crash"
    let ob = Outbox::open(&dir).unwrap();
    assert_eq!(ob.get(&item.id).unwrap().state, OutboxState::Queued);
    assert_eq!(
        ob.get(&item.id).unwrap().attempts,
        1,
        "the interrupted attempt is still counted"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn state_machine_rejects_out_of_order_calls() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    assert!(
        ob.finish_attempt(&item.id, &delivered(&[]), 1, 0.0)
            .is_err(),
        "no attempt running"
    );
    ob.begin_attempt(&item.id).unwrap();
    assert!(ob.begin_attempt(&item.id).is_err(), "already sending");
    assert!(ob.cancel(&item.id).is_err(), "cannot cancel mid-send");
    assert!(ob.retry_now(&item.id, 5).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn cancel_removes_waiting_messages_entirely() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    ob.cancel(&item.id).unwrap();
    assert!(ob.list().unwrap().items.is_empty());
    assert!(ob.message_bytes(&item.id).is_err());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn ids_from_outside_cannot_escape_the_directory() {
    let (ob, _item, dir) = setup(&["bob@example.org"]);
    for bad in ["../../etc/passwd", "", "..", "a/b"] {
        assert!(ob.get(bad).is_err());
        assert!(ob.message_bytes(bad).is_err());
        assert!(ob.cancel(bad).is_err());
    }
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn corrupt_status_files_are_reported_not_fatal_and_left_in_place() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let junk = dir.join(format!("{}.json", new_id()));
    fs::write(&junk, b"{not json").unwrap();
    let l = ob.list().unwrap();
    assert_eq!(l.items.len(), 1);
    assert_eq!(l.unreadable, vec![junk.clone()]);
    assert!(junk.exists());
    // A file whose id field disagrees with its name is also rejected.
    let liar = dir.join(format!("{}.json", new_id()));
    fs::copy(dir.join(format!("{}.json", item.id)), &liar).unwrap();
    assert_eq!(ob.list().unwrap().unreadable.len(), 2);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn bounce_marks_failed_recipients_and_ignores_other_accounts() {
    let (ob, item, dir) = setup(&["good@example.org", "gone@example.org"]);
    attempt(
        &ob,
        &item.id,
        &delivered(&["good@example.org", "gone@example.org"]),
        2000,
    );
    let report = BounceReport {
        original_message_id: item.message_id.clone(),
        recipients: vec![BounceRecipient {
            address: "gone@example.org".into(),
            action: BounceAction::Failed,
            status: Some("5.1.1".into()),
            diagnostic: Some("smtp; 550 5.1.1 unknown".into()),
        }],
        kind: BounceKind::Hard,
        confidence: Confidence::Standard,
    };
    assert!(
        !ob.record_bounce(&AccountId::generate(), &report, 3000)
            .unwrap(),
        "other account's bounce ignored"
    );
    assert!(ob.record_bounce(&item.account, &report, 3000).unwrap());
    let b = ob.get(&item.id).unwrap();
    assert_eq!(b.state, OutboxState::Bounced);
    let gone = b
        .recipients
        .iter()
        .find(|r| r.address == "gone@example.org")
        .unwrap();
    assert_eq!(gone.status, RecipientStatus::Failed);
    assert!(
        gone.note
            .as_ref()
            .unwrap()
            .advice
            .contains("does not exist")
    );
    let good = b
        .recipients
        .iter()
        .find(|r| r.address == "good@example.org")
        .unwrap();
    assert_eq!(good.status, RecipientStatus::Delivered);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn soft_bounces_and_unknown_ids_change_nothing() {
    let (ob, item, dir) = setup(&["slow@example.org"]);
    attempt(&ob, &item.id, &delivered(&["slow@example.org"]), 2000);
    let soft = BounceReport {
        original_message_id: item.message_id.clone(),
        recipients: vec![BounceRecipient {
            address: "slow@example.org".into(),
            action: BounceAction::Delayed,
            status: Some("4.4.1".into()),
            diagnostic: None,
        }],
        kind: BounceKind::Soft,
        confidence: Confidence::Standard,
    };
    ob.record_bounce(&item.account, &soft, 3000).unwrap();
    assert_eq!(ob.get(&item.id).unwrap().state, OutboxState::Sent);
    let stranger = BounceReport {
        original_message_id: "unknown@example.com".into(),
        ..soft
    };
    assert!(!ob.record_bounce(&item.account, &stranger, 3000).unwrap());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unspecific_hard_bounce_flags_the_item_without_blaming_a_recipient() {
    let (ob, item, dir) = setup(&["a@example.org", "b@example.org"]);
    attempt(
        &ob,
        &item.id,
        &delivered(&["a@example.org", "b@example.org"]),
        2000,
    );
    let report = BounceReport {
        original_message_id: item.message_id.clone(),
        recipients: vec![],
        kind: BounceKind::Hard,
        confidence: Confidence::Heuristic,
    };
    ob.record_bounce(&item.account, &report, 3000).unwrap();
    let b = ob.get(&item.id).unwrap();
    assert_eq!(b.state, OutboxState::Bounced);
    assert!(
        b.recipients
            .iter()
            .all(|r| r.status == RecipientStatus::Delivered)
    );
    assert!(b.last_error.is_some());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn purge_removes_only_old_finished_items() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    attempt(&ob, &item.id, &delivered(&["bob@example.org"]), 2000);
    let waiting = ob
        .enqueue(
            &AccountId::generate(),
            &composed(&["x@example.org"]),
            2000,
            false,
        )
        .unwrap();
    assert_eq!(ob.purge_finished(2000 + 10, 3600).unwrap(), 0);
    assert_eq!(ob.purge_finished(2000 + 7200, 3600).unwrap(), 1);
    let left = ob.list().unwrap().items;
    assert_eq!(left.len(), 1);
    assert_eq!(left[0].id, waiting.id);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn sent_copy_pending_items_are_not_purged_until_handled() {
    let dir = tmpdir();
    let ob = Outbox::open(&dir).unwrap();
    let item = ob
        .enqueue(
            &AccountId::generate(),
            &composed(&["bob@example.org"]),
            1000,
            true,
        )
        .unwrap();
    let done = attempt(&ob, &item.id, &delivered(&["bob@example.org"]), 2000);
    assert_eq!(done.sent_copy, SentCopy::Pending);
    assert_eq!(ob.purge_finished(i64::MAX / 2, 0).unwrap(), 0);
    ob.set_sent_copy(&item.id, SentCopy::Saved).unwrap();
    assert_eq!(ob.purge_finished(i64::MAX / 2, 0).unwrap(), 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn due_lists_oldest_first_and_skips_future_and_finished() {
    let dir = tmpdir();
    let ob = Outbox::open(&dir).unwrap();
    let acct = AccountId::generate();
    let a = ob
        .enqueue(&acct, &composed(&["a@example.org"]), 100, false)
        .unwrap();
    let b = ob
        .enqueue(&acct, &composed(&["b@example.org"]), 50, false)
        .unwrap();
    let c = ob
        .enqueue(&acct, &composed(&["c@example.org"]), 10, false)
        .unwrap();
    attempt(&ob, &c.id, &delivered(&["c@example.org"]), 200);
    let ids: Vec<_> = ob.due(1_000).unwrap().into_iter().map(|i| i.id).collect();
    assert_eq!(ids, vec![b.id, a.id]);
    assert!(ob.due(40).unwrap().len() <= 1);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn unsendable_messages_fail_without_retry_and_stay_retryable() {
    let (ob, item, dir) = setup(&["bob@example.org"]);
    let a = attempt(
        &ob,
        &item.id,
        &AttemptResult::Unsendable("file damaged".into()),
        2000,
    );
    assert_eq!(a.state, OutboxState::Failed);
    assert!(!a.last_error.as_ref().unwrap().account_problem);
    assert_eq!(a.recipients[0].status, RecipientStatus::Failed);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn stored_message_is_kept_until_the_sent_copy_is_dealt_with() {
    let dir = tmpdir();
    let ob = Outbox::open(&dir).unwrap();
    let item = ob
        .enqueue(
            &AccountId::generate(),
            &composed(&["bob@example.org"]),
            1000,
            true,
        )
        .unwrap();
    attempt(&ob, &item.id, &delivered(&["bob@example.org"]), 2000);
    assert!(
        ob.message_bytes(&item.id).is_ok(),
        "needed to make the Sent copy"
    );
    ob.set_sent_copy(&item.id, SentCopy::Saved).unwrap();
    assert!(
        ob.message_bytes(&item.id).is_err(),
        "removed once nothing needs it"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn only_finished_messages_can_be_dismissed() {
    let dir = tmpdir();
    let ob = Outbox::open(&dir).unwrap();
    let item = ob
        .enqueue(
            &AccountId::generate(),
            &composed(&["bob@example.org"]),
            1000,
            false,
        )
        .unwrap();
    assert!(ob.forget(&item.id).is_err(), "still queued");
    attempt(&ob, &item.id, &delivered(&["bob@example.org"]), 2000);
    ob.forget(&item.id).unwrap();
    assert!(ob.get(&item.id).is_err());
    assert!(ob.message_bytes(&item.id).is_err());

    let waiting = ob
        .enqueue(
            &AccountId::generate(),
            &composed(&["bob@example.org"]),
            1000,
            true,
        )
        .unwrap();
    attempt(&ob, &waiting.id, &delivered(&["bob@example.org"]), 2000);
    assert!(
        ob.forget(&waiting.id).is_err(),
        "the Sent copy is still owed"
    );
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn forgetting_an_account_deletes_its_queue_and_message_files_only() {
    let dir = tmpdir();
    let ob = Outbox::open(&dir).unwrap();
    let (gone, kept) = (AccountId::generate(), AccountId::generate());
    let a = ob
        .enqueue(&gone, &composed(&["bob@example.com"]), 1000, false)
        .unwrap();
    let b = ob
        .enqueue(&gone, &composed(&["carol@example.com"]), 1001, false)
        .unwrap();
    let c = ob
        .enqueue(&kept, &composed(&["dave@example.com"]), 1002, false)
        .unwrap();
    assert_eq!(ob.forget_account(&gone).unwrap(), 2);
    for id in [&a.id, &b.id] {
        assert!(!ob.meta_path(id).exists() && !ob.eml_path(id).exists());
    }
    assert!(ob.get(&c.id).is_ok() && ob.message_bytes(&c.id).is_ok());
    assert_eq!(
        ob.forget_account(&gone).unwrap(),
        0,
        "doing it twice is harmless"
    );
}

#[test]
fn start_up_removes_message_files_without_a_status_file_and_temp_files() {
    let dir = tmpdir();
    let ob = Outbox::open(&dir).unwrap();
    let item = ob
        .enqueue(
            &AccountId::generate(),
            &composed(&["bob@example.com"]),
            1000,
            false,
        )
        .unwrap();
    let orphan = dir.join(format!("{}.eml", new_id()));
    fs::write(&orphan, b"Subject: left behind\r\n\r\nsecret").unwrap();
    let tmp_file = dir.join(format!(".{}.json.tmp", new_id()));
    fs::write(&tmp_file, b"{").unwrap();
    let stranger = dir.join("not-an-id.eml");
    fs::write(&stranger, b"x").unwrap();
    drop(ob);
    let ob = Outbox::open(&dir).unwrap();
    assert!(!orphan.exists() && !tmp_file.exists() && !stranger.exists());
    assert!(
        ob.message_bytes(&item.id).is_ok(),
        "a queued message must survive"
    );
}

#[test]
fn cancelling_and_starting_to_send_at_the_same_moment_never_both_win() {
    // The window (Cancel button) and the background sender use the same outbox from
    // different threads. For every message exactly one of them may succeed: if both did,
    // the person would be told "cancelled" while the message was sent anyway.
    use std::sync::{Arc, Barrier};
    let dir = tmpdir();
    let ob = Arc::new(Outbox::open(&dir).unwrap());
    let acct = AccountId::generate();
    let mut both = 0;
    let mut neither = 0;
    for _ in 0..150 {
        let item = ob
            .enqueue(&acct, &composed(&["bob@example.org"]), 1000, false)
            .unwrap();
        let gate = Arc::new(Barrier::new(2));
        let (o1, g1, id1) = (Arc::clone(&ob), Arc::clone(&gate), item.id.clone());
        let (o2, g2, id2) = (Arc::clone(&ob), Arc::clone(&gate), item.id.clone());
        let sender = std::thread::spawn(move || {
            g1.wait();
            o1.begin_attempt(&id1).is_ok()
        });
        let canceller = std::thread::spawn(move || {
            g2.wait();
            o2.cancel(&id2).is_ok()
        });
        let (started, cancelled) = (sender.join().unwrap(), canceller.join().unwrap());
        match (started, cancelled) {
            (true, true) => both += 1,
            (false, false) => neither += 1,
            _ => {}
        }
        // Whatever won, the files agree with the answer given.
        if cancelled {
            assert!(ob.get(&item.id).is_err());
        }
    }
    assert_eq!(both, 0, "{both} messages were both cancelled and started");
    assert_eq!(neither, 0, "{neither} messages were neither");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_delayed_message_is_not_due_until_its_time_and_can_be_taken_back_before() {
    let dir = tmpdir();
    let ob = Outbox::open(&dir).unwrap();
    let item = ob
        .enqueue_after(
            &AccountId::generate(),
            &composed(&["bob@example.org"]),
            1000,
            false,
            10,
        )
        .unwrap();
    assert_eq!(item.next_attempt_at, 1010);
    assert!(ob.due(1005).unwrap().is_empty(), "still waiting");
    assert_eq!(
        ob.due(1010).unwrap().len(),
        1,
        "due at the end of the delay"
    );
    // Taking it back removes it completely, as for any waiting message.
    ob.cancel(&item.id).unwrap();
    assert!(ob.due(2000).unwrap().is_empty());
    assert!(ob.list().unwrap().items.is_empty());
    // A silly delay is capped (5 minutes at most), and 0 means "now".
    let a = ob
        .enqueue_after(
            &AccountId::generate(),
            &composed(&["bob@example.org"]),
            1000,
            false,
            u32::MAX,
        )
        .unwrap();
    assert_eq!(a.next_attempt_at, 1300);
    let b = ob
        .enqueue(
            &AccountId::generate(),
            &composed(&["bob@example.org"]),
            1000,
            false,
        )
        .unwrap();
    assert_eq!(b.next_attempt_at, 1000);
}
