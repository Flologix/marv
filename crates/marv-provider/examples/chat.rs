//! One prompt in, a streamed reply out: the M1 round-trip, by hand (tests never call a model).
//!
//! ```sh
//! OPENROUTER_API_KEY=sk-… cargo run -p marv-provider --example chat -- "hi"
//! MARV_PROVIDER=ollama MARV_MODEL=qwen3.5:9b cargo run -p marv-provider --example chat -- "hi"
//! ```
//!
//! `MARV_MODEL` picks the model (OpenRouter's default: `anthropic/claude-sonnet-5.5`), `MARV_THINK=1`
//! turns `/think` on, `OLLAMA_HOST` points at another Ollama. Ctrl+C stops the reply.

use std::io::Write;

use futures::StreamExt;
use marv_core::{ChatTurn, ProviderEvent, printable};
use marv_provider::{ProviderConfig, ProviderKind, StreamOptions, create_provider};
use tokio_util::sync::CancellationToken;

#[tokio::main]
async fn main() {
    let prompt = std::env::args().skip(1).collect::<Vec<_>>().join(" ");
    if prompt.is_empty() {
        eprintln!("usage: chat <prompt>");
        std::process::exit(2);
    }
    let env = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
    let thinking = env("MARV_THINK").is_some_and(|v| v != "0");
    let config = if env("MARV_PROVIDER").as_deref() == Some("ollama") {
        let host = env("OLLAMA_HOST").unwrap_or_else(|| "localhost:11434".into());
        ProviderConfig {
            provider: ProviderKind::Ollama,
            model: env("MARV_MODEL").unwrap_or_else(|| "qwen3.5:9b".into()),
            base_url: if host.contains("://") { host } else { format!("http://{host}") },
            api_key: None,
            context_length: 32768,
            thinking,
        }
    } else {
        let Some(api_key) = env("OPENROUTER_API_KEY") else {
            eprintln!("Set OPENROUTER_API_KEY (or MARV_PROVIDER=ollama).");
            std::process::exit(2);
        };
        ProviderConfig {
            provider: ProviderKind::OpenRouter,
            model: env("MARV_MODEL").unwrap_or_else(|| "anthropic/claude-sonnet-5.5".into()),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key: Some(api_key),
            context_length: 32768,
            thinking,
        }
    };

    let provider = create_provider(&config, None, None);
    eprintln!("{}", printable(provider.name()));
    let cancel = CancellationToken::new();
    let on_ctrl_c = cancel.clone();
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            on_ctrl_c.cancel();
        }
    });

    let mut stream = provider.stream(&[ChatTurn::User { text: prompt }], StreamOptions { cancel, ..Default::default() });
    let mut out = std::io::stdout();
    let mut failed = false;
    while let Some(event) = stream.next().await {
        match event {
            // Model text never reaches the terminal raw.
            ProviderEvent::TextDelta(text) => {
                let _ = write!(out, "{}", printable(&text));
                let _ = out.flush();
            }
            ProviderEvent::ThinkingDelta(text) => eprint!("\x1b[2m{}\x1b[0m", printable(&text)),
            ProviderEvent::Retry { message, .. } => eprintln!("\n[{}]", printable(&message)),
            ProviderEvent::Usage(usage) => eprintln!(
                "\n[{} in ({} cached), {} out{}]",
                usage.prompt_tokens,
                usage.cached_tokens.unwrap_or(0),
                usage.completion_tokens,
                usage.cost.map(|c| format!(", ${c:.5}")).unwrap_or_default()
            ),
            ProviderEvent::Error(message) => {
                eprintln!("\n{}", printable(&message));
                failed = true;
            }
            ProviderEvent::Done { reason } => eprintln!("\n[done: {}]", reason.as_deref().map(printable).unwrap_or_default()),
            ProviderEvent::ToolCallDelta { .. }
            | ProviderEvent::ToolCall(_)
            | ProviderEvent::Reasoning(_)
            | ProviderEvent::Discard { .. } => {}
        }
    }
    println!();
    if failed {
        std::process::exit(1);
    }
}
