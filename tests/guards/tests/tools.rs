//! The built-in tools as a contract: what the model and saved sessions depend on can't change by accident.
//!
//! - `docs/TOOLS.md` is generated from the code (`marv_tools::table`) and must match it.
//! - The built-in specs' bytes (names, order, descriptions, schemas) match `golden/tool_specs.jsonl`: the prompt
//!   cache and saved sessions depend on them.
//! - A tool name is never reused: every name ever offered is a tool today or has a tombstone.
//!
//! A golden that differs fails with the command that writes it again:
//! `UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools`. Review the diff before committing it.

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The repository's root (this crate is `tests/guards`).
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("the repository root")
}

/// Compares `actual` with the checked-in `path` (relative to the repository root), or writes it with
/// `UPDATE_GOLDEN=1`.
fn golden(path: &str, actual: &str) {
    let file = repo_root().join(path);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).expect("the golden's folder");
        }
        std::fs::write(&file, actual).expect("write the golden");
        return;
    }
    let expected = std::fs::read_to_string(&file).unwrap_or_default();
    if expected != actual {
        let line = expected
            .lines()
            .zip(actual.lines())
            .position(|(a, b)| a != b)
            .unwrap_or_else(|| expected.lines().count().min(actual.lines().count()));
        panic!(
            "{path} is out of date (first difference at line {}). If the change is deliberate, write it again with\n\n    \
             UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools\n\nand review the diff.\n\nexpected: {:?}\n  actual: {:?}",
            line + 1,
            expected.lines().nth(line).unwrap_or(""),
            actual.lines().nth(line).unwrap_or(""),
        );
    }
}

/// **The tools table is the code, printed.** `docs/TOOLS.md` is generated from the `Tool` trait of every built-in
/// tool, so a change to a tool's kind, source, approval, network or persistence shows up in review as a table diff.
///
/// Proven: changed `Glob::kind` to `Write` → this failed (line of `glob`).
#[test]
fn tools_md_matches_the_code() {
    golden("docs/TOOLS.md", &marv_tools::table::tools_table());
}

/// The built-in specs exactly as the model gets them (`tool_specs`, every switch on): one compact JSON line each,
/// in the order sent. The bytes the prompt cache keys on.
fn spec_bytes() -> String {
    marv_tools::tool_specs().iter().map(|s| serde_json::to_string(s).expect("a spec") + "\n").collect()
}

/// **The specs the model gets change only on purpose.** A tool's name, order, description or schema is what the
/// model and every saved session depend on, and the prompt cache keys on their bytes: any change to them, even a
/// reworded description or a reordered list, fails here until the golden is written again, so it's always
/// deliberate (and noted in the changelog). Schemas change additively: a new optional field is fine; removing,
/// renaming or narrowing one makes a new tool.
///
/// Proven: added a word to `Glob`'s description → this failed (line of `glob`).
#[test]
fn builtin_specs_match_the_golden() {
    golden("tests/guards/golden/tool_specs.jsonl", &spec_bytes());
}

/// **A session's specs are what `tool_specs_for` filters, never reordered.** Every switch combination keeps the
/// full list's order, so turning skills, plan or github off drops a tool without moving the others.
#[test]
fn every_tool_list_keeps_the_specs_order() {
    let full: Vec<String> = marv_tools::tool_specs().iter().map(|s| s.name.clone()).collect();
    for bits in 0..8u8 {
        let names: Vec<String> =
            marv_tools::tool_specs_for(bits & 1 != 0, bits & 2 != 0, bits & 4 != 0).into_iter().map(|s| s.name).collect();
        let positions: Vec<usize> = names.iter().map(|n| full.iter().position(|f| f == n).expect("a built-in")).collect();
        assert!(positions.windows(2).all(|w| w[0] < w[1]), "{names:?} out of order");
    }
}

/// Every built-in tool name Marv has ever offered, one per line, in the order they appeared. Append-only.
const TOOL_NAMES: &str = "tests/guards/golden/tool_names.txt";

/// **A tool name is never reused.** Every name Marv ever offered stays in `tool_names.txt`: each is either a
/// built-in tool today or has a tombstone (`REMOVED_TOOLS`), so a saved session's call of a removed tool gets "was
/// removed" rather than another tool's behaviour; and no tombstoned name is offered again. A new tool is added to
/// the file by `UPDATE_GOLDEN=1`; a removed one stays.
///
/// Proven: removed `memory` from `builtin_tools` → this failed (no tombstone); added a `Tombstone` named `glob` →
/// this failed (reused).
#[test]
fn a_removed_tool_keeps_its_name_and_tombstone() {
    let file = repo_root().join(TOOL_NAMES);
    let mut known: Vec<String> =
        std::fs::read_to_string(&file).unwrap_or_default().lines().filter(|l| !l.is_empty()).map(str::to_string).collect();
    let builtin: Vec<&str> = marv_tools::builtin_tools().iter().map(|t| t.name()).collect();
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        for name in &builtin {
            if !known.iter().any(|k| k == name) {
                known.push(name.to_string());
            }
        }
        std::fs::create_dir_all(file.parent().expect("a folder")).expect("the golden's folder");
        std::fs::write(&file, known.iter().map(|n| format!("{n}\n")).collect::<String>()).expect("write the names");
    }
    for name in &builtin {
        assert!(
            known.iter().any(|k| k == name),
            "{name} isn't in {TOOL_NAMES}: add it with UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools"
        );
    }
    for tomb in marv_tools::REMOVED_TOOLS {
        assert!(
            !builtin.contains(&tomb.name),
            "{} has a tombstone and is offered again: give the new tool another name",
            tomb.name
        );
        assert!(known.iter().any(|k| k == tomb.name), "{} has a tombstone but was never in {TOOL_NAMES}", tomb.name);
    }
    for name in &known {
        assert!(
            builtin.contains(&name.as_str()) || marv_tools::REMOVED_TOOLS.iter().any(|t| t.name == name),
            "{name} was removed without a tombstone: add it to marv_tools::REMOVED_TOOLS with the version that drops it"
        );
    }
}

/// A built-in tool with everything but its effects: the approval flow (`run_tool`) sees the real tool's kind,
/// approval, scope, network, protected paths and yolo answer, while the preview and the run do nothing. So the
/// approvals golden records what `run_tool` decides for each call without reading, writing or fetching anything.
struct Probe(marv_tools::ToolRef);

type Input<'a> = &'a (dyn std::any::Any + Send + Sync);

#[async_trait::async_trait]
impl marv_tools::DynTool for Probe {
    fn name(&self) -> &str {
        self.0.name()
    }
    fn spec(&self) -> marv_core::ToolSpec {
        self.0.spec()
    }
    fn kind(&self) -> marv_core::ToolKind {
        self.0.kind()
    }
    fn provenance(&self) -> marv_core::Provenance {
        self.0.provenance()
    }
    fn provenance_in(&self, input: Input<'_>, ctx: &marv_tools::ToolContext) -> marv_core::Provenance {
        self.0.provenance_in(input, ctx)
    }
    fn parallel(&self) -> bool {
        self.0.parallel()
    }
    fn checkpoint(&self) -> bool {
        self.0.checkpoint()
    }
    fn parse(&self, raw: serde_json::Value) -> Result<Box<dyn std::any::Any + Send + Sync>, String> {
        self.0.parse(raw)
    }
    fn label(&self, input: Input<'_>) -> String {
        self.0.label(input)
    }
    fn needs_approval(&self, input: Input<'_>) -> Option<bool> {
        self.0.needs_approval(input)
    }
    fn uses_network(&self, input: Input<'_>, ctx: &marv_tools::ToolContext) -> bool {
        self.0.uses_network(input, ctx)
    }
    async fn preview(
        &self,
        _input: Input<'_>,
        _ctx: &marv_tools::ToolContext,
    ) -> Result<marv_tools::Preview, marv_tools::ToolError> {
        Ok(marv_tools::Preview { title: self.name().to_string(), ..marv_tools::Preview::default() })
    }
    fn scope(&self, input: Input<'_>, ctx: &marv_tools::ToolContext) -> Option<marv_tools::Scope> {
        self.0.scope(input, ctx)
    }
    fn auto_safe(&self, input: Input<'_>, ctx: &marv_tools::ToolContext) -> bool {
        self.0.auto_safe(input, ctx)
    }
    fn protected_target(&self, input: Input<'_>, ctx: &marv_tools::ToolContext) -> Option<marv_sandbox::ProtectedRule> {
        self.0.protected_target(input, ctx)
    }
    fn repair(&self, raw: &serde_json::Value, ctx: &marv_tools::ToolContext) -> Option<marv_tools::Repaired> {
        self.0.repair(raw, ctx)
    }
    fn repair_mode(&self) -> marv_tools::RepairMode {
        self.0.repair_mode()
    }
    fn egress_url(&self, input: Input<'_>) -> Option<String> {
        self.0.egress_url(input)
    }
    fn door(&self, input: Input<'_>, ctx: &marv_tools::ToolContext) -> Option<marv_core::Door> {
        self.0.door(input, ctx)
    }
    fn once(&self, input: Input<'_>, ctx: &marv_tools::ToolContext) -> bool {
        self.0.once(input, ctx)
    }
    fn persists(&self, input: Input<'_>, ctx: &marv_tools::ToolContext) -> bool {
        self.0.persists(input, ctx)
    }
    async fn approve_itself(
        &self,
        input: Input<'_>,
        ctx: &marv_tools::ToolContext,
        preview: &marv_tools::Preview,
    ) -> Option<Result<marv_tools::Decision, marv_tools::ToolError>> {
        self.0.approve_itself(input, ctx, preview).await
    }
    async fn run(
        &self,
        _input: Input<'_>,
        _ctx: &marv_tools::ToolContext,
    ) -> Result<marv_tools::ToolOutput, marv_tools::ToolError> {
        Ok(marv_tools::ToolOutput::ok("ran", "ran"))
    }
}

/// The kinds of turn a call can run in, as the approvals golden names them.
#[derive(Debug, Clone, Copy)]
enum Turn {
    /// The person typed it.
    Person,
    /// `marv -p` or the SDK.
    Headless,
    /// Another agent's message woke the session (the mesh inbox).
    Woken,
    /// A radio message, or the person's turn that handles radio requests.
    Radio,
    /// A subagent of a turn the person started.
    SubagentOfPerson,
    /// A subagent of a turn another agent's message started.
    SubagentOfWoken,
}

const TURNS: [(Turn, &str); 6] = [
    (Turn::Person, "person"),
    (Turn::Headless, "headless"),
    (Turn::Woken, "woken"),
    (Turn::Radio, "radio"),
    (Turn::SubagentOfPerson, "subagent"),
    (Turn::SubagentOfWoken, "subagent of woken"),
];

/// The context and tool list a call in `turn` runs with, built as the session builds them (`run_turn`,
/// `subagent_context`): yolo only where the person started the turn, no automatic approval in a turn they didn't.
fn turn_context(
    root: &Path,
    turn: Turn,
    yolo: bool,
    tainted: bool,
    approver: Arc<dyn marv_tools::Approver>,
) -> (marv_tools::ToolContext, Vec<marv_tools::ToolRef>) {
    let all: Vec<marv_tools::ToolRef> =
        marv_tools::builtin_tools().iter().map(|t| Arc::new(Probe(t.clone())) as marv_tools::ToolRef).collect();
    let mut ctx = marv_tools::table::example_context();
    // A real folder, so the path rules (`.git`, protected paths) resolve as in a project; nothing is written to it.
    ctx.root = root.to_path_buf();
    ctx.taint = Arc::new(marv_sandbox::TaintState::new());
    if tainted {
        ctx.taint.mark(marv_core::Provenance::Web);
    }
    let unattended = matches!(turn, Turn::Woken | Turn::Radio | Turn::SubagentOfWoken);
    ctx.yolo = yolo && !unattended;
    ctx.no_auto_approval = unattended;
    ctx.approver = Some(match turn {
        Turn::Radio => Arc::new(marv_session::radio::RadioTurnApprover(approver, marv_session::radio::RADIO_TURN_WARNING)),
        _ => approver,
    });
    match turn {
        Turn::SubagentOfPerson | Turn::SubagentOfWoken => {
            let parent_approver = ctx.approver.clone();
            let sub = marv_session::subagent::subagent_context(&ctx, "general", None, parent_approver);
            let offered = all.into_iter().filter(|t| marv_session::agents::SUBAGENT_TOOLS.contains(&t.name())).collect();
            (sub, offered)
        }
        _ => (ctx, all),
    }
}

/// What `run_tool` did with one call: `-` not offered, `runs` without asking, `yolo` ran by yolo, else asked:
/// `ask` (an "always" can answer it), `once` (asked every time), `must` (a person must answer: tainted egress, a
/// protected path), `must once`.
async fn decided(root: &Path, example: &marv_tools::table::Example, turn: Turn, yolo: bool, tainted: bool) -> String {
    let recorder = marv_guards::Recorder::new(marv_tools::Decision::No);
    let (ctx, available) = turn_context(root, turn, yolo, tainted, recorder.clone());
    let call = marv_guards::call(example.tool, example.input.clone());
    let outcome = marv_tools::run_tool(&call, &ctx, &available).await;
    if outcome.result.output.starts_with("Unknown tool") {
        return "-".into();
    }
    match recorder.requests().as_slice() {
        [] if outcome.result.approval == Some(marv_tools::Approval::Auto) => "yolo".into(),
        [] if outcome.result.output == "ran" => "runs".into(),
        [] => format!("error: {}", outcome.result.output),
        [request] => match (request.must_ask, request.once) {
            (false, false) => "ask".into(),
            (false, true) => "once".into(),
            (true, false) => "must".into(),
            (true, true) => "must once".into(),
        },
        more => format!("asked {} times", more.len()),
    }
}

/// Today's approval decisions: every example call × every kind of turn, clean and tainted, yolo off and on.
async fn approval_decisions() -> String {
    let project = marv_guards::TempDir::new("approvals-golden");
    let root = project.path();
    let mut out = String::from(
        "# What run_tool decides for each example call (marv_tools::table::examples), by the kind of turn.\n\
         # Columns: clean, clean + yolo, tainted, tainted + yolo. `-` not offered, `runs` without asking, `yolo` ran by\n\
         # yolo, `ask` asked (an \"always\" can answer), `once` asked every time, `must` a person must answer.\n\
         # Written by UPDATE_GOLDEN=1 cargo test -p marv-guards --test tools\n",
    );
    for example in marv_tools::table::examples() {
        out.push_str(&format!("\n{}\n", marv_tools::table::example_label(&example)));
        for (turn, name) in TURNS {
            let mut cells = Vec::new();
            for (tainted, yolo) in [(false, false), (false, true), (true, false), (true, true)] {
                cells.push(decided(root, &example, turn, yolo, tainted).await);
            }
            out.push_str(&format!("  {name:<18} {}\n", cells.iter().map(|c| format!("{c:<10}")).collect::<String>().trim_end()));
        }
    }
    out
}

/// **Who started a turn changes approvals only as it did.** The golden holds what `run_tool` decides for every
/// example call of every built-in tool, in every kind of turn, clean and tainted, with yolo off and on. A change to
/// how a turn's kind reaches the approval flow must leave it as it is.
///
/// Proven: dropped `|| ctx.no_auto_approval` from `once` in `run_tool` → this failed (woken `ask` for `once`).
#[tokio::test]
async fn approvals_per_turn_match_the_golden() {
    golden("tests/guards/golden/approvals.txt", &approval_decisions().await);
}
