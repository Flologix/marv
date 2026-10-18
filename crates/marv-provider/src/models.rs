//! Which models a provider offers, for the model picker in `/setup` and `/model`, and what the
//! session needs to know about the current one (context window, prices, reasoning). Ports
//! `src/provider/models.ts`.
//!
//! - OpenRouter: the public `GET {baseUrl}/models` (no key needed), filtered to models that can
//!   call tools; `:batch` variants are for offline bulk jobs, not chat. Prices come back in USD
//!   per token as strings and are shown per million tokens.
//! - Ollama: the native `GET /api/tags`, then per model `POST /api/show` for its `capabilities`
//!   (a failed `/api/show` leaves `tools` unknown rather than failing the list).
//! - Z.ai's GLM Coding Plan (#159): Marv's own table ([`ZAI_CODING_MODELS`]), no request. The coding endpoint
//!   documents no model list, and the plan offers a handful of models: the table is the one place to update.
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
    /// Marv #96: the most tokens one reply may have (OpenRouter's `top_provider.max_completion_tokens`), when known.
    pub max_output: Option<u64>,
}

/// #159: Z.ai's OpenAI-compatible endpoint for GLM Coding Plan keys. A plan key gets 401 on the general
/// `https://api.z.ai/api/paas/v4`, and calls there don't draw on the plan.
pub const ZAI_CODING_BASE_URL: &str = "https://api.z.ai/api/coding/paas/v4";

/// One model the GLM Coding Plan offers: (id, context window, most output tokens), `None` where Z.ai doesn't say.
pub type ZaiModel = (&'static str, Option<u64>, Option<u64>);

/// #159: the models every GLM Coding Plan includes (docs.z.ai/devpack/overview, 2026-10: "All plans support GLM-5.3,
/// GLM-5.3-Flash"; older ids are routed to these two). GLM-5.3: 1M-token window, 128K output (docs.z.ai/guides/llm/glm-5.3).
/// The flash model's window isn't documented. Both always reason ("Disabling reasoning is no longer supported").
/// Update this table when the plan changes; the first entry is the default.
pub const ZAI_CODING_MODELS: &[ZaiModel] = &[("glm-5.3", Some(1_000_000), Some(131_072)), ("glm-5.3-flash", None, Some(131_072))];

/// The model setup suggests for the Coding Plan.
pub const ZAI_DEFAULT_MODEL: &str = ZAI_CODING_MODELS[0].0;

/// [`ZAI_CODING_MODELS`] as model infos: tools, always reasoning, no per-token price (the plan is a subscription).
pub fn zai_models() -> Vec<ModelInfo> {
    ZAI_CODING_MODELS
        .iter()
        .map(|&(id, context, max_output)| ModelInfo {
            id: id.into(),
            tools: Some(true),
            context,
            max_output,
            reasoning: Some(ReasoningSupport::Mandatory),
            ..Default::default()
        })
        .collect()
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
    #[serde(default)]
    pub top_provider: Option<OpenRouterTopProvider>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OpenRouterTopProvider {
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
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
                max_output: m.top_provider.and_then(|t| t.max_completion_tokens).filter(|&n| n > 0),
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
///
/// The crate owns the HTTP client (one, shared), so callers without reqwest (marv-session's
/// `ConfigFactory::load`, the TUI) can list models. `api_key` is sent as `Authorization: Bearer`
/// when given; Marv's lists need none (OpenRouter's is public, Ollama's local), so its callers pass
/// `None`.
pub async fn list_models(provider: ProviderKind, base_url: &str, api_key: Option<&str>) -> Result<Vec<ModelInfo>, String> {
    static CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(reqwest::Client::new);
    list_models_with(&CLIENT, provider, base_url, api_key).await
}

/// [`list_models`] through a given client (tests point it at a local server).
pub async fn list_models_with(
    client: &reqwest::Client,
    provider: ProviderKind,
    base_url: &str,
    api_key: Option<&str>,
) -> Result<Vec<ModelInfo>, String> {
    match provider {
        ProviderKind::OpenRouter => {
            // Public endpoint, no key needed. Prices come back in USD per token.
            let list: OpenRouterList = get_json(client, &format!("{base_url}/models"), api_key, None).await?;
            Ok(from_openrouter(list.data))
        }
        // #159: Marv's own table; the coding endpoint documents no model list.
        ProviderKind::Zai => Ok(zai_models()),
        // #159: the signed-in account's models (`api_key` is the sign-in's access token): the entries listed for
        // display, by slug.
        ProviderKind::ChatGpt => {
            let token = api_key.ok_or_else(|| "Sign in with ChatGPT first.".to_string())?;
            let list: ChatGptList = get_json(client, &format!("{base_url}/models"), Some(token), None).await?;
            Ok(list
                .models
                .into_iter()
                .filter(|m| m.visibility.as_deref() == Some("list"))
                .map(|m| ModelInfo { id: m.slug, tools: Some(true), ..Default::default() })
                .collect())
        }
        ProviderKind::Ollama => {
            // Ollama's native API (not the /v1 compatibility layer) lists local models and, per
            // model, its capabilities.
            let origin = reqwest::Url::parse(base_url).map_err(|e| format!("{base_url}: {e}"))?.origin().ascii_serialization();
            let tags: OllamaTags = get_json(client, &format!("{origin}/api/tags"), api_key, None).await?;
            let show = format!("{origin}/api/show");
            let models = tags.models.into_iter().map(|OllamaTag { name }| {
                let show = &show;
                async move {
                    let body = serde_json::json!({ "model": name });
                    let info: Option<OllamaShow> = get_json(client, show, api_key, Some(&body)).await.ok();
                    let tools = info.and_then(|i| i.capabilities).map(|c| c.iter().any(|c| c == "tools"));
                    ModelInfo { id: name, tools, local: true, ..Default::default() }
                }
            });
            Ok(futures::future::join_all(models).await)
        }
    }
}

#[derive(Deserialize)]
struct OpenRouterList {
    data: Vec<OpenRouterModel>,
}

#[derive(Deserialize)]
struct ChatGptList {
    #[serde(default)]
    models: Vec<ChatGptModel>,
}

#[derive(Deserialize)]
struct ChatGptModel {
    slug: String,
    #[serde(default)]
    visibility: Option<String>,
}

#[derive(Deserialize)]
struct OllamaTags {
    models: Vec<OllamaTag>,
}

#[derive(Deserialize)]
struct OllamaTag {
    name: String,
}

#[derive(Deserialize)]
struct OllamaShow {
    #[serde(default)]
    capabilities: Option<Vec<String>>,
}

/// GET `url` (POST with `body`) and parse the JSON answer; any failure is a plain message.
async fn get_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    api_key: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Result<T, String> {
    let mut request = match body {
        Some(body) => client.post(url).json(body),
        None => client.get(url),
    };
    if let Some(key) = api_key {
        request = request.bearer_auth(key);
    }
    let response = request.send().await.map_err(|e| format!("{url}: {e}"))?;
    if !response.status().is_success() {
        return Err(format!("{url} returned {}", response.status().as_u16()));
    }
    response.json().await.map_err(|e| format!("{url}: {e}"))
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
            {"id":"a/tools","context_length":1000,"pricing":{"prompt":"0.000002","completion":"0.00001","input_cache_read":"0.0000002"},"supported_parameters":["tools"],"reasoning":{"mandatory":true},"top_provider":{"max_completion_tokens":800}},
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
        // #96: the output limit, when the list knows it.
        assert_eq!((models[0].max_output, models[1].max_output), (Some(800), None));
    }

    #[tokio::test]
    async fn the_zai_plan_lists_its_own_table_without_a_request() {
        // An unreachable address: Z.ai's list must not touch the network.
        let models = list_models(ProviderKind::Zai, "http://127.0.0.1:9", None).await.unwrap();
        assert_eq!(models.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["glm-5.3", "glm-5.3-flash"]);
        assert_eq!(models[0].id, ZAI_DEFAULT_MODEL);
        assert!(models.iter().all(|m| m.tools == Some(true) && m.reasoning == Some(ReasoningSupport::Mandatory)));
        assert_eq!(format_price(&models[0]), "", "a subscription: no per-token price");
    }
}
