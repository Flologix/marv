//! `plan`: a checklist the model keeps for a long task (Marv #15).
//!
//! Ports Marv's `src/tools/plan.ts` (fully). Each call sends the whole list, so the latest call in the history *is*
//! the plan: nothing to patch, no ids to carry, and it survives in the conversation without notes of Marv's own
//! (compaction re-injects it, marv-agent's `compact`). The list is the session's state ([`ToolContext::plan`]); the
//! TUI pins it above the prompt.
//!
//! - No `kind`: it changes nothing outside the session, like a read tool, so it never asks (yolo doesn't matter).
//! - `ToolOutput::plan` is for the screen only, like `diff`: the model reads one line; the list is in its own call.
//! - Main agent only: the session gives the main agent's context a [`PlanHost`], as it does `agent_host`; a call
//!   without one is a `ToolError` (defense in depth: `run_tool` already refuses tools a subagent wasn't offered).

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::types::{PlanItem, PlanStatus, Tool, ToolContext, ToolError, ToolOutput};

/// At most this many items: a plan, not a backlog (and at most ~1k tokens to resend).
pub const MAX_PLAN_ITEMS: usize = 20;
/// Each item's text is at most this many characters.
const MAX_TEXT: usize = 200;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanInput {
    pub items: Vec<PlanItem>,
}

/// "2/5 done": how far a plan is.
pub fn plan_progress(items: &[PlanItem]) -> String {
    let done = items.iter().filter(|i| i.status == PlanStatus::Done).count();
    format!("{done}/{} done", items.len())
}

/// A plan with something left to do (an all-done or empty plan has nothing to keep or show).
pub fn plan_open(items: Option<&[PlanItem]>) -> bool {
    items.is_some_and(|items| items.iter().any(|i| i.status != PlanStatus::Done))
}

/// The plan as it stood, for the compacted summary's message (#15): the summary replaces the plan calls the model
/// made, so the session's own copy goes in instead, as structured text rather than the model's paraphrase.
pub fn plan_text(items: &[PlanItem]) -> String {
    let lines: Vec<String> = items
        .iter()
        .map(|i| {
            let status = match i.status {
                PlanStatus::Done => "done",
                PlanStatus::InProgress => "in progress",
                PlanStatus::Pending => "pending",
            };
            format!("- [{status}] {}", i.text)
        })
        .collect();
    format!("Your plan (from the plan tool), as it stood:\n{}\nKeep updating it with the plan tool.", lines.join("\n"))
}

/// Deliberate difference (marv-rs #85): Marv's says "a task of three or more steps" and nothing about when to send
/// it. On the GLM bench 33 of 47 plan calls were requests of their own, so: four or more steps, and each update in
/// the same reply as the next tool call. The oracle maps it back to Marv's (`marv_oracle::runner::MARV_RS_TEXT`).
pub const PLAN_DESCRIPTION: &str = "Keep a checklist for a task of four or more steps, or when the user asks for several things. Write the plan before starting; mark an item in_progress before working on it and done as soon as it is; add items you discover and remove ones that turn out unnecessary. Send the whole list every time (it replaces the previous one), in the same reply as your next tool call; only the final update, marking the last items done, may go alone. Skip it for a short task or a question.";

pub struct Plan;

#[async_trait]
impl Tool for Plan {
    type Input = PlanInput;

    fn name(&self) -> &str {
        "plan"
    }
    fn description(&self) -> &str {
        PLAN_DESCRIPTION
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "maxItems": MAX_PLAN_ITEMS,
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string", "minLength": 1, "maxLength": MAX_TEXT, "description": "one step, a short sentence"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}
                        },
                        "required": ["text", "status"],
                        "additionalProperties": false
                    },
                    "description": "the whole plan, in order; an empty list clears it"
                }
            },
            "required": ["items"],
            "additionalProperties": false
        })
    }
    fn check(&self, input: &PlanInput) -> Result<(), String> {
        if input.items.len() > MAX_PLAN_ITEMS {
            return Err(format!("items: at most {MAX_PLAN_ITEMS} items"));
        }
        for (n, item) in input.items.iter().enumerate() {
            let chars = item.text.chars().count();
            if chars == 0 {
                return Err(format!("items.{n}.text: must not be empty"));
            }
            if chars > MAX_TEXT {
                return Err(format!("items.{n}.text: at most {MAX_TEXT} characters"));
            }
        }
        if input.items.iter().filter(|i| i.status == PlanStatus::InProgress).count() > 1 {
            return Err("items: At most one item can be in_progress: finish it (done) before starting the next.".into());
        }
        Ok(())
    }
    fn kind(&self) -> marv_core::ToolKind {
        marv_core::ToolKind::Read
    }
    /// Marv's own words about the model's own list.
    fn provenance(&self) -> marv_core::Provenance {
        marv_core::Provenance::Marv
    }
    fn label(&self, _input: &PlanInput) -> String {
        "Plan".into()
    }

    async fn run(&self, input: &PlanInput, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let Some(host) = &ctx.plan else {
            return Err(ToolError::new("The plan tool isn't available here."));
        };
        let items = input.items.clone();
        host.set(items.clone());
        if items.is_empty() {
            let mut out = ToolOutput::ok("Plan cleared.", "cleared");
            out.plan = Some(Vec::new());
            return Ok(out);
        }
        let done = items.iter().filter(|i| i.status == PlanStatus::Done).count();
        let now = items
            .iter()
            .find(|i| i.status == PlanStatus::InProgress)
            .map(|i| format!("; now: {}", i.text.strip_suffix('.').unwrap_or(&i.text)))
            .unwrap_or_default();
        let mut out = ToolOutput::ok(format!("Plan saved: {done} of {} done{now}.", items.len()), plan_progress(&items));
        out.plan = Some(items);
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{builtin_tools, run_tool};
    use crate::types::{ApprovalRequest, Approver, Decision, PlanHost};
    use marv_core::ToolCall;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[test]
    fn the_description_asks_for_long_tasks_only_and_updates_alongside_real_calls() {
        // #85: on the GLM bench 33 of 47 plan calls were requests of their own (7% of request time).
        let d = Plan.description();
        assert!(d.starts_with("Keep a checklist for a task of four or more steps"));
        assert!(d.contains(
            "in the same reply as your next tool call; only the final update, marking the last items done, may go alone"
        ));
    }

    fn call(items: Value) -> ToolCall {
        ToolCall { id: "c1".into(), name: "plan".into(), arguments: json!({ "items": items }).to_string() }
    }

    #[derive(Default)]
    struct Host(Mutex<Vec<Vec<PlanItem>>>);
    impl PlanHost for Host {
        fn set(&self, items: Vec<PlanItem>) {
            self.0.lock().unwrap().push(items);
        }
    }

    fn ctx(host: &Arc<Host>) -> ToolContext {
        ToolContext { plan: Some(host.clone()), ..ToolContext::new("/tmp") }
    }
    fn item(text: &str, status: PlanStatus) -> PlanItem {
        PlanItem { text: text.into(), status }
    }

    #[tokio::test]
    async fn saves_the_list_and_says_where_it_stands() {
        let host = Arc::new(Host::default());
        let items = json!([
            { "text": "Read", "status": "done" },
            { "text": "Write tests", "status": "in_progress" },
            { "text": "Docs", "status": "pending" },
        ]);
        let r = run_tool(&call(items), &ctx(&host), builtin_tools()).await;
        assert!(!r.result.is_error);
        assert_eq!(r.result.output, "Plan saved: 1 of 3 done; now: Write tests.");
        assert_eq!(r.result.summary, "1/3 done");
        assert_eq!(r.label, "Plan");
        let expected =
            vec![item("Read", PlanStatus::Done), item("Write tests", PlanStatus::InProgress), item("Docs", PlanStatus::Pending)];
        assert_eq!(r.result.plan, Some(expected.clone()));
        assert_eq!(*host.0.lock().unwrap(), vec![expected]);
    }

    #[tokio::test]
    async fn without_an_item_in_progress_it_says_so() {
        let host = Arc::new(Host::default());
        let items = json!([{ "text": "A", "status": "done" }, { "text": "B", "status": "pending" }]);
        let r = run_tool(&call(items), &ctx(&host), builtin_tools()).await;
        assert_eq!(r.result.output, "Plan saved: 1 of 2 done.");
    }

    #[tokio::test]
    async fn the_current_items_final_period_is_dropped() {
        let host = Arc::new(Host::default());
        let r =
            run_tool(&call(json!([{ "text": "Write the tests.", "status": "in_progress" }])), &ctx(&host), builtin_tools()).await;
        assert_eq!(r.result.output, "Plan saved: 0 of 1 done; now: Write the tests.");
    }

    #[tokio::test]
    async fn an_empty_list_clears_it() {
        let host = Arc::new(Host::default());
        let r = run_tool(&call(json!([])), &ctx(&host), builtin_tools()).await;
        assert_eq!(r.result.output, "Plan cleared.");
        assert_eq!(r.result.summary, "cleared");
        assert_eq!(r.result.plan, Some(vec![]));
        assert_eq!(*host.0.lock().unwrap(), vec![Vec::<PlanItem>::new()]);
    }

    #[tokio::test]
    async fn refuses_two_in_progress_21_items_empty_or_201_char_text_as_errors_the_model_reads() {
        let host = Arc::new(Host::default());
        let many: Vec<Value> = (0..21).map(|i| json!({ "text": format!("step {i}"), "status": "pending" })).collect();
        let two = json!([{ "text": "A", "status": "in_progress" }, { "text": "B", "status": "in_progress" }]);
        let bad = [
            two.clone(),
            Value::Array(many),
            json!([{ "text": "", "status": "pending" }]),
            json!([{ "text": "x".repeat(201), "status": "pending" }]),
            json!([{ "text": "A", "status": "started" }]),
        ];
        for items in bad {
            let r = run_tool(&call(items), &ctx(&host), builtin_tools()).await;
            assert!(r.result.is_error);
            assert!(r.result.output.starts_with("Invalid input for plan:"), "{}", r.result.output);
        }
        let r = run_tool(&call(two), &ctx(&host), builtin_tools()).await;
        assert!(r.result.output.contains("At most one item can be in_progress"));
        assert!(host.0.lock().unwrap().is_empty());
        // 200 characters (not bytes) is fine.
        let r = run_tool(&call(json!([{ "text": "é".repeat(200), "status": "pending" }])), &ctx(&host), builtin_tools()).await;
        assert!(!r.result.is_error, "{}", r.result.output);
    }

    #[tokio::test]
    async fn without_a_host_its_a_tool_error() {
        let r = run_tool(&call(json!([{ "text": "A", "status": "pending" }])), &ToolContext::new("/tmp"), builtin_tools()).await;
        assert!(r.result.is_error);
        assert_eq!(r.result.output, "The plan tool isn't available here.");
    }

    struct SaysNo(AtomicUsize);
    #[async_trait]
    impl Approver for SaysNo {
        async fn approve(&self, _request: ApprovalRequest) -> Decision {
            self.0.fetch_add(1, Ordering::SeqCst);
            Decision::No
        }
    }

    #[tokio::test]
    async fn never_asks_even_with_an_approver_that_says_no_and_yolo_off() {
        let host = Arc::new(Host::default());
        let no = Arc::new(SaysNo(AtomicUsize::new(0)));
        let c = ToolContext { approver: Some(no.clone()), yolo: false, ..ctx(&host) };
        let r = run_tool(&call(json!([{ "text": "A", "status": "in_progress" }])), &c, builtin_tools()).await;
        assert_eq!(no.0.load(Ordering::SeqCst), 0);
        assert!(!r.result.is_error);
        assert_eq!(r.result.approval, None);
        assert_eq!(host.0.lock().unwrap().len(), 1);
    }

    #[test]
    fn plan_text_lists_each_item_with_its_status() {
        let items = [item("Read", PlanStatus::Done), item("Write", PlanStatus::InProgress), item("Docs", PlanStatus::Pending)];
        assert_eq!(
            plan_text(&items),
            "Your plan (from the plan tool), as it stood:\n- [done] Read\n- [in progress] Write\n- [pending] Docs\nKeep updating it with the plan tool."
        );
    }

    #[test]
    fn progress_and_open() {
        let items = [item("a", PlanStatus::Done), item("b", PlanStatus::Pending)];
        assert_eq!(plan_progress(&items), "1/2 done");
        assert!(plan_open(Some(&items)));
        assert!(!plan_open(Some(&items[..1])));
        assert!(!plan_open(Some(&[])));
        assert!(!plan_open(None));
    }
}
