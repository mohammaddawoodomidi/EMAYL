//! Server settings discovery.
//!
//! Order (first success wins):
//! 1. Built-in preset for the address's domain (offline, nothing leaves the machine).
//! 2. `https://autoconfig.<domain>/mail/config-v1.1.xml?emailaddress=<address>`
//! 3. `https://<domain>/.well-known/autoconfig/mail/config-v1.1.xml`
//! 4. `https://autoconfig.thunderbird.net/v1.1/<domain>` (only the domain is sent)
//!
//! Not implemented yet: MX-based lookup and RFC 6186 SRV records (both need DNS
//! queries). Custom domains hosted by Google Workspace or Microsoft 365 therefore
//! need manual setup for now.
//!
//! Security properties:
//! * HTTPS only, at most 3 redirects, 5 s connect / 10 s total timeout, 128 KiB cap.
//! * Plain `http://` is never requested. A network attacker who could rewrite
//!   discovery data could point the client at a server that then receives the
//!   user's password.
//! * The XML parser has DTDs disabled and a node limit (no entity expansion).
//! * Unencrypted `socketType` values are ignored.
//! * Text taken from the network is stripped of control characters before it can
//!   be displayed.
//! * Discovered host names are *not* trusted: callers must show them to the user
//!   before the first login.

use std::fmt::Write as _;
use std::future::Future;
use std::time::Duration;

use reqwest::Url;
use thiserror::Error;

use crate::account::{Security, ServerConfig, ServerSettings, validate_text};
use crate::email::EmailAddress;
use crate::provider::{AuthOption, ConfigSource, ResolvedConfig, find_preset};
use crate::{Error, Result};

pub const MAX_CONFIG_BYTES: usize = 128 * 1024;
const MAX_XML_NODES: u32 = 5_000;
const MAX_PROVIDER_NAME_CHARS: usize = 64;

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("HTTP status {0}")]
    Status(u16),
    #[error("response larger than {0} bytes")]
    TooLarge(usize),
    #[error("request failed: {0}")]
    Request(String),
}

/// Abstraction over "GET this https URL, return the body", so discovery can be
/// tested without a network.
pub trait Fetcher {
    fn fetch(
        &self,
        url: &Url,
    ) -> impl Future<Output = std::result::Result<Vec<u8>, FetchError>> + Send;
}

/// Production fetcher.
#[derive(Debug, Clone)]
pub struct HttpFetcher {
    client: reqwest::Client,
}

impl HttpFetcher {
    pub fn new() -> Result<Self> {
        let client = reqwest::Client::builder()
            .tls_backend_preconfigured(crate::net::http_tls_config()?)
            .https_only(true)
            .redirect(reqwest::redirect::Policy::limited(3))
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(10))
            .user_agent(concat!("emayl/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| Error::HttpClient(e.to_string()))?;
        Ok(Self { client })
    }
}

impl Fetcher for HttpFetcher {
    async fn fetch(&self, url: &Url) -> std::result::Result<Vec<u8>, FetchError> {
        if url.scheme() != "https" {
            return Err(FetchError::Request("refusing non-HTTPS URL".into()));
        }
        let req_err = |e: reqwest::Error| FetchError::Request(describe_request_error(e));
        let mut resp = self.client.get(url.clone()).send().await.map_err(req_err)?;
        if !resp.status().is_success() {
            return Err(FetchError::Status(resp.status().as_u16()));
        }
        if resp
            .content_length()
            .is_some_and(|len| len > MAX_CONFIG_BYTES as u64)
        {
            return Err(FetchError::TooLarge(MAX_CONFIG_BYTES));
        }
        let mut body = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(req_err)? {
            if body.len() + chunk.len() > MAX_CONFIG_BYTES {
                return Err(FetchError::TooLarge(MAX_CONFIG_BYTES));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }
}

/// Renders a request error with its cause chain (DNS failure, TLS handshake
/// error, timeout...) so the user can tell *why* discovery failed. `without_url`
/// keeps the email address, which is in the query string, out of errors and logs.
pub(crate) fn describe_request_error(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut message = e.to_string();
    let mut cause = std::error::Error::source(&e);
    for _ in 0..6 {
        let Some(c) = cause else { break };
        message.push_str(": ");
        message.push_str(&c.to_string());
        cause = c.source();
    }
    message
}

/// Resolve server settings for `email`.
pub async fn discover<F: Fetcher>(fetcher: &F, email: &EmailAddress) -> Result<ResolvedConfig> {
    if let Some(preset) = find_preset(email.domain()) {
        return Ok(preset.resolve(email));
    }

    let mut attempts = String::new();
    for (source, url) in candidate_urls(email) {
        let host = url.host_str().unwrap_or("?").to_owned();
        let outcome = match fetcher.fetch(&url).await {
            Ok(body) => parse_client_config(&body, email, source).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        match outcome {
            Ok(config) => return Ok(config),
            Err(reason) => {
                // Writing to a String cannot fail.
                let _ = writeln!(attempts, "- {} ({host}): {reason}", source.label());
            }
        }
    }
    attempts.push_str("- no built-in preset for this domain");
    Err(Error::DiscoveryFailed(attempts))
}

pub(crate) fn candidate_urls(email: &EmailAddress) -> Vec<(ConfigSource, Url)> {
    let domain = email.domain();
    let address = email.to_string();
    let mut out = Vec::with_capacity(3);
    if let Ok(url) = Url::parse_with_params(
        &format!("https://autoconfig.{domain}/mail/config-v1.1.xml"),
        [("emailaddress", address.as_str())],
    ) {
        out.push((ConfigSource::IspAutoconfig, url));
    }
    if let Ok(url) = Url::parse(&format!(
        "https://{domain}/.well-known/autoconfig/mail/config-v1.1.xml"
    )) {
        out.push((ConfigSource::IspWellKnown, url));
    }
    if let Ok(url) = Url::parse(&format!("https://autoconfig.thunderbird.net/v1.1/{domain}")) {
        out.push((ConfigSource::ThunderbirdIspDb, url));
    }
    out
}

/// Parse a Thunderbird-format `clientConfig` document (version 1.1).
///
/// Picks the first IMAP and the first SMTP server that use TLS or STARTTLS and
/// offer a password-style authentication method.
pub fn parse_client_config(
    xml: &[u8],
    email: &EmailAddress,
    source: ConfigSource,
) -> Result<ResolvedConfig> {
    let bad = |m: &str| Error::BadAutoconfig(m.to_owned());
    if xml.len() > MAX_CONFIG_BYTES {
        return Err(bad("document is too large"));
    }
    let text = std::str::from_utf8(xml).map_err(|_| bad("document is not valid UTF-8"))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);

    let options = roxmltree::ParsingOptions {
        allow_dtd: false,
        nodes_limit: MAX_XML_NODES,
        ..roxmltree::ParsingOptions::default()
    };
    let doc = roxmltree::Document::parse_with_options(text, options)
        .map_err(|e| Error::BadAutoconfig(format!("XML error: {e}")))?;

    let root = doc.root_element();
    if root.tag_name().name() != "clientConfig" {
        return Err(bad("root element is not <clientConfig>"));
    }
    let provider_node = root
        .children()
        .find(|n| n.is_element() && n.tag_name().name() == "emailProvider")
        .ok_or_else(|| bad("no <emailProvider> element"))?;

    let provider = child_text(provider_node, "displayName")
        .map(sanitize_display_text)
        .filter(|s| !s.is_empty());

    let (imap, imap_username) = pick_server(provider_node, "incomingServer", "imap", email)?;
    let (smtp, smtp_username) = pick_server(provider_node, "outgoingServer", "smtp", email)?;

    Ok(ResolvedConfig {
        source,
        provider,
        settings: ServerSettings {
            imap,
            imap_username,
            smtp,
            smtp_username,
        },
        auth_options: vec![AuthOption::Password],
        notes: vec![
            "Settings were published by the provider or a community database; \
             check that the server names belong to your provider before signing in."
                .to_owned(),
        ],
    })
}

fn pick_server(
    provider: roxmltree::Node<'_, '_>,
    tag: &str,
    kind: &str,
    email: &EmailAddress,
) -> Result<(ServerConfig, String)> {
    let mut reasons = Vec::new();
    for node in provider.children().filter(|n| {
        n.is_element() && n.tag_name().name() == tag && n.attribute("type") == Some(kind)
    }) {
        match parse_server(node, email, kind) {
            Ok(found) => return Ok(found),
            Err(reason) => reasons.push(reason),
        }
    }
    if reasons.is_empty() {
        Err(Error::BadAutoconfig(format!("no {kind} server listed")))
    } else {
        Err(Error::BadAutoconfig(format!(
            "no usable {kind} server: {}",
            reasons.join("; ")
        )))
    }
}

/// Ports a mail server is normally reached on. A settings file that came from the
/// internet may not point the program at anything else (a database, a web admin page,
/// SSH): it would make the program probe other services on the person's behalf.
fn port_is_a_mail_port(kind: &str, port: u16) -> bool {
    match kind {
        "imap" => matches!(port, 143 | 993),
        "smtp" => matches!(port, 25 | 465 | 587 | 2525),
        _ => false,
    }
}

/// Names that only mean something inside one network (or nowhere). A settings file from
/// the internet has no business naming them; people who really use such a server can
/// type the settings in themselves.
fn is_private_style_name(host: &str) -> bool {
    const SUFFIXES: &[&str] = &[
        "local",
        "localhost",
        "localdomain",
        "internal",
        "intranet",
        "lan",
        "home",
        "corp",
        "home.arpa",
        "invalid",
        "test",
    ];
    SUFFIXES
        .iter()
        .any(|s| host == *s || host.ends_with(&format!(".{s}")))
}

fn parse_server(
    node: roxmltree::Node<'_, '_>,
    email: &EmailAddress,
    kind: &str,
) -> std::result::Result<(ServerConfig, String), String> {
    let hostname = substitute(
        child_text(node, "hostname").ok_or("missing hostname")?,
        email,
    )?;
    let port: u16 = child_text(node, "port")
        .ok_or("missing port")?
        .parse()
        .map_err(|_| "port is not a number from 1 to 65535".to_owned())?;
    let security = match child_text(node, "socketType")
        .map(str::to_ascii_uppercase)
        .as_deref()
    {
        Some("SSL") => Security::Tls,
        Some("STARTTLS") => Security::StartTls,
        Some(_) | None => {
            return Err("unencrypted or unspecified connection security is not supported".into());
        }
    };
    let username = substitute(
        child_text(node, "username").ok_or("missing username")?,
        email,
    )?;
    validate_text(&username, "username", false).map_err(|e| e.to_string())?;

    let offered: Vec<String> = node
        .children()
        .filter(|n| n.is_element() && n.tag_name().name() == "authentication")
        .filter_map(|n| n.text())
        .map(|t| t.trim().to_ascii_lowercase())
        .collect();
    let password_style =
        |m: &str| matches!(m, "password-cleartext" | "password-encrypted" | "plain");
    if !offered.is_empty() && !offered.iter().any(|m| password_style(m)) {
        return Err(format!(
            "needs an authentication method this client does not support for discovered servers ({})",
            sanitize_display_text(&offered.join(", "))
        ));
    }

    let server = ServerConfig::new(&hostname, port, security).map_err(|e| e.to_string())?;
    if !port_is_a_mail_port(kind, port) {
        return Err(format!(
            "port {port} is not a usual {} port; enter the settings yourself if this is right",
            kind.to_ascii_uppercase()
        ));
    }
    if is_private_style_name(&server.host) {
        return Err(format!(
            "{} is a name that only works inside one network; enter the settings yourself if this is right",
            sanitize_display_text(&server.host)
        ));
    }
    Ok((server, username))
}

/// Expands the three placeholders defined by the autoconfig format. Anything
/// still containing `%` afterwards is refused rather than passed through.
fn substitute(template: &str, email: &EmailAddress) -> std::result::Result<String, String> {
    let out = template
        .replace("%EMAILADDRESS%", &email.to_string())
        .replace("%EMAILLOCALPART%", email.local_part())
        .replace("%EMAILDOMAIN%", email.domain());
    if out.contains('%') {
        return Err("contains an unsupported placeholder".into());
    }
    // Host names are validated later by ServerConfig::new; this only guards
    // against a value that is empty after trimming.
    if out.trim().is_empty() {
        return Err("value is empty".into());
    }
    Ok(out.trim().to_owned())
}

fn child_text<'a>(node: roxmltree::Node<'a, '_>, name: &str) -> Option<&'a str> {
    node.children()
        .find(|n| n.is_element() && n.tag_name().name() == name)
        .and_then(|n| n.text())
        .map(str::trim)
}

/// Strip control characters (terminal-escape and log injection) and cap length.
fn sanitize_display_text(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control())
        .take(MAX_PROVIDER_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0"?>
<clientConfig version="1.1">
  <emailProvider id="example.com">
    <domain>example.com</domain>
    <displayName>Example Mail</displayName>
    <incomingServer type="imap">
      <hostname>imap.%EMAILDOMAIN%</hostname>
      <port>993</port>
      <socketType>SSL</socketType>
      <username>%EMAILLOCALPART%</username>
      <authentication>password-cleartext</authentication>
    </incomingServer>
    <outgoingServer type="smtp">
      <hostname>smtp.example.com</hostname>
      <port>587</port>
      <socketType>STARTTLS</socketType>
      <username>%EMAILADDRESS%</username>
      <authentication>password-cleartext</authentication>
    </outgoingServer>
  </emailProvider>
</clientConfig>"#;

    fn email() -> EmailAddress {
        EmailAddress::parse("alice@example.com").unwrap()
    }

    fn parse(xml: &str) -> Result<ResolvedConfig> {
        parse_client_config(xml.as_bytes(), &email(), ConfigSource::ThunderbirdIspDb)
    }

    #[test]
    fn parses_sample_and_expands_placeholders() {
        let r = parse(SAMPLE).unwrap();
        assert_eq!(r.provider.as_deref(), Some("Example Mail"));
        assert_eq!(r.settings.imap.host, "imap.example.com");
        assert_eq!(r.settings.imap.port, 993);
        assert_eq!(r.settings.imap.security, Security::Tls);
        assert_eq!(r.settings.imap_username, "alice");
        assert_eq!(r.settings.smtp.security, Security::StartTls);
        assert_eq!(r.settings.smtp_username, "alice@example.com");
        assert_eq!(r.auth_options, vec![AuthOption::Password]);
    }

    #[test]
    fn skips_plaintext_server_and_uses_next_one() {
        let xml = SAMPLE.replace(
            r#"<incomingServer type="imap">"#,
            r#"<incomingServer type="imap">
      <hostname>old.example.com</hostname><port>143</port><socketType>plain</socketType>
      <username>x</username><authentication>password-cleartext</authentication>
    </incomingServer>
    <incomingServer type="imap">"#,
        );
        let r = parse(&xml).unwrap();
        assert_eq!(r.settings.imap.host, "imap.example.com");
    }

    #[test]
    fn rejects_when_only_plaintext_is_offered() {
        let xml = SAMPLE.replace(
            "<socketType>SSL</socketType>",
            "<socketType>plain</socketType>",
        );
        assert!(parse(&xml).is_err());
    }

    #[test]
    fn rejects_oauth_only_servers() {
        let xml = SAMPLE.replace(
            "<authentication>password-cleartext</authentication>\n    </incomingServer>",
            "<authentication>OAuth2</authentication>\n    </incomingServer>",
        );
        let err = parse(&xml).unwrap_err().to_string();
        assert!(err.contains("authentication"), "{err}");
    }

    #[test]
    fn rejects_dtd_and_entity_bombs() {
        let bomb = r#"<?xml version="1.0"?>
<!DOCTYPE lolz [<!ENTITY a "aaaaaaaaaa"><!ENTITY b "&a;&a;&a;&a;&a;&a;&a;&a;">]>
<clientConfig version="1.1"><emailProvider id="x"><displayName>&b;</displayName></emailProvider></clientConfig>"#;
        assert!(parse(bomb).is_err());
    }

    #[test]
    fn rejects_unknown_placeholders_bad_hosts_and_bad_ports() {
        assert!(parse(&SAMPLE.replace("%EMAILLOCALPART%", "%EVIL%")).is_err());
        assert!(parse(&SAMPLE.replace("imap.%EMAILDOMAIN%", "10.0.0.1")).is_err());
        assert!(parse(&SAMPLE.replace("imap.%EMAILDOMAIN%", "imap.exa mple.com")).is_err());
        assert!(parse(&SAMPLE.replace("<port>993</port>", "<port>0</port>")).is_err());
        assert!(parse(&SAMPLE.replace("<port>993</port>", "<port>99999</port>")).is_err());
    }

    #[test]
    fn a_settings_file_cannot_point_the_program_at_other_services() {
        // Redis, SSH, a web admin page, a mail port of the wrong kind.
        for (from, to) in [
            ("<port>993</port>", "<port>6379</port>"),
            ("<port>993</port>", "<port>22</port>"),
            ("<port>993</port>", "<port>587</port>"),
            ("<port>587</port>", "<port>993</port>"),
            ("<port>587</port>", "<port>8080</port>"),
        ] {
            let xml = SAMPLE.replace(from, to);
            let err = parse(&xml).unwrap_err().to_string();
            assert!(err.contains("port"), "{from}->{to}: {err}");
        }
    }

    #[test]
    fn the_usual_mail_ports_are_accepted() {
        assert!(port_is_a_mail_port("imap", 993) && port_is_a_mail_port("imap", 143));
        for p in [25, 465, 587, 2525] {
            assert!(port_is_a_mail_port("smtp", p), "{p}");
        }
        assert!(!port_is_a_mail_port("imap", 465));
        assert!(!port_is_a_mail_port("smtp", 993));
        assert!(!port_is_a_mail_port("pop3", 995));
        assert!(parse(SAMPLE).is_ok());
    }

    #[test]
    fn a_settings_file_cannot_name_a_machine_inside_the_local_network() {
        for host in [
            "mail.corp",
            "nas.local",
            "localhost",
            "x.localhost",
            "db.internal",
            "router.home.arpa",
            "mail.lan",
            "mail.test",
            "mail.invalid",
        ] {
            let xml = SAMPLE.replace("imap.%EMAILDOMAIN%", host);
            assert!(parse(&xml).is_err(), "{host} was accepted");
        }
        // Look-alikes that are ordinary public names stay allowed.
        assert!(!is_private_style_name("imap.localhost-mail.com"));
        assert!(!is_private_style_name("mail.corporate.com"));
        assert!(!is_private_style_name("imap.example.com"));
    }

    #[test]
    fn rejects_control_characters_in_username() {
        let xml = SAMPLE.replace("%EMAILLOCALPART%", "al&#13;&#10;ice");
        assert!(parse(&xml).is_err());
    }

    #[test]
    fn strips_control_characters_from_provider_name() {
        // U+009B is the 8-bit "CSI" terminal escape. XML 1.0 allows it, so it
        // reaches us and must be stripped before the name can be displayed.
        let xml = SAMPLE.replace("Example Mail", "Evil&#x9B;2JMail");
        let r = parse(&xml).unwrap();
        let name = r.provider.unwrap();
        assert!(!name.chars().any(char::is_control), "{name:?}");
        assert_eq!(name, "Evil2JMail");
    }

    #[test]
    fn xml_forbidden_control_characters_reject_the_whole_document() {
        // ESC (U+001B) is not a legal XML 1.0 character, so the parser refuses
        // the document outright.
        let xml = SAMPLE.replace("Example Mail", "Evil&#27;[2JMail");
        assert!(parse(&xml).is_err());
    }

    #[test]
    fn rejects_oversize_non_utf8_and_wrong_root() {
        let big = vec![b' '; MAX_CONFIG_BYTES + 1];
        assert!(parse_client_config(&big, &email(), ConfigSource::IspWellKnown).is_err());
        assert!(parse_client_config(&[0xff, 0xfe], &email(), ConfigSource::IspWellKnown).is_err());
        assert!(parse("<other/>").is_err());
        assert!(parse("").is_err());
    }

    #[test]
    fn tolerates_utf8_bom() {
        let with_bom = format!("\u{feff}{SAMPLE}");
        assert!(parse(&with_bom).is_ok());
    }

    #[test]
    fn candidate_urls_are_https_and_encode_the_address() {
        let e = EmailAddress::parse("first+last@example.com").unwrap();
        let urls = candidate_urls(&e);
        assert_eq!(urls.len(), 3);
        assert!(urls.iter().all(|(_, u)| u.scheme() == "https"));
        assert_eq!(urls[0].1.host_str(), Some("autoconfig.example.com"));
        let query = urls[0].1.query().unwrap();
        assert!(query.starts_with("emailaddress="));
        assert!(!query.contains('+') && !query.contains('@'), "{query}");
        assert_eq!(
            urls[1].1.path(),
            "/.well-known/autoconfig/mail/config-v1.1.xml"
        );
        assert_eq!(
            urls[2].1.as_str(),
            "https://autoconfig.thunderbird.net/v1.1/example.com"
        );
    }

    #[derive(Default)]
    struct FakeFetcher {
        responses: HashMap<String, Vec<u8>>,
        requested: Mutex<Vec<String>>,
    }

    impl Fetcher for FakeFetcher {
        async fn fetch(&self, url: &Url) -> std::result::Result<Vec<u8>, FetchError> {
            self.requested.lock().unwrap().push(url.to_string());
            let key = format!(
                "{}://{}{}",
                url.scheme(),
                url.host_str().unwrap_or(""),
                url.path()
            );
            self.responses
                .get(&key)
                .cloned()
                .ok_or(FetchError::Status(404))
        }
    }

    #[tokio::test]
    async fn preset_domains_never_touch_the_network() {
        let fetcher = FakeFetcher::default();
        let e = EmailAddress::parse("me@gmail.com").unwrap();
        let r = discover(&fetcher, &e).await.unwrap();
        assert_eq!(r.source, ConfigSource::BuiltInPreset);
        assert!(fetcher.requested.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn falls_through_in_order_to_the_isp_database() {
        let mut fetcher = FakeFetcher::default();
        fetcher.responses.insert(
            "https://autoconfig.thunderbird.net/v1.1/example.com".into(),
            SAMPLE.as_bytes().to_vec(),
        );
        let r = discover(&fetcher, &email()).await.unwrap();
        assert_eq!(r.source, ConfigSource::ThunderbirdIspDb);

        let requested = fetcher.requested.lock().unwrap();
        assert_eq!(requested.len(), 3);
        assert!(requested.iter().all(|u| u.starts_with("https://")));
        assert!(requested[0].starts_with("https://autoconfig.example.com/"));
        assert!(requested[1].starts_with("https://example.com/.well-known/"));
        assert!(requested[2].starts_with("https://autoconfig.thunderbird.net/"));
    }

    #[tokio::test]
    async fn first_working_source_wins_and_stops() {
        let mut fetcher = FakeFetcher::default();
        fetcher.responses.insert(
            "https://autoconfig.example.com/mail/config-v1.1.xml".into(),
            SAMPLE.as_bytes().to_vec(),
        );
        let r = discover(&fetcher, &email()).await.unwrap();
        assert_eq!(r.source, ConfigSource::IspAutoconfig);
        assert_eq!(fetcher.requested.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_garbage_answer_falls_through_to_the_next_source() {
        let mut fetcher = FakeFetcher::default();
        fetcher.responses.insert(
            "https://autoconfig.example.com/mail/config-v1.1.xml".into(),
            b"<html>captive portal</html>".to_vec(),
        );
        fetcher.responses.insert(
            "https://example.com/.well-known/autoconfig/mail/config-v1.1.xml".into(),
            SAMPLE.as_bytes().to_vec(),
        );
        let r = discover(&fetcher, &email()).await.unwrap();
        assert_eq!(r.source, ConfigSource::IspWellKnown);
    }

    #[tokio::test]
    async fn total_failure_explains_every_attempt_without_leaking_the_address() {
        let fetcher = FakeFetcher::default();
        let err = discover(&fetcher, &email()).await.unwrap_err().to_string();
        assert!(err.contains("autoconfig.example.com"), "{err}");
        assert!(err.contains("/") || err.contains("well-known"), "{err}");
        assert!(err.contains("autoconfig.thunderbird.net"), "{err}");
        assert!(err.contains("no built-in preset"), "{err}");
        assert!(!err.contains("alice"), "{err}");
    }
}
