//! Which subscriptions setup offers (#159): one switch each, in this one place.
//!
//! Both are built and work when a config names them; these only decide whether the setup screen and `/setup` offer
//! them. Each follows its vendor's terms for a third-party tool:
//! - Sign in with ChatGPT is on: OpenAI's program for open-source, locally run apps registers each person's client
//!   dynamically (developers.openai.com/siwc/token-sharing-open-source).
//! - Z.ai's GLM Coding Plan is off: its subscription terms (§4.2) allow the plan only in the tools Z.ai lists
//!   (docs.z.ai/devpack/tool/others), and Marv isn't listed yet. Turn it on when it is.
//!
//! A build overrides either at build time: `MARV_OFFER_ZAI_CODING_PLAN=1` turns Z.ai on, `MARV_OFFER_CHATGPT_SIGN_IN=0`
//! turns ChatGPT off. Kimi Code, DeepSeek and Qwen's Coding Plan have no switch: setup always offers them.

/// The switch at compile time: `"1"` turns it on, `"0"` off, anything else (or unset) leaves `default`.
const fn switch(value: Option<&str>, default: bool) -> bool {
    match value {
        Some(v) if v.len() == 1 && v.as_bytes()[0] == b'1' => true,
        Some(v) if v.len() == 1 && v.as_bytes()[0] == b'0' => false,
        _ => default,
    }
}

/// Offer Z.ai's GLM Coding Plan. Off until Z.ai lists Marv among the plan's supported tools.
pub const ZAI_CODING_PLAN: bool = switch(option_env!("MARV_OFFER_ZAI_CODING_PLAN"), false);

/// Offer Sign in with ChatGPT.
pub const CHATGPT_SIGN_IN: bool = switch(option_env!("MARV_OFFER_CHATGPT_SIGN_IN"), true);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_is_turned_by_1_or_0_and_otherwise_keeps_its_default() {
        assert!(switch(Some("1"), false) && !switch(Some("0"), true));
        for default in [true, false] {
            for v in [None, Some(""), Some("yes"), Some("no"), Some("10")] {
                assert_eq!(switch(v, default), default, "{v:?}");
            }
        }
    }
}
