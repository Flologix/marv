//! Sign in with ChatGPT against a local stand-in for OpenAI's authorization server and API (#159): the authorize
//! address, the callback, the code exchange, the ID token's checks, the private token file, refresh, sign-out and
//! the account's model list. Never the network.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine;
use marv_provider::chatgpt::{
    AuthStore, ChatGptCredentials, Endpoints, PLAN_SCOPE, StoredAuth, check_id_token, list_models, sign_out, start_sign_in,
};
use marv_provider::responses::Credentials;
use reqwest::Url;
use ring::signature::{KeyPair, RsaKeyPair};
use serde_json::{Value, json};
use support::{FakeServer, Reply, Request};
use tokio_util::sync::CancellationToken;

/// A key made for these tests only: it signs the stand-in's ID tokens.
const TEST_KEY: &[u8] = include_bytes!("fixtures/test-only-rsa-key.pk8");

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn key() -> RsaKeyPair {
    RsaKeyPair::from_pkcs8(TEST_KEY).unwrap()
}

fn jwks() -> Value {
    let public: ring::rsa::PublicKeyComponents<Vec<u8>> = key().public_key().into();
    json!({"keys": [{"kty": "RSA", "kid": "k1", "alg": "RS256", "n": b64(&public.n), "e": b64(&public.e)}]})
}

fn sign(claims: &Value) -> String {
    let header = b64(json!({"alg": "RS256", "kid": "k1", "typ": "JWT"}).to_string().as_bytes());
    let payload = b64(claims.to_string().as_bytes());
    let input = format!("{header}.{payload}");
    let key = key();
    let mut signature = vec![0; key.public().modulus_len()];
    key.sign(&ring::signature::RSA_PKCS1_SHA256, &ring::rand::SystemRandom::new(), input.as_bytes(), &mut signature).unwrap();
    format!("{input}.{}", b64(&signature))
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn form(request: &Request) -> Vec<(String, String)> {
    url::form_urlencoded::parse(&request.body).into_owned().collect()
}

fn field(request: &Request, name: &str) -> Option<String> {
    form(request).into_iter().find(|(k, _)| k == name).map(|(_, v)| v)
}

/// What the stand-in's token endpoint signs into the next ID token, and what it answers.
#[derive(Default)]
struct Stage {
    nonce: String,
    audience: String,
    scope: String,
    refresh_error: Option<&'static str>,
}

async fn openai(stage: Arc<Mutex<Stage>>) -> FakeServer {
    let issuer = Arc::new(Mutex::new(String::new()));
    let seen = issuer.clone();
    let server = FakeServer::start(move |r| {
        let stage = stage.lock().unwrap();
        match r.path.as_str() {
            "/.well-known/jwks.json" => Reply::json(200, jwks()),
            "/api/accounts/oauth/token" if field(r, "grant_type").as_deref() == Some("refresh_token") => {
                match stage.refresh_error {
                    Some(code) => Reply::json(400, json!({"error": code})),
                    None => Reply::json(
                        200,
                        json!({"access_token": "at-2", "refresh_token": "rt-2", "expires_in": 3600, "scope": stage.scope}),
                    ),
                }
            }
            "/api/accounts/oauth/token" => {
                let id_token = sign(&json!({
                    "iss": seen.lock().unwrap().clone(), "sub": "user-1", "aud": stage.audience, "exp": now() + 600,
                    "nonce": stage.nonce, "email": "ada@example.com"
                }));
                Reply::json(
                    200,
                    json!({"id_token": id_token, "access_token": "at-1", "refresh_token": "rt-1", "token_type": "Bearer",
                           "expires_in": 3600, "scope": stage.scope}),
                )
            }
            "/api/accounts/oauth/revoke" => Reply::new(200, "text/plain", ""),
            "/v1/models" => Reply::json(
                200,
                json!({"models": [{"slug": "gpt-6.1-sol", "visibility": "list"}, {"slug": "internal", "visibility": "hide"}]}),
            ),
            _ => Reply::text(404, "no"),
        }
    })
    .await;
    *issuer.lock().unwrap() = server.url();
    server
}

fn endpoints(server: &FakeServer) -> Endpoints {
    Endpoints { issuer: server.url(), api: format!("{}/v1", server.url()) }
}

fn param(url: &Url, name: &str) -> Option<String> {
    url.query_pairs().find(|(k, _)| k == name).map(|(_, v)| v.into_owned())
}

/// Plays the browser: opens the callback once the listener waits.
fn browser(callback: String) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = reqwest::get(callback).await;
    });
}

/// Signs in through the stand-in: returns the authorize address the browser was sent to and the outcome.
async fn sign_in(
    store: &AuthStore,
    server: &FakeServer,
    stage: &Arc<Mutex<Stage>>,
    callback_extra: &str,
) -> (Url, Result<marv_provider::chatgpt::Account, String>) {
    let sign_in = start_sign_in(store.clone(), endpoints(server)).await.unwrap();
    let url = Url::parse(sign_in.url()).unwrap();
    {
        let mut stage = stage.lock().unwrap();
        stage.nonce = param(&url, "nonce").unwrap();
    }
    let redirect = param(&url, "redirect_uri").unwrap();
    browser(format!("{redirect}?code=the-code&state={}{callback_extra}", param(&url, "state").unwrap()));
    (url, sign_in.finish_within(Duration::from_secs(5), CancellationToken::new()).await)
}

fn plan_stage() -> Arc<Mutex<Stage>> {
    Arc::new(Mutex::new(Stage {
        audience: "oaiapp_1".into(),
        scope: format!("openid profile email offline_access resource.invoke {PLAN_SCOPE}"),
        ..Stage::default()
    }))
}

#[tokio::test]
async fn a_first_sign_in_registers_and_saves_the_tokens_privately() {
    let stage = plan_stage();
    let server = openai(stage.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    let (url, outcome) = sign_in(&store, &server, &stage, "&client_id=oaiapp_1").await;
    assert_eq!(outcome.unwrap().email.as_deref(), Some("ada@example.com"));

    // The authorize address: a dynamic registration under Marv's name, PKCE S256, a 127.0.0.1 loopback callback.
    assert_eq!(url.path(), "/api/accounts/authorize");
    assert_eq!(param(&url, "client_id").as_deref(), Some("dynamic_agent_client"));
    assert_eq!(param(&url, "agent_name_hint").as_deref(), Some("Marv"));
    assert!(param(&url, "ext_agent_host_id").unwrap().starts_with("urn:uuid:"));
    assert_eq!(param(&url, "code_challenge_method").as_deref(), Some("S256"));
    assert_eq!(param(&url, "resource").as_deref(), Some("https://api.openai.com/v1"));
    assert!(param(&url, "scope").unwrap().contains(PLAN_SCOPE));
    let redirect = param(&url, "redirect_uri").unwrap();
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/auth/callback"), "{redirect}");

    // The exchange: the issued client id, the verifier, the same redirect and resource; no secret.
    let exchange = server.requests().into_iter().find(|r| r.path == "/api/accounts/oauth/token").unwrap();
    assert_eq!(field(&exchange, "client_id").as_deref(), Some("oaiapp_1"));
    assert_eq!(field(&exchange, "code").as_deref(), Some("the-code"));
    assert_eq!(field(&exchange, "redirect_uri"), Some(redirect));
    assert!(field(&exchange, "code_verifier").is_some_and(|v| v.len() >= 43));
    assert!(field(&exchange, "client_secret").is_none());

    let saved = store.load().unwrap();
    assert_eq!((saved.client_id.as_deref(), saved.subject.as_deref()), (Some("oaiapp_1"), Some("user-1")));
    assert_eq!((saved.access_token.as_deref(), saved.refresh_token.as_deref()), (Some("at-1"), Some("rt-1")));
    assert!(store.signed_in());
    assert_eq!(std::fs::metadata(&store.path).unwrap().permissions().mode() & 0o777, 0o600);

    // The next sign-in reuses the issued client and the host id, and names the account.
    let (again, outcome) = sign_in(&store, &server, &stage, "").await;
    outcome.unwrap();
    assert_eq!(param(&again, "client_id").as_deref(), Some("oaiapp_1"));
    assert_eq!(param(&again, "agent_name_hint"), None);
    assert!(param(&again, "id_token_hint").is_some());
    assert_eq!(param(&again, "ext_agent_host_id"), param(&url, "ext_agent_host_id"));
}

#[tokio::test]
async fn nothing_is_saved_without_plan_usage_a_client_id_or_a_matching_audience() {
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::new(dir.path());

    let stage =
        Arc::new(Mutex::new(Stage { audience: "oaiapp_1".into(), scope: "openid offline_access".into(), ..Stage::default() }));
    let server = openai(stage.clone()).await;
    let (_, outcome) = sign_in(&store, &server, &stage, "&client_id=oaiapp_1").await;
    assert!(outcome.unwrap_err().contains("didn't allow plan usage"));
    assert!(store.load().is_none());

    let stage = plan_stage();
    let server = openai(stage.clone()).await;
    let (_, outcome) = sign_in(&store, &server, &stage, "").await;
    assert!(outcome.unwrap_err().contains("didn't issue Marv a client id"));

    stage.lock().unwrap().audience = "oaiapp_other".into();
    let (_, outcome) = sign_in(&store, &server, &stage, "&client_id=oaiapp_1").await;
    assert!(outcome.unwrap_err().contains("audience"));
    assert!(store.load().is_none());
}

#[tokio::test]
async fn a_refused_sign_in_says_so_and_saves_nothing() {
    let stage = plan_stage();
    let server = openai(stage.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    let sign_in = start_sign_in(store.clone(), endpoints(&server)).await.unwrap();
    let url = Url::parse(sign_in.url()).unwrap();
    browser(format!("{}?error=access_denied&state={}", param(&url, "redirect_uri").unwrap(), param(&url, "state").unwrap()));
    let outcome = sign_in.finish_within(Duration::from_secs(5), CancellationToken::new()).await;
    assert_eq!(outcome.unwrap_err(), "OpenAI refused the sign-in (access_denied)");
    assert!(store.load().is_none());
    assert_eq!(server.count("/api/accounts/oauth/token"), 0, "no code exchanged");
}

#[test]
fn the_id_token_is_checked_signature_issuer_audience_expiry_and_nonce() {
    let public: ring::rsa::PublicKeyComponents<Vec<u8>> = key().public_key().into();
    let keys = vec![(Some("k1".to_string()), public.n.clone(), public.e.clone())];
    let claims = |over: Value| {
        let mut c = json!({"iss": "https://auth.openai.com", "sub": "u", "aud": "oaiapp_1", "exp": 2000, "nonce": "N"});
        c.as_object_mut().unwrap().extend(over.as_object().unwrap().clone());
        c
    };
    let check = |token: &str| check_id_token(token, &keys, "https://auth.openai.com", "oaiapp_1", "N", 1000);
    assert_eq!(check(&sign(&claims(json!({})))).unwrap().sub, "u");
    assert!(check(&sign(&claims(json!({"aud": ["x", "oaiapp_1"]})))).is_ok());
    for (over, why) in [
        (json!({"iss": "https://evil.example"}), "issuer"),
        (json!({"aud": "oaiapp_2"}), "audience"),
        (json!({"exp": 900}), "expired"),
        (json!({"nonce": "M"}), "nonce"),
    ] {
        assert!(check(&sign(&claims(over.clone()))).unwrap_err().contains(why), "{over}");
    }
    // A changed payload breaks the signature; so does an unsigned token.
    let token = sign(&claims(json!({})));
    let parts: Vec<&str> = token.split('.').collect();
    let forged = format!("{}.{}.{}", parts[0], b64(claims(json!({"sub": "mallory"})).to_string().as_bytes()), parts[2]);
    assert!(check(&forged).unwrap_err().contains("signature"));
    let none = format!("{}.{}.", b64(br#"{"alg":"none"}"#), parts[1]);
    assert!(check(&none).unwrap_err().contains("algorithm"));
}

fn signed_in(expires_in_secs: u64) -> StoredAuth {
    StoredAuth {
        host_id: "urn:uuid:x".into(),
        client_id: Some("oaiapp_1".into()),
        subject: Some("user-1".into()),
        access_token: Some("at-1".into()),
        refresh_token: Some("rt-1".into()),
        expires_at: (now() + expires_in_secs) * 1000,
        scopes: vec![PLAN_SCOPE.into()],
        ..StoredAuth::default()
    }
}

#[tokio::test]
async fn an_expiring_token_is_refreshed_and_the_rotated_one_saved() {
    let stage = plan_stage();
    let server = openai(stage.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    let credentials = ChatGptCredentials { store: store.clone(), endpoints: endpoints(&server) };

    store.save(&signed_in(3600)).await.unwrap();
    assert_eq!(credentials.bearer().await.unwrap(), "at-1", "still fresh: no refresh");
    assert_eq!(server.count("/api/accounts/oauth/token"), 0);

    store.save(&signed_in(60)).await.unwrap();
    assert_eq!(credentials.bearer().await.unwrap(), "at-2");
    let refresh = server.last();
    assert_eq!(field(&refresh, "refresh_token").as_deref(), Some("rt-1"));
    assert_eq!(field(&refresh, "client_id").as_deref(), Some("oaiapp_1"));
    assert_eq!(field(&refresh, "scope"), None);
    let saved = store.load().unwrap();
    assert_eq!((saved.access_token.as_deref(), saved.refresh_token.as_deref()), (Some("at-2"), Some("rt-2")));

    // A refused token gets a fresh one even before its expiry.
    store.save(&signed_in(3600)).await.unwrap();
    assert_eq!(credentials.refused("at-1").await.unwrap(), "at-2");

    // A refresh token OpenAI refuses ends the sign-in: the tokens go, the client and host stay.
    store.save(&signed_in(60)).await.unwrap();
    stage.lock().unwrap().refresh_error = Some("invalid_grant");
    let why = credentials.bearer().await.unwrap_err();
    assert!(why.contains("invalid_grant") && why.contains("/setup"), "{why}");
    let saved = store.load().unwrap();
    assert!(!saved.signed_in());
    assert_eq!((saved.client_id.as_deref(), saved.host_id.as_str()), (Some("oaiapp_1"), "urn:uuid:x"));
}

#[tokio::test]
async fn signing_out_revokes_and_clears_but_keeps_the_client() {
    let stage = plan_stage();
    let server = openai(stage.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    store.save(&signed_in(3600)).await.unwrap();
    assert!(sign_out(&store, &endpoints(&server)).await.unwrap());
    let revoke = server.last();
    assert_eq!(revoke.path, "/api/accounts/oauth/revoke");
    assert_eq!(
        (field(&revoke, "token").as_deref(), field(&revoke, "token_type_hint").as_deref()),
        (Some("rt-1"), Some("refresh_token"))
    );
    let saved = store.load().unwrap();
    assert!(!saved.signed_in() && saved.id_token.is_none());
    assert_eq!(saved.client_id.as_deref(), Some("oaiapp_1"));
    let credentials = ChatGptCredentials { store, endpoints: endpoints(&server) };
    assert!(credentials.bearer().await.unwrap_err().contains("not signed in"));
}

#[tokio::test]
async fn the_accounts_listed_models_by_slug() {
    let stage = plan_stage();
    let server = openai(stage.clone()).await;
    let dir = tempfile::tempdir().unwrap();
    let store = AuthStore::new(dir.path());
    store.save(&signed_in(3600)).await.unwrap();
    let models = list_models(&ChatGptCredentials { store, endpoints: endpoints(&server) }).await.unwrap();
    assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["gpt-6.1-sol"]);
    assert_eq!(server.last().header("authorization"), Some("Bearer at-1"));
}
