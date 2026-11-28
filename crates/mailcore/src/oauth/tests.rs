use std::collections::VecDeque;

use super::*;
use crate::secrets::MemorySecretStore;

fn google() -> OAuthClient {
    OAuthClient {
        client_id: "cid.apps.example".into(),
        client_secret: Some(SecretString::from("shh".to_owned())),
        tenant: None,
    }
}

fn microsoft(tenant: Option<&str>) -> OAuthClient {
    OAuthClient {
        client_id: "ms-guid".into(),
        client_secret: None,
        tenant: tenant.map(str::to_owned),
    }
}

fn ok_reply(json: &str) -> HttpReply {
    HttpReply {
        status: 200,
        body: json.as_bytes().to_vec(),
    }
}

/// Scripted token endpoint that records every request body.
#[derive(Default)]
struct Fake {
    bodies: Mutex<Vec<String>>,
    replies: Mutex<VecDeque<HttpReply>>,
    delay: Duration,
}

impl Fake {
    fn with(replies: Vec<HttpReply>) -> Self {
        Self {
            replies: Mutex::new(replies.into()),
            ..Self::default()
        }
    }
    fn bodies(&self) -> Vec<String> {
        self.bodies.lock().unwrap().clone()
    }
}

impl TokenTransport for Fake {
    async fn post_form(&self, _url: &str, body: String) -> Result<HttpReply> {
        self.bodies.lock().unwrap().push(body);
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.replies
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| OAuthError::Transient("no scripted reply".into()))
    }
}

fn query_of(body: &str) -> HashMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

// ---- endpoints ------------------------------------------------------------

#[test]
fn google_endpoints_and_scope() {
    let ep = endpoints(OAuthProvider::Google, &google()).unwrap();
    assert_eq!(ep.auth_url, "https://accounts.google.com/o/oauth2/v2/auth");
    assert_eq!(ep.token_url, "https://oauth2.googleapis.com/token");
    assert_eq!(ep.scope, "https://mail.google.com/");
    assert_eq!(ep.redirect_host, "127.0.0.1");
}

#[test]
fn microsoft_endpoints_default_to_common_and_validate_tenant() {
    let ep = endpoints(OAuthProvider::Microsoft, &microsoft(None)).unwrap();
    assert!(
        ep.token_url
            .starts_with("https://login.microsoftonline.com/common/oauth2/v2.0/")
    );
    assert!(ep.scope.contains("IMAP.AccessAsUser.All"));
    assert!(ep.scope.contains("SMTP.Send"));
    assert!(ep.scope.contains("offline_access"));
    assert_eq!(ep.redirect_host, "localhost");

    let ep = endpoints(
        OAuthProvider::Microsoft,
        &microsoft(Some("contoso.onmicrosoft.com")),
    )
    .unwrap();
    assert!(ep.auth_url.contains("/contoso.onmicrosoft.com/"));

    for bad in [
        "",
        "a/b",
        "evil.com/../x",
        "a b",
        "x?y=1",
        "a@b",
        &"a".repeat(65),
    ] {
        assert!(
            endpoints(OAuthProvider::Microsoft, &microsoft(Some(bad))).is_err(),
            "{bad:?}"
        );
    }
}

#[test]
fn empty_client_id_is_rejected() {
    let mut c = google();
    c.client_id = "  ".into();
    assert!(endpoints(OAuthProvider::Google, &c).is_err());
}

#[test]
fn debug_output_redacts_secrets() {
    let text = format!("{:?}", google());
    assert!(!text.contains("shh"));
    let t = TokenSet {
        access_token: SecretString::from("AT-secret".to_owned()),
        refresh_token: Some(SecretString::from("RT-secret".to_owned())),
        expires_in: Duration::from_secs(1),
    };
    let text = format!("{t:?}");
    assert!(!text.contains("AT-secret") && !text.contains("RT-secret"));
}

// ---- PKCE -----------------------------------------------------------------

#[test]
fn pkce_matches_rfc7636_appendix_b_vector() {
    assert_eq!(
        pkce_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}

// ---- callback request judging --------------------------------------------

fn req(path: &str, host: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nUser-Agent: x\r\n\r\n")
}

#[test]
fn accepts_valid_callback_on_any_loopback_name() {
    for host in [
        "127.0.0.1:5000",
        "localhost:5000",
        "[::1]:5000",
        "LOCALHOST:5000",
    ] {
        match judge_request(&req("/?code=abc%2F1&state=S1", host), 5000, "S1") {
            Ok(Callback::Code(c)) => assert_eq!(c, "abc/1"),
            _ => panic!("{host}"),
        }
    }
}

#[test]
fn rejects_wrong_state_host_method_and_path() {
    assert!(judge_request(&req("/?code=abc&state=BAD", "127.0.0.1:5000"), 5000, "S1").is_err());
    assert!(judge_request(&req("/?code=abc", "127.0.0.1:5000"), 5000, "S1").is_err());
    assert!(judge_request(&req("/?code=abc&state=S1", "evil.example:5000"), 5000, "S1").is_err());
    assert!(judge_request(&req("/?code=abc&state=S1", "127.0.0.1:6000"), 5000, "S1").is_err());
    assert!(judge_request(&req("/favicon.ico", "127.0.0.1:5000"), 5000, "S1").is_err());
    assert!(
        judge_request(
            "POST /?code=a&state=S1 HTTP/1.1\r\nHost: 127.0.0.1:5000\r\n\r\n",
            5000,
            "S1"
        )
        .is_err()
    );
    assert!(judge_request("GET /?code=a&state=S1 HTTP/1.1\r\n\r\n", 5000, "S1").is_err()); // no Host
    assert!(judge_request("garbage", 5000, "S1").is_err());
    assert!(judge_request(&req("/?state=S1", "127.0.0.1:5000"), 5000, "S1").is_err()); // no code
    assert!(judge_request(&req("/?code=&state=S1", "127.0.0.1:5000"), 5000, "S1").is_err());
}

#[test]
fn provider_error_needs_the_right_state_too() {
    assert!(matches!(
        judge_request(&req("/?error=access_denied&state=S1", "127.0.0.1:5000"), 5000, "S1"),
        Ok(Callback::Denied(e)) if e == "access_denied"
    ));
    assert!(
        judge_request(
            &req("/?error=access_denied&state=nope", "127.0.0.1:5000"),
            5000,
            "S1"
        )
        .is_err()
    );
}

// ---- token reply parsing --------------------------------------------------

#[test]
fn parses_good_reply_and_caps_lifetime() {
    let t = parse_token_reply(&ok_reply(
        r#"{"access_token":"a","refresh_token":"r","token_type":"Bearer","expires_in":999999999}"#,
    ))
    .unwrap();
    assert_eq!(t.access_token.expose_secret(), "a");
    assert_eq!(t.refresh_token.unwrap().expose_secret(), "r");
    assert_eq!(t.expires_in, Duration::from_secs(24 * 3600));
    let t = parse_token_reply(&ok_reply(r#"{"access_token":"a"}"#)).unwrap();
    assert_eq!(t.expires_in, Duration::from_secs(3600));
    assert!(t.refresh_token.is_none());
}

#[test]
fn classifies_error_replies() {
    let bad = |status, body: &str| {
        parse_token_reply(&HttpReply {
            status,
            body: body.as_bytes().to_vec(),
        })
        .unwrap_err()
    };
    assert!(matches!(
        bad(400, r#"{"error":"invalid_grant"}"#),
        OAuthError::ReauthRequired
    ));
    assert!(matches!(
        bad(401, r#"{"error":"invalid_client"}"#),
        OAuthError::ClientRejected(_)
    ));
    assert!(matches!(bad(503, "<html>"), OAuthError::Transient(_)));
    assert!(matches!(bad(429, "{}"), OAuthError::Transient(_)));
    assert!(matches!(
        bad(400, r#"{"error":"server_error"}"#),
        OAuthError::Transient(_)
    ));
    assert!(matches!(
        bad(400, r#"{"error":"weird"}"#),
        OAuthError::Rejected(_)
    ));
    assert!(matches!(bad(200, "not json"), OAuthError::Protocol(_)));
    assert!(matches!(bad(200, "{}"), OAuthError::Protocol(_)));
    assert!(matches!(
        bad(200, r#"{"access_token":"a","token_type":"mac"}"#),
        OAuthError::Protocol(_)
    ));
    assert!(matches!(bad(302, "{}"), OAuthError::Rejected(_)));
}

#[test]
fn error_descriptions_are_trimmed_and_cleaned() {
    let long = format!(
        r#"{{"error":"invalid_scope","error_description":"{}\u0007x"}}"#,
        "y".repeat(500)
    );
    let e = parse_token_reply(&HttpReply {
        status: 400,
        body: long.into_bytes(),
    })
    .unwrap_err()
    .to_string();
    assert!(e.len() < 400 && !e.contains('\u{7}'));
}

#[test]
fn transient_flag_and_advice_are_set() {
    assert!(OAuthError::Transient("x".into()).is_transient());
    assert!(!OAuthError::ReauthRequired.is_transient());
    assert!(
        OAuthError::ReauthRequired
            .advice()
            .contains("sign in again")
    );
}

// ---- full loopback flow ---------------------------------------------------

/// Plays the browser: sends one raw request to the loopback listener.
fn browser(port: u16, path: impl Into<String>) -> impl Future<Output = String> {
    let path: String = path.into(); // copied eagerly so callers may pass temporaries
    async move {
        let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        s.write_all(req(&path, &format!("127.0.0.1:{port}")).as_bytes())
            .await
            .unwrap();
        let mut out = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(5), s.read_to_end(&mut out)).await;
        String::from_utf8_lossy(&out).into_owned()
    }
}

fn params_of(auth_url: &str) -> HashMap<String, String> {
    let url = url::Url::parse(auth_url).unwrap();
    url.query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

#[tokio::test]
async fn full_google_flow_with_stray_requests_in_between() {
    let pending = begin(OAuthProvider::Google, &google(), "me@gmail.com")
        .await
        .unwrap();
    let p = params_of(pending.auth_url());
    assert_eq!(p["response_type"], "code");
    assert_eq!(p["code_challenge_method"], "S256");
    assert_eq!(p["access_type"], "offline");
    assert_eq!(p["login_hint"], "me@gmail.com");
    assert_eq!(p["scope"], "https://mail.google.com/");
    let state = p["state"].clone();
    assert!(state.len() >= 20);
    let port = pending.port;
    assert_eq!(p["redirect_uri"], format!("http://127.0.0.1:{port}/"));

    let fake = Fake::with(vec![ok_reply(
        r#"{"access_token":"AT","refresh_token":"RT","token_type":"Bearer","expires_in":3600}"#,
    )]);

    let driver = async {
        let r = browser(port, "/favicon.ico").await;
        assert!(r.starts_with("HTTP/1.1 404"), "{r}");
        let r = browser(port, "/?code=evil&state=wrong").await;
        assert!(r.starts_with("HTTP/1.1 400"), "{r}");
        let r = browser(port, format!("/?code=good-code&state={state}")).await;
        assert!(r.starts_with("HTTP/1.1 200"), "{r}");
        assert!(r.contains("Referrer-Policy: no-referrer"));
        assert!(!r.contains("good-code"), "page must not echo input");
    };
    let (tokens, ()) = tokio::join!(pending.complete(&fake, Duration::from_secs(10)), driver);
    let tokens = tokens.unwrap();
    assert_eq!(tokens.access_token.expose_secret(), "AT");

    let bodies = fake.bodies();
    assert_eq!(
        bodies.len(),
        1,
        "stray requests must not reach the token endpoint"
    );
    let q = query_of(&bodies[0]);
    assert_eq!(q["grant_type"], "authorization_code");
    assert_eq!(q["code"], "good-code");
    assert_eq!(q["client_secret"], "shh");
    assert_eq!(q["redirect_uri"], format!("http://127.0.0.1:{port}/"));
    // The verifier sent now must hash to the challenge sent in the browser URL.
    assert_eq!(pkce_challenge(&q["code_verifier"]), p["code_challenge"]);
}

#[tokio::test]
async fn microsoft_flow_uses_localhost_redirect_and_no_secret() {
    let pending = begin(OAuthProvider::Microsoft, &microsoft(None), "")
        .await
        .unwrap();
    let p = params_of(pending.auth_url());
    assert!(p["redirect_uri"].starts_with("http://localhost:"));
    assert!(!p.contains_key("login_hint"));
    assert!(!p.contains_key("access_type"));
    let (port, state) = (pending.port, p["state"].clone());
    let fake = Fake::with(vec![ok_reply(
        r#"{"access_token":"AT","refresh_token":"RT"}"#,
    )]);
    let (tokens, _) = tokio::join!(
        pending.complete(&fake, Duration::from_secs(10)),
        browser(port, format!("/?code=c&state={state}"))
    );
    tokens.unwrap();
    let q = query_of(&fake.bodies()[0]);
    assert!(!q.contains_key("client_secret"));
    assert!(q["scope"].contains("offline_access"));
}

#[tokio::test]
async fn user_denying_access_is_reported() {
    let pending = begin(OAuthProvider::Google, &google(), "").await.unwrap();
    let (port, state) = (pending.port, params_of(pending.auth_url())["state"].clone());
    let fake = Fake::default();
    let (res, _) = tokio::join!(
        pending.complete(&fake, Duration::from_secs(10)),
        browser(port, format!("/?error=access_denied&state={state}"))
    );
    assert!(matches!(res, Err(OAuthError::Denied(e)) if e == "access_denied"));
    assert!(fake.bodies().is_empty());
}

#[tokio::test]
async fn missing_refresh_token_is_an_error() {
    let pending = begin(OAuthProvider::Google, &google(), "").await.unwrap();
    let (port, state) = (pending.port, params_of(pending.auth_url())["state"].clone());
    let fake = Fake::with(vec![ok_reply(r#"{"access_token":"AT"}"#)]);
    let (res, _) = tokio::join!(
        pending.complete(&fake, Duration::from_secs(10)),
        browser(port, format!("/?code=c&state={state}"))
    );
    assert!(matches!(res, Err(OAuthError::NoRefreshToken)));
}

#[tokio::test]
async fn abandoned_sign_in_times_out() {
    let pending = begin(OAuthProvider::Google, &google(), "").await.unwrap();
    let res = pending
        .complete(&Fake::default(), Duration::from_millis(150))
        .await;
    assert!(matches!(res, Err(OAuthError::Timeout)));
}

#[tokio::test]
async fn stalled_connection_does_not_block_the_real_callback_forever() {
    let pending = begin(OAuthProvider::Google, &google(), "").await.unwrap();
    let (port, state) = (pending.port, params_of(pending.auth_url())["state"].clone());
    let fake = Fake::with(vec![ok_reply(
        r#"{"access_token":"AT","refresh_token":"RT"}"#,
    )]);
    let driver = async {
        // A local process connects and says nothing; the 5 s read limit must free the loop.
        let _stall = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        browser(port, format!("/?code=c&state={state}")).await
    };
    let (res, _) = tokio::join!(pending.complete(&fake, Duration::from_secs(20)), driver);
    res.unwrap();
}

// ---- token manager --------------------------------------------------------

fn manager(replies: Vec<HttpReply>) -> (TokenManager<Fake>, AccountId, Arc<MemorySecretStore>) {
    let store = Arc::new(MemorySecretStore::new());
    let id = AccountId::generate();
    let m = TokenManager::new(store.clone(), Fake::with(replies));
    (m, id, store)
}

fn stored(store: &MemorySecretStore, id: &AccountId) -> Option<String> {
    store
        .get(id, SecretKind::OAuthRefreshToken)
        .unwrap()
        .map(|s| s.expose_secret().to_owned())
}

fn tokens(access: &str, refresh: Option<&str>, secs: u64) -> TokenSet {
    TokenSet {
        access_token: SecretString::from(access.to_owned()),
        refresh_token: refresh.map(|r| SecretString::from(r.to_owned())),
        expires_in: Duration::from_secs(secs),
    }
}

#[tokio::test]
async fn fresh_cached_token_is_reused_without_network() {
    let (m, id, store) = manager(vec![]);
    m.save_initial(&id, tokens("AT1", Some("RT1"), 3600))
        .await
        .unwrap();
    assert_eq!(stored(&store, &id).as_deref(), Some("RT1"));
    for _ in 0..3 {
        let t = m
            .access_token(&id, OAuthProvider::Google, &google())
            .await
            .unwrap();
        assert_eq!(t.expose_secret(), "AT1");
    }
    assert!(m.transport.bodies().is_empty());
}

#[tokio::test]
async fn expired_token_is_refreshed_and_rotation_is_persisted() {
    let (m, id, store) = manager(vec![ok_reply(
        r#"{"access_token":"AT2","refresh_token":"RT2","expires_in":3600}"#,
    )]);
    // 30 s left is inside the 60 s safety margin, so it counts as expired.
    m.save_initial(&id, tokens("AT1", Some("RT1"), 30))
        .await
        .unwrap();
    let t = m
        .access_token(&id, OAuthProvider::Microsoft, &microsoft(None))
        .await
        .unwrap();
    assert_eq!(t.expose_secret(), "AT2");
    assert_eq!(
        stored(&store, &id).as_deref(),
        Some("RT2"),
        "rotated token must replace the old one"
    );
    let q = query_of(&m.transport.bodies()[0]);
    assert_eq!(q["grant_type"], "refresh_token");
    assert_eq!(q["refresh_token"], "RT1");
}

#[tokio::test]
async fn refresh_without_rotation_keeps_the_stored_token() {
    let (m, id, store) = manager(vec![ok_reply(
        r#"{"access_token":"AT2","expires_in":3600}"#,
    )]);
    m.save_initial(&id, tokens("AT1", Some("RT1"), 1))
        .await
        .unwrap();
    m.access_token(&id, OAuthProvider::Google, &google())
        .await
        .unwrap();
    assert_eq!(stored(&store, &id).as_deref(), Some("RT1"));
}

#[tokio::test]
async fn parallel_callers_trigger_exactly_one_refresh() {
    let store = Arc::new(MemorySecretStore::new());
    let id = AccountId::generate();
    let mut fake = Fake::with(vec![ok_reply(
        r#"{"access_token":"AT2","expires_in":3600}"#,
    )]);
    fake.delay = Duration::from_millis(100);
    let m = Arc::new(TokenManager::new(store.clone(), fake));
    m.save_initial(&id, tokens("AT1", Some("RT1"), 1))
        .await
        .unwrap();

    let mut handles = Vec::new();
    for _ in 0..6 {
        let (m, id) = (m.clone(), id.clone());
        handles.push(tokio::spawn(async move {
            m.access_token(&id, OAuthProvider::Google, &google())
                .await
                .unwrap()
                .expose_secret()
                .to_owned()
        }));
    }
    for h in handles {
        assert_eq!(h.await.unwrap(), "AT2");
    }
    assert_eq!(m.transport.bodies().len(), 1);
}

#[tokio::test]
async fn revoked_grant_requires_sign_in_again() {
    let (m, id, _store) = manager(vec![HttpReply {
        status: 400,
        body: br#"{"error":"invalid_grant"}"#.to_vec(),
    }]);
    m.save_initial(&id, tokens("AT1", Some("RT1"), 1))
        .await
        .unwrap();
    let e = m
        .access_token(&id, OAuthProvider::Google, &google())
        .await
        .unwrap_err();
    assert!(matches!(e, OAuthError::ReauthRequired));
}

#[tokio::test]
async fn no_stored_refresh_token_requires_sign_in() {
    let (m, id, _store) = manager(vec![]);
    let e = m
        .access_token(&id, OAuthProvider::Google, &google())
        .await
        .unwrap_err();
    assert!(matches!(e, OAuthError::ReauthRequired));
    assert!(m.transport.bodies().is_empty());
}

#[tokio::test]
async fn force_refresh_ignores_a_fresh_cache() {
    let (m, id, _s) = manager(vec![ok_reply(
        r#"{"access_token":"AT2","expires_in":3600}"#,
    )]);
    m.save_initial(&id, tokens("AT1", Some("RT1"), 3600))
        .await
        .unwrap();
    let t = m
        .force_refresh(&id, OAuthProvider::Google, &google())
        .await
        .unwrap();
    assert_eq!(t.expose_secret(), "AT2");
}

#[tokio::test]
async fn transient_failure_keeps_the_refresh_token() {
    let (m, id, store) = manager(vec![HttpReply {
        status: 503,
        body: vec![],
    }]);
    m.save_initial(&id, tokens("AT1", Some("RT1"), 1))
        .await
        .unwrap();
    let e = m
        .access_token(&id, OAuthProvider::Google, &google())
        .await
        .unwrap_err();
    assert!(e.is_transient());
    assert_eq!(stored(&store, &id).as_deref(), Some("RT1"));
}

#[tokio::test]
async fn save_initial_requires_a_refresh_token() {
    let (m, id, _s) = manager(vec![]);
    assert!(matches!(
        m.save_initial(&id, tokens("AT", None, 60)).await,
        Err(OAuthError::NoRefreshToken)
    ));
}

#[test]
fn http_clients_accept_our_tls_configuration() {
    // reqwest silently refuses a TLS config of the wrong type at build time, so a
    // successful build proves the preconfigured ring + OS-verifier config is used.
    assert!(HttpTransport::new().is_ok());
    assert!(crate::autoconfig::HttpFetcher::new().is_ok());
}

#[test]
fn text_from_the_provider_cannot_hide_or_flip_what_the_person_reads() {
    // Direction override, zero-width space, line break and a very long tail.
    let nasty = format!("denied\u{202E}evil\u{200B}\r\nline{}", "x".repeat(500));
    let t = tidy(&nasty);
    assert!(
        !t.chars()
            .any(|c| c.is_control() || c == '\u{202E}' || c == '\u{200B}'),
        "{t:?}"
    );
    assert!(t.chars().count() <= 200);
    assert!(t.starts_with("deniedevil"));
}
