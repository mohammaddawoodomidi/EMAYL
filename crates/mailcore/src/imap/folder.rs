//! Folder model and role detection.
//!
//! Roles (Sent, Drafts, Trash...) come from the server's SPECIAL-USE attributes
//! (RFC 6154, recalled). Only when a role has no attribute anywhere do we fall
//! back to well-known **English** folder names; other languages rely on the
//! attributes. The raw name is the folder's identity; the display name is decoded
//! for people.

use std::collections::HashSet;

use super::utf7;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FolderRole {
    Inbox,
    Drafts,
    Sent,
    Archive,
    Junk,
    Trash,
    /// Gmail's "All Mail" style virtual folder.
    All,
    Flagged,
    Other,
}

impl FolderRole {
    fn order(self) -> u8 {
        match self {
            Self::Inbox => 0,
            Self::Drafts => 1,
            Self::Sent => 2,
            Self::Archive => 3,
            Self::Junk => 4,
            Self::Trash => 5,
            Self::All => 6,
            Self::Flagged => 7,
            Self::Other => 8,
        }
    }
}

/// One LIST line, reduced to what we use.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListedFolder {
    pub raw_name: String,
    pub delimiter: Option<String>,
    /// From a SPECIAL-USE attribute.
    pub attr_role: Option<FolderRole>,
    pub noselect: bool,
    pub nonexistent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// Exactly as the server spelled it; use this in commands.
    pub raw_name: String,
    pub display_name: String,
    pub delimiter: Option<String>,
    pub role: FolderRole,
    pub selectable: bool,
}

fn leaf<'a>(name: &'a str, delim: Option<&str>) -> &'a str {
    match delim.and_then(|d| d.chars().next()) {
        Some(d) => name.rsplit(d).next().unwrap_or(name),
        None => name,
    }
}

fn role_by_name(leaf: &str) -> Option<FolderRole> {
    match leaf.trim().to_lowercase().as_str() {
        "sent" | "sent items" | "sent mail" | "sent messages" => Some(FolderRole::Sent),
        "drafts" | "draft" => Some(FolderRole::Drafts),
        "trash" | "deleted items" | "deleted messages" | "bin" => Some(FolderRole::Trash),
        "junk" | "spam" | "junk email" | "junk e-mail" | "bulk mail" => Some(FolderRole::Junk),
        "archive" | "archives" => Some(FolderRole::Archive),
        _ => None,
    }
}

/// Upper bound on folders accepted from one server.
pub const MAX_FOLDERS: usize = 20_000;

pub fn assign_roles(listed: Vec<ListedFolder>) -> Vec<Folder> {
    let mut seen = HashSet::new();
    let mut items: Vec<(ListedFolder, FolderRole)> = Vec::new();
    for f in listed.into_iter().take(MAX_FOLDERS) {
        if f.nonexistent || !seen.insert(f.raw_name.clone()) {
            continue;
        }
        let role = if f.raw_name.eq_ignore_ascii_case("INBOX") {
            FolderRole::Inbox
        } else {
            f.attr_role.unwrap_or(FolderRole::Other)
        };
        items.push((f, role));
    }

    // A role claimed by an attribute is final; the first claimant wins.
    let mut taken: HashSet<FolderRole> = HashSet::new();
    for (_, role) in &mut items {
        if *role != FolderRole::Other && !taken.insert(*role) {
            *role = FolderRole::Other;
        }
    }
    // Name guesses only fill roles nobody claimed, and only selectable folders.
    for (f, role) in &mut items {
        if *role != FolderRole::Other || f.noselect {
            continue;
        }
        let name = utf7::decode(&f.raw_name).unwrap_or_else(|| f.raw_name.clone());
        if let Some(guess) = role_by_name(leaf(&name, f.delimiter.as_deref()))
            && taken.insert(guess)
        {
            *role = guess;
        }
    }

    let mut out: Vec<Folder> = items
        .into_iter()
        .map(|(f, role)| Folder {
            display_name: utf7::decode(&f.raw_name).unwrap_or_else(|| f.raw_name.clone()),
            selectable: !f.noselect,
            raw_name: f.raw_name,
            delimiter: f.delimiter,
            role,
        })
        .collect();
    out.sort_by(|a, b| {
        a.role.order().cmp(&b.role.order()).then_with(|| {
            a.display_name
                .to_lowercase()
                .cmp(&b.display_name.to_lowercase())
        })
    });
    out
}

/// A folder name the way a person should read it. The server's own wrapper is dropped:
/// Gmail files its standard folders under "[Gmail]/" (or "[Google Mail]/" in some countries)
/// and some servers (Dovecot, Courier) put everything under "INBOX." or "INBOX/". Real
/// folders inside the person's own hierarchy ("Projects/Launch") are left as they are.
/// The stored name never changes: only what is shown.
pub fn pretty_name(name: &str) -> String {
    fn strip<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
        let n = prefix.len();
        (s.len() > n && s.is_char_boundary(n) && s[..n].eq_ignore_ascii_case(prefix))
            .then(|| &s[n..])
    }
    let name = name.trim();
    for wrapper in ["[Gmail]", "[Google Mail]", "INBOX"] {
        if let Some(rest) = strip(name, wrapper)
            && let Some(rest) = rest.strip_prefix(['/', '.'])
        {
            let rest = rest.trim();
            if !rest.is_empty() {
                return rest.to_owned();
            }
        }
    }
    name.to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lf(name: &str, role: Option<FolderRole>) -> ListedFolder {
        ListedFolder {
            raw_name: name.into(),
            delimiter: Some("/".into()),
            attr_role: role,
            noselect: false,
            nonexistent: false,
        }
    }

    fn role_of(v: &[Folder], raw: &str) -> FolderRole {
        v.iter().find(|f| f.raw_name == raw).unwrap().role
    }

    #[test]
    fn attributes_win_over_names() {
        let v = assign_roles(vec![
            lf("INBOX", None),
            lf("[Gmail]/Sent Mail", Some(FolderRole::Sent)),
            lf("Sent", None), // a user folder that merely looks like Sent
            lf("[Gmail]/All Mail", Some(FolderRole::All)),
        ]);
        assert_eq!(role_of(&v, "INBOX"), FolderRole::Inbox);
        assert_eq!(role_of(&v, "[Gmail]/Sent Mail"), FolderRole::Sent);
        assert_eq!(role_of(&v, "Sent"), FolderRole::Other);
        assert_eq!(role_of(&v, "[Gmail]/All Mail"), FolderRole::All);
    }

    #[test]
    fn name_fallback_when_no_attributes() {
        let v = assign_roles(vec![
            lf("INBOX", None),
            lf("Sent Items", None),
            lf("Deleted Items", None),
            lf("Junk Email", None),
            lf("Drafts", None),
            lf("Projects", None),
        ]);
        assert_eq!(role_of(&v, "Sent Items"), FolderRole::Sent);
        assert_eq!(role_of(&v, "Deleted Items"), FolderRole::Trash);
        assert_eq!(role_of(&v, "Junk Email"), FolderRole::Junk);
        assert_eq!(role_of(&v, "Drafts"), FolderRole::Drafts);
        assert_eq!(role_of(&v, "Projects"), FolderRole::Other);
    }

    #[test]
    fn inbox_is_case_insensitive_and_sorted_first() {
        let v = assign_roles(vec![
            lf("Zebra", None),
            lf("Inbox", None),
            lf("apple", None),
        ]);
        assert_eq!(v[0].raw_name, "Inbox");
        assert_eq!(v[0].role, FolderRole::Inbox);
        assert_eq!(v[1].raw_name, "apple");
    }

    #[test]
    fn decodes_names_and_keeps_raw_for_commands() {
        let v = assign_roles(vec![lf("Entw&APw-rfe", Some(FolderRole::Drafts))]);
        assert_eq!(v[0].display_name, "Entwürfe");
        assert_eq!(v[0].raw_name, "Entw&APw-rfe");
        assert_eq!(v[0].role, FolderRole::Drafts);
    }

    #[test]
    fn malformed_name_shows_raw_instead_of_failing() {
        let v = assign_roles(vec![lf("Bad&!!-", None)]);
        assert_eq!(v[0].display_name, "Bad&!!-");
    }

    #[test]
    fn duplicates_nonexistent_and_double_claims_are_handled() {
        let mut gone = lf("Ghost", None);
        gone.nonexistent = true;
        let mut container = lf("Containers", None);
        container.noselect = true;
        let v = assign_roles(vec![
            lf("A", Some(FolderRole::Trash)),
            lf("A", None),
            lf("B", Some(FolderRole::Trash)),
            gone,
            container,
        ]);
        assert_eq!(v.len(), 3);
        assert_eq!(role_of(&v, "A"), FolderRole::Trash);
        assert_eq!(role_of(&v, "B"), FolderRole::Other);
        assert!(
            !v.iter()
                .find(|f| f.raw_name == "Containers")
                .unwrap()
                .selectable
        );
    }

    #[test]
    fn name_guess_uses_the_last_path_segment() {
        let v = assign_roles(vec![lf("INBOX/Archive", None)]);
        assert_eq!(v[0].role, FolderRole::Archive);
    }

    #[test]
    fn provider_wrappers_are_not_shown_to_people() {
        assert_eq!(pretty_name("[Gmail]/Sent Mail"), "Sent Mail");
        assert_eq!(pretty_name("[Gmail]/All Mail"), "All Mail");
        assert_eq!(pretty_name("[GMAIL]/Spam"), "Spam");
        assert_eq!(pretty_name("[Google Mail]/Papierkorb"), "Papierkorb");
        assert_eq!(pretty_name("INBOX.Sent"), "Sent");
        assert_eq!(pretty_name("INBOX/Receipts"), "Receipts");
        assert_eq!(pretty_name("inbox.Drafts"), "Drafts");
    }

    #[test]
    fn other_names_are_left_alone() {
        for n in [
            "INBOX",
            "Inbox",
            "Sent",
            "Projects/Launch",
            "[Gmail]",
            "[Gmail]/",
            "INBOX.",
            "INBOXES/Old",
            "[Gmailer]/x",
            "Entwürfe",
            "",
        ] {
            assert_eq!(pretty_name(n), n, "{n:?}");
        }
        assert_eq!(pretty_name("  [Gmail]/Drafts  "), "Drafts");
    }
}
