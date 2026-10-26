//! Which subscriptions setup offers (#159): one switch each, in this one place.
//!
//! Both are built and work when a config names them; these only decide whether the setup screen and `/setup` offer
//! them. Both are on: the person chooses a plan they pay for, and its vendor's terms are theirs to keep:
//! - Z.ai's GLM Coding Plan may only be used from tools Z.ai lists (docs.z.ai/devpack/faq).
//! - Sign in with ChatGPT goes through OpenAI's program for open-source agents.
//!
//! Turn one off for a build with its environment variable set to `0` at build time (`MARV_OFFER_ZAI_CODING_PLAN=0
//! cargo build`). Kimi Code, DeepSeek and Qwen's Coding Plan have no switch: setup always offers them.

/// On unless `option_env!(name) == Some("0")`, at compile time.
const fn on(value: Option<&str>) -> bool {
    !matches!(value, Some(v) if v.len() == 1 && v.as_bytes()[0] == b'0')
}

/// Offer Z.ai's GLM Coding Plan.
pub const ZAI_CODING_PLAN: bool = on(option_env!("MARV_OFFER_ZAI_CODING_PLAN"));

/// Offer Sign in with ChatGPT.
pub const CHATGPT_SIGN_IN: bool = on(option_env!("MARV_OFFER_CHATGPT_SIGN_IN"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_is_off_only_for_0() {
        assert!(!on(Some("0")));
        assert!(on(Some("1")) && on(Some("no")) && on(Some("")) && on(None));
    }
}
