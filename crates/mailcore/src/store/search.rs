//! Searching the local mail (FTS5), with a small, forgiving query language:
//!
//! `words` (all must match; the last one also matches as a prefix), `"exact phrase"`,
//! `from:` `to:` `subject:` (a word or a quoted phrase), `is:unread|read|starred|answered`,
//! `has:attachment`, `in:inbox|sent|drafts|archive|junk|trash`, `before:YYYY-MM-DD`,
//! `after:YYYY-MM-DD`. Anything else is plain text. There is no way to write
//! raw FTS5 syntax: every term is quoted before it reaches SQLite.

use rusqlite::types::Value;

use super::Store;
use super::messages::{MSG_COLS, MessageRow, msg_row};
use crate::Result;

const MAX_TERMS: usize = 20;
const MAX_QUERY_CHARS: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SearchSort {
    #[default]
    Newest,
    Relevance,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SearchScope {
    #[default]
    Everywhere,
    Account(String),
    Folder(i64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Any,
    Subject,
    Sender,
    Recipients,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Term {
    column: Column,
    text: String,
    prefix: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SearchQuery {
    terms: Vec<Term>,
    unread: Option<bool>,
    starred: bool,
    answered: bool,
    attachment: bool,
    roles: Vec<&'static str>,
    before: Option<i64>,
    after: Option<i64>,
}

/// A piece of text with the parts that matched marked.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Snippet {
    pub text: String,
    /// Byte ranges in `text`.
    pub highlights: Vec<std::ops::Range<usize>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    pub message: MessageRow,
    pub snippet: Snippet,
}

struct Token {
    key: Option<String>,
    text: String,
    quoted: bool,
}

fn tokenize(input: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut chars = input.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let mut key = String::new();
        let mut text = String::new();
        let mut quoted = false;
        let mut in_quote = false;
        let mut seen_colon = false;
        for c in chars.by_ref() {
            if c == '"' {
                in_quote = !in_quote;
                quoted = true;
                continue;
            }
            if !in_quote && c.is_whitespace() {
                break;
            }
            if !in_quote
                && !seen_colon
                && !quoted
                && c == ':'
                && !text.is_empty()
                && text.chars().all(|x| x.is_ascii_alphabetic())
            {
                seen_colon = true;
                key = std::mem::take(&mut text).to_ascii_lowercase();
                continue;
            }
            text.push(c);
        }
        out.push(Token {
            key: seen_colon.then_some(key),
            text,
            quoted,
        });
        if out.len() >= MAX_TERMS * 3 {
            break;
        }
    }
    out
}

fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if (y % 4 == 0 && y % 100 != 0) || y % 400 == 0 {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD` → seconds since the epoch at 00:00 UTC.
fn parse_date(s: &str) -> Option<i64> {
    let mut it = s.trim().split(['-', '/']);
    let (y, m, d) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some()
        || y.len() != 4
        || m.is_empty()
        || m.len() > 2
        || d.is_empty()
        || d.len() > 2
    {
        return None;
    }
    let (y, m, d): (i64, i64, i64) = (y.parse().ok()?, m.parse().ok()?, d.parse().ok()?);
    if !(1970..=9999).contains(&y) || !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    Some(days_from_civil(y, m, d) * 86_400)
}

fn role_name(s: &str) -> Option<&'static str> {
    Some(match s.to_ascii_lowercase().as_str() {
        "inbox" => "inbox",
        "sent" => "sent",
        "drafts" | "draft" => "drafts",
        "archive" => "archive",
        "junk" | "spam" => "junk",
        "trash" | "bin" | "deleted" => "trash",
        _ => return None,
    })
}

/// True if the text contains something the index would keep.
fn has_word_chars(s: &str) -> bool {
    s.chars().any(char::is_alphanumeric)
}

fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

impl SearchQuery {
    /// Parses what the person typed. Never fails: what is not understood is
    /// searched as plain text.
    pub fn parse(input: &str) -> Self {
        // Control characters (a NUL makes SQLite's text-search parser fail) act as spaces.
        let input: String = input
            .chars()
            .take(MAX_QUERY_CHARS)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect();
        let mut q = Self::default();
        let mut last_plain_is_open = false;
        for t in tokenize(&input) {
            let as_text = || (t.text.clone(), t.quoted);
            let handled = match (t.key.as_deref(), t.text.to_ascii_lowercase().as_str()) {
                (Some("is"), "unread") => {
                    q.unread = Some(true);
                    true
                }
                (Some("is"), "read") => {
                    q.unread = Some(false);
                    true
                }
                (Some("is"), "starred" | "flagged") => {
                    q.starred = true;
                    true
                }
                (Some("is"), "answered" | "replied") => {
                    q.answered = true;
                    true
                }
                (Some("has"), "attachment" | "attachments") => {
                    q.attachment = true;
                    true
                }
                (Some("in"), v) => match role_name(v) {
                    Some(r) => {
                        if !q.roles.contains(&r) {
                            q.roles.push(r);
                        }
                        true
                    }
                    None => false,
                },
                (Some(k @ ("before" | "after")), _) => match parse_date(&t.text) {
                    Some(ts) => {
                        if k == "before" {
                            q.before = Some(ts)
                        } else {
                            q.after = Some(ts)
                        }
                        true
                    }
                    None => false,
                },
                _ => false,
            };
            if handled {
                continue;
            }
            let column = match t.key.as_deref() {
                Some("from") => Some(Column::Sender),
                Some("to") => Some(Column::Recipients),
                Some("subject") => Some(Column::Subject),
                _ => None,
            };
            let (text, quoted) = match column {
                Some(_) => as_text(),
                None => match &t.key {
                    // An unrecognised "key:value" is just text ("re:", "http://...").
                    Some(k) => (format!("{k} {}", t.text), t.quoted),
                    None => as_text(),
                },
            };
            if !has_word_chars(&text) || q.terms.len() >= MAX_TERMS {
                continue;
            }
            q.terms.push(Term {
                column: column.unwrap_or(Column::Any),
                text,
                prefix: !quoted,
            });
            last_plain_is_open = !quoted;
        }
        // Only the final term is matched as a prefix (the person may still be typing it).
        let n = q.terms.len();
        for (i, t) in q.terms.iter_mut().enumerate() {
            t.prefix = t.prefix && i + 1 == n && last_plain_is_open;
        }
        q
    }

    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
            && self.unread.is_none()
            && !self.starred
            && !self.answered
            && !self.attachment
            && self.roles.is_empty()
            && self.before.is_none()
            && self.after.is_none()
    }

    /// The FTS5 expression, or `None` if only filters were given.
    fn match_expression(&self) -> Option<String> {
        if self.terms.is_empty() {
            return None;
        }
        let parts: Vec<String> = self
            .terms
            .iter()
            .map(|t| {
                let col = match t.column {
                    Column::Any => "",
                    Column::Subject => "subject : ",
                    Column::Sender => "sender : ",
                    Column::Recipients => "recipients : ",
                };
                format!("{col}{}{}", quote(&t.text), if t.prefix { "*" } else { "" })
            })
            .collect();
        Some(parts.join(" "))
    }
}

fn parse_snippet(s: &str) -> Snippet {
    let mut out = Snippet::default();
    let mut start: Option<usize> = None;
    for c in s.chars() {
        match c {
            '\u{1}' => start = Some(out.text.len()),
            '\u{2}' => {
                if let Some(st) = start.take()
                    && out.text.len() > st
                {
                    out.highlights.push(st..out.text.len());
                }
            }
            c => out.text.push(c),
        }
    }
    out
}

impl Store {
    /// Runs a search. Junk and trash are left out unless asked for (`in:`) or
    /// unless the scope is one specific folder.
    pub fn search(
        &self,
        query: &SearchQuery,
        scope: &SearchScope,
        sort: SearchSort,
        limit: usize,
    ) -> Result<Vec<SearchHit>> {
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let limit = i64::try_from(limit.clamp(1, 500)).unwrap_or(500);
        let mut where_parts: Vec<String> = Vec::new();
        let mut args: Vec<Value> = Vec::new();
        let mut push = |sql: &str, v: Option<Value>| {
            where_parts.push(sql.to_owned());
            if let Some(v) = v {
                args.push(v);
            }
        };
        push("(m.flags & 8) = 0", None); // messages marked for deletion are hidden
        let fts = query.match_expression();
        if let Some(e) = &fts {
            push("search MATCH ?", Some(Value::Text(e.clone())));
        }
        match scope {
            SearchScope::Everywhere => {}
            SearchScope::Account(a) => push("f.account = ?", Some(Value::Text(a.clone()))),
            SearchScope::Folder(id) => push("m.folder_id = ?", Some(Value::Integer(*id))),
        }
        match query.unread {
            Some(true) => push("(m.flags & 1) = 0", None),
            Some(false) => push("(m.flags & 1) <> 0", None),
            None => {}
        }
        if query.starred {
            push("(m.flags & 4) <> 0", None);
        }
        if query.answered {
            push("(m.flags & 2) <> 0", None);
        }
        if query.attachment {
            push("m.maybe_attachments <> 0", None);
        }
        if let Some(b) = query.before {
            push("m.date < ?", Some(Value::Integer(b)));
        }
        if let Some(a) = query.after {
            push("m.date >= ?", Some(Value::Integer(a)));
        }
        if !query.roles.is_empty() {
            let marks = vec!["?"; query.roles.len()].join(",");
            where_parts.push(format!("f.role IN ({marks})"));
            args.extend(query.roles.iter().map(|r| Value::Text((*r).to_owned())));
        } else if !matches!(scope, SearchScope::Folder(_)) {
            where_parts.push("f.role NOT IN ('trash','junk')".to_owned());
        }
        let order = match (sort, fts.is_some()) {
            (SearchSort::Relevance, true) => "ORDER BY rank, m.date DESC, m.id DESC",
            _ => "ORDER BY m.date DESC, m.id DESC",
        };
        let (from, snippet_col) = if fts.is_some() {
            (
                "search JOIN messages m ON m.id = search.rowid JOIN folders f ON f.id = m.folder_id",
                "snippet(search, 3, char(1), char(2), '…', 12)",
            )
        } else {
            ("messages m JOIN folders f ON f.id = m.folder_id", "''")
        };
        let sql = format!(
            "SELECT {MSG_COLS}, {snippet_col} FROM {from} WHERE {} {order} LIMIT {limit}",
            where_parts.join(" AND ")
        );
        self.with(|c| {
            let mut st = c.prepare(&sql)?;
            let rows = st.query_map(rusqlite::params_from_iter(args.iter()), |r| {
                Ok((msg_row(r)?, r.get::<_, String>(16)?))
            })?;
            let mut out = Vec::new();
            for row in rows {
                let (message, snip) = row?;
                let mut snippet = parse_snippet(&snip);
                if snippet.text.trim().is_empty() {
                    snippet = Snippet {
                        text: message.preview.clone(),
                        highlights: Vec::new(),
                    };
                }
                out.push(SearchHit { message, snippet });
            }
            Ok(out)
        })
    }
}

#[cfg(test)]
pub(super) fn test_parse_date(s: &str) -> Option<i64> {
    parse_date(s)
}

#[cfg(test)]
pub(super) fn test_expression(q: &SearchQuery) -> Option<String> {
    q.match_expression()
}
