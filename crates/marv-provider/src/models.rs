//! Which models a provider offers, for the model picker in `/setup` and `/model`, and what the
//! session needs to know about the current one (context window, prices, reasoning). Ports
//! `src/provider/models.ts`.
//!
//! - OpenRouter: the public `GET {baseUrl}/models` (no key needed), filtered to models that can
//!   call tools; `:batch` variants are for offline bulk jobs, not chat. Prices come back in USD
//!   per token as strings and are shown per million tokens.
//! - Ollama: the native `GET /api/tags`, then per model `POST /api/show` for its `capabilities`
//!   (a failed `/api/show` leaves `tools` unknown rather than failing the list).
//! - `reasoning`: "optional" can be turned off, "mandatory" can't (OpenRouter rejects trying):
//!   `crate::reasoning_for` depends on this.

use serde::Deserialize;

use crate::ProviderKind;

/// Whether a model can reason before answering, and whether that can be turned off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSupport {
    Optional,
    Mandatory,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelInfo {
    pub id: String,
    /// Can call tools. `None` = unknown.
    pub tools: Option<bool>,
    /// USD per million tokens.
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    /// For input served from the provider's cache.
    pub price_cache_read: Option<f64>,
    pub context: Option<u64>,
    /// Runs on this machine (Ollama).
    pub local: bool,
    pub reasoning: Option<ReasoningSupport>,
}

/// One entry of OpenRouter's `/models` list (only the fields Marv reads).
#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenRouterModel {
    pub id: String,
    #[serde(default)]
    pub context_length: Option<u64>,
    #[serde(default)]
    pub pricing: Option<OpenRouterPricing>,
    #[serde(default)]
    pub supported_parameters: Option<Vec<String>>,
    #[serde(default)]
    pub reasoning: Option<OpenRouterReasoning>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenRouterPricing {
    pub prompt: Option<String>,
    pub completion: Option<String>,
    pub input_cache_read: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenRouterReasoning {
    #[serde(default)]
    pub mandatory: Option<bool>,
}

/// OpenRouter's list → Marv's: only models that can call tools, no `:batch` variants.
pub fn from_openrouter(data: Vec<OpenRouterModel>) -> Vec<ModelInfo> {
    data.into_iter()
        .filter(|m| m.supported_parameters.as_ref().is_some_and(|p| p.iter().any(|s| s == "tools")) && !m.id.ends_with(":batch"))
        .map(|m| {
            let pricing = m.pricing.unwrap_or_default();
            ModelInfo {
                tools: Some(true),
                price_in: per_million(pricing.prompt.as_deref()),
                price_out: per_million(pricing.completion.as_deref()),
                price_cache_read: pricing.input_cache_read.as_deref().and_then(|p| per_million(Some(p))),
                context: m.context_length,
                local: false,
                reasoning: m
                    .reasoning
                    .map(|r| if r.mandatory == Some(true) { ReasoningSupport::Mandatory } else { ReasoningSupport::Optional }),
                id: m.id,
            }
        })
        .collect()
}

/// USD per token (a string) → USD per million tokens, rounded to 6 decimals (float noise).
pub fn per_million(per_token: Option<&str>) -> Option<f64> {
    let n: f64 = per_token?.trim().parse().ok()?;
    Some((n * 1e6 * 1e6).round() / 1e6)
}

/// Reads a provider's model list. Errors are plain messages (`"<url> returned 503"`); the picker
/// shows them.
pub async fn list_models(_provider: ProviderKind, _base_url: &str, _client: &reqwest::Client) -> Result<Vec<ModelInfo>, String> {
    todo!("port: listModels (OpenRouter /models via from_openrouter; Ollama /api/tags + /api/show capabilities)")
}

/// `n` rounded to `digits` significant digits, then printed shortest (JS `Number(n.toPrecision(d))`).
fn to_precision(n: f64, digits: i32) -> f64 {
    if n == 0.0 {
        return 0.0;
    }
    let scale = 10f64.powi(digits - 1 - n.abs().log10().floor() as i32);
    (n * scale).round() / scale
}

/// "$2 / $10" (in / out per million tokens), "free", or "local".
pub fn format_price(model: &ModelInfo) -> String {
    if model.local {
        return "local".into();
    }
    let (Some(price_in), Some(price_out)) = (model.price_in, model.price_out) else { return String::new() };
    if price_in == 0.0 && price_out == 0.0 {
        return "free".into();
    }
    let usd = |n: f64| format!("${}", to_precision(n, if n < 1.0 { 2 } else { 3 }));
    format!("{} / {}", usd(price_in), usd(price_out))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ported from tests/models.test.ts (formatPrice; the list filters are tested without a server here).

    fn priced(price_in: f64, price_out: f64) -> ModelInfo {
        ModelInfo { id: "x".into(), price_in: Some(price_in), price_out: Some(price_out), ..Default::default() }
    }

    #[test]
    fn format_price_shows_dollars_per_million_in_and_out() {
        assert_eq!(format_price(&priced(2.0, 10.0)), "$2 / $10");
        assert_eq!(format_price(&priced(0.0188, 1.28)), "$0.019 / $1.28");
        assert_eq!(format_price(&priced(0.0, 0.0)), "free");
        assert_eq!(format_price(&ModelInfo { id: "x".into(), local: true, ..Default::default() }), "local");
    }

    #[test]
    fn openrouter_list_keeps_tool_capable_non_batch_models() {
        let json = r#"[
            {"id":"a/tools","context_length":1000,"pricing":{"prompt":"0.000002","completion":"0.00001","input_cache_read":"0.0000002"},"supported_parameters":["tools"],"reasoning":{"mandatory":true}},
            {"id":"a/tools:batch","supported_parameters":["tools"]},
            {"id":"a/chat","supported_parameters":["temperature"]},
            {"id":"a/opt","supported_parameters":["tools"],"reasoning":{}}
        ]"#;
        let models = from_openrouter(serde_json::from_str(json).unwrap());
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["a/tools", "a/opt"]);
        assert_eq!(models[0].price_in, Some(2.0));
        assert_eq!(models[0].price_out, Some(10.0));
        assert_eq!(models[0].price_cache_read, Some(0.2));
        assert_eq!(models[0].context, Some(1000));
        assert_eq!(models[0].reasoning, Some(ReasoningSupport::Mandatory));
        assert_eq!(models[1].reasoning, Some(ReasoningSupport::Optional));
        assert_eq!(models[1].price_in, None);
    }
}
