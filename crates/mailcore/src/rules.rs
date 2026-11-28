//! Blocked senders and filter rules.
//!
//! A rule looks at one part of a *new* message in an Inbox (who it is from, the subject, who it
//! was sent to) and then does one thing with it: send it to Junk, archive it, mark it read or
//! star it. Blocking a sender is a rule "From = that address → Junk".
//!
//! Rules are plain, local and predictable on purpose: no scripts, no regular expressions (a
//! hostile or mistyped pattern cannot hang the program), and no message text is ever looked at,
//! only the header fields that are already stored. They are kept in their own small file
//! (`rules.json`, readable only by you), not in the mail database, so rebuilding the mail copy
//! never loses them.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsutil::{create_private_dir, new_id, read_limited, write_atomic};
use crate::store::MessageRow;
use crate::{Error, Result};

pub const MAX_RULES: usize = 200;
pub const MAX_NEEDLE_CHARS: usize = 200;
const MIN_NEEDLE_CHARS: usize = 3;
const FILE_NAME: &str = "rules.json";
const MAX_FILE_BYTES: u64 = 1024 * 1024;
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleField {
    From,
    Subject,
    To,
}

impl RuleField {
    pub const ALL: [Self; 3] = [Self::From, Self::Subject, Self::To];
    pub fn label(self) -> &'static str {
        match self {
            Self::From => "From",
            Self::Subject => "Subject",
            Self::To => "To",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuleAction {
    Junk,
    Archive,
    MarkRead,
    Star,
}

impl RuleAction {
    pub const ALL: [Self; 4] = [Self::Junk, Self::Archive, Self::MarkRead, Self::Star];
    pub fn label(self) -> &'static str {
        match self {
            Self::Junk => "Move to Junk",
            Self::Archive => "Archive",
            Self::MarkRead => "Mark as read",
            Self::Star => "Star",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rule {
    pub id: String,
    /// The account this applies to; `None` means every account.
    pub account: Option<String>,
    pub field: RuleField,
    /// Lower-case text to look for (see [`Rule::matches`]).
    pub needle: String,
    pub action: RuleAction,
}

/// What to do with one message.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Decision {
    /// `Junk` or `Archive`: the first matching rule decides.
    pub move_to: Option<RuleAction>,
    pub mark_read: bool,
    pub star: bool,
}

impl Decision {
    pub fn is_empty(&self) -> bool {
        self.move_to.is_none() && !self.mark_read && !self.star
    }
}

impl Rule {
    /// Checks and normalizes what the person typed.
    pub fn new(
        account: Option<String>,
        field: RuleField,
        needle: &str,
        action: RuleAction,
    ) -> std::result::Result<Self, String> {
        let needle = needle.trim().to_lowercase();
        let chars = needle.chars().count();
        if needle.chars().any(char::is_control) {
            return Err(
                "The text to look for cannot contain line breaks or control characters.".into(),
            );
        }
        if chars > MAX_NEEDLE_CHARS {
            return Err(format!("Use at most {MAX_NEEDLE_CHARS} characters."));
        }
        if chars < MIN_NEEDLE_CHARS {
            return Err(format!(
                "Use at least {MIN_NEEDLE_CHARS} characters, otherwise the rule would catch far too much."
            ));
        }
        if field == RuleField::From {
            if let Some(domain) = needle.strip_prefix('@')
                && (!domain.contains('.') || domain.starts_with('.') || domain.ends_with('.'))
            {
                return Err("A whole domain looks like @example.com.".into());
            }
            if needle.matches('@').count() > 1 {
                return Err("That does not look like an address.".into());
            }
        }
        Ok(Self {
            id: new_id(),
            account,
            field,
            needle,
            action,
        })
    }

    /// A blocked sender is the rule "From → Junk".
    pub fn is_block(&self) -> bool {
        self.field == RuleField::From && self.action == RuleAction::Junk
    }

    /// From: `@example.com` matches that domain and its subdomains; a full address matches
    /// exactly; anything else is looked for inside the sender's name and address.
    /// Subject and To: the text is looked for anywhere. Upper/lower case never matters.
    pub fn matches(&self, m: &MessageRow) -> bool {
        if self.account.as_ref().is_some_and(|a| *a != m.account) {
            return false;
        }
        match self.field {
            RuleField::From => {
                let addr = m.from_addr.trim().to_lowercase();
                if let Some(domain) = self.needle.strip_prefix('@') {
                    addr.ends_with(&format!("@{domain}")) || addr.ends_with(&format!(".{domain}"))
                } else if self.needle.contains('@') {
                    addr == self.needle
                } else {
                    addr.contains(&self.needle) || m.from_name.to_lowercase().contains(&self.needle)
                }
            }
            RuleField::Subject => m.subject.to_lowercase().contains(&self.needle),
            RuleField::To => m.to.to_lowercase().contains(&self.needle),
        }
    }

    pub fn describe(&self) -> String {
        format!(
            "{} contains “{}” → {}",
            self.field.label(),
            self.needle,
            self.action.label()
        )
    }
}

/// Combines every matching rule into one decision.
pub fn decide(rules: &[Rule], m: &MessageRow) -> Decision {
    let mut d = Decision::default();
    for r in rules.iter().filter(|r| r.matches(m)) {
        match r.action {
            RuleAction::Junk | RuleAction::Archive => {
                if d.move_to.is_none() {
                    d.move_to = Some(r.action);
                }
            }
            RuleAction::MarkRead => d.mark_read = true,
            RuleAction::Star => d.star = true,
        }
    }
    d
}

#[derive(Serialize, Deserialize)]
struct RuleFile {
    version: u32,
    rules: Vec<Rule>,
}

/// The rules file.
#[derive(Debug, Clone)]
pub struct RuleBook {
    path: PathBuf,
}

impl RuleBook {
    pub fn open(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    /// All rules. A missing file is an empty list; a damaged one is an error (it is never
    /// silently replaced, so a typo in it cannot wipe the list).
    pub fn load(&self) -> Result<Vec<Rule>> {
        let raw = match read_limited(&self.path, MAX_FILE_BYTES) {
            Ok(r) => r,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let f: RuleFile = serde_json::from_slice(&raw).map_err(|e| Error::CorruptFile {
            path: self.path.clone(),
            reason: e.to_string(),
        })?;
        if f.version != FORMAT_VERSION {
            return Err(Error::CorruptFile {
                path: self.path.clone(),
                reason: format!("unknown format {}", f.version),
            });
        }
        Ok(f.rules
            .into_iter()
            .filter(|r| {
                r.needle.chars().count() >= MIN_NEEDLE_CHARS
                    && r.needle.chars().count() <= MAX_NEEDLE_CHARS
            })
            .take(MAX_RULES)
            .collect())
    }

    pub fn save(&self, rules: &[Rule]) -> Result<()> {
        let f = RuleFile {
            version: FORMAT_VERSION,
            rules: rules.iter().take(MAX_RULES).cloned().collect(),
        };
        let json = serde_json::to_vec_pretty(&f).map_err(|e| Error::CorruptFile {
            path: self.path.clone(),
            reason: e.to_string(),
        })?;
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir)?;
        }
        write_atomic(&self.path, &json)
    }

    /// Adds a rule unless an identical one exists. Returns the new list.
    pub fn add(&self, rule: Rule) -> Result<Vec<Rule>> {
        let mut all = self.load()?;
        if all.iter().any(|r| {
            r.account == rule.account
                && r.field == rule.field
                && r.needle == rule.needle
                && r.action == rule.action
        }) {
            return Ok(all);
        }
        if all.len() >= MAX_RULES {
            return Err(Error::InvalidState("too many rules (200 at most)"));
        }
        all.push(rule);
        self.save(&all)?;
        Ok(all)
    }

    pub fn remove(&self, id: &str) -> Result<Vec<Rule>> {
        let mut all = self.load()?;
        all.retain(|r| r.id != id);
        self.save(&all)?;
        Ok(all)
    }

    /// Drops the rules that belong to a removed account.
    pub fn remove_account(&self, account: &str) -> Result<Vec<Rule>> {
        let mut all = self.load()?;
        all.retain(|r| r.account.as_deref() != Some(account));
        self.save(&all)?;
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("emayl-rules-{}", new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn msg(from: &str, name: &str, subject: &str, to: &str) -> MessageRow {
        MessageRow {
            id: 1,
            account: "acc1".into(),
            folder_id: 1,
            uid: 1,
            message_id: None,
            thread_key: "t".into(),
            subject: subject.into(),
            from_name: name.into(),
            from_addr: from.into(),
            to: to.into(),
            date: 0,
            size: None,
            flags: 0,
            maybe_attachments: false,
            has_body: false,
            preview: String::new(),
        }
    }

    fn rule(f: RuleField, n: &str, a: RuleAction) -> Rule {
        Rule::new(None, f, n, a).unwrap()
    }

    #[test]
    fn a_full_address_matches_only_that_address() {
        let r = rule(RuleField::From, "Spam@Example.com", RuleAction::Junk);
        assert!(r.matches(&msg("spam@example.com", "", "", "")));
        assert!(r.matches(&msg("SPAM@EXAMPLE.COM", "", "", "")));
        assert!(!r.matches(&msg("notspam@example.com", "", "", "")));
        assert!(!r.matches(&msg("spam@example.com.au", "", "", "")));
    }

    #[test]
    fn a_domain_matches_subdomains_but_not_look_alikes() {
        let r = rule(RuleField::From, "@example.com", RuleAction::Junk);
        assert!(r.matches(&msg("a@example.com", "", "", "")));
        assert!(r.matches(&msg("a@mail.example.com", "", "", "")));
        assert!(!r.matches(&msg("a@evilexample.com", "", "", "")));
        assert!(!r.matches(&msg("a@example.com.evil.net", "", "", "")));
    }

    #[test]
    fn plain_text_looks_in_name_and_address() {
        let r = rule(RuleField::From, "newsletter", RuleAction::Archive);
        assert!(r.matches(&msg("x@shop.example", "Weekly Newsletter", "", "")));
        assert!(r.matches(&msg("newsletter@shop.example", "Shop", "", "")));
        assert!(!r.matches(&msg("x@shop.example", "Shop", "newsletter", "")));
    }

    #[test]
    fn subject_and_to_rules_work_and_ignore_case() {
        let s = rule(RuleField::Subject, "invoice", RuleAction::Star);
        assert!(s.matches(&msg("a@b.c", "", "Your INVOICE is ready", "")));
        let t = rule(RuleField::To, "team@work.example", RuleAction::MarkRead);
        assert!(t.matches(&msg("a@b.c", "", "", "Team <Team@Work.example>")));
    }

    #[test]
    fn a_rule_for_one_account_ignores_the_others() {
        let r = Rule::new(
            Some("acc2".into()),
            RuleField::Subject,
            "sale",
            RuleAction::Junk,
        )
        .unwrap();
        assert!(!r.matches(&msg("a@b.c", "", "big sale", "")));
        let r = Rule::new(
            Some("acc1".into()),
            RuleField::Subject,
            "sale",
            RuleAction::Junk,
        )
        .unwrap();
        assert!(r.matches(&msg("a@b.c", "", "big sale", "")));
    }

    #[test]
    fn bad_rules_are_refused() {
        assert!(Rule::new(None, RuleField::Subject, "", RuleAction::Junk).is_err());
        assert!(Rule::new(None, RuleField::Subject, "ab", RuleAction::Junk).is_err());
        assert!(Rule::new(None, RuleField::Subject, "a\nb c", RuleAction::Junk).is_err());
        assert!(Rule::new(None, RuleField::From, "@com", RuleAction::Junk).is_err());
        assert!(Rule::new(None, RuleField::From, "@.com", RuleAction::Junk).is_err());
        assert!(Rule::new(None, RuleField::From, "a@b@c.d", RuleAction::Junk).is_err());
        assert!(Rule::new(None, RuleField::Subject, &"x".repeat(201), RuleAction::Junk).is_err());
    }

    #[test]
    fn the_first_move_wins_and_flags_add_up() {
        let rules = vec![
            rule(RuleField::Subject, "sale", RuleAction::Archive),
            rule(RuleField::From, "@shop.example", RuleAction::Junk),
            rule(RuleField::Subject, "sale", RuleAction::MarkRead),
            rule(RuleField::Subject, "sale", RuleAction::Star),
        ];
        let d = decide(&rules, &msg("a@shop.example", "", "Big sale", ""));
        assert_eq!(
            d,
            Decision {
                move_to: Some(RuleAction::Archive),
                mark_read: true,
                star: true
            }
        );
        assert!(decide(&rules, &msg("a@b.c", "", "hello", "")).is_empty());
    }

    #[test]
    fn rules_survive_a_restart_and_a_damaged_file_is_not_overwritten() {
        let dir = tmp();
        let book = RuleBook::open(&dir);
        assert!(book.load().unwrap().is_empty());
        let r = rule(RuleField::From, "a@b.example", RuleAction::Junk);
        let id = r.id.clone();
        book.add(r.clone()).unwrap();
        assert_eq!(book.add(r).unwrap().len(), 1, "duplicates are ignored");
        assert_eq!(RuleBook::open(&dir).load().unwrap().len(), 1);
        assert!(RuleBook::open(&dir).load().unwrap()[0].is_block());
        assert!(book.remove(&id).unwrap().is_empty());

        std::fs::write(dir.join(FILE_NAME), b"{ not json").unwrap();
        assert!(book.load().is_err());
        assert!(
            book.add(rule(RuleField::From, "x@y.example", RuleAction::Junk))
                .is_err()
        );
        assert_eq!(std::fs::read(dir.join(FILE_NAME)).unwrap(), b"{ not json");
    }

    #[test]
    fn removing_an_account_drops_only_its_rules() {
        let dir = tmp();
        let book = RuleBook::open(&dir);
        book.add(
            Rule::new(
                Some("acc1".into()),
                RuleField::Subject,
                "aaa",
                RuleAction::Junk,
            )
            .unwrap(),
        )
        .unwrap();
        book.add(rule(RuleField::Subject, "bbb", RuleAction::Junk))
            .unwrap();
        let left = book.remove_account("acc1").unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].needle, "bbb");
    }
}
