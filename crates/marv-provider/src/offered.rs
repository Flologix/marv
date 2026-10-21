//! Which subscriptions setup offers (#159): one switch each, in this one place.
//!
//! Both are built and work when a config names them; these only decide whether the setup screen, `/setup` and the
//! desktop's onboarding offer them. Each is off until the vendor has cleared Marv:
//! - Z.ai's GLM Coding Plan may only be used from tools Z.ai lists (docs.z.ai/devpack/faq), and Marv isn't listed yet.
//! - Sign in with ChatGPT: OpenAI's program for open-source agents registers each person's client dynamically, but
//!   whether Marv takes part is the maintainer's call with OpenAI.
//!
//! Turn one on for a build with its environment variable set to `1` at build time (`MARV_OFFER_ZAI_CODING_PLAN=1
//! cargo build`), or for good by changing its default here.

/// `option_env!(name) == Some("1")`, at compile time.
const fn on(value: Option<&str>) -> bool {
    matches!(value, Some(v) if v.len() == 1 && v.as_bytes()[0] == b'1')
}

/// Offer Z.ai's GLM Coding Plan. Pending Z.ai listing Marv among the plan's supported tools.
pub const ZAI_CODING_PLAN: bool = on(option_env!("MARV_OFFER_ZAI_CODING_PLAN"));

/// Offer Sign in with ChatGPT. Pending the maintainer's confirmation with OpenAI.
pub const CHATGPT_SIGN_IN: bool = on(option_env!("MARV_OFFER_CHATGPT_SIGN_IN"));

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_switch_is_on_only_for_1() {
        assert!(on(Some("1")));
        assert!(!on(Some("0")) && !on(Some("yes")) && !on(Some("")) && !on(None));
    }
}
