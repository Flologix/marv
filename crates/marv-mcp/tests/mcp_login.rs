//! Ports Marv's `tests/mcp-login.test.ts`: OAuth logins against a fake authorization server (Marv #18). What a
//! background connection may do (never register, never open anything), /mcp login and logout, refresh, and two
//! managers sharing one token store. Nothing leaves the machine.

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use marv_mcp::config::ServerSource;
use marv_mcp::manager::{McpLogin, McpManager, McpState};
use marv_mcp::oauth::{Authorize, CallbackCheck, Revoked, probe_login, probe_login_within};
use marv_mcp::oauth_store::{McpTokenStore, login_key};
use marv_mcp::trust::McpTrust;
use serde_json::{Value, json};
use support::*;
use tokio_util::sync::CancellationToken;
use url::Url;

struct Env {
    root: std::path::PathBuf,
    store: McpTokenStore,
}

fn env() -> Env {
    let root = temp_dir("login-root");
    let store = McpTokenStore::new(temp_dir("login-conf").join("mcp-tokens.json"));
    Env { root, store }
}

impl Env {
    fn opts(&self) -> Opts {
        let mut o = Opts::new(self.root.clone());
        o.logins = Some(self.store.clone());
        o
    }
    fn manager(&self, servers: Vec<marv_mcp::config::McpServerConfig>) -> Arc<McpManager> {
        manager(servers, self.opts())
    }
    fn key(&self, source: ServerSource, url: &str) -> String {
        login_key(source, url, &format!("proj-{}", self.root.display()))
    }
}

fn project(name: &str, url: &str) -> marv_mcp::config::McpServerConfig {
    config(name, ServerSource::Project, json!({"type": "http", "url": url}))
}

fn browser() -> Authorize {
    Arc::new(|url| {
        tokio::spawn(browse(url));
    })
}

async fn log_in(m: &Arc<McpManager>, name: &str) -> marv_mcp::Result<marv_mcp::manager::McpServerStatus> {
    m.login(name, browser())?.done.await.expect("the login answers")
}

/// The authorization URL once the login has asked for it; and the browser's address after the redirect, unfollowed.
fn capture() -> (Authorize, Arc<Mutex<Option<Url>>>) {
    let slot = Arc::new(Mutex::new(None));
    let s = slot.clone();
    (Arc::new(move |url| *s.lock().unwrap() = Some(url)), slot)
}

async fn opened(slot: &Arc<Mutex<Option<Url>>>) -> Url {
    for _ in 0..500 {
        if let Some(url) = slot.lock().unwrap().clone() {
            return url;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the login never opened anything");
}

async fn err(login: McpLogin) -> String {
    login.done.await.unwrap().unwrap_err().to_string()
}

#[tokio::test]
async fn without_a_stored_login_needs_login_and_nothing_is_registered_or_opened() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    let started = Instant::now();
    m.start().await;
    assert!(started.elapsed() < Duration::from_secs(5));
    let st = &m.status()[0];
    assert_eq!((st.state, st.tools.len(), st.error.clone()), (McpState::NeedsLogin, 0, None));
    for path in ["/register", "/authorize", "/token"] {
        assert!(!s.paths().contains(&path.to_string()), "{path}");
    }
}

#[tokio::test]
async fn an_own_authorization_header_thats_refused_is_failed_not_needs_login() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({"headers": {"Authorization": "Bearer wrong"}}))]);
    m.start().await;
    let st = &m.status()[0];
    assert_eq!(st.state, McpState::Failed);
    assert!(st.error.as_deref().unwrap().contains("refused the token in headers"));
}

#[tokio::test]
async fn logs_in_through_the_browser_and_stores_client_tokens_and_discovery() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    let seen = Arc::new(Mutex::new(vec![]));
    let log = seen.clone();
    let login = m
        .login(
            "remote",
            Arc::new(move |url: Url| {
                log.lock().unwrap().push(url.clone());
                tokio::spawn(browse(url));
            }),
        )
        .unwrap();
    let status = login.done.await.unwrap().unwrap();
    assert_eq!(status.state, McpState::Connected);
    let l = status.login.unwrap();
    assert!(l.logged_in && l.host.unwrap().contains("127.0.0.1"));
    assert!(m.specs().iter().any(|x| x.name == "mcp__remote__echo"));
    let seen = seen.lock().unwrap().clone();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].query_pairs().find(|(k, _)| k == "code_challenge_method").unwrap().1, "S256");
    let saved = e.store.get(&e.key(ServerSource::Personal, &s.url)).unwrap();
    assert!(saved.client.unwrap().port.unwrap() > 0);
    assert!(saved.tokens.unwrap().access_token.starts_with("at-"));
    assert!(saved.discovery.unwrap().authorization_server_url.contains("127.0.0.1"));
    assert_eq!(call(&m, "mcp__remote__echo", json!({"text": "hi"})).await.0, "echo: hi");
}

#[tokio::test]
async fn the_same_through_a_pasted_callback_address_and_a_wrong_state_is_refused() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    let (authorize, slot) = capture();
    let mut login = m.login("remote", authorize).unwrap();
    let real = Url::parse(&redirect_of(opened(&slot).await).await).unwrap();
    let mut forged = real.clone();
    let pairs: Vec<(String, String)> = real
        .query_pairs()
        .map(|(k, v)| {
            let v = if k == "state" { "guessed".into() } else { v.into_owned() };
            (k.into_owned(), v)
        })
        .collect();
    forged.query_pairs_mut().clear().extend_pairs(pairs);
    assert_eq!((login.paste)(forged.as_str()), CallbackCheck::Refused { why: "state".into() });
    assert!(matches!((login.paste)(real.as_str()), CallbackCheck::Ok { .. }));
    assert_eq!(login.done.await.unwrap().unwrap().state, McpState::Connected);
}

#[tokio::test]
async fn a_restart_reuses_the_login_without_discovery_or_a_browser() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let first = e.manager(vec![remote("remote", &s.url, json!({}))]);
    first.start().await;
    log_in(&first, "remote").await.unwrap();
    let before = s.requests();
    let second = e.manager(vec![remote("remote", &s.url, json!({}))]);
    second.start().await;
    assert_eq!(second.status()[0].state, McpState::Connected);
    assert!(s.paths()[before..].iter().all(|p| p == "/mcp"));
}

#[tokio::test]
async fn renaming_keeps_the_login_a_project_server_with_the_same_url_doesnt_get_it() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let first = e.manager(vec![remote("remote", &s.url, json!({}))]);
    first.start().await;
    log_in(&first, "remote").await.unwrap();
    let renamed = e.manager(vec![remote("other", &s.url, json!({})), project("proj", &s.url)]);
    renamed.start().await;
    let states: Vec<_> = renamed.status().iter().map(|x| x.state).collect();
    assert_eq!(states, [McpState::Connected, McpState::NeedsLogin]);
}

#[tokio::test]
async fn a_second_login_reuses_the_port_with_it_taken_it_registers_again() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    log_in(&m, "remote").await.unwrap();
    let key = e.key(ServerSource::Personal, &s.url);
    let port = e.store.get(&key).unwrap().client.unwrap().port.unwrap();
    log_in(&m, "remote").await.unwrap();
    assert_eq!(s.count("/register"), 1);
    // The last login's dropped listener closes its socket only when its task next runs, so binding may take a moment.
    let mut busy = None;
    for _ in 0..50 {
        if let Ok(l) = std::net::TcpListener::bind(("127.0.0.1", port)) {
            busy = Some(l);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let busy = busy.expect("the last login's port is free again");
    log_in(&m, "remote").await.unwrap();
    drop(busy);
    assert_eq!(s.count("/register"), 2);
    assert_ne!(e.store.get(&key).unwrap().client.unwrap().port, Some(port));
}

#[tokio::test]
async fn without_dynamic_registration_it_needs_a_client_id_and_with_one_never_registers() {
    let (e, s) = (env(), oauth_server(OAuthOptions { no_dcr: true, ..Default::default() }).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    let err = log_in(&m, "remote").await.unwrap_err().to_string();
    assert!(err.contains("this server needs a registered client: add oauth.clientId to its config"), "{err}");

    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let pre = oauth_server(OAuthOptions {
        no_dcr: true,
        pre_registered: Some(("hand-made".into(), Some("shh".into()), redirect)),
        ..Default::default()
    })
    .await;
    let n = e.manager(vec![remote(
        "remote",
        &pre.url,
        json!({"oauth": {"clientId": "hand-made", "clientSecret": "shh", "callbackPort": port}}),
    )]);
    n.start().await;
    assert_eq!(log_in(&n, "remote").await.unwrap().state, McpState::Connected);
    assert!(!pre.paths().contains(&"/register".to_string()));
    // The configured secret isn't copied into the store.
    assert!(!std::fs::read_to_string(&e.store.path).unwrap().contains("shh"));
}

#[tokio::test]
async fn oauth_scope_is_what_marv_asks_for() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({"oauth": {"scope": "mcp:read"}}))]);
    m.start().await;
    log_in(&m, "remote").await.unwrap();
    assert_eq!(s.scopes(), [Some("mcp:read".to_string())]);
    let t = oauth_server(OAuthOptions::default()).await;
    let n = e.manager(vec![remote("plain", &t.url, json!({}))]);
    n.start().await;
    log_in(&n, "plain").await.unwrap();
    assert_eq!(t.scopes(), [Some("mcp:read mcp:write".to_string())]);
}

#[tokio::test]
async fn an_untrusted_project_server_refuses_login_and_logout_with_no_request_at_all() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let mut o = e.opts();
    o.trust = Some(McpTrust::new(e.root.join("trust.json")));
    let m = manager(vec![project("remote", &s.url)], o);
    m.start().await;
    assert_eq!(m.status()[0].state, McpState::Untrusted);
    assert!(log_in(&m, "remote").await.unwrap_err().to_string().contains("trust it first: /mcp trust"));
    let key = e.key(ServerSource::Project, &s.url);
    e.store
        .update(&key, |_| {
            let mut l = marv_mcp::oauth_store::Login::new("remote");
            l.tokens = serde_json::from_value(json!({"access_token": "a", "refresh_token": "r", "token_type": "Bearer"})).ok();
            Ok(Some(l))
        })
        .await
        .unwrap();
    assert!(m.logout("remote").await.unwrap_err().to_string().contains("trust it first: /mcp trust"));
    assert_eq!(m.status()[0].state, McpState::Untrusted);
    assert!(e.store.get(&key).is_some());
    assert_eq!(s.requests(), 0);
}

#[tokio::test]
async fn an_unreadable_token_store_is_never_written_over() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    std::fs::write(&e.store.path, r#"{"v":2,"logins":{}}"#).unwrap();
    let err = log_in(&m, "remote").await.unwrap_err().to_string();
    assert!(err.contains("won't write over it"), "{err}");
    assert_eq!(std::fs::read_to_string(&e.store.path).unwrap(), r#"{"v":2,"logins":{}}"#);
}

#[tokio::test]
async fn a_call_during_a_logins_reconnect_gets_an_answer_or_marvs_sentence() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    log_in(&m, "remote").await.unwrap();
    let results = Arc::new(Mutex::new(vec![]));
    let (weak, out) = (Arc::downgrade(&m), results.clone());
    m.set_on_change(move || {
        let Some(m) = weak.upgrade() else { return };
        if m.status()[0].state == McpState::Connecting {
            out.lock()
                .unwrap()
                .push(tokio::spawn(async move { call(&m, "mcp__remote__echo", json!({"text": "during"})).await.0 }));
        }
    });
    log_in(&m, "remote").await.unwrap();
    let handles: Vec<_> = std::mem::take(&mut *results.lock().unwrap());
    assert!(!handles.is_empty());
    for h in handles {
        let out = h.await.unwrap();
        assert!(
            [
                "echo: during",
                "remote's echo failed: the server is reconnecting after a login: call it again in a moment.",
                "remote needs a new login: the user can run /mcp login remote"
            ]
            .contains(&out.as_str()),
            "{out}"
        );
    }
    assert_eq!(call(&m, "mcp__remote__echo", json!({"text": "after"})).await.0, "echo: after");
}

#[tokio::test]
async fn a_stdio_server_an_own_header_one_or_an_unknown_name_refused_saying_why() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![
        config("local", ServerSource::Personal, json!({"command": "true"})),
        remote("headed", &s.url, json!({"headers": {"authorization": "Bearer x"}})),
    ]);
    m.start().await;
    assert!(log_in(&m, "local").await.unwrap_err().to_string().contains("only remote (http) servers log in"));
    assert!(log_in(&m, "headed").await.unwrap_err().to_string().contains("Authorization header"));
    assert!(m.login("nope", browser()).err().unwrap().to_string().contains("no MCP server named \"nope\""));
}

#[tokio::test]
async fn a_second_login_cancels_the_first_and_a_login_times_out() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let mut o = e.opts();
    o.login_timeout = Duration::from_millis(300);
    let m = manager(vec![remote("remote", &s.url, json!({}))], o);
    m.start().await;
    let nothing: Authorize = Arc::new(|_| {});
    let first = m.login("remote", nothing.clone()).unwrap();
    let second = m.login("remote", nothing).unwrap();
    assert!(err(first).await.contains("cancelled"));
    assert!(err(second).await.contains("timed out"));
    assert_eq!(m.status()[0].state, McpState::NeedsLogin);
}

async fn tokens(e: &Env, url: &str) -> Option<Value> {
    e.store.get(&e.key(ServerSource::Personal, url)).and_then(|l| l.tokens).map(|t| serde_json::to_value(t).unwrap())
}

async fn until(f: impl Fn() -> bool) {
    for _ in 0..1000 {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("never happened");
}

#[tokio::test]
async fn remove_while_the_browser_is_out_cancels_and_a_late_callback_does_nothing() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    let (authorize, slot) = capture();
    let mut login = m.login("remote", authorize).unwrap();
    let href = redirect_of(opened(&slot).await).await;
    m.remove("remote").await.unwrap();
    let done = login.done.await.unwrap();
    assert!(done.unwrap_err().to_string().contains("cancelled: /mcp remove"));
    assert!(!matches!((login.paste)(&href), CallbackCheck::Ok { .. }));
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(m.status()[0].state, McpState::Removed);
    assert!(!s.paths().contains(&"/token".to_string()));
    m.forget_removed();
    assert!(m.status().is_empty());
}

#[tokio::test]
async fn remove_while_the_code_is_exchanged_saves_no_tokens_and_doesnt_reconnect() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    let release = s.hold_tokens();
    let login = m.login("remote", browser()).unwrap();
    until(|| s.count("/token") > 0).await;
    m.remove("remote").await.unwrap();
    release.send_replace(true);
    assert!(err(login).await.contains("cancelled: /mcp remove"));
    assert!(tokens(&e, &s.url).await.is_none());
    assert_eq!(m.status()[0].state, McpState::Removed);
}

#[tokio::test]
async fn logout_while_a_new_logins_code_is_exchanged_the_late_tokens_dont_undo_it() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    log_in(&m, "remote").await.unwrap();
    let before = s.count("/token");
    let release = s.hold_tokens();
    let login = m.login("remote", browser()).unwrap();
    until(|| s.count("/token") > before).await;
    assert_eq!(m.logout("remote").await.unwrap(), Revoked::Revoked);
    release.send_replace(true);
    assert!(err(login).await.contains("cancelled: /mcp logout"));
    assert!(tokens(&e, &s.url).await.is_none());
    assert_eq!(m.status()[0].state, McpState::NeedsLogin);
}

#[tokio::test]
async fn a_login_superseded_while_its_code_is_exchanged_saves_nothing() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    let release = s.hold_tokens();
    let first = m.login("remote", browser()).unwrap();
    until(|| s.count("/token") > 0).await;
    let _second = m.login("remote", Arc::new(|_| {})).unwrap();
    release.send_replace(true);
    assert!(err(first).await.contains("cancelled: another login started"));
    assert!(tokens(&e, &s.url).await.is_none());
    assert_eq!(m.status()[0].state, McpState::NeedsLogin);
}

#[tokio::test]
async fn an_expired_access_token_is_refreshed_transparently() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    log_in(&m, "remote").await.unwrap();
    let old = tokens(&e, &s.url).await.unwrap();
    s.expire();
    assert_eq!(call(&m, "mcp__remote__echo", json!({"text": "again"})).await.0, "echo: again");
    let now = tokens(&e, &s.url).await.unwrap();
    assert_ne!(now["access_token"], old["access_token"]);
    assert_ne!(now["refresh_token"], old["refresh_token"]);
}

#[tokio::test]
async fn a_refused_refresh_mid_call_needs_a_login_and_opens_nothing() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    log_in(&m, "remote").await.unwrap();
    let changes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let c = changes.clone();
    m.set_on_change(move || {
        c.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    });
    let authorizes = s.count("/authorize");
    s.expire();
    s.refuse_refresh();
    let sentence = "remote needs a new login: the user can run /mcp login remote".to_string();
    assert_eq!(call(&m, "mcp__remote__echo", json!({"text": "x"})).await, (sentence.clone(), true));
    assert_eq!(m.status()[0].state, McpState::NeedsLogin);
    assert!(changes.load(std::sync::atomic::Ordering::SeqCst) > 0);
    assert_eq!(s.count("/authorize"), authorizes);
    // Its tools stay listed, and the next call fails the same way without asking the server.
    assert!(m.tools().iter().any(|t| t.name == "mcp__remote__echo"));
    let before = s.requests();
    assert_eq!(call(&m, "mcp__remote__echo", json!({"text": "y"})).await.0, sentence);
    assert_eq!(s.requests(), before);
    log_in(&m, "remote").await.unwrap();
    assert_eq!(call(&m, "mcp__remote__echo", json!({"text": "z"})).await.0, "echo: z");
}

#[tokio::test]
async fn two_managers_on_one_store_survive_a_rotating_refresh() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let a = e.manager(vec![remote("remote", &s.url, json!({}))]);
    a.start().await;
    log_in(&a, "remote").await.unwrap();
    let mut o = e.opts();
    o.logins = Some(McpTokenStore::new(e.store.path.clone()));
    let b = manager(vec![remote("remote", &s.url, json!({}))], o);
    b.start().await;
    assert_eq!(b.status()[0].state, McpState::Connected);
    s.expire();
    assert_eq!(call(&a, "mcp__remote__echo", json!({"text": "a"})).await.0, "echo: a");
    assert_eq!(call(&b, "mcp__remote__echo", json!({"text": "b"})).await.0, "echo: b");
    s.expire();
    assert_eq!(call(&b, "mcp__remote__echo", json!({"text": "b2"})).await.0, "echo: b2");
    assert_eq!(call(&a, "mcp__remote__echo", json!({"text": "a2"})).await.0, "echo: a2");
}

#[tokio::test]
async fn logout_revokes_forgets_and_the_next_call_needs_a_login() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    log_in(&m, "remote").await.unwrap();
    let refresh = tokens(&e, &s.url).await.unwrap()["refresh_token"].as_str().unwrap().to_string();
    assert_eq!(m.logout("remote").await.unwrap(), Revoked::Revoked);
    assert_eq!(s.revoked(), [refresh]);
    assert!(e.store.get(&e.key(ServerSource::Personal, &s.url)).is_none());
    assert_eq!(m.status()[0].state, McpState::NeedsLogin);
    assert_eq!(
        call(&m, "mcp__remote__echo", json!({"text": "x"})).await.0,
        "remote needs a new login: the user can run /mcp login remote"
    );
}

#[tokio::test]
async fn without_a_revocation_endpoint_logout_still_forgets_and_says_the_grant_remains() {
    let (e, s) = (env(), oauth_server(OAuthOptions { no_revocation: true, ..Default::default() }).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    log_in(&m, "remote").await.unwrap();
    assert_eq!(m.logout("remote").await.unwrap(), Revoked::Unsupported);
    assert!(e.store.get(&e.key(ServerSource::Personal, &s.url)).is_none());
}

#[tokio::test]
async fn logging_out_of_a_server_that_isnt_logged_in_says_so() {
    let (e, s) = (env(), oauth_server(OAuthOptions::default()).await);
    let m = e.manager(vec![remote("remote", &s.url, json!({}))]);
    m.start().await;
    assert!(m.logout("remote").await.unwrap_err().to_string().contains("isn't logged in"));
}

#[tokio::test]
async fn probe_login_true_for_a_login_server_false_otherwise() {
    let s = oauth_server(OAuthOptions::default()).await;
    let open = oauth_server(OAuthOptions { open: true, ..Default::default() }).await;
    assert!(probe_login(&s.url, CancellationToken::new()).await);
    assert!(!probe_login(&open.url, CancellationToken::new()).await);
    let port = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let dead = format!("http://127.0.0.1:{port}/mcp");
    assert!(!probe_login_within(&dead, Duration::from_secs(2), CancellationToken::new()).await);
    assert!(!s.paths().contains(&"/register".to_string()));
}

#[tokio::test]
async fn probe_login_stops_when_the_caller_cancels() {
    // A server that accepts and never answers.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = vec![];
        while let Ok((s, _)) = listener.accept().await {
            held.push(s);
        }
    });
    let cancel = CancellationToken::new();
    let later = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        later.cancel();
    });
    let started = Instant::now();
    assert!(!probe_login(&format!("http://127.0.0.1:{port}/mcp"), cancel).await);
    assert!(started.elapsed() < Duration::from_secs(1));
}
