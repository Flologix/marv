//! Token and cost accounting for a session. Ports Marv's `src/usage.ts`.
//!
//! OpenRouter reports what each request actually cost (`usage.cost`, which accounts for cache discounts and which
//! provider served it). When a cost isn't reported, it's estimated from the model's prices (`~$`); local models
//! (Ollama) are free. Totals survive `/clear`; subagents' and summaries' requests count too.
//!
//! Port notes:
//! - [`tokens`] rounds in integer arithmetic, half up, as JavaScript's `toFixed`/`Math.round` did for these inputs
//!   (Rust's float formatting rounds ties to even: 1250 would print "1.2k" instead of Marv's "1.3k").
//! - [`cost_text`] still uses float formatting; a cost exactly on a half-cent tie may round differently.
//! - Character counts (`estimate_tokens`) count `char`s, Marv counted UTF-16 units.

use marv_core::Usage;
use serde::{Deserialize, Serialize};

/// The whole session's requests. Saved with sessions in Marv's field names.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Totals {
    pub requests: u64,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub completion_tokens: u64,
    /// USD; `None` when neither reported nor estimable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost: Option<f64>,
    /// Some of the cost was estimated from prices rather than reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub estimated: Option<bool>,
    /// A local model (Ollama): no cost.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub local: Option<bool>,
}

/// $ per million tokens.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Prices {
    pub price_in: Option<f64>,
    pub price_out: Option<f64>,
    pub price_cache_read: Option<f64>,
}

impl From<&marv_provider::ModelInfo> for Prices {
    fn from(info: &marv_provider::ModelInfo) -> Self {
        Self { price_in: info.price_in, price_out: info.price_out, price_cache_read: info.price_cache_read }
    }
}

pub fn estimate_cost(usage: &Usage, prices: &Prices) -> Option<f64> {
    let (price_in, price_out) = (prices.price_in?, prices.price_out?);
    let cache_rate = prices.price_cache_read.unwrap_or(price_in);
    let cached = usage.cached_tokens.unwrap_or(0) as f64;
    Some(
        ((usage.prompt_tokens as f64 - cached) * price_in + cached * cache_rate + usage.completion_tokens as f64 * price_out)
            / 1_000_000.0,
    )
}

pub fn add_usage(totals: &Totals, usage: &Usage, prices: &Prices) -> Totals {
    let reported = usage.cost;
    let cost = reported.or_else(|| estimate_cost(usage, prices));
    Totals {
        requests: totals.requests + 1,
        prompt_tokens: totals.prompt_tokens + usage.prompt_tokens,
        cached_tokens: totals.cached_tokens + usage.cached_tokens.unwrap_or(0),
        completion_tokens: totals.completion_tokens + usage.completion_tokens,
        cost: match cost {
            None => totals.cost,
            Some(c) => Some(totals.cost.unwrap_or(0.0) + c),
        },
        estimated: Some(totals.estimated.unwrap_or(false) || (reported.is_none() && cost.is_some())),
        local: totals.local,
    }
}

/// Tokens in text that's still streaming, before the provider counts them: about four characters a token.
pub fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

/// `950`, `12.3k`, `240k`, `1.5M`.
pub fn tokens(n: u64) -> String {
    let tenths = |unit: u64| {
        let t = (n * 10 + unit / 2) / unit;
        if t.is_multiple_of(10) { format!("{}", t / 10) } else { format!("{}.{}", t / 10, t % 10) }
    };
    if n < 1000 {
        n.to_string()
    } else if n < 100_000 {
        format!("{}k", tenths(1000))
    } else if n < 1_000_000 {
        format!("{}k", (n + 500) / 1000)
    } else {
        format!("{}M", tenths(1_000_000))
    }
}

/// "$0.042", "~$0.042" (estimated), "<$0.001", "local", or "" before any request.
pub fn cost_text(totals: &Totals) -> String {
    if totals.requests == 0 {
        return String::new();
    }
    if totals.local == Some(true) {
        return "local".into();
    }
    let Some(cost) = totals.cost else {
        return String::new();
    };
    let amount = if cost == 0.0 {
        "$0.00".to_string()
    } else if cost < 0.001 {
        "<$0.001".to_string()
    } else if cost < 1.0 {
        format!("${cost:.3}")
    } else {
        format!("${cost:.2}")
    };
    if totals.estimated == Some(true) && cost >= 0.001 { format!("~{amount}") } else { amount }
}

/// `Math.round(100 * part / whole)`, half up.
fn percent(part: u64, whole: u64) -> u64 {
    if whole == 0 { 0 } else { (200 * part + whole) / (2 * whole) }
}

/// `/cost`: the session so far, and how full the context is.
pub fn usage_report(totals: &Totals, last: Option<&Usage>, context_length: Option<u64>) -> String {
    if totals.requests == 0 {
        return "No requests yet this session.".into();
    }
    let cost = if totals.local == Some(true) {
        "Cost: none (a local model)".to_string()
    } else if totals.cost.is_none() {
        "Cost: unknown (the provider didn't report one and the model's prices aren't known)".to_string()
    } else {
        format!(
            "Cost: {}{}",
            cost_text(totals),
            if totals.estimated == Some(true) { " (partly estimated from the model's prices)" } else { "" }
        )
    };
    let mut lines = vec![
        format!("This session: {} request{}", totals.requests, if totals.requests == 1 { "" } else { "s" }),
        format!(
            "  {} input tokens ({} from the cache, {}%)",
            tokens(totals.prompt_tokens),
            tokens(totals.cached_tokens),
            percent(totals.cached_tokens, totals.prompt_tokens)
        ),
        format!("  {} output tokens", tokens(totals.completion_tokens)),
        cost,
    ];
    if let Some(last) = last {
        let used = last.prompt_tokens + last.completion_tokens;
        let of = match context_length {
            Some(c) if c > 0 => format!(" of {} tokens ({}%)", tokens(c), percent(used, c)),
            _ => " tokens".into(),
        };
        lines.push(format!("Context: {}{of}", tokens(used)));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn usage(p: u64, c: u64, cached: Option<u64>, cost: Option<f64>) -> Usage {
        Usage { prompt_tokens: p, completion_tokens: c, cached_tokens: cached, cost }
    }

    #[test]
    fn totals_add_up_preferring_the_reported_cost() {
        let mut t = Totals::default();
        t = add_usage(&t, &usage(1000, 100, Some(800), Some(0.002)), &Prices::default());
        t = add_usage(&t, &usage(1200, 50, Some(1000), Some(0.001)), &Prices::default());
        assert_eq!((t.requests, t.prompt_tokens, t.completion_tokens, t.cached_tokens), (2, 2200, 150, 1800));
        assert!((t.cost.unwrap() - 0.003).abs() < 1e-12);
        assert_eq!(t.estimated, Some(false));
    }

    #[test]
    fn estimates_from_prices_when_no_cost_is_reported() {
        let prices = Prices { price_in: Some(2.0), price_out: Some(10.0), price_cache_read: Some(0.2) };
        let est = estimate_cost(&usage(1000, 100, Some(800), None), &prices).unwrap();
        assert!((est - (0.0004 + 0.00016 + 0.001)).abs() < 1e-12);
        assert_eq!(add_usage(&Totals::default(), &usage(1000, 100, Some(800), None), &prices).estimated, Some(true));
        assert_eq!(add_usage(&Totals::default(), &usage(1000, 100, None, None), &Prices::default()).cost, None);
    }

    #[test]
    fn estimate_tokens_is_four_chars_a_token_rounded_up() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens(&"x".repeat(4800)), 1200);
    }

    #[test]
    fn token_counts_read_like_marvs() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(1000), "1k");
        assert_eq!(tokens(1250), "1.3k");
        assert_eq!(tokens(24_000), "24k");
        assert_eq!(tokens(240_400), "240k");
        assert_eq!(tokens(1_000_000), "1M");
        assert_eq!(tokens(1_550_000), "1.6M");
    }

    #[test]
    fn cost_text_labels_local_and_estimates() {
        let one = |cost: f64| Totals { requests: 1, cost: Some(cost), ..Totals::default() };
        assert_eq!(cost_text(&one(0.0423)), "$0.042");
        assert_eq!(cost_text(&one(1.5)), "$1.50");
        assert_eq!(cost_text(&one(0.0004)), "<$0.001");
        assert_eq!(cost_text(&one(0.0)), "$0.00");
        assert_eq!(cost_text(&Totals { local: Some(true), ..one(0.0) }), "local");
        assert_eq!(cost_text(&Totals { estimated: Some(true), ..one(0.0423) }), "~$0.042");
        assert_eq!(cost_text(&Totals::default()), "");
    }

    #[test]
    fn usage_report_breaks_the_session_down() {
        let totals = Totals {
            requests: 3,
            prompt_tokens: 24_000,
            cached_tokens: 18_000,
            completion_tokens: 1200,
            cost: Some(0.0312),
            ..Totals::default()
        };
        let report = usage_report(&totals, Some(&usage(9000, 400, None, None)), Some(1_000_000));
        assert!(report.contains("3 requests"));
        assert!(report.contains("24k input tokens (18k from the cache, 75%)"));
        assert!(report.contains("1.2k output tokens"));
        assert!(report.contains("$0.031"));
        assert!(report.contains("9.4k of 1M tokens"), "{report}");
    }

    #[test]
    fn totals_save_in_marvs_field_names() {
        let json = serde_json::to_string(&Totals { requests: 1, prompt_tokens: 2, ..Totals::default() }).unwrap();
        assert_eq!(json, r#"{"requests":1,"promptTokens":2,"cachedTokens":0,"completionTokens":0}"#);
    }
}
