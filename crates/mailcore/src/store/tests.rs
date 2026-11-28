use super::search::{test_expression, test_parse_date};
use super::*;
use crate::imap::{Folder, FolderRole, MessageFlags};

fn tmp() -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("emayl-store-{}", crate::fsutil::new_id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn folder(raw: &str, role: FolderRole) -> Folder {
    Folder {
        raw_name: raw.into(),
        display_name: raw.into(),
        delimiter: Some("/".into()),
        role,
        selectable: true,
    }
}

fn store_with_inbox() -> (Store, i64) {
    let s = Store::open_in_memory().unwrap();
    let f = s
        .upsert_folders(
            "a1",
            &[
                folder("INBOX", FolderRole::Inbox),
                folder("Sent", FolderRole::Sent),
            ],
            true,
        )
        .unwrap();
    let id = f.iter().find(|f| f.raw_name == "INBOX").unwrap().id;
    (s, id)
}

fn msg(uid: u32, subject: &str, from: &str, date: i64) -> NewMessage {
    NewMessage {
        uid,
        message_id: Some(format!("m{uid}@x")),
        subject: subject.into(),
        from_name: from.into(),
        from_addr: format!("{}@example.com", from.to_lowercase()),
        to: "Me <me@example.org>".into(),
        date,
        ..NewMessage::default()
    }
}

fn one(s: &Store, q: &str) -> Vec<SearchHit> {
    s.search(
        &SearchQuery::parse(q),
        &SearchScope::Everywhere,
        SearchSort::Newest,
        50,
    )
    .unwrap()
}

#[test]
fn sqlite_build_has_expected_features() {
    let s = Store::open_in_memory().unwrap();
    let opts: Vec<String> = s
        .with(|c| {
            let mut st = c.prepare("PRAGMA compile_options")?;
            Ok(st
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<_>>()?)
        })
        .unwrap();
    assert!(
        opts.iter().any(|o| o == "ENABLE_FTS5"),
        "FTS5 missing: {opts:?}"
    );
    assert!(
        !opts.iter().any(|o| o == "ENABLE_RTREE"),
        "R-Tree should be off: {opts:?}"
    );
    assert!(
        opts.iter().any(|o| o == "OMIT_LOAD_EXTENSION"),
        "extension loading should be off: {opts:?}"
    );
    let v = s.sqlite_version().unwrap();
    let nums: Vec<u32> = v.split('.').filter_map(|p| p.parse().ok()).collect();
    assert!(nums >= vec![3, 54, 1], "SQLite too old: {v}");
}

#[test]
fn double_quoted_strings_are_errors_not_literals() {
    // DQS=0: a mistyped column name must fail instead of silently becoming a string.
    let s = Store::open_in_memory().unwrap();
    let r = s.with(|c| Ok(c.query_row("SELECT \"nosuchcolumn\"", [], |r| r.get::<_, String>(0))?));
    assert!(r.is_err());
}

#[test]
fn pragmas_are_applied() {
    let dir = tmp();
    let s = Store::open(dir.as_path()).unwrap();
    s.with(|c| {
        let mode: String = c.query_row("PRAGMA journal_mode", [], |r| r.get(0))?;
        assert_eq!(mode, "wal");
        let sync: i64 = c.query_row("PRAGMA synchronous", [], |r| r.get(0))?;
        assert_eq!(sync, 2, "FULL");
        let fk: i64 = c.query_row("PRAGMA foreign_keys", [], |r| r.get(0))?;
        assert_eq!(fk, 1);
        Ok(())
    })
    .unwrap();
    s.full_check().unwrap();
}

#[test]
fn reopen_keeps_data_and_newer_version_is_refused() {
    let dir = tmp();
    {
        let s = Store::open(dir.as_path()).unwrap();
        let f = s
            .upsert_folders("a1", &[folder("INBOX", FolderRole::Inbox)], true)
            .unwrap();
        s.insert_messages(f[0].id, &[msg(1, "hello", "Ann", 100)])
            .unwrap();
        s.maintain().unwrap();
    }
    let s = Store::open(dir.as_path()).unwrap();
    assert_eq!(
        s.uids_in_folder(s.folders("a1").unwrap()[0].id).unwrap(),
        vec![1]
    );
    s.with(|c| {
        c.pragma_update(None, "user_version", 99)?;
        Ok(())
    })
    .unwrap();
    drop(s);
    let err = Store::open_or_recover(dir.as_path()).unwrap_err();
    assert!(err.to_string().contains("newer version"), "{err}");
    assert!(
        dir.as_path().join("mail.db").exists(),
        "newer database must be left alone"
    );
}

#[test]
fn damaged_database_is_set_aside_and_replaced() {
    let dir = tmp();
    std::fs::write(dir.as_path().join("mail.db"), vec![0xAB; 5000]).unwrap();
    let (s, rec) = Store::open_or_recover(dir.as_path()).unwrap();
    match rec {
        Recovery::Recreated { backup, .. } => assert!(backup.exists()),
        Recovery::None => panic!("expected recovery"),
    }
    s.upsert_folders("a1", &[folder("INBOX", FolderRole::Inbox)], true)
        .unwrap();
    assert_eq!(s.folders("a1").unwrap().len(), 1);
}

#[test]
fn folders_upsert_and_removal_rules() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "x", "Ann", 1)]).unwrap();
    // An empty list never deletes anything.
    s.upsert_folders("a1", &[], true).unwrap();
    assert_eq!(s.folders("a1").unwrap().len(), 2);
    // A list without "Sent" removes it; INBOX and its mail stay.
    let f = s
        .upsert_folders("a1", &[folder("INBOX", FolderRole::Inbox)], true)
        .unwrap();
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].total, 1);
    assert_eq!(f[0].unread, 1);
    // Removing INBOX removes its messages and search entries.
    s.upsert_folders("a1", &[folder("Other", FolderRole::Other)], true)
        .unwrap();
    assert!(one(&s, "x").is_empty());
    let orphans: i64 = s
        .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM search", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(orphans, 0);
}

#[test]
fn insert_is_idempotent_and_indexed() {
    let (s, inbox) = store_with_inbox();
    assert_eq!(
        s.insert_messages(
            inbox,
            &[
                msg(1, "Quarterly report", "Ann", 10),
                msg(2, "Lunch", "Bob", 20)
            ]
        )
        .unwrap(),
        2
    );
    assert_eq!(
        s.insert_messages(inbox, &[msg(1, "Quarterly report", "Ann", 10)])
            .unwrap(),
        0
    );
    let hits = one(&s, "quart");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].message.uid, 1);
    assert_eq!(one(&s, "from:bob").len(), 1);
    assert_eq!(one(&s, "from:bob lunch").len(), 1);
    assert_eq!(one(&s, "from:ann lunch").len(), 0);
    assert_eq!(one(&s, "subject:lunch").len(), 1);
    assert_eq!(one(&s, "to:me").len(), 2);
}

#[test]
fn diacritics_and_case_are_ignored() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "Café Müller Straße", "Zoë", 10)])
        .unwrap();
    assert_eq!(one(&s, "cafe").len(), 1);
    assert_eq!(one(&s, "MULLER").len(), 1);
    assert_eq!(one(&s, "from:zoe").len(), 1);
}

#[test]
fn query_text_cannot_inject_fts_syntax() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "alpha beta", "Ann", 10)])
        .unwrap();
    for q in [
        "alpha OR beta",
        "alpha NOT beta",
        "NEAR(alpha beta)",
        "\"unbalanced",
        "alpha*",
        "{subject}: alpha",
        "^alpha",
        "alpha AND (",
        "a\"b\"\"c",
        "-alpha",
        ")(*&^%$#@!",
        "col:val:more",
        "from:",
        "in:nowhere",
        "before:2026-13-45",
        "\u{0}\u{1}\u{2}",
        "'; DROP TABLE messages; --",
    ] {
        let r = s.search(
            &SearchQuery::parse(q),
            &SearchScope::Everywhere,
            SearchSort::Relevance,
            10,
        );
        assert!(r.is_ok(), "query {q:?} failed: {:?}", r.err());
    }
    // "OR" is a plain word here, so it does not widen the search.
    assert_eq!(one(&s, "alpha OR gamma").len(), 0);
    let n: i64 = s
        .with(|c| Ok(c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn expression_shape() {
    let q = SearchQuery::parse("hello \"big wolf\" from:ann wor");
    assert_eq!(
        test_expression(&q).unwrap(),
        "\"hello\" \"big wolf\" sender : \"ann\" \"wor\"*"
    );
    let q = SearchQuery::parse("hello \"big wolf\"");
    assert_eq!(
        test_expression(&q).unwrap(),
        "\"hello\" \"big wolf\"",
        "a quoted last term is not a prefix"
    );
    assert!(test_expression(&SearchQuery::parse("is:unread")).is_none());
    assert!(SearchQuery::parse("   ").is_empty());
    assert!(SearchQuery::parse("!!! ???").is_empty());
    let long = "a ".repeat(100);
    assert!(
        test_expression(&SearchQuery::parse(&long))
            .unwrap()
            .matches('"')
            .count()
            <= 2 * 20 + 2
    );
}

#[test]
fn filters_work_without_text() {
    let (s, inbox) = store_with_inbox();
    let mut a = msg(1, "one", "Ann", 1_794_873_600 - 10);
    a.flags = FLAG_SEEN;
    let mut b = msg(2, "two", "Bob", 1_794_873_600 + 10);
    b.maybe_attachments = true;
    s.insert_messages(inbox, &[a, b]).unwrap();
    assert_eq!(one(&s, "is:unread").len(), 1);
    assert_eq!(one(&s, "is:read")[0].message.uid, 1);
    assert_eq!(one(&s, "has:attachment")[0].message.uid, 2);
    assert_eq!(one(&s, "before:2026-11-17")[0].message.uid, 1);
    assert_eq!(one(&s, "after:2026-11-17")[0].message.uid, 2);
    assert_eq!(one(&s, "in:inbox").len(), 2);
    assert_eq!(one(&s, "in:sent").len(), 0);
}

#[test]
fn dates() {
    assert_eq!(test_parse_date("2026-11-17"), Some(1_794_873_600));
    assert_eq!(test_parse_date("1970-01-01"), Some(0));
    assert_eq!(test_parse_date("2024-02-29"), Some(1_709_164_800));
    assert_eq!(test_parse_date("2025-02-29"), None);
    assert_eq!(test_parse_date("2026-00-10"), None);
    assert_eq!(
        test_parse_date("2026-1-5"),
        Some(test_parse_date("2026-01-05").unwrap())
    );
    assert_eq!(test_parse_date("nonsense"), None);
    assert_eq!(test_parse_date("2026-11-17-3"), None);
}

#[test]
fn junk_and_trash_are_hidden_unless_asked() {
    let s = Store::open_in_memory().unwrap();
    let f = s
        .upsert_folders(
            "a1",
            &[
                folder("INBOX", FolderRole::Inbox),
                folder("Spam", FolderRole::Junk),
                folder("Bin", FolderRole::Trash),
            ],
            true,
        )
        .unwrap();
    for (i, fr) in f.iter().enumerate() {
        s.insert_messages(
            fr.id,
            &[msg(1, "needle", "Ann", 10 + i64::try_from(i).unwrap())],
        )
        .unwrap();
    }
    assert_eq!(one(&s, "needle").len(), 1);
    assert_eq!(one(&s, "needle in:junk").len(), 1);
    assert_eq!(one(&s, "needle in:trash").len(), 1);
    let trash = f.iter().find(|f| f.role == FolderRole::Trash).unwrap().id;
    let r = s
        .search(
            &SearchQuery::parse("needle"),
            &SearchScope::Folder(trash),
            SearchSort::Newest,
            10,
        )
        .unwrap();
    assert_eq!(r.len(), 1);
}

#[test]
fn deleting_leaves_no_search_orphans() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(
        inbox,
        &[msg(1, "alpha", "Ann", 1), msg(2, "beta", "Bob", 2)],
    )
    .unwrap();
    assert_eq!(s.delete_uids(inbox, &[1, 99]).unwrap(), 1);
    assert!(one(&s, "alpha").is_empty());
    assert_eq!(one(&s, "beta").len(), 1);
    s.full_check().unwrap();
    let (m, fts): (i64, i64) = s
        .with(|c| {
            Ok((
                c.query_row("SELECT COUNT(*) FROM messages", [], |r| r.get(0))?,
                c.query_row("SELECT COUNT(*) FROM search", [], |r| r.get(0))?,
            ))
        })
        .unwrap();
    assert_eq!(m, fts);
}

#[test]
fn body_is_stored_rendered_and_searchable() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "Plans", "Ann", 5)])
        .unwrap();
    let id = s.message_by_uid(inbox, 1).unwrap().unwrap().id;
    assert!(!s.message(id).unwrap().unwrap().has_body);
    let raw = b"From: Ann <ann@example.com>\r\nSubject: Plans\r\nContent-Type: text/plain\r\n\r\nWe meet near the lighthouse at   noon.\r\n";
    s.store_body(id, raw).unwrap();
    let m = s.message(id).unwrap().unwrap();
    assert!(m.has_body);
    assert_eq!(m.preview, "We meet near the lighthouse at noon.");
    assert_eq!(s.body(id).unwrap().unwrap(), raw);
    let hits = one(&s, "lighthouse");
    assert_eq!(hits.len(), 1);
    assert!(hits[0].snippet.text.contains("lighthouse"));
    assert!(!hits[0].snippet.highlights.is_empty());
    let h = &hits[0].snippet.highlights[0];
    assert!(
        hits[0].snippet.text[h.clone()]
            .to_lowercase()
            .starts_with("lighthouse")
    );
    // Trimming drops the raw bytes but keeps the search text.
    assert!(s.trim_bodies(0).unwrap() > 0);
    assert!(s.body(id).unwrap().is_none());
    assert!(!s.message(id).unwrap().unwrap().has_body);
    assert_eq!(one(&s, "lighthouse").len(), 1);
    // A full index rebuild keeps body text too.
    s.rebuild_search_index().unwrap();
    assert_eq!(one(&s, "lighthouse").len(), 1);
    assert_eq!(one(&s, "from:ann").len(), 1);
    s.full_check().unwrap();
}

#[test]
fn garbage_body_is_kept_without_crashing() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "x", "Ann", 5)]).unwrap();
    let id = s.message_by_uid(inbox, 1).unwrap().unwrap().id;
    s.store_body(id, &[0xFF, 0x00, 0xFE, b'\r', b'\n']).unwrap();
    assert!(s.body(id).unwrap().is_some());
    assert!(s.store_body(9999, b"x").is_err());
}

#[test]
fn threads_follow_replies_and_late_ancestors() {
    let (s, inbox) = store_with_inbox();
    let root = msg(1, "Topic", "Ann", 10);
    let mut reply = msg(2, "Re: Topic", "Bob", 20);
    reply.in_reply_to = Some("m1@x".into());
    reply.references = vec!["m1@x".into()];
    let mut third = msg(3, "Re: Topic", "Cy", 30);
    third.in_reply_to = Some("m2@x".into());
    third.references = vec!["m1@x".into(), "m2@x".into()];
    // Arrive out of order: the newest reply first, then root, then middle.
    s.insert_messages(inbox, &[third]).unwrap();
    s.insert_messages(inbox, &[root]).unwrap();
    s.insert_messages(inbox, &[reply]).unwrap();
    s.insert_messages(inbox, &[msg(4, "Other", "Dee", 40)])
        .unwrap();
    let threads = s
        .list_threads(&ListScope::Folder(inbox), false, None, 10)
        .unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].subject, "Other");
    assert_eq!(threads[1].count, 3);
    assert_eq!(threads[1].unread, 3);
    assert_eq!(threads[1].newest_date, 30);
    let all = s.thread_messages(&threads[1].thread_key).unwrap();
    assert_eq!(all.iter().map(|m| m.uid).collect::<Vec<_>>(), vec![1, 2, 3]);
}

#[test]
fn child_that_only_knows_its_parent_is_rejoined() {
    let (s, inbox) = store_with_inbox();
    // Y replies to X only; X replies to R. Y arrives before X.
    let mut y = msg(3, "Re: Re: T", "Cy", 30);
    y.in_reply_to = Some("m2@x".into());
    y.references = vec!["m2@x".into()];
    let mut x = msg(2, "Re: T", "Bob", 20);
    x.in_reply_to = Some("m1@x".into());
    x.references = vec!["m1@x".into()];
    s.insert_messages(inbox, &[y]).unwrap();
    s.insert_messages(inbox, &[x]).unwrap();
    let t = s
        .list_threads(&ListScope::Folder(inbox), false, None, 10)
        .unwrap();
    assert_eq!(t.len(), 1, "{t:?}");
    assert_eq!(t[0].count, 2);
}

#[test]
fn threads_do_not_cross_accounts() {
    let s = Store::open_in_memory().unwrap();
    let a = s
        .upsert_folders("a1", &[folder("INBOX", FolderRole::Inbox)], true)
        .unwrap()[0]
        .id;
    let b = s
        .upsert_folders("a2", &[folder("INBOX", FolderRole::Inbox)], true)
        .unwrap()[0]
        .id;
    let mut m1 = msg(1, "Same", "Ann", 1);
    m1.message_id = Some("shared@x".into());
    let mut m2 = m1.clone();
    m2.uid = 1;
    s.insert_messages(a, &[m1]).unwrap();
    s.insert_messages(b, &[m2]).unwrap();
    let all = s
        .list_threads(&ListScope::AllInboxes, false, None, 10)
        .unwrap();
    assert_eq!(all.len(), 2);
}

#[test]
fn message_list_pages_by_cursor() {
    let (s, inbox) = store_with_inbox();
    let batch: Vec<NewMessage> = (1..=25)
        .map(|i| msg(i, &format!("s{i}"), "Ann", i64::from(i / 5)))
        .collect();
    s.insert_messages(inbox, &batch).unwrap();
    let mut seen = Vec::new();
    let mut cursor = None;
    loop {
        let page = s
            .list_messages(&ListScope::Folder(inbox), false, cursor, 7)
            .unwrap();
        if page.is_empty() {
            break;
        }
        cursor = Some(page.last().unwrap().cursor());
        seen.extend(page.iter().map(|m| m.uid));
    }
    assert_eq!(seen.len(), 25, "ties on date must not skip or repeat rows");
    let mut sorted = seen.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), 25);
}

#[test]
fn thread_list_pages_by_cursor() {
    let (s, inbox) = store_with_inbox();
    let batch: Vec<NewMessage> = (1..=12)
        .map(|i| msg(i, &format!("s{i}"), "Ann", i64::from(i / 3)))
        .collect();
    s.insert_messages(inbox, &batch).unwrap();
    let mut n = 0;
    let mut cursor: Option<(i64, String)> = None;
    loop {
        let page = s
            .list_threads(&ListScope::Folder(inbox), false, cursor.clone(), 5)
            .unwrap();
        if page.is_empty() {
            break;
        }
        let l = page.last().unwrap();
        cursor = Some((l.newest_date, l.thread_key.clone()));
        n += page.len();
    }
    assert_eq!(n, 12);
}

#[test]
fn future_dates_are_not_believed() {
    use crate::imap::MessageSummary;
    let hdr = b"From: Spam <s@example.com>\r\nSubject: hi\r\nDate: Mon, 01 Jan 2035 00:00:00 +0000\r\nMessage-ID: <f@x>\r\n".to_vec();
    let sum = MessageSummary {
        uid: 1,
        flags: crate::imap::MessageFlags::default(),
        size: Some(10),
        internal_date: Some(1_794_873_600),
        header: hdr,
        header_truncated: false,
    };
    let m = NewMessage::from_summary(&sum);
    assert_eq!(m.date, 1_794_873_600);
    assert_eq!(m.message_id.as_deref(), Some("f@x"));
    assert_eq!(m.from_addr, "s@example.com");
}

#[test]
fn from_summary_handles_garbage_headers() {
    use crate::imap::MessageSummary;
    for h in [
        &b""[..],
        &b"\xff\xfe\x00garbage"[..],
        &b"Subject: \r\n"[..],
        &b"From: <<<>>>\r\nReferences: <a b> <c>\r\n"[..],
    ] {
        let sum = MessageSummary {
            uid: 7,
            flags: crate::imap::MessageFlags::default(),
            size: None,
            internal_date: None,
            header: h.to_vec(),
            header_truncated: false,
        };
        let m = NewMessage::from_summary(&sum);
        assert_eq!(m.uid, 7);
        assert_eq!(m.date, 0);
    }
}

#[test]
fn flag_updates_respect_waiting_local_changes() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "a", "Ann", 1), msg(2, "b", "Bob", 2)])
        .unwrap();
    s.queue_flag(inbox, 1, "\\Seen", true).unwrap();
    assert_eq!(
        s.message_by_uid(inbox, 1).unwrap().unwrap().flags & FLAG_SEEN,
        FLAG_SEEN
    );
    // The server still says "unread" for 1 (our change not sent yet) and "starred" for 2.
    let starred = MessageFlags {
        flagged: true,
        ..MessageFlags::default()
    };
    let changed = s
        .apply_flag_updates(inbox, &[(1, MessageFlags::default()), (2, starred)])
        .unwrap();
    assert_eq!(changed, 1);
    assert!(s.message_by_uid(inbox, 1).unwrap().unwrap().flags & FLAG_SEEN != 0);
    assert!(s.message_by_uid(inbox, 2).unwrap().unwrap().is_flagged());
}

#[test]
fn pending_ops_coalesce_and_validate() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "a", "Ann", 1)]).unwrap();
    s.queue_flag(inbox, 1, "\\Seen", true).unwrap();
    s.queue_flag(inbox, 1, "\\Seen", false).unwrap();
    s.queue_flag(inbox, 1, "\\Flagged", true).unwrap();
    let ops = s.pending_for_folder(inbox).unwrap();
    assert_eq!(ops.len(), 2);
    assert_eq!(ops[0].kind, OpKind::RemoveFlag);
    assert_eq!(ops[1].kind, OpKind::AddFlag);
    assert!(s.queue_flag(inbox, 1, "\\Bogus", true).is_err());
    assert!(s.queue_flag(inbox, 1, "bad flag", true).is_err());
    assert!(s.queue_flag(inbox, 1, "", true).is_err());
    assert!(s.queue_flag(inbox, 99, "\\Seen", true).is_err());
    assert!(s.queue_move(inbox, 1, "").is_err());
    assert_eq!(s.pending_count("a1").unwrap(), 2);
    assert_eq!(s.folders_with_pending("a1").unwrap(), vec![inbox]);
    s.complete_op(ops[0].id).unwrap();
    assert_eq!(s.pending_count("a1").unwrap(), 1);
}

#[test]
fn move_and_expunge_remove_locally_and_clear_flag_ops() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "a", "Ann", 1), msg(2, "b", "Bob", 2)])
        .unwrap();
    s.queue_flag(inbox, 1, "\\Seen", true).unwrap();
    s.queue_move(inbox, 1, "Archive").unwrap();
    s.queue_expunge(inbox, 2).unwrap();
    assert!(s.uids_in_folder(inbox).unwrap().is_empty());
    assert!(one(&s, "a").is_empty());
    let ops = s.pending_for_folder(inbox).unwrap();
    assert_eq!(
        ops.iter().map(|o| o.kind).collect::<Vec<_>>(),
        vec![OpKind::Move, OpKind::Expunge]
    );
    assert_eq!(ops[0].arg, "Archive");
}

#[test]
fn failing_ops_are_dropped_after_the_limit() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "a", "Ann", 1)]).unwrap();
    s.queue_flag(inbox, 1, "\\Seen", true).unwrap();
    let id = s.pending_for_folder(inbox).unwrap()[0].id;
    assert!(!s.fail_op(id, "boom", 3).unwrap());
    assert!(!s.fail_op(id, "boom", 3).unwrap());
    assert_eq!(s.pending_for_folder(inbox).unwrap()[0].attempts, 2);
    assert!(s.fail_op(id, "boom", 3).unwrap());
    assert!(s.pending_for_folder(inbox).unwrap().is_empty());
}

#[test]
fn reset_folder_clears_messages_and_pending() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "alpha", "Ann", 1)])
        .unwrap();
    s.queue_flag(inbox, 1, "\\Seen", true).unwrap();
    s.set_folder_state(
        inbox,
        FolderState {
            uid_validity: 5,
            uid_next: 2,
            highest_modseq: Some(9),
            floor_uid: 1,
            last_synced: 1,
        },
    )
    .unwrap();
    s.reset_folder(inbox).unwrap();
    let f = s.folder(inbox).unwrap().unwrap();
    assert_eq!((f.uid_validity, f.uid_next, f.total), (None, None, 0));
    assert!(s.pending_for_folder(inbox).unwrap().is_empty());
    assert!(one(&s, "alpha").is_empty());
}

#[test]
fn deleting_an_account_removes_everything_of_it() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(inbox, &[msg(1, "alpha", "Ann", 1)])
        .unwrap();
    s.queue_flag(inbox, 1, "\\Seen", true).unwrap();
    s.delete_account("a1").unwrap();
    assert!(s.folders("a1").unwrap().is_empty());
    let n: i64 = s.with(|c| Ok(c.query_row("SELECT (SELECT COUNT(*) FROM messages)+(SELECT COUNT(*) FROM search)+(SELECT COUNT(*) FROM pending_ops)+(SELECT COUNT(*) FROM bodies)", [], |r| r.get(0))?)).unwrap();
    assert_eq!(n, 0);
}

#[test]
fn very_long_fields_are_capped() {
    use crate::imap::MessageSummary;
    let long = "x".repeat(100_000);
    let hdr =
        format!("From: {long} <a@example.com>\r\nSubject: {long}\r\nTo: {long}@example.com\r\n")
            .into_bytes();
    let sum = MessageSummary {
        uid: 1,
        flags: crate::imap::MessageFlags::default(),
        size: None,
        internal_date: Some(5),
        header: hdr,
        header_truncated: false,
    };
    let m = NewMessage::from_summary(&sum);
    assert!(m.subject.chars().count() <= 500);
    assert!(m.from_name.chars().count() <= 500);
    assert!(m.to.chars().count() <= 2000);
}

#[test]
fn deleting_an_account_leaves_no_trace_in_the_rows_and_other_accounts_stay() {
    let s = Store::open_in_memory().unwrap();
    let folders = |acct: &str| {
        s.upsert_folders(acct, &[folder("INBOX", FolderRole::Inbox)], true)
            .unwrap()
    };
    let (a, b) = (folders("gone")[0].id, folders("kept")[0].id);
    s.insert_messages(a, &[msg(1, "private plans", "Ann", 10)])
        .unwrap();
    s.insert_messages(b, &[msg(1, "other plans", "Ben", 10)])
        .unwrap();
    s.delete_account("gone").unwrap();
    assert!(
        one(&s, "private").is_empty(),
        "search index must be empty too"
    );
    assert_eq!(one(&s, "other").len(), 1);
}

#[test]
fn start_up_cleanup_removes_only_accounts_that_no_longer_exist() {
    let s = Store::open_in_memory().unwrap();
    for (acct, word) in [("a", "alpha"), ("b", "bravo"), ("c", "charlie")] {
        let f = s
            .upsert_folders(acct, &[folder("INBOX", FolderRole::Inbox)], true)
            .unwrap();
        s.insert_messages(f[0].id, &[msg(1, &format!("note {word}"), "Ann", 10)])
            .unwrap();
    }
    assert_eq!(s.retain_accounts(&["a", "c"]).unwrap(), 1);
    assert!(one(&s, "bravo").is_empty());
    assert_eq!(one(&s, "alpha").len(), 1);
    assert_eq!(one(&s, "charlie").len(), 1);
    assert_eq!(s.retain_accounts(&["a", "c"]).unwrap(), 0);
}

#[test]
fn deleted_mail_is_overwritten_and_the_schema_is_not_trusted() {
    let s = Store::open_in_memory().unwrap();
    let pragma = |name: &str| -> i64 {
        s.with(|c| Ok(c.query_row(&format!("PRAGMA {name}"), [], |r| r.get(0))?))
            .unwrap()
    };
    assert_eq!(pragma("secure_delete"), 1, "ON, not FAST");
    assert_eq!(pragma("trusted_schema"), 0);
}

#[test]
fn the_text_of_a_deleted_account_is_not_left_in_the_database_files() {
    let dir = tmp();
    let s = Store::open(&dir).unwrap();
    let f = s
        .upsert_folders("gone", &[folder("INBOX", FolderRole::Inbox)], true)
        .unwrap();
    let needle = "zebra-unique-marker-4711";
    s.insert_messages(f[0].id, &[msg(1, needle, "Ann", 10)])
        .unwrap();
    s.delete_account("gone").unwrap();
    // Look while the database is still open: closing it would fold the log in by itself
    // and hide the problem this test is about.
    for name in ["mail.db", "mail.db-wal"] {
        let bytes = std::fs::read(dir.join(name)).unwrap_or_default();
        assert!(
            !bytes.windows(needle.len()).any(|w| w == needle.as_bytes()),
            "{name} still contains deleted text"
        );
    }
}

#[cfg(unix)]
#[test]
fn database_files_are_private() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tmp();
    let s = Store::open(&dir).unwrap();
    let f = s
        .upsert_folders("a", &[folder("INBOX", FolderRole::Inbox)], true)
        .unwrap();
    s.insert_messages(f[0].id, &[msg(1, "x", "Ann", 10)])
        .unwrap();
    let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(dir.join("mail.db")), 0o600);
    for side in ["mail.db-wal", "mail.db-shm"] {
        let p = dir.join(side);
        if p.exists() {
            assert_eq!(mode(p), 0o600, "{side}");
        }
    }
}

#[test]
fn nothing_typed_into_the_search_box_can_break_the_search() {
    let (s, inbox) = store_with_inbox();
    s.insert_messages(
        inbox,
        &[msg(1, "alpha report", "Ann", 1), msg(2, "beta", "Bob", 2)],
    )
    .unwrap();
    let long = "word ".repeat(5000);
    let hostile: Vec<&str> = vec![
        "\"",
        "\"\"\"",
        "\"unclosed phrase",
        "alpha AND",
        "OR OR OR",
        "NOT",
        "NEAR(alpha beta, 2)",
        "alpha NEAR beta",
        "alpha*",
        "*",
        "^alpha",
        "-alpha",
        "(alpha",
        "alpha)",
        "{alpha}",
        "subject:",
        "from:",
        "from::",
        ":alpha",
        "search:alpha",
        "rank:alpha",
        "col1:alpha",
        "before:",
        "before:9999-99-99",
        "after:0000-00-00",
        "before:9999-12-31",
        "after:1970-01-01 before:1970-01-01",
        "in:",
        "in:nonexistent",
        "has:attachment",
        "is:unread is:read",
        "alpha'; DROP TABLE messages; --",
        "\0alpha",
        "al\0pha",
        "\u{202e}alpha",
        "\u{1F600}",
        "\u{0}\u{1}\u{2}",
        "%",
        "_",
        "\\",
        "alpha\r\nbeta",
        &long,
    ];
    for q in hostile {
        let parsed = SearchQuery::parse(q);
        let r = s.search(&parsed, &SearchScope::default(), SearchSort::default(), 50);
        assert!(r.is_ok(), "search for {q:?} failed: {:?}", r.err());
    }
    // The tables are all still there afterwards.
    assert_eq!(one(&s, "alpha").len(), 1);
    s.full_check().unwrap();
}

#[test]
fn messages_with_nul_and_control_characters_can_be_stored_and_found() {
    let (s, inbox) = store_with_inbox();
    let mut m = msg(1, "plans\0 for\u{7} lunch", "An\0n", 5);
    m.from_addr = "ann\0@example.com".into();
    s.insert_messages(inbox, &[m]).unwrap();
    let id = s.message_by_uid(inbox, 1).unwrap().unwrap().id;
    let raw = b"From: ann@example.com\r\nSubject: x\r\nContent-Type: text/plain\r\n\r\nmeet\0 at the\x07 lighthouse\r\n";
    s.store_body(id, raw).unwrap();
    assert_eq!(one(&s, "lighthouse").len(), 1);
    assert_eq!(one(&s, "lunch").len(), 1);
    s.full_check().unwrap();
}

#[test]
fn uncached_recent_lists_newest_small_messages_without_text() {
    let (s, inbox) = store_with_inbox();
    let mut big = msg(3, "Huge", "Cy", 30);
    big.size = Some(9_000_000);
    let mut small = msg(2, "Small", "Bob", 20);
    small.size = Some(4_000);
    s.insert_messages(
        inbox,
        &[
            msg(1, "Old", "Ann", 10),
            small,
            big,
            msg(4, "New", "Di", 40),
        ],
    )
    .unwrap();
    let id = |uid| s.message_by_uid(inbox, uid).unwrap().unwrap().id;
    // Newest first; the huge one is skipped; an unknown size is allowed.
    assert_eq!(
        s.uncached_recent(inbox, 1_000_000, 10).unwrap(),
        vec![id(4), id(2), id(1)]
    );
    // The limit applies after the filter.
    assert_eq!(
        s.uncached_recent(inbox, 1_000_000, 2).unwrap(),
        vec![id(4), id(2)]
    );
    // A message whose text is stored drops out of the list.
    let raw = b"From: a@example.com\r\nSubject: x\r\nContent-Type: text/plain\r\n\r\nhi\r\n";
    s.store_body(id(4), raw).unwrap();
    assert_eq!(
        s.uncached_recent(inbox, 1_000_000, 10).unwrap(),
        vec![id(2), id(1)]
    );
}
