//! Saved reply texts ("templates"): a name and some text, kept in their own small file
//! (`templates.json`, readable only by you). Inserting one into a message is a plain text paste;
//! nothing in a template is ever run or looked up.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::fsutil::{create_private_dir, new_id, read_limited, write_atomic};
use crate::{Error, Result};

pub const MAX_TEMPLATES: usize = 100;
pub const MAX_NAME_CHARS: usize = 60;
pub const MAX_BODY_CHARS: usize = 20_000;
const FILE_NAME: &str = "templates.json";
const MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub body: String,
}

impl Template {
    pub fn new(name: &str, body: &str) -> std::result::Result<Self, String> {
        let name = name.trim();
        if name.is_empty() {
            return Err("Give the template a name.".into());
        }
        if name.chars().count() > MAX_NAME_CHARS || name.chars().any(char::is_control) {
            return Err(format!(
                "The name can have at most {MAX_NAME_CHARS} characters and no line breaks."
            ));
        }
        // Text may have line breaks; other control characters are removed.
        let body: String = body
            .replace("\r\n", "\n")
            .chars()
            .filter(|c| *c == '\n' || *c == '\t' || !c.is_control())
            .collect();
        let body = body.trim_end().to_owned();
        if body.trim().is_empty() {
            return Err("The template has no text.".into());
        }
        if body.chars().count() > MAX_BODY_CHARS {
            return Err(format!(
                "The text can have at most {MAX_BODY_CHARS} characters."
            ));
        }
        Ok(Self {
            id: new_id(),
            name: name.to_owned(),
            body,
        })
    }
}

#[derive(Serialize, Deserialize)]
struct TemplateFile {
    version: u32,
    templates: Vec<Template>,
}

#[derive(Debug, Clone)]
pub struct TemplateBook {
    path: PathBuf,
}

impl TemplateBook {
    pub fn open(dir: &Path) -> Self {
        Self {
            path: dir.join(FILE_NAME),
        }
    }

    pub fn load(&self) -> Result<Vec<Template>> {
        let raw = match read_limited(&self.path, MAX_FILE_BYTES) {
            Ok(r) => r,
            Err(Error::Io { source, .. }) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
        let f: TemplateFile = serde_json::from_slice(&raw).map_err(|e| Error::CorruptFile {
            path: self.path.clone(),
            reason: e.to_string(),
        })?;
        if f.version != FORMAT_VERSION {
            return Err(Error::CorruptFile {
                path: self.path.clone(),
                reason: format!("unknown format {}", f.version),
            });
        }
        Ok(f.templates.into_iter().take(MAX_TEMPLATES).collect())
    }

    fn save(&self, templates: &[Template]) -> Result<()> {
        let f = TemplateFile {
            version: FORMAT_VERSION,
            templates: templates.to_vec(),
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

    /// Adds a template; a template with the same name (any case) is replaced.
    pub fn add(&self, t: Template) -> Result<Vec<Template>> {
        let mut all = self.load()?;
        if let Some(old) = all
            .iter_mut()
            .find(|x| x.name.to_lowercase() == t.name.to_lowercase())
        {
            old.body = t.body;
            old.name = t.name;
        } else {
            if all.len() >= MAX_TEMPLATES {
                return Err(Error::InvalidState("too many templates (100 at most)"));
            }
            all.push(t);
        }
        all.sort_by_key(|x| x.name.to_lowercase());
        self.save(&all)?;
        Ok(all)
    }

    pub fn remove(&self, id: &str) -> Result<Vec<Template>> {
        let mut all = self.load()?;
        all.retain(|t| t.id != id);
        self.save(&all)?;
        Ok(all)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("emayl-tpl-{}", new_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn checks_name_and_text() {
        assert!(Template::new("", "hello").is_err());
        assert!(Template::new("a\nb", "hello").is_err());
        assert!(Template::new(&"x".repeat(61), "hello").is_err());
        assert!(Template::new("Thanks", "   ").is_err());
        assert!(Template::new("Thanks", &"x".repeat(20_001)).is_err());
        let t = Template::new("  Thanks ", "Hi,\r\nthanks!\u{0}\n\n").unwrap();
        assert_eq!(t.name, "Thanks");
        assert_eq!(t.body, "Hi,\nthanks!");
    }

    #[test]
    fn saved_replaced_by_name_sorted_and_removed() {
        let dir = tmp();
        let book = TemplateBook::open(&dir);
        book.add(Template::new("Thanks", "Thank you").unwrap())
            .unwrap();
        book.add(Template::new("Away", "I am away").unwrap())
            .unwrap();
        let all = book
            .add(Template::new("thanks", "Many thanks").unwrap())
            .unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].name, "Away");
        assert_eq!(all[1].body, "Many thanks");
        let id = all[0].id.clone();
        assert_eq!(TemplateBook::open(&dir).load().unwrap().len(), 2);
        assert_eq!(book.remove(&id).unwrap().len(), 1);
    }

    #[test]
    fn a_damaged_file_is_never_overwritten() {
        let dir = tmp();
        std::fs::write(dir.join(FILE_NAME), b"nope").unwrap();
        let book = TemplateBook::open(&dir);
        assert!(book.load().is_err());
        assert!(book.add(Template::new("A name", "text").unwrap()).is_err());
        assert_eq!(std::fs::read(dir.join(FILE_NAME)).unwrap(), b"nope");
    }
}
