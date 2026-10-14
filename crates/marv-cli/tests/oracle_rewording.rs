//! The oracle maps marv-rs's deliberate rewordings back to Marv's text (`marv_oracle::runner::MARV_RS_TEXT`, #85)
//! from copies of the sources' strings. CI doesn't run the oracle, so a copy that drifts would only show up there:
//! this checks the copies against what marv-rs really sends.

use marv_oracle::runner::MARV_RS_TEXT;
use marv_session::prompt::{PromptInput, system_prompt};

#[test]
fn the_oracles_reworded_text_matches_what_marv_rs_sends() {
    let prompt = system_prompt(&PromptInput { tools: vec!["read_file".into(), "plan".into()], ..Default::default() });
    let (plan_sentence, _) = MARV_RS_TEXT[0];
    assert!(prompt.contains(plan_sentence), "MARV_RS_TEXT[0] isn't the prompt's plan sentence:\n{prompt}");
    let (plan_description, _) = MARV_RS_TEXT[1];
    assert_eq!(plan_description, marv_tools::plan::PLAN_DESCRIPTION);
}
