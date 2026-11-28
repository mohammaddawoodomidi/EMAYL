//! `mailcli`: a small harness for exercising `mailcore` without the GUI.
//!
//! Commands:
//!   mailcli discover <email>   Find IMAP/SMTP settings for an address
//!   mailcli presets            List built-in provider presets

use std::process::ExitCode;

use mailcore::autoconfig::{HttpFetcher, discover};
use mailcore::email::EmailAddress;
use mailcore::provider::{AuthOption, PRESETS, ResolvedConfig};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["discover", address] => run_discover(address).await,
        ["presets"] => {
            list_presets();
            ExitCode::SUCCESS
        }
        _ => {
            eprintln!("usage:\n  mailcli discover <email>\n  mailcli presets");
            ExitCode::from(2)
        }
    }
}

async fn run_discover(address: &str) -> ExitCode {
    let email = match EmailAddress::parse(address) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };
    let fetcher = match HttpFetcher::new() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    match discover(&fetcher, &email).await {
        Ok(config) => {
            print_config(&email, &config);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn print_config(email: &EmailAddress, c: &ResolvedConfig) {
    println!("Address:   {email}");
    println!(
        "Provider:  {}",
        c.provider.as_deref().unwrap_or("(not named)")
    );
    println!("Source:    {}", c.source.label());
    let s = &c.settings;
    println!(
        "IMAP:      {}:{} ({:?}), user '{}'",
        s.imap.host, s.imap.port, s.imap.security, s.imap_username
    );
    println!(
        "SMTP:      {}:{} ({:?}), user '{}'",
        s.smtp.host, s.smtp.port, s.smtp.security, s.smtp_username
    );
    let auth: Vec<String> = c.auth_options.iter().map(describe_auth).collect();
    println!("Sign-in:   {}", auth.join(" or "));
    for note in &c.notes {
        println!("Note:      {note}");
    }
    println!();
    println!(
        "Your credentials will be sent to {} and {}. Continue only if those belong to your provider.",
        s.imap.host, s.smtp.host
    );
}

fn describe_auth(a: &AuthOption) -> String {
    match a {
        AuthOption::OAuth2(p) => format!("OAuth2 ({p:?})"),
        AuthOption::AppPassword => "app password".to_owned(),
        AuthOption::Password => "password".to_owned(),
    }
}

fn list_presets() {
    for p in PRESETS {
        println!(
            "{:<9} {:<32} IMAP {}:{}  SMTP {}:{}  [{}]",
            p.id,
            p.name,
            p.imap.host,
            p.imap.port,
            p.smtp.host,
            p.smtp.port,
            p.domains.join(", ")
        );
    }
}
