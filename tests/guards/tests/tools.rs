//! The built-in tools as a contract: what the model and saved sessions depend on can't change by accident.
//!
//! - `docs/TOOLS.md` is generated from the code (`marv_tools::table`) and must match it.
//! - The built-in specs' bytes (names, order, descriptions, schemas) match `golden/tool_specs.<os>.jsonl`: the prompt
//!   cache and saved sessions depend on them.
//! - A tool name is never reused: every name ever offered is a tool today or has a tombstone.
//! - Who started a turn changes approvals only as it did (`golden/approvals.txt`), and the floor is what `run_tool`
//!   decides.
//! - Only `run_tool` runs a tool (a scan of the non-test code).
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

/// The built-in specs exactly as the model gets them on `os` (`tool_specs`, every switch on): one compact JSON line
/// each, in the order sent. The bytes the prompt cache keys on. The platforms differ only in bash's description
/// (what its sandbox does), and both are compiled everywhere, so this builds either platform's from any.
fn spec_bytes(os: &str) -> String {
    let bash = if os == "macos" { marv_tools::bash::SEATBELT_DESCRIPTION } else { marv_tools::bash::BUBBLEWRAP_DESCRIPTION };
    marv_tools::tool_specs()
        .iter()
        .map(|s| {
            let mut spec = s.clone();
            if spec.name == "bash" {
                spec.description = bash.to_string();
            }
            serde_json::to_string(&spec).expect("a spec") + "\n"
        })
        .collect()
}

/// This build's platform, as the spec goldens are named.
const THIS_OS: &str = if cfg!(target_os = "macos") { "macos" } else { "linux" };

/// **The specs the model gets change only on purpose.** A tool's name, order, description or schema is what the
/// model and every saved session depend on, and the prompt cache keys on their bytes: any change to them, even a
/// reworded description or a reordered list, fails here until the golden is written again, so it's always
/// deliberate (and noted in the changelog). Schemas change additively: a new optional field is fine; removing,
/// renaming or narrowing one makes a new tool.
///
/// Proven: added a word to `Glob`'s description → this failed (line of `glob`).
#[test]
fn builtin_specs_match_the_golden() {
    // What this build really sends is the derived spec for its platform (else the derivation is wrong).
    let sent: String = marv_tools::tool_specs().iter().map(|s| serde_json::to_string(s).expect("a spec") + "\n").collect();
    assert_eq!(sent, spec_bytes(THIS_OS), "the {THIS_OS} specs aren't what this build sends");
    // Both platforms' goldens, from either: macOS CI checks the same files Linux writes.
    for os in ["linux", "macos"] {
        golden(&format!("tests/guards/golden/tool_specs.{os}.jsonl"), &spec_bytes(os));
    }
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
    /// `marv -p` or an SDK one-shot session.
    Headless,
    /// Another agent's message woke the session (the mesh inbox).
    Woken,
    /// A radio message, or the person's turn that handles radio requests.
    Radio,
    /// A subagent of a turn of each kind.
    SubagentOf(Started),
}

/// Who started the turn a subagent runs in.
#[derive(Debug, Clone, Copy)]
enum Started {
    Person,
    Headless,
    Woken,
    Radio,
}

const TURNS: [(Turn, &str); 8] = [
    (Turn::Person, "person"),
    (Turn::Headless, "headless"),
    (Turn::Woken, "woken"),
    (Turn::Radio, "radio"),
    (Turn::SubagentOf(Started::Person), "subagent"),
    (Turn::SubagentOf(Started::Headless), "subagent of headless"),
    (Turn::SubagentOf(Started::Woken), "subagent of woken"),
    (Turn::SubagentOf(Started::Radio), "subagent of radio"),
];

/// The context and tool list a call in `turn` runs with, built as the session builds them (`run_turn`,
/// `subagent_context`), in `setting`'s sandbox and doors. Unlike the session, yolo is left as the person set it in
/// every kind of turn, so the golden shows the mode alone keeping it off where the person didn't start the turn. A
/// subagent gets the full tool list: `run_tool` refuses the main agent's tools there by itself.
fn turn_context(
    root: &Path,
    setting: marv_tools::table::Setting,
    turn: Turn,
    yolo: bool,
    tainted: bool,
    approver: Arc<dyn marv_tools::Approver>,
) -> (marv_tools::ToolContext, Vec<marv_tools::ToolRef>) {
    let all: Vec<marv_tools::ToolRef> =
        marv_tools::builtin_tools().iter().map(|t| Arc::new(Probe(t.clone())) as marv_tools::ToolRef).collect();
    let mut ctx = marv_tools::table::example_context(setting);
    // A real folder, so the path rules (`.git`, protected paths) resolve as in a project; nothing is written to it.
    ctx.root = root.to_path_buf();
    ctx.taint = Arc::new(marv_sandbox::TaintState::new());
    if tainted {
        ctx.taint.mark(marv_core::Provenance::Web);
    }
    ctx.yolo = yolo;
    let started = match turn {
        Turn::Person => Started::Person,
        Turn::Headless => Started::Headless,
        Turn::Woken => Started::Woken,
        Turn::Radio => Started::Radio,
        Turn::SubagentOf(started) => started,
    };
    ctx.mode = match started {
        Started::Person => marv_tools::CallMode::Person,
        Started::Headless => marv_tools::CallMode::Headless,
        Started::Woken => marv_tools::CallMode::Woken,
        Started::Radio => marv_tools::CallMode::Radio,
    };
    ctx.approver = Some(match started {
        Started::Radio => Arc::new(marv_session::radio::RadioTurnApprover(approver, marv_session::radio::RADIO_TURN_WARNING)),
        _ => approver,
    });
    match turn {
        Turn::SubagentOf(_) => {
            let parent_approver = ctx.approver.clone();
            (marv_session::subagent::subagent_context(&ctx, "general", None, parent_approver), all)
        }
        _ => (ctx, all),
    }
}

/// What `run_tool` did with one call: `-` not offered, `runs` without asking, `yolo` ran by yolo, else asked:
/// `ask` (an "always" can answer it), `once` (asked every time), `must` (a person must answer: tainted egress, a
/// protected path), `must once`.
async fn decided(
    root: &Path,
    setting: marv_tools::table::Setting,
    example: &marv_tools::table::Example,
    turn: Turn,
    yolo: bool,
    tainted: bool,
) -> String {
    let recorder = marv_guards::Recorder::new(marv_tools::Decision::No);
    let (ctx, available) = turn_context(root, setting, turn, yolo, tainted, recorder.clone());
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
    for setting in marv_tools::table::Setting::ALL {
        out.push_str(&format!("\n## {}\n", setting.name()));
        for example in marv_tools::table::examples() {
            out.push_str(&format!("\n{}\n", marv_tools::table::example_label(&example)));
            for (turn, name) in TURNS {
                let mut cells = Vec::new();
                for (tainted, yolo) in [(false, false), (false, true), (true, false), (true, true)] {
                    cells.push(decided(root, setting, &example, turn, yolo, tainted).await);
                }
                let cells = cells.iter().map(|c| format!("{c:<10}")).collect::<String>();
                out.push_str(&format!("  {name:<21} {}\n", cells.trim_end()));
            }
        }
    }
    out
}

/// **Who started a turn changes approvals only as it did.** The golden holds what `run_tool` decides for every
/// example call of every built-in tool, in every kind of turn (subagents of each), clean and tainted, with yolo off
/// and on, with the sandbox on and every door open and with the sandbox off. A change to how a turn's kind reaches
/// the approval flow must leave it as it is.
///
/// Proven: dropped `|| ctx.no_auto_approval` from `once` in `run_tool` → this failed (woken `ask` for `once`). Unchanged
/// by the move of the turn's kind onto `ToolContext::mode` (`marv_tools::floor`). Since yolo is left on in every
/// kind of turn: dropped `ctx.mode.auto_approval() &&` from the yolo check in `run_tool` → failed (woken `yolo`).
#[tokio::test]
async fn approvals_per_turn_match_the_golden() {
    golden("tests/guards/golden/approvals.txt", &approval_decisions().await);
}

/// **The floor is what `run_tool` does.** For every example call and kind of turn, clean and without yolo,
/// `approval_floor` (the per-mode columns of `docs/TOOLS.md`) names what `run_tool` decided: `never` ran without
/// asking, `asks` asked with "always" allowed, `every time` asked once, `refused` wasn't offered. (A protected path
/// raises a call to "a person must answer" on top: the later check, never lower.)
///
/// Proven: made `approval_floor` answer `Asks` where `asked_every_time` holds → this failed (woken `once`, floor
/// `asks`).
#[tokio::test]
async fn the_floor_matches_todays_answers_per_mode() {
    let project = marv_guards::TempDir::new("floor-golden");
    for setting in marv_tools::table::Setting::ALL {
        for example in marv_tools::table::examples() {
            for (turn, name) in TURNS {
                let recorder = marv_guards::Recorder::new(marv_tools::Decision::No);
                let (ctx, _) = turn_context(project.path(), setting, turn, false, false, recorder);
                let tool = marv_tools::table::example_tool(&example);
                let input = tool.parse(example.input.clone()).expect("an example that parses");
                let floor = marv_tools::approval_floor(tool.as_ref(), input.as_ref(), &ctx);
                let expected = match floor {
                    marv_tools::Floor::NeverAsks => "runs",
                    marv_tools::Floor::Asks => "ask",
                    marv_tools::Floor::AsksEveryTime => "once",
                    marv_tools::Floor::Refused => "-",
                };
                let label = marv_tools::table::example_label(&example);
                // A protected path raises the call to `must` (a person answers): the floor still decides `once`.
                let decided = decided(project.path(), setting, &example, turn, false, false).await;
                let decided = match decided.as_str() {
                    "must" => "ask",
                    "must once" => "once",
                    other => other,
                };
                assert_eq!(decided, expected, "{label} in a {name} turn, {}", setting.name());
            }
        }
    }
}

/// The trait methods that make a tool act or ask: what only `run_tool` may call. `(name, arguments)`.
const TOOL_ENTRIES: [(&str, usize); 3] = [("run", 2), ("preview", 2), ("approve_itself", 3)];

/// Calls of a tool entry's shape that run no tool, each with why: `(file, enclosing fn, the call as written)`.
/// Keep it narrow: an entry names one call in one function.
const NOT_A_TOOL: [(&str, &str, &str); 4] = [
    // The agent tool's `run` and `preview` hand the call to the session's `AgentHost`, which starts the subagent (whose
    // own calls go through `run_tool`).
    ("crates/marv-tools/src/agent.rs", "run", "host . run (input , ctx)"),
    ("crates/marv-tools/src/agent.rs", "preview", "host . preview (input , ctx)"),
    // The agent loop's `ToolRunner::run(call, cancel)`: the session's runner, which calls `run_tool`.
    ("crates/marv-agent/src/agent.rs", "tools", "runner . run (call , cancel)"),
    // The oracle's fake-tool tracker answering a scripted call by name (a test harness's own function).
    ("crates/marv-oracle/src/tracker.rs", "call", "self . run (name , args)"),
];

/// Whether `attrs` mark test-only code: `#[cfg(test)]`, `#[test]`, `#[tokio::test]`.
fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        let path = a.path();
        path.is_ident("test")
            || path.segments.last().is_some_and(|s| s.ident == "test")
            || (path.is_ident("cfg") && a.parse_args::<syn::Ident>().is_ok_and(|i| i == "test"))
    })
}

/// One finding: where, and the code as written.
struct Finding {
    file: String,
    line: usize,
    function: String,
    implementing: String,
    code: String,
}

/// Finds every call (or mention, as a path: a function pointer, an alias's `X::run`) of a tool entry in one file's
/// non-test code. Macro bodies are searched by token, since syn doesn't parse them.
struct EntryFinder {
    file: String,
    function: Vec<String>,
    /// The `impl` blocks around, as `Trait for Type` (or `Type`).
    implementing: Vec<String>,
    found: Vec<Finding>,
    test_modules: Vec<String>,
}

impl EntryFinder {
    fn push(&mut self, span: proc_macro2::Span, code: String) {
        self.found.push(Finding {
            file: self.file.clone(),
            line: span.start().line,
            function: self.function.last().cloned().unwrap_or_default(),
            implementing: self.implementing.last().cloned().unwrap_or_default(),
            code,
        });
    }

    /// `.run(…)` and `Type::run` in a macro's tokens, nested groups included (syn leaves macro bodies unparsed).
    fn scan_tokens(&mut self, tokens: proc_macro2::TokenStream) {
        use proc_macro2::{Delimiter, TokenTree};
        let trees: Vec<TokenTree> = tokens.into_iter().collect();
        let punct = |i: usize, c: char| matches!(trees.get(i), Some(TokenTree::Punct(p)) if p.as_char() == c);
        for (i, tree) in trees.iter().enumerate() {
            match tree {
                TokenTree::Group(group) => self.scan_tokens(group.stream()),
                TokenTree::Ident(ident) if TOOL_ENTRIES.iter().any(|(name, _)| ident == name) => {
                    let called = matches!(trees.get(i + 1), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis);
                    let method = i > 0 && punct(i - 1, '.') && called;
                    let by_type = i > 2
                        && punct(i - 1, ':')
                        && punct(i - 2, ':')
                        && matches!(&trees[i - 3], TokenTree::Ident(t) if t.to_string().starts_with(char::is_uppercase))
                        || i > 2 && punct(i - 1, ':') && punct(i - 2, ':') && punct(i - 3, '>');
                    if method || by_type {
                        self.push(ident.span(), format!("(in a macro) {ident}"));
                    }
                }
                _ => {}
            }
        }
    }
}

impl<'ast> syn::visit::Visit<'ast> for EntryFinder {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        if is_test_only(&item.attrs) {
            if item.content.is_none() {
                self.test_modules.push(item.ident.to_string());
            }
            return;
        }
        syn::visit::visit_item_mod(self, item);
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        if is_test_only(&item.attrs) {
            return;
        }
        self.function.push(item.sig.ident.to_string());
        syn::visit::visit_item_fn(self, item);
        self.function.pop();
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if is_test_only(&item.attrs) {
            return;
        }
        self.function.push(item.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, item);
        self.function.pop();
    }
    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        if is_test_only(&item.attrs) {
            return;
        }
        let ty = quote_tokens(&item.self_ty);
        self.implementing.push(match &item.trait_ {
            Some((_, path, _)) => format!("{} for {ty}", quote_tokens(path)),
            None => ty,
        });
        syn::visit::visit_item_impl(self, item);
        self.implementing.pop();
    }
    /// A `use` that renames the trait (`use marv_tools::Tool as T`) would hide `T::run` from a reader, and one that
    /// brings a method in by name doesn't exist for trait methods; flag the rename.
    fn visit_use_rename(&mut self, rename: &'ast syn::UseRename) {
        if rename.ident == "Tool" || rename.ident == "DynTool" {
            self.push(rename.ident.span(), quote_tokens(rename));
        }
    }
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if TOOL_ENTRIES.iter().any(|(name, args)| call.method == name && call.args.len() == *args) {
            self.push(call.method.span(), quote_tokens(call));
        }
        syn::visit::visit_expr_method_call(self, call);
    }
    /// A path to a tool entry: `<T as Tool>::run`, or `Type::run` (a trait or a type: `Tool::run`, `Glob::run`, an
    /// alias), called or taken as a function pointer. A lowercase module's free function (`tui::run`) isn't one.
    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        let segments = &path.path.segments;
        let last = segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
        let owner = segments.len().checked_sub(2).map(|i| segments[i].ident.to_string()).unwrap_or_default();
        let by_type = path.qself.is_some() || owner.starts_with(char::is_uppercase);
        if by_type && TOOL_ENTRIES.iter().any(|(name, _)| last == *name) {
            self.push(syn::spanned::Spanned::span(&path.path), quote_tokens(path));
        }
        syn::visit::visit_expr_path(self, path);
    }
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.scan_tokens(mac.tokens.clone());
    }
}

/// The tokens of `node` as one line (`a . b (c)`), the way the allowlist names a call.
fn quote_tokens(node: &impl quote::ToTokens) -> String {
    node.to_token_stream().to_string()
}

/// Every Rust source of the product under `crates/`: everything but each crate's own `tests/`, `benches/` and
/// `examples/` folders (integration tests, not code Marv runs) and `target/`.
fn product_sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, top: bool, files: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if path.is_dir() {
                let skipped = name == "target" || (top && matches!(name.as_str(), "tests" | "benches" | "examples"));
                if !skipped {
                    walk(&path, false, files);
                }
            } else if name.ends_with(".rs") {
                files.push(path);
            }
        }
    }
    let mut files = Vec::new();
    for krate in std::fs::read_dir(repo_root().join("crates")).expect("crates/").flatten() {
        walk(&krate.path(), true, &mut files);
    }
    files.sort();
    files
}

/// Every call of a tool entry in non-test code outside `run_tool`, and the allowlist entries that matched nothing.
fn tool_entry_calls() -> (Vec<Finding>, Vec<String>) {
    let root = repo_root();
    let mut found = Vec::new();
    let mut test_files: Vec<PathBuf> = Vec::new();
    let mut parsed = Vec::new();
    for file in product_sources() {
        let text = std::fs::read_to_string(&file).expect("a source file");
        let ast = syn::parse_file(&text).unwrap_or_else(|e| panic!("{} doesn't parse: {e}", file.display()));
        let shown = file.strip_prefix(&root).unwrap_or(&file).display().to_string();
        let mut finder = EntryFinder {
            file: shown,
            function: Vec::new(),
            implementing: Vec::new(),
            found: Vec::new(),
            test_modules: Vec::new(),
        };
        syn::visit::Visit::visit_file(&mut finder, &ast);
        let dir = if file.file_name().is_some_and(|n| n == "mod.rs" || n == "lib.rs" || n == "main.rs") {
            file.parent().expect("a folder").to_path_buf()
        } else {
            file.with_extension("")
        };
        for name in &finder.test_modules {
            test_files.push(dir.join(format!("{name}.rs")));
            test_files.push(dir.join(name).join("mod.rs"));
        }
        parsed.push((file, finder.found));
    }
    for (file, findings) in parsed {
        if !test_files.contains(&file) {
            found.extend(findings);
        }
    }
    // The one place: run_tool's own body, and the blanket `impl<T: Tool> DynTool for T`, which only forwards.
    found.retain(|f| {
        let in_run_tool = f.file == "crates/marv-tools/src/registry.rs" && f.function == "run_tool";
        let forwarding = f.file == "crates/marv-tools/src/types.rs"
            && f.implementing == "DynTool for T"
            && TOOL_ENTRIES.iter().any(|(name, _)| f.function == *name)
            && f.code == format!("Tool :: {}", f.function);
        !in_run_tool && !forwarding
    });
    let mut unused = Vec::new();
    for (file, function, code) in NOT_A_TOOL {
        let before = found.len();
        found.retain(|f| !(f.file == file && f.function == function && f.code == code));
        if found.len() == before {
            unused.push(format!("{file} {function}: {code}"));
        }
    }
    (found, unused)
}

/// **Only `run_tool` runs a tool.** It's the one place a call is checked (parsed, its floor, protected paths,
/// taint, yolo, the approval, the checkpoint) before it runs or asks, and its result is marked (the source, the
/// fence, the taint). A tool's `run`, `preview` or `approve_itself` called anywhere else would skip all of that, so
/// the product's code (every `.rs` under `crates/` but test code: `#[cfg(test)]` items and modules, `#[test]`
/// functions, each crate's `tests/`) is parsed with syn and searched for: a method call of that name and arity,
/// a path to one (`Tool::run`, `<T as Tool>::run`, an alias's `X::run`, a function pointer), a `use` of one, and
/// `.run`/`::run` in a macro's tokens. The exceptions are `run_tool`'s body, the blanket `impl DynTool for T`
/// (which forwards), and the narrow `NOT_A_TOOL` list, whose entries must each still match something.
///
/// Proven: added to `marv_tools::table::example_row` a `tool.run(input, ctx)`, a `<crate::glob::Glob as
/// crate::Tool>::run` function pointer, a `tool.preview(…)` split over five lines, a `tool.run(…)` inside
/// `assert!(…)`, and a `use crate::Tool as T;` → each was reported (file, line, function).
#[test]
fn only_run_tool_runs_a_tool() {
    let (found, unused) = tool_entry_calls();
    let lines: Vec<String> = found.iter().map(|f| format!("{}:{} (in fn {}): {}", f.file, f.line, f.function, f.code)).collect();
    assert!(lines.is_empty(), "a tool is run outside run_tool (call it through marv_tools::run_tool):\n{}", lines.join("\n"));
    assert!(unused.is_empty(), "NOT_A_TOOL entries that match nothing any more (remove them):\n{}", unused.join("\n"));
}
