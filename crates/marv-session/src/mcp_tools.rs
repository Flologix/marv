//! MCP tools as Marv tools (new in marv-rs: Marv's `src/mcp/tool.ts` made `Tool`s directly; here marv-mcp stays free
//! of marv-tools, so the session adapts its plain `McpTool`s, and runs the deferred pair from `src/mcp/deferred.ts`).
//!
//! - [`McpToolAdapter`]: `mcp__<server>__<tool>`, the server's own schema, provenance MCP (taints), never yolo-safe.
//!   Its result is cut here (the same 10k head and 20k tail as marv-mcp's cap), and a cut one is saved whole (#61).
//!   Its kind reads the session's taint *at call time*: `readOnlyHint` tools run freely only while the session is
//!   clean; once tainted every MCP call is Egress and asks, even in yolo, even after an "always".
//! - [`ToolSearchTool`] / [`McpCallTool`]: a deferred conversation's two constant specs (#14). `mcp_call` hands the
//!   call to `run_tool` with the real MCP tool, so its preview, approval, scope and label apply unchanged.
//!
//! The session owns the [`McpHandle`]; `/mcp` reads its `status()` through it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use marv_core::{Provenance, ToolCall, ToolKind};
use marv_mcp::deferred::{SearchInput, arguments_as_object, schema_hint, tool_search_output, unknown_tool_error};
use marv_mcp::manager::McpManager;
use marv_mcp::tool::McpTool;
use marv_sandbox::TaintState;
use marv_tools::output::cut_for_call;
use marv_tools::{Preview, Repaired, Scope, Tool, ToolContext, ToolError, ToolOutput, ToolRef};
use serde::Deserialize;
use serde_json::{Map, Value};

/// The session's MCP servers. `McpManager` synchronizes itself (`&self` methods), so `/mcp` reads its status while a
/// turn waits on `ready()`.
pub type McpHandle = Arc<McpManager>;

/// One MCP tool as a Marv tool.
pub struct McpToolAdapter {
    pub tool: McpTool,
    /// The session's taint: read when the call is checked.
    pub taint: Arc<TaintState>,
}

#[async_trait]
impl Tool for McpToolAdapter {
    type Input = Map<String, Value>;

    fn name(&self) -> &str {
        &self.tool.name
    }
    fn description(&self) -> &str {
        &self.tool.description
    }
    fn schema(&self) -> Value {
        self.tool.spec.parameters.clone()
    }
    fn kind(&self) -> ToolKind {
        self.tool.kind(self.taint.is_tainted())
    }
    /// An MCP server acts outside the project: nothing for /undo to put back (#16).
    fn checkpoint(&self) -> bool {
        false
    }
    fn provenance(&self) -> Provenance {
        Provenance::Mcp
    }
    fn label(&self, input: &Self::Input) -> String {
        self.tool.label(input)
    }
    fn uses_network(&self, _input: &Self::Input) -> bool {
        self.tool.uses_network()
    }
    async fn preview(&self, input: &Self::Input, _ctx: &ToolContext) -> Result<Preview, ToolError> {
        let p = self.tool.preview(input);
        Ok(Preview { title: p.title, text: p.text, note: Some(p.note), ..Preview::default() })
    }
    fn scope(&self, _input: &Self::Input) -> Option<Scope> {
        let (key, description) = self.tool.scope();
        Some(Scope { key, description })
    }
    fn auto_safe(&self, _input: &Self::Input, _ctx: &ToolContext) -> bool {
        self.tool.auto_safe() // never: yolo doesn't skip an MCP call
    }
    async fn run(&self, input: &Self::Input, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let result = self.tool.run_whole(input.clone(), ctx.cancel.clone()).await.map_err(ToolError::new)?;
        // Cut here, not in marv-mcp: a cut result is saved whole for read_file and grep (#61).
        let mut out = ToolOutput::ok(cut_for_call(&result.output, ctx), result.summary.unwrap_or_default());
        out.is_error = result.is_error;
        out.provenance = Provenance::Mcp;
        Ok(out)
    }
}

/// `tool_search`: names or words → up to 5 full specs. Reads the manager's current specs.
pub struct ToolSearchTool {
    pub mcp: McpHandle,
    /// MCP tools whose schema this conversation has seen: their errors don't repeat it.
    pub loaded: Arc<Mutex<HashSet<String>>>,
}

#[async_trait]
impl Tool for ToolSearchTool {
    type Input = SearchInput;
    fn name(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        marv_mcp::deferred::SEARCH_DESCRIPTION
    }
    fn schema(&self) -> Value {
        marv_mcp::deferred::deferred_specs()[0].parameters.clone()
    }
    fn label(&self, input: &Self::Input) -> String {
        if input.names.is_empty() { input.query.clone().unwrap_or_default() } else { input.names.join(", ") }
    }
    async fn run(&self, input: &Self::Input, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let tools = self.mcp.tools();
        // Compact JSON: a loaded schema costs what its spec would have, once instead of in every request.
        let (output, summary) = {
            let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
            tool_search_output(&tools, input, &mut loaded)
        };
        Ok(ToolOutput::ok(output, summary))
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCallInput {
    pub tool: String,
    #[serde(default)]
    pub arguments: Value,
}

/// `mcp_call`: runs the named MCP tool through `run_tool`, so the real tool's approval applies (this wrapper asks
/// nothing itself). An unknown tool's error names the closest; a rejected call to a tool whose schema the
/// conversation hasn't seen gets the schema appended, once.
pub struct McpCallTool {
    pub mcp: McpHandle,
    pub taint: Arc<TaintState>,
    pub loaded: Arc<Mutex<HashSet<String>>>,
}

#[async_trait]
impl Tool for McpCallTool {
    type Input = McpCallInput;
    fn name(&self) -> &str {
        "mcp_call"
    }
    fn description(&self) -> &str {
        marv_mcp::deferred::CALL_DESCRIPTION
    }
    fn schema(&self) -> Value {
        marv_mcp::deferred::deferred_specs()[1].parameters.clone()
    }
    /// Execute even once tainted: the real tool is Egress then and asks, with `must_ask` and its own scope
    /// (`mcp:<server>:<tool>`). An Egress wrapper was gated as well, under the bare scope `mcp_call`, which
    /// `marv -p --approve mcp` doesn't cover: every MCP call after the first MCP read was refused there, and the TUI
    /// asked twice.
    fn kind(&self) -> ToolKind {
        ToolKind::Execute
    }
    fn checkpoint(&self) -> bool {
        false
    }
    fn provenance(&self) -> Provenance {
        Provenance::Mcp
    }
    fn label(&self, input: &Self::Input) -> String {
        input.tool.clone()
    }
    /// The inner `run_tool` with the real MCP tool asks; this wrapper doesn't ask twice.
    fn needs_approval(&self, _input: &Self::Input) -> Option<bool> {
        Some(false)
    }
    /// Marv #10: `arguments` sent as a JSON string of an object is read as the object (with a note).
    fn repair(&self, raw: &Value, _ctx: &ToolContext) -> Option<Repaired> {
        arguments_as_object(raw).map(|(input, note)| Repaired { input, note: Some(note.into()) })
    }
    async fn run(&self, input: &Self::Input, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let (tools, specs) = (self.mcp.tools(), self.mcp.specs());
        let Some(tool) = tools.iter().find(|t| t.name == input.tool) else {
            return Err(ToolError::new(unknown_tool_error(&specs, &input.tool)));
        };
        let adapter: ToolRef = Arc::new(McpToolAdapter { tool: tool.clone(), taint: self.taint.clone() });
        let arguments = if input.arguments.is_null() { Value::Object(Map::new()) } else { input.arguments.clone() };
        let call = ToolCall {
            id: ctx.call_id.clone().unwrap_or_else(|| "mcp_call".into()),
            name: tool.name.clone(),
            arguments: arguments.to_string(),
        };
        let mut result = marv_tools::run_tool(&call, ctx, &[adapter]).await.result;
        // A guess at the arguments that the server rejected: show the schema once, so a guess costs one step, not two.
        if result.is_error
            && !result.declined
            && self.loaded.lock().unwrap_or_else(|e| e.into_inner()).insert(tool.name.clone())
            && let Some(spec) = specs.iter().find(|s| s.name == tool.name)
        {
            result.output.push_str(&schema_hint(spec));
        }
        Ok(result)
    }
}

/// Ports the `deferredTools` cases of `tests/mcp-deferred.test.ts` that need no connected server (`McpManager::start`
/// is still todo in marv-mcp, so a manager can't hold tools yet; the search and schema-hint logic is tested in
/// `marv_mcp::deferred`).
#[cfg(test)]
mod tests {
    use super::*;
    use marv_mcp::manager::{CONNECT_TIMEOUT, LOGIN_TIMEOUT, ManagerOptions};
    use serde_json::json;

    fn empty() -> McpHandle {
        McpManager::new(
            vec![],
            ManagerOptions {
                root: "/nonexistent/p".into(),
                project_key: "p".into(),
                home: "/nonexistent".into(),
                version: "0".into(),
                trust: None,
                logins: None,
                env: Default::default(),
                connect_timeout: CONNECT_TIMEOUT,
                login_timeout: LOGIN_TIMEOUT,
            },
        )
    }

    #[tokio::test]
    async fn tool_search_with_nothing_found_says_so_and_names_unknown_tools() {
        let search = ToolSearchTool { mcp: empty(), loaded: Arc::default() };
        let ctx = ToolContext::new("/nonexistent/p");
        let out = search.run(&SearchInput { names: vec![], query: Some("issues".into()) }, &ctx).await.unwrap();
        assert_eq!(out.output, "No MCP tool matches. Marv's note in this conversation lists them all.");
        assert_eq!(out.summary, "0 tools");
        let out = search.run(&SearchInput { names: vec!["mcp__x__y".into()], query: None }, &ctx).await.unwrap();
        assert_eq!(out.output, "Marv: no MCP tool is named mcp__x__y.");
    }

    #[test]
    fn tool_search_labels_names_first_then_the_query() {
        let search = ToolSearchTool { mcp: empty(), loaded: Arc::default() };
        assert_eq!(search.label(&SearchInput { names: vec!["a".into(), "b".into()], query: Some("q".into()) }), "a, b");
        assert_eq!(search.label(&SearchInput { names: vec![], query: Some("q".into()) }), "q");
    }

    #[tokio::test]
    async fn mcp_call_to_an_unknown_tool_fails_and_string_arguments_are_repaired() {
        let call = McpCallTool { mcp: empty(), taint: Arc::new(TaintState::new()), loaded: Arc::default() };
        let ctx = ToolContext::new("/nonexistent/p");
        let input = McpCallInput { tool: "mcp__gh__nope".into(), arguments: json!({}) };
        let err = call.run(&input, &ctx).await.unwrap_err();
        assert_eq!(err.to_string(), "No MCP tool is named \"mcp__gh__nope\".");
        let repaired = call.repair(&json!({ "tool": "mcp__a__b", "arguments": "{\"x\":1}" }), &ctx).unwrap();
        assert_eq!(repaired.input, json!({ "tool": "mcp__a__b", "arguments": { "x": 1 } }));
        assert!(repaired.note.unwrap().contains("came as a JSON string"));
    }

    struct Long;
    #[async_trait]
    impl marv_mcp::tool::McpCaller for Long {
        async fn call(
            &self,
            _tool: &str,
            _args: Map<String, Value>,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> marv_mcp::Result<marv_mcp::tool::McpCallResult> {
            let text = format!("first\n{}\nlast", "y".repeat(40_000));
            let block = marv_mcp::tool::ContentBlock { kind: "text".into(), text: Some(text), ..Default::default() };
            Ok(marv_mcp::tool::McpCallResult { content: vec![block], ..Default::default() })
        }
    }

    #[tokio::test]
    async fn a_long_mcp_result_is_cut_and_saved_whole() {
        let server = marv_mcp::config::parse_server(
            "gh",
            &json!({"command": "x"}),
            marv_mcp::config::ServerSource::Project,
            &|_| None,
            "f",
        )
        .unwrap();
        let info =
            marv_mcp::tool::McpToolInfo { name: "logs".into(), description: None, input_schema: Map::new(), annotations: None };
        let adapter = McpToolAdapter {
            tool: marv_mcp::tool::make_mcp_tool(&server, info, Arc::new(Long)),
            taint: Arc::new(TaintState::new()),
        };
        let store = std::env::temp_dir().join(format!("marv-mcp-spill-{}", std::process::id()));
        let ctx =
            ToolContext { tool_output: Some(store.join("s")), call_id: Some("m1".into()), ..ToolContext::new("/nonexistent/p") };
        let out = adapter.run(&Map::new(), &ctx).await.unwrap();
        let saved = store.join("s/m1.txt");
        assert!(out.output.starts_with("first\n") && out.output.ends_with("\nlast"));
        assert!(out.output.contains(&format!("characters omitted; full output: {}", saved.display())));
        assert_eq!(std::fs::read_to_string(&saved).unwrap(), format!("first\n{}\nlast", "y".repeat(40_000)));
        std::fs::remove_dir_all(store).unwrap();
    }

    struct Recording(std::sync::Mutex<Vec<String>>);
    #[async_trait]
    impl marv_tools::Approver for Recording {
        async fn approve(&self, request: marv_tools::ApprovalRequest) -> marv_tools::Decision {
            self.0.lock().unwrap().push(request.scope.key);
            marv_tools::Decision::Unavailable
        }
    }

    #[tokio::test]
    async fn a_tainted_mcp_call_wrapper_leaves_the_asking_to_the_real_tool() {
        let approver = Arc::new(Recording(std::sync::Mutex::new(vec![])));
        let ctx = ToolContext { approver: Some(approver.clone()), ..ToolContext::new("/nonexistent/p") };
        ctx.taint.mark(Provenance::Mcp);
        let call = McpCallTool { mcp: empty(), taint: ctx.taint.clone(), loaded: Arc::default() };
        assert_eq!(call.kind(), ToolKind::Execute);
        let tools: Vec<ToolRef> = vec![Arc::new(call)];
        let args = json!({ "tool": "mcp__gh__nope", "arguments": {} }).to_string();
        let out =
            marv_tools::run_tool(&ToolCall { id: "c1".into(), name: "mcp_call".into(), arguments: args }, &ctx, &tools).await;
        // Reaches the lookup (no such tool here) without an approval under the wrapper's own name.
        assert_eq!(out.result.output, "No MCP tool is named \"mcp__gh__nope\".");
        assert!(approver.0.lock().unwrap().is_empty());
    }
}
