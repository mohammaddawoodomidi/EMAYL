//! Headless core of the mail client.
//!
//! Scope so far: validated account model, an atomic on-disk account registry,
//! built-in provider presets, HTTPS-only server autoconfig, an OS-keyring secret
//! store, TLS/network plumbing, an IMAP engine and OAuth2 sign-in.
//!
//! Design rules enforced by this crate:
//! * No `unsafe` (see workspace lints).
//! * No unencrypted server connections can be represented ([`account::Security`]
//!   has no "plain" variant).
//! * Secrets never touch disk in plaintext; there is no fallback secret store.
//! * Everything that arrives from the network or from the config file is
//!   treated as hostile and validated.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod account;
pub mod autoconfig;
pub mod bounce;
pub mod compose;
pub mod diagnostics;
pub mod drafts;
pub mod email;
pub mod error;
pub(crate) mod fsutil;
pub mod imap;
pub mod instance;
pub mod net;
pub mod oauth;
pub mod outbox;
pub mod provider;
pub mod registry;
pub mod render;
pub mod replies;
pub mod retry;
pub mod rules;
pub mod secrets;
pub mod sender;
pub mod smtp;
pub mod store;
pub mod syncer;
pub mod templates;
pub(crate) mod textsafe;

pub use error::{Error, Result};
