//! marv-rs #202: a turn cut off by Marv quitting (a crash, a force-quit during a hung approval) is kept, and marked.
//!
//! Marv saved a session only when a turn ended, so a Marv that quit mid-turn lost the user's message and every call
//! the model made in it: resumed, the model denied a call the user had approved. Now the session is saved as the turn
//! goes (`MarvSession::save_soon`: after the user's message, each reply and each call's start and end, debounced; at
//! once before any approval is asked), with the turn's [`OpenTurn`] state:
//! - each call still waiting is answered ([`OpenTurn::result`]): its output when it had finished (a parallel group's
//!   results go into the history only once all are done), [`not_run::QUIT_APPROVAL`] when it waited for approval,
//!   [`not_run::QUIT`] otherwise. Every call has a result, so the file stays valid to continue from;
//! - `SavedSession::cut_off` says what cut it off ([`OpenTurn::detail`]). The turn's end saves again without it.
//!
//! A resume of a file with `cut_off` set shows [`cut_off_notice`] in the transcript and gives the model
//! [`cut_off_note`] at the start of the next message (the history isn't touched: the prompt cache).

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use marv_agent::agent::not_run;
use marv_core::{ChatTurn, ToolCall, printable};
use marv_tools::{ApprovalRequest, Approver, Decision};

use crate::session::MarvSession;

/// The running turn, as a quit now would leave it.
#[derive(Debug, Default)]
pub(crate) struct OpenTurn {
    /// The calls that have started (`ToolStart`).
    started: HashSet<String>,
    /// Finished calls' results, by call id: a parallel group's reach the history only when the whole group is done.
    results: HashMap<String, String>,
    /// The approvals being asked now: (id, subagent, tool).
    asking: Vec<(u64, Option<String>, String)>,
    next_ask: u64,
}

impl OpenTurn {
    pub(crate) fn started(&mut self, call: &ToolCall) {
        self.started.insert(call.id.clone());
    }

    pub(crate) fn finished(&mut self, call: &ToolCall, output: &str) {
        self.results.insert(call.id.clone(), output.to_string());
    }

    pub(crate) fn ask(&mut self, request: &ApprovalRequest) -> u64 {
        self.next_ask += 1;
        self.asking.push((self.next_ask, request.agent.clone(), request.tool.clone()));
        self.next_ask
    }

    pub(crate) fn answered(&mut self, id: u64) {
        self.asking.retain(|(k, ..)| *k != id);
    }

    /// The saved result of a call still waiting (`waiting`: [`OpenTurn::waiting`]): its output if it finished,
    /// otherwise why it has none. "Not run" only where that's certain: the main agent is asking about this tool and
    /// no other waiting call has its name (an approval doesn't carry its call's id), and the call isn't `agent` (its
    /// subagent may have done work before it asked). Anything else may have run in part ([`not_run::QUIT`]), so the
    /// model checks before it repeats it.
    pub(crate) fn result(&self, call: &ToolCall, waiting: &[&ToolCall]) -> String {
        if let Some(output) = self.results.get(&call.id) {
            return output.clone();
        }
        let asked = call.name != "agent"
            && self.asking.iter().any(|(_, agent, tool)| agent.is_none() && *tool == call.name)
            && waiting.iter().filter(|c| c.name == call.name).count() == 1;
        if asked { not_run::QUIT_APPROVAL } else { not_run::QUIT }.into()
    }

    /// What a quit now cuts off: "Marv quit while `mesh_send` waited for approval".
    pub(crate) fn detail(&self, history: &[ChatTurn]) -> String {
        if let Some((_, agent, tool)) = self.asking.first() {
            return match agent {
                None => format!("Marv quit while `{}` waited for approval", printable(tool)),
                Some(agent) => {
                    format!("Marv quit while `{}` (subagent {}) waited for approval", printable(tool), printable(agent))
                }
            };
        }
        let waiting = self.waiting(history);
        let names =
            |calls: &[&ToolCall]| calls.iter().map(|c| format!("`{}`", printable(&c.name))).collect::<Vec<_>>().join(", ");
        let running: Vec<&ToolCall> = waiting.iter().copied().filter(|c| self.started.contains(&c.id)).collect();
        if !running.is_empty() {
            format!("Marv quit while {} ran", names(&running))
        } else if !waiting.is_empty() {
            format!("Marv quit before {} ran", names(&waiting))
        } else {
            "Marv quit before the turn finished".into()
        }
    }

    /// The last reply's calls with no result, in the history or here.
    pub(crate) fn waiting<'a>(&self, history: &'a [ChatTurn]) -> Vec<&'a ToolCall> {
        let answered: HashSet<&str> = history
            .iter()
            .filter_map(|t| match t {
                ChatTurn::Tool { call_id, .. } => Some(call_id.as_str()),
                _ => None,
            })
            .collect();
        history
            .iter()
            .rev()
            .find_map(|t| match t {
                ChatTurn::Assistant { tool_calls, .. } if !tool_calls.is_empty() => Some(tool_calls),
                _ => None,
            })
            .into_iter()
            .flatten()
            .filter(|c| !answered.contains(c.id.as_str()) && !self.results.contains_key(&c.id))
            .collect()
    }
}

/// The turn was saved by another process that's still running: that Marv is still in the turn, not cut off. Its pid
/// may have been reused by an unrelated process since (then the mark is left out: a known limit, the safe side).
/// The same process (a session reopened by an SDK host, a test) is never "another".
pub fn saved_by_another_live_marv(pid: Option<u32>) -> bool {
    let Some(pid) = pid.filter(|p| *p != std::process::id() && *p != 0) else { return false };
    if cfg!(target_os = "linux") {
        return std::path::Path::new(&format!("/proc/{pid}")).exists();
    }
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// The transcript's line on a resumed turn that was cut off.
pub fn cut_off_notice(detail: &str) -> String {
    format!("The previous turn was cut off: {detail}. Calls without a result didn't run, or didn't finish.")
}

/// The model's note on it, at the start of the next message.
pub fn cut_off_note(detail: &str) -> String {
    format!(
        "(Automatic note from Marv, the program running you) Your previous turn was cut off: {detail}, so it never \
         finished. A call answered \"Not run\" or \"No result\" there didn't run, or didn't finish: check before you \
         rely on it."
    )
}

/// The client's approver, saving the turn before each question (a person may never answer it: Marv is force-quit).
pub(crate) struct SavingApprover {
    pub(crate) session: MarvSession,
    pub(crate) inner: std::sync::Arc<dyn Approver>,
}

#[async_trait]
impl Approver for SavingApprover {
    async fn approve(&self, request: ApprovalRequest) -> Decision {
        let asked = Asked(&self.session, self.session.asking(&request));
        let decision = self.inner.approve(request).await;
        drop(asked);
        decision
    }
}

/// Takes the question off the open turn however its wait ends (answered, or the turn dropped).
struct Asked<'a>(&'a MarvSession, Option<u64>);

impl Drop for Asked<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.1 {
            self.0.answered(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall { id: id.into(), name: name.into(), arguments: "{}".into() }
    }

    fn request(tool: &str, agent: Option<&str>) -> ApprovalRequest {
        let mut r = crate::runner::step_limit_request(1, 1);
        r.tool = tool.into();
        r.agent = agent.map(String::from);
        r
    }

    fn history() -> Vec<ChatTurn> {
        vec![
            ChatTurn::User { text: "go".into() },
            ChatTurn::Assistant {
                text: String::new(),
                tool_calls: vec![call("c1", "read_file"), call("c2", "mesh_send")],
                reasoning: None,
            },
        ]
    }

    #[test]
    fn only_another_live_process_holds_a_turn_open() {
        assert!(!saved_by_another_live_marv(None));
        assert!(!saved_by_another_live_marv(Some(std::process::id())), "this process: not another");
        let mut child = std::process::Command::new("sleep").arg("30").spawn().unwrap();
        assert!(saved_by_another_live_marv(Some(child.id())));
        child.kill().unwrap();
        child.wait().unwrap();
        assert!(!saved_by_another_live_marv(Some(child.id())), "it quit");
    }

    #[test]
    fn a_waiting_call_gets_its_output_or_why_it_has_none() {
        let mut open = OpenTurn::default();
        open.finished(&call("c1", "read_file"), "contents");
        let id = open.ask(&request("mesh_send", None));
        let h = history();
        let waiting = open.waiting(&h);
        assert_eq!(open.result(&call("c1", "read_file"), &waiting), "contents");
        assert_eq!(open.result(&call("c2", "mesh_send"), &waiting), not_run::QUIT_APPROVAL);
        assert_eq!(open.detail(&h), "Marv quit while `mesh_send` waited for approval");
        open.answered(id);
        assert_eq!(open.result(&call("c2", "mesh_send"), &waiting), not_run::QUIT);
    }

    #[test]
    fn the_detail_says_what_was_running_or_waiting() {
        let mut open = OpenTurn::default();
        assert_eq!(open.detail(&history()), "Marv quit before `read_file`, `mesh_send` ran");
        open.started(&call("c1", "read_file"));
        assert_eq!(open.detail(&history()), "Marv quit while `read_file` ran");
        open.finished(&call("c1", "read_file"), "x");
        open.finished(&call("c2", "mesh_send"), "sent");
        assert_eq!(open.detail(&history()), "Marv quit before the turn finished");
        open.ask(&request("bash", Some("explore · Find it")));
        assert_eq!(open.detail(&history()), "Marv quit while `bash` (subagent explore · Find it) waited for approval");
    }

    // Review of #212: a subagent waiting for approval at the quit may have changed files already: its `agent` call
    // (and the other `agent` calls beside it) "may have run in part", never "not run".
    #[test]
    fn an_agent_call_whose_subagent_waits_may_have_run_in_part() {
        let calls = vec![call("a1", "agent"), call("a2", "agent")];
        let h = vec![
            ChatTurn::User { text: "go".into() },
            ChatTurn::Assistant { text: String::new(), tool_calls: calls.clone(), reasoning: None },
        ];
        let mut open = OpenTurn::default();
        open.ask(&request("write_file", Some("implementer · Task 1")));
        let waiting = open.waiting(&h);
        assert_eq!(open.result(&calls[0], &waiting), not_run::QUIT);
        assert_eq!(open.result(&calls[1], &waiting), not_run::QUIT);
        // Even the main agent asking about a tool named `agent` can't tell which.
        open.ask(&request("agent", None));
        assert_eq!(open.result(&calls[0], &waiting), not_run::QUIT);
    }

    // Two parallel calls of one tool, one asking: the approval can't say which, so neither is "not run".
    #[test]
    fn two_waiting_calls_of_one_tool_may_each_have_run() {
        let calls = vec![call("w1", "write_file"), call("w2", "write_file"), call("r1", "read_file")];
        let h = vec![
            ChatTurn::User { text: "go".into() },
            ChatTurn::Assistant { text: String::new(), tool_calls: calls.clone(), reasoning: None },
        ];
        let mut open = OpenTurn::default();
        open.ask(&request("write_file", None));
        let waiting = open.waiting(&h);
        assert_eq!(open.result(&calls[0], &waiting), not_run::QUIT);
        assert_eq!(open.result(&calls[1], &waiting), not_run::QUIT);
        assert_eq!(open.result(&calls[2], &waiting), not_run::QUIT);
        // Once one has finished, the one left is the one asking.
        open.finished(&calls[0], "written");
        let waiting = open.waiting(&h);
        assert_eq!(open.result(&calls[0], &waiting), "written");
        assert_eq!(open.result(&calls[1], &waiting), not_run::QUIT_APPROVAL);
    }
}
