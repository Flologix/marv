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
    // #81: Marv's process description is marv-rs's without its last sentence.
    let (process_sentence, nothing) = MARV_RS_TEXT[2];
    assert!(marv_tools::process::PROCESS_DESCRIPTION.ends_with(process_sentence) && nothing.is_empty());
    // `recall` (after Marv #63), last in the tool list the prompt names.
    let recall = system_prompt(&PromptInput { tools: vec!["read_file".into(), "recall".into()], ..Default::default() });
    let (recall_listed, _) = MARV_RS_TEXT[3];
    assert!(recall.contains(recall_listed), "MARV_RS_TEXT[3] isn't how the prompt names recall last:\n{recall}");
    // The `.git` placeholder sentence, which Marv doesn't have.
    let (placeholder, nothing) = MARV_RS_TEXT[4];
    assert!(placeholder == marv_session::prompt::GIT_PLACEHOLDER && nothing.is_empty());
    // #186: the built-in explore type's line in the agent list, and the agent tool's sentence about it.
    let agents = system_prompt(&PromptInput {
        tools: vec!["read_file".into(), "agent".into()],
        agents: marv_session::agents::builtin_agents()
            .iter()
            .map(|a| marv_session::prompt::Listed { name: a.name.clone(), description: a.description.clone() })
            .collect(),
        ..Default::default()
    });
    let (explore_listed, nothing) = MARV_RS_TEXT[5];
    assert!(agents.contains(explore_listed) && nothing.is_empty(), "MARV_RS_TEXT[5] isn't explore's line:\n{agents}");
    let (explore_sentence, nothing) = MARV_RS_TEXT[6];
    assert!(explore_sentence == marv_tools::agent::EXPLORE_SENTENCE && nothing.is_empty());
    assert!(marv_tools::agent::AGENT_DESCRIPTION.ends_with(explore_sentence));
    // A chat's /tmp, in the sandbox text of the system prompt; and the process tool's serve sentence.
    let (tmp_clause, nothing) = MARV_RS_TEXT[7];
    assert!(nothing.is_empty());
    if cfg!(target_os = "linux") {
        assert!(prompt.contains(tmp_clause), "MARV_RS_TEXT[7] isn't the prompt's /tmp clause:\n{prompt}");
    }
    let (serve_sentence, nothing) = MARV_RS_TEXT[8];
    assert!(marv_tools::process::PROCESS_DESCRIPTION.contains(serve_sentence) && nothing.is_empty());
    // #268: the access facts' first line, by which the oracle drops the block.
    assert_eq!(marv_oracle::runner::MARV_RS_BLOCKS, [marv_session::prompt::ACCESS_HEADING]);
    let bash = system_prompt(&PromptInput { tools: vec!["read_file".into(), "bash".into()], ..Default::default() });
    assert!(bash.contains(&format!("\n\n{}\n", marv_session::prompt::ACCESS_HEADING)), "{bash}");
}
