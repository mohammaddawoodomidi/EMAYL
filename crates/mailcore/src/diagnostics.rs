//! "Check connection": tries to reach and sign in to an account's incoming and
//! outgoing servers and says, step by step, where it stopped and what to do.
//! The text report can be copied to a helper or to the provider's support.
//! It never contains passwords or tokens.

use std::time::{Duration, Instant};

use crate::account::{Account, Security};
use crate::imap::account_connect::with_credentials;
use crate::imap::connect_account;
use crate::net::{ConnError, Stage};
use crate::oauth::TokenTransport;
use crate::sender::Env;
use crate::smtp::{SmtpParams, connect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepStatus {
    Ok,
    Failed,
    NotReached,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub label: &'static str,
    pub status: StepStatus,
    /// Technical detail, only for the failed step.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerReport {
    pub role: &'static str,
    pub host: String,
    pub port: u16,
    pub security: Security,
    pub steps: Vec<Step>,
    pub ok: bool,
    /// Plain-language next step when something failed.
    pub advice: Option<String>,
    /// Whether waiting and retrying could help.
    pub transient: bool,
    pub elapsed: Duration,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub account: String,
    pub servers: Vec<ServerReport>,
    /// Settings that look wrong even though nothing failed (yet).
    pub warnings: Vec<String>,
}

impl Diagnosis {
    pub fn all_ok(&self) -> bool {
        self.servers.iter().all(|s| s.ok)
    }

    /// Plain text for copying.
    pub fn render(&self) -> String {
        let mut out = format!("Connection check for {}\n", self.account);
        for s in &self.servers {
            let sec = match s.security {
                Security::Tls => "TLS",
                Security::StartTls => "STARTTLS",
            };
            out.push_str(&format!(
                "\n{} - {}:{} ({sec}) - {:.1}s\n",
                s.role,
                s.host,
                s.port,
                s.elapsed.as_secs_f32()
            ));
            for st in &s.steps {
                let mark = match st.status {
                    StepStatus::Ok => "[ok]         ",
                    StepStatus::Failed => "[FAILED]     ",
                    StepStatus::NotReached => "[not reached]",
                };
                out.push_str(&format!("  {mark} {}\n", st.label));
                if let Some(d) = &st.detail {
                    out.push_str(&format!("                 detail: {d}\n"));
                }
            }
            if let Some(a) = &s.advice {
                out.push_str(&format!("  What to do: {a}\n"));
            }
            for n in &s.notes {
                out.push_str(&format!("  Note: {n}\n"));
            }
        }
        for w in &self.warnings {
            out.push_str(&format!("\nWarning: {w}\n"));
        }
        out.push_str(if self.all_ok() {
            "\nResult: everything works.\n"
        } else {
            "\nResult: there is a problem (see above).\n"
        });
        out
    }
}

fn stage_label(stage: Stage) -> &'static str {
    match stage {
        Stage::Dns => "Finding the server",
        Stage::Connect => "Connecting to the server",
        Stage::Tls => "Secure connection and certificate",
        Stage::Greeting => "Server greeting",
        Stage::StartTls => "Switching to a secure connection",
        Stage::Login => "Signing in",
        Stage::Session => "Using the connection",
    }
}

fn sequence(security: Security) -> Vec<Stage> {
    match security {
        Security::Tls => vec![
            Stage::Dns,
            Stage::Connect,
            Stage::Tls,
            Stage::Greeting,
            Stage::Login,
        ],
        Security::StartTls => vec![
            Stage::Dns,
            Stage::Connect,
            Stage::Greeting,
            Stage::StartTls,
            Stage::Tls,
            Stage::Login,
        ],
    }
}

/// Turns the outcome of one connection attempt into step-by-step results.
fn report(
    role: &'static str,
    host: &str,
    port: u16,
    security: Security,
    outcome: Result<Vec<String>, ConnError>,
    elapsed: Duration,
) -> ServerReport {
    let mut seq = sequence(security);
    let (failed_stage, err) = match &outcome {
        Ok(_) => (None, None),
        Err(e) => {
            if !seq.contains(&e.stage) {
                seq.push(e.stage); // e.g. a problem after sign-in
            }
            (Some(e.stage), Some(e))
        }
    };
    let mut reached_failure = false;
    let steps = seq
        .into_iter()
        .map(|st| {
            let (status, detail) = if failed_stage == Some(st) {
                reached_failure = true;
                (StepStatus::Failed, err.map(|e| e.detail.clone()))
            } else if reached_failure {
                (StepStatus::NotReached, None)
            } else {
                (StepStatus::Ok, None)
            };
            Step {
                label: stage_label(st),
                status,
                detail,
            }
        })
        .collect();
    ServerReport {
        role,
        host: host.to_owned(),
        port,
        security,
        steps,
        ok: outcome.is_ok(),
        advice: err.map(|e| e.advice().to_owned()),
        transient: err.is_some_and(ConnError::is_transient),
        elapsed,
        notes: outcome.unwrap_or_default(),
    }
}

/// Checks both servers of an account (in parallel).
pub async fn diagnose<T: TokenTransport>(env: &Env<'_, T>, account: &Account) -> Diagnosis {
    let imap = async {
        let t = Instant::now();
        let r = connect_account(
            account,
            env.secrets,
            env.tokens,
            env.clients,
            env.tls,
            env.timeouts,
        )
        .await;
        let outcome = match r {
            Ok(session) => {
                session.logout().await;
                Ok(Vec::new())
            }
            Err(e) => Err(e),
        };
        report(
            "Incoming mail (IMAP)",
            &account.imap.host,
            account.imap.port,
            account.imap.security,
            outcome,
            t.elapsed(),
        )
    };
    let smtp = async {
        let t = Instant::now();
        let params = SmtpParams {
            server: &account.smtp,
            username: &account.smtp_username,
            ehlo_name: "localhost",
            timeouts: env.timeouts,
        };
        let r = with_credentials(
            account,
            env.secrets,
            env.tokens,
            env.clients,
            async |creds| connect(&params, creds, env.tls).await,
        )
        .await;
        let outcome = match r {
            Ok(session) => {
                let mut notes = Vec::new();
                if let Some(limit) = session.caps().size {
                    notes.push(format!(
                        "This server accepts messages up to {} MB.",
                        limit / 1_000_000
                    ));
                }
                session.quit().await;
                Ok(notes)
            }
            Err(e) => Err(e),
        };
        report(
            "Outgoing mail (SMTP)",
            &account.smtp.host,
            account.smtp.port,
            account.smtp.security,
            outcome,
            t.elapsed(),
        )
    };
    let (imap, smtp) = tokio::join!(imap, smtp);
    Diagnosis {
        account: account.email.to_string(),
        servers: vec![imap, smtp],
        warnings: account.warnings().into_iter().map(str::to_owned).collect(),
    }
}

#[cfg(test)]
mod tests;
