//! IMAP engine: connection, folders, sync planning, flags, IDLE.

pub mod account_connect;
pub mod folder;
pub mod session;
pub mod sync;
pub mod utf7;

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod testserver;

pub use account_connect::connect_account;
pub use folder::{Folder, FolderRole};
pub use session::{
    ConnectParams, Credentials, Features, FlagChange, FlagUpdate, IdleOutcome, ImapSession,
    MessageFlags, MessageSummary, MoveOutcome, connect,
};
