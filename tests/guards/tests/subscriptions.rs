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
