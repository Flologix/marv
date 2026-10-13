//! MCP tools as Marv tools (new in marv-rs: Marv's `src/mcp/tool.ts` made `Tool`s directly; here marv-mcp stays free
//! of marv-tools, so the session adapts its plain `McpTool`s, and runs the deferred pair from `src/mcp/deferred.ts`).
//!
//! - [`McpToolAdapter`]: `mcp__<server>__<tool>`, the server's own schema, provenance MCP (taints), never yolo-safe.
//!   Its kind reads the session's taint *at call time*: `readOnlyHint` tools run freely only while the session is
//!   clean; once tainted every MCP call is Egress and asks, even in yolo, even after an "always".
//! - [`ToolSearchTool`] / [`McpCallTool`]: a deferred conversation's two constant specs (#14). `mcp_call` hands the
//!   call to `run_tool` with the real MCP tool, so its preview, approval, scope and label apply unchanged.
//!
//! The session owns the [`McpHandle`]; `/mcp` reads its `status()` through it.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use marv_core::{Provenance, ToolKind};
use marv_mcp::deferred::SearchInput;
use marv_mcp::manager::McpManager;
use marv_mcp::tool::McpTool;
use marv_sandbox::TaintState;
use marv_tools::{Preview, Scope, Tool, ToolContext, ToolError, ToolOutput};
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
        let result = self.tool.run(input.clone(), ctx.cancel.clone()).await.map_err(ToolError::new)?;
        let mut out = ToolOutput::ok(result.output, result.summary.unwrap_or_default());
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
        input.query.clone().unwrap_or_else(|| input.names.join(", "))
    }
    async fn run(&self, _input: &Self::Input, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        todo!("port: tool_search run (deferred::tool_search_output over the manager's specs, marking `loaded`)")
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
    fn kind(&self) -> ToolKind {
        if self.taint.is_tainted() { ToolKind::Egress } else { ToolKind::Execute }
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
    async fn run(&self, _input: &Self::Input, _ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        todo!(
            "port: mcp_call run (find the tool, arguments_as_object, run_tool with [adapter], unknown_tool_error, schema_hint once)"
        )
    }
}
