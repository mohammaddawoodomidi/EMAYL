use std::path::PathBuf;

use thiserror::Error;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("local mail database problem: {0}")]
    Database(String),

    #[error("invalid email address: {0}")]
    InvalidEmail(&'static str),

    #[error("invalid {field}: {reason}")]
    InvalidField {
        field: &'static str,
        reason: &'static str,
    },

    #[error("an account for {0} on that server already exists")]
    DuplicateAccount(String),

    #[error("no account with id {0}")]
    AccountNotFound(String),

    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("account file {path} is not valid and was left untouched: {reason}")]
    CorruptAccountFile { path: PathBuf, reason: String },

    #[error("account file {path} is larger than the {limit} byte limit")]
    AccountFileTooLarge { path: PathBuf, limit: u64 },

    #[error("secure secret store unavailable or failed: {0}")]
    SecretStore(String),

    #[error("stored file {path} is not valid and was left untouched: {reason}")]
    CorruptFile { path: PathBuf, reason: String },

    #[error("no such item: {0}")]
    NotFound(String),

    #[error("operation not allowed in the current state: {0}")]
    InvalidState(&'static str),

    #[error("TLS setup failed: {0}")]
    Tls(String),

    #[error("could not build HTTP client: {0}")]
    HttpClient(String),

    #[error("could not discover server settings:\n{0}")]
    DiscoveryFailed(String),

    #[error("invalid autoconfig data: {0}")]
    BadAutoconfig(String),

    #[error(
        "EMAYL is already open for these mail files. Close the other EMAYL window first \
         (look in the taskbar), then start it again."
    )]
    AlreadyRunning,
}

impl From<rusqlite::Error> for Error {
    fn from(e: rusqlite::Error) -> Self {
        Self::Database(e.to_string())
    }
}
