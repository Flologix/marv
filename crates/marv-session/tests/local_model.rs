//! #278: a GGUF model through a llama-server Marv starts, against a stand-in server (`fixtures/fake-llama-server`,
//! Python): started with the model, a local port only, a random API key, `-c` and `--jinja`; each request waits until
//! `/health` says 200 and carries the key; one server per model per process; stopped when Marv exits.

use std::path::{Path, PathBuf};

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent};
use marv_provider::{StreamOptions, create_provider};
use marv_session::config::{Env, FileConfig, resolve_config};
use marv_session::local_model;

fn python3() -> bool {
    std::process::Command::new("python3").arg("--version").output().is_ok_and(|o| o.status.success())
}

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-llama-server")
}

#[tokio::test]
async fn a_local_model_runs_through_the_llama_server_marv_starts_and_stops() {
    if !python3() {
        eprintln!("skipped: no python3 for the stand-in llama-server");
        return;
    }
    let dir = std::env::temp_dir().join(format!("marv-llama-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let model = dir.join("tiny-q4.gguf");
    std::fs::write(&model, b"GGUF").unwrap();
    let file = FileConfig::from_value(&serde_json::json!({
        "provider": "openrouter",
        "contextLength": 4096,
        "llamaCpp": {"server": fixture().display().to_string(), "args": ["-ngl", "0"]}
    }))
    .unwrap();
    // `marv --local-model <gguf>`: this run only, whatever the file's provider.
    let env = Env::from([
        ("MARV_LOCAL_MODEL".to_string(), model.display().to_string()),
        ("HOME".to_string(), dir.display().to_string()),
    ]);
    let config = resolve_config(Some(&file), &env);
    assert_eq!(config.provider, marv_provider::ProviderKind::LlamaCpp);
    let endpoint = config.endpoint.clone().unwrap();
    assert_eq!(endpoint.problem, None);
    assert!(endpoint.base_url.starts_with("http://127.0.0.1:"), "{}", endpoint.base_url);
    let key = endpoint.api_key.clone().unwrap();
    assert!(key.len() >= 32, "a random key, so other local programs can't use the server");
    assert_eq!(endpoint.context_length, Some(4096));

    // The same model again (the config is resolved more than once): the same server.
    assert_eq!(resolve_config(Some(&file), &env).endpoint.unwrap().base_url, endpoint.base_url);

    let provider = create_provider(&config.provider_config(), None, None);
    assert_eq!(provider.name(), "llamacpp · tiny-q4", "the file's name, not its path");
    let events: Vec<_> = provider.stream(&[ChatTurn::User { text: "hi".into() }], StreamOptions::default()).collect().await;
    assert!(events.iter().any(|e| matches!(e, ProviderEvent::TextDelta(t) if t == "hello from llama")), "{events:?}");

    let args: Vec<String> = serde_json::from_str(&std::fs::read_to_string(dir.join("tiny-q4.gguf.args")).unwrap()).unwrap();
    let after = |flag: &str| args.iter().position(|a| a == flag).map(|i| args[i + 1].clone());
    assert_eq!(after("-m"), Some(std::fs::canonicalize(&model).unwrap().display().to_string()));
    assert_eq!(after("--host").as_deref(), Some("127.0.0.1"));
    assert_eq!(after("--api-key"), Some(key));
    assert_eq!(after("-c").as_deref(), Some("4096"));
    assert!(args.contains(&"--jinja".to_string()));
    assert_eq!(&args[args.len() - 2..], ["-ngl", "0"], "the config's args come last");
    assert!(dir.join("tiny-q4.gguf.request").exists());

    let port: u16 = endpoint.base_url.trim_start_matches("http://127.0.0.1:").trim_end_matches("/v1").parse().unwrap();
    local_model::stop_all();
    assert!(std::net::TcpStream::connect(("127.0.0.1", port)).is_err(), "stopped with Marv");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test]
async fn a_server_that_exits_while_loading_says_so_with_its_last_lines() {
    let dir = std::env::temp_dir().join(format!("marv-llama-exit-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let model = dir.join("broken.gguf");
    std::fs::write(&model, b"GGUF").unwrap();
    let server = dir.join("llama-server");
    std::fs::write(&server, "#!/bin/sh\necho 'error: failed to load model: out of memory' >&2\nexit 1\n").unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&server, std::fs::Permissions::from_mode(0o755)).unwrap();
    let setting = local_model::LlamaCppSetting { server: Some(server.display().to_string()), args: Vec::new() };
    let env = Env::from([("HOME".to_string(), dir.display().to_string())]);
    let endpoint = local_model::endpoint(&model.display().to_string(), Some(&setting), 4096, &env);
    let ready = endpoint.ready.clone().unwrap();
    let why = ready.0.wait(Default::default()).await.unwrap_err();
    assert!(why.contains("llama-server exited") && why.contains("out of memory"), "{why}");
    std::fs::remove_dir_all(&dir).unwrap();
}
