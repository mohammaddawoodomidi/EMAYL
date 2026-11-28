//! Pure sync planning for IMAP folders: no I/O, so every rule is unit-tested.
//!
//! A message is identified by (account, folder, UIDVALIDITY, UID). If the server
//! reports a different UIDVALIDITY, every cached UID for that folder is
//! meaningless and the folder must be dropped and downloaded again; reusing the
//! old UIDs would attach the wrong mail to the wrong cache entries.

use std::collections::BTreeSet;

/// What the local cache remembers about a folder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalFolderState {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub highest_modseq: Option<u64>,
}

/// What `SELECT` just told us about the folder on the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteFolderState {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub exists: u32,
    /// `Some` only if the server supports CONDSTORE and reported it.
    pub highest_modseq: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlagSync {
    /// Nothing changed since last time.
    Skip,
    /// Ask only for changes after this mod-sequence (CONDSTORE).
    ChangedSince(u64),
    /// Re-read the flags of every cached message.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncPlan {
    /// First time we see this folder.
    Initial,
    /// UIDVALIDITY changed (or the server went backwards): discard the cached
    /// folder and treat it as new.
    Reset,
    Incremental {
        /// Fetch headers for UIDs `>= new_from`, if any new mail may exist.
        new_from: Option<u32>,
        flags: FlagSync,
    },
}

pub fn plan(local: Option<LocalFolderState>, remote: RemoteFolderState) -> SyncPlan {
    let Some(local) = local else {
        return SyncPlan::Initial;
    };
    if local.uid_validity != remote.uid_validity {
        return SyncPlan::Reset;
    }
    // RFC 3501: UIDNEXT must not decrease while UIDVALIDITY is unchanged. A server
    // that does so is broken; refetching is safer than trusting it.
    if remote.uid_next < local.uid_next {
        return SyncPlan::Reset;
    }
    let new_from = (remote.uid_next > local.uid_next).then_some(local.uid_next);
    let flags = match (local.highest_modseq, remote.highest_modseq) {
        (Some(l), Some(r)) if l == r => FlagSync::Skip,
        (Some(l), Some(_)) => FlagSync::ChangedSince(l),
        _ => FlagSync::Full,
    };
    SyncPlan::Incremental { new_from, flags }
}

/// Result of comparing cached UIDs with the server's current UID list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct UidDiff {
    /// On the server but not cached.
    pub added: Vec<u32>,
    /// Cached but gone from the server (deleted or moved elsewhere).
    pub removed: Vec<u32>,
}

pub fn diff_uids(local: &[u32], remote: &[u32]) -> UidDiff {
    let l: BTreeSet<u32> = local.iter().copied().collect();
    let r: BTreeSet<u32> = remote.iter().copied().collect();
    UidDiff {
        added: r.difference(&l).copied().collect(),
        removed: l.difference(&r).copied().collect(),
    }
}

/// Turns UIDs into an IMAP sequence-set such as `1:5,9,12:14`. Input may be
/// unsorted and contain duplicates. UID 0 is not a valid UID and is dropped.
pub fn compress_uids(uids: &[u32]) -> String {
    let set: BTreeSet<u32> = uids.iter().copied().filter(|&u| u != 0).collect();
    let mut out = String::new();
    let mut iter = set.into_iter();
    let Some(first) = iter.next() else {
        return out;
    };
    let (mut start, mut end) = (first, first);
    let push = |out: &mut String, s: u32, e: u32| {
        if !out.is_empty() {
            out.push(',');
        }
        if s == e {
            out.push_str(&s.to_string());
        } else {
            out.push_str(&format!("{s}:{e}"));
        }
    };
    for u in iter {
        if u == end + 1 {
            end = u;
        } else {
            push(&mut out, start, end);
            start = u;
            end = u;
        }
    }
    push(&mut out, start, end);
    out
}

/// Splits UIDs into batches of at most `max_per_batch`, newest first, so the
/// user sees recent mail before old mail during a first download. Each batch is
/// returned already compressed into a sequence-set.
pub fn newest_first_batches(uids: &[u32], max_per_batch: usize) -> Vec<String> {
    let max_per_batch = max_per_batch.max(1);
    let mut sorted: Vec<u32> = uids.iter().copied().filter(|&u| u != 0).collect();
    sorted.sort_unstable_by(|a, b| b.cmp(a));
    sorted.dedup();
    sorted.chunks(max_per_batch).map(compress_uids).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote(v: u32, next: u32, modseq: Option<u64>) -> RemoteFolderState {
        RemoteFolderState {
            uid_validity: v,
            uid_next: next,
            exists: 0,
            highest_modseq: modseq,
        }
    }

    fn local(v: u32, next: u32, modseq: Option<u64>) -> Option<LocalFolderState> {
        Some(LocalFolderState {
            uid_validity: v,
            uid_next: next,
            highest_modseq: modseq,
        })
    }

    #[test]
    fn first_time_is_initial() {
        assert_eq!(plan(None, remote(1, 10, None)), SyncPlan::Initial);
    }

    #[test]
    fn changed_uidvalidity_resets() {
        assert_eq!(
            plan(local(1, 10, None), remote(2, 10, None)),
            SyncPlan::Reset
        );
    }

    #[test]
    fn uidnext_going_backwards_resets() {
        assert_eq!(
            plan(local(1, 10, None), remote(1, 5, None)),
            SyncPlan::Reset
        );
    }

    #[test]
    fn new_mail_is_fetched_from_old_uidnext() {
        assert_eq!(
            plan(local(1, 10, None), remote(1, 14, None)),
            SyncPlan::Incremental {
                new_from: Some(10),
                flags: FlagSync::Full
            }
        );
    }

    #[test]
    fn nothing_new_nothing_changed_with_condstore() {
        assert_eq!(
            plan(local(1, 10, Some(7)), remote(1, 10, Some(7))),
            SyncPlan::Incremental {
                new_from: None,
                flags: FlagSync::Skip
            }
        );
    }

    #[test]
    fn condstore_asks_only_for_changes() {
        assert_eq!(
            plan(local(1, 10, Some(7)), remote(1, 10, Some(9))),
            SyncPlan::Incremental {
                new_from: None,
                flags: FlagSync::ChangedSince(7)
            }
        );
    }

    #[test]
    fn server_that_loses_condstore_falls_back_to_full() {
        assert_eq!(
            plan(local(1, 10, Some(7)), remote(1, 10, None)),
            SyncPlan::Incremental {
                new_from: None,
                flags: FlagSync::Full
            }
        );
    }

    #[test]
    fn diff_finds_added_and_removed() {
        let d = diff_uids(&[1, 2, 3, 5], &[2, 3, 4, 5, 6]);
        assert_eq!(d.added, vec![4, 6]);
        assert_eq!(d.removed, vec![1]);
        assert_eq!(diff_uids(&[], &[]), UidDiff::default());
    }

    #[test]
    fn compress_makes_ranges() {
        assert_eq!(compress_uids(&[]), "");
        assert_eq!(compress_uids(&[7]), "7");
        assert_eq!(
            compress_uids(&[1, 2, 3, 4, 5, 9, 12, 13, 14]),
            "1:5,9,12:14"
        );
        assert_eq!(compress_uids(&[14, 1, 2, 2, 3, 12, 13]), "1:3,12:14");
        assert_eq!(compress_uids(&[0, 0]), "");
        assert_eq!(compress_uids(&[0, 1, 2]), "1:2");
        assert_eq!(
            compress_uids(&[u32::MAX - 1, u32::MAX]),
            "4294967294:4294967295"
        );
    }

    #[test]
    fn batches_are_newest_first_and_bounded() {
        let uids: Vec<u32> = (1..=10).collect();
        assert_eq!(newest_first_batches(&uids, 4), vec!["7:10", "3:6", "1:2"]);
        assert_eq!(newest_first_batches(&[], 4), Vec::<String>::new());
        // A zero batch size cannot loop forever or panic.
        assert_eq!(newest_first_batches(&[1, 2], 0), vec!["2", "1"]);
    }

    #[test]
    fn uid_sets_survive_the_largest_possible_numbers() {
        // Server-supplied numbers can be anything; release builds abort on overflow.
        assert_eq!(
            compress_uids(&[u32::MAX - 1, u32::MAX, 0]),
            "4294967294:4294967295"
        );
        assert_eq!(compress_uids(&[u32::MAX]), "4294967295");
        assert_eq!(compress_uids(&[0]), "");
        assert_eq!(
            newest_first_batches(&[1, u32::MAX, u32::MAX], 1),
            vec!["4294967295".to_owned(), "1".to_owned()]
        );
        let p = plan(
            Some(LocalFolderState {
                uid_validity: 1,
                uid_next: u32::MAX,
                highest_modseq: None,
            }),
            RemoteFolderState {
                uid_validity: 1,
                uid_next: u32::MAX,
                exists: 0,
                highest_modseq: None,
            },
        );
        assert!(matches!(p, SyncPlan::Incremental { new_from: None, .. }));
    }
}
