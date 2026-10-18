//! `docs/SECURITY.md`, "Provider keys and subscription sign-ins" (#159): each provider gets only its own credential,
//! and a plan's limit is a hard stop, never a quiet switch to another key.

use marv_provider::ProviderKind;
use marv_session::config::{Env, FileConfig, needs_setup, resolve_config};

/// The Z.ai Coding Plan never gets the OpenRouter key (saved or in the environment), and OpenRouter never gets the
/// plan's: switching providers asks for the other key instead (setup opens).
#[test]
fn a_providers_key_never_goes_to_another_provider() {
    let env = Env::from([("OPENROUTER_API_KEY".to_string(), "sk-or-env".to_string())]);
    let zai = FileConfig {
        provider: Some(ProviderKind::Zai),
        model: Some("glm-5.3".into()),
        api_key: Some("sk-or-saved".into()),
        ..FileConfig::default()
    };
    let config = resolve_config(Some(&zai), &env);
    assert_eq!(config.api_key, None, "the OpenRouter key must never reach Z.ai");
    assert!(needs_setup(Some(&zai), &config));
    let zai = FileConfig { zai_api_key: Some("zai-saved".into()), ..zai };
    assert_eq!(resolve_config(Some(&zai), &env).api_key.as_deref(), Some("zai-saved"));

    let openrouter = FileConfig { provider: Some(ProviderKind::OpenRouter), api_key: None, ..zai };
    let config = resolve_config(Some(&openrouter), &Env::from([("ZAI_API_KEY".to_string(), "zai-env".to_string())]));
    assert_eq!(config.api_key, None, "the plan's key must never reach OpenRouter");
}

mod sign_in_listener {
    use std::time::Duration;

    use marv_provider::chatgpt::{Callback, StoredAuth, listen};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio_util::sync::CancellationToken;

    /// One GET to the listener; its status line.
    async fn get(port: u16, target: &str) -> std::io::Result<String> {
        let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;
        stream.write_all(format!("GET {target} HTTP/1.1\r\nhost: 127.0.0.1\r\n\r\n").as_bytes()).await?;
        let mut answer = String::new();
        stream.read_to_string(&mut answer).await?;
        Ok(answer.lines().next().unwrap_or_default().to_string())
    }

    /// The ChatGPT sign-in's callback listener binds 127.0.0.1 only, ignores any callback without the sign-in's
    /// state (a sandboxed command shares the loopback network), takes exactly one callback and then stops listening.
    #[tokio::test]
    async fn the_sign_in_listener_is_loopback_only_needs_the_state_and_takes_one_callback() {
        let listener = listen().await.unwrap();
        assert!(listener.local_addr().unwrap().ip().is_loopback(), "bound to 127.0.0.1 only, never 0.0.0.0");
        assert_eq!(listener.local_addr().unwrap().ip().to_string(), "127.0.0.1");
        let port = listener.port;
        let waiting =
            tokio::spawn(async move { listener.wait("the-state", Duration::from_secs(10), &CancellationToken::new()).await });
        assert!(get(port, "/auth/callback?code=planted&state=guess").await.unwrap().contains("400"));
        assert!(get(port, "/auth/callback?code=planted").await.unwrap().contains("400"));
        assert!(!waiting.is_finished(), "a callback without the state doesn't end the sign-in");
        assert!(get(port, "/auth/callback?code=real&state=the-state").await.unwrap().contains("200"));
        let callback = waiting.await.unwrap().unwrap();
        assert!(matches!(callback, Callback::Code { ref code, .. } if code == "real"), "{callback:?}");
        // Single use: nothing listens any more.
        assert!(get(port, "/auth/callback?code=late&state=the-state").await.is_err());
    }

    /// A sign-in nobody finishes stops listening after its timeout (and on cancel).
    #[tokio::test]
    async fn the_sign_in_listener_times_out() {
        let listener = listen().await.unwrap();
        let port = listener.port;
        let outcome = listener.wait("s", Duration::from_millis(100), &CancellationToken::new()).await;
        assert!(outcome.unwrap_err().contains("timed out"));
        assert!(get(port, "/auth/callback?code=c&state=s").await.is_err());
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(listen().await.unwrap().wait("s", Duration::from_secs(10), &cancel).await.unwrap_err().contains("cancelled"));
    }

    /// The saved sign-in's tokens never show in a `{:?}` (a log line, an error).
    #[test]
    fn the_sign_in_tokens_never_show_in_debug_output() {
        let auth = StoredAuth {
            access_token: Some("at-secret".into()),
            refresh_token: Some("rt-secret".into()),
            id_token: Some("id-secret".into()),
            ..StoredAuth::default()
        };
        assert!(!format!("{auth:?}").contains("secret"));
    }
}
