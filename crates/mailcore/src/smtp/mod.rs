//! SMTP submission: protocol replies, secure connection, sending.

pub mod reply;
pub mod session;

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) mod testserver;

pub use reply::{EnhancedStatus, Reply, explain};
pub use session::{
    Phase, Refusal, Rejection, SendError, SendReport, SmtpCaps, SmtpParams, SmtpSession, connect,
    prepare_data,
};
