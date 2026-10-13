//! Taint: once web, MCP, mesh or image content enters a session's context, every exfiltration-capable call asks,
//! even in yolo, even after an "always".
//!
//! New in marv-rs (`docs/SECURITY.md`, "Provenance and taint"). Marv had no taint: yolo ran sandboxed bash and file
//! edits after a `web_fetch`, and remembered scopes ("don't ask again") answered network calls for the rest of the
//! session. The principle: an injected model must never hold private data, untrusted content and a way out together;
//! taint marks the moment the second arrives, so the third needs a person.
//!
//! What asks once tainted ([`TaintState::must_ask`]): `ToolKind::Egress` calls and any call that `uses_network`:
//! network bash, every MCP call (including `readOnlyHint` tools), web requests to a new URL prefix, mesh calls on
//! unapproved wires; and writes to protected paths (those ask regardless of taint).
//!
//! A subagent starts tainted if its parent is (its prompt was written by a tainted model), but inherits no network,
//! web or mesh approvals (`for_subagent`).

use std::collections::BTreeSet;
use std::sync::Mutex;

use marv_core::{Provenance, ToolKind};

/// A session's taint. Shared (`Arc`) between the session, its tool context and the approver.
#[derive(Debug, Default)]
pub struct TaintState {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// What tainted it, in order, each once.
    sources: Vec<Provenance>,
    /// URL prefixes the user approved while tainted (`https://docs.rs/`): a web request under one doesn't ask again.
    approved_prefixes: BTreeSet<String>,
}

impl TaintState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records text from `source` entering the context. Returns true when this call tainted a clean session.
    pub fn mark(&self, source: Provenance) -> bool {
        if !source.taints() {
            return false;
        }
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let was_clean = inner.sources.is_empty();
        if !inner.sources.contains(&source) {
            inner.sources.push(source);
        }
        was_clean
    }

    pub fn is_tainted(&self) -> bool {
        !self.inner.lock().unwrap_or_else(|e| e.into_inner()).sources.is_empty()
    }

    /// What tainted the session, first first (for the status bar and the approval's warning).
    pub fn sources(&self) -> Vec<Provenance> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).sources.clone()
    }

    /// Whether a call must ask a person now, whatever yolo or remembered scopes say.
    pub fn must_ask(&self, kind: ToolKind, uses_network: bool) -> bool {
        self.is_tainted() && (kind == ToolKind::Egress || uses_network)
    }

    /// Remembers a web prefix the user approved after taint.
    pub fn approve_prefix(&self, prefix: &str) {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).approved_prefixes.insert(prefix.to_string());
    }

    /// Whether a URL is under a prefix approved after taint.
    pub fn prefix_approved(&self, url: &str) -> bool {
        self.inner.lock().unwrap_or_else(|e| e.into_inner()).approved_prefixes.iter().any(|p| url.starts_with(p.as_str()))
    }

    /// A subagent's state: tainted like its parent, with no approvals.
    pub fn for_subagent(&self) -> TaintState {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        TaintState { inner: Mutex::new(Inner { sources: inner.sources.clone(), approved_prefixes: BTreeSet::new() }) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_outside_sources_taint() {
        let taint = TaintState::new();
        assert!(!taint.mark(Provenance::User));
        assert!(!taint.mark(Provenance::Project));
        assert!(!taint.is_tainted());
        assert!(taint.mark(Provenance::Web));
        assert!(!taint.mark(Provenance::Mcp)); // already tainted
        assert_eq!(taint.sources(), vec![Provenance::Web, Provenance::Mcp]);
    }

    #[test]
    fn egress_asks_once_tainted() {
        let taint = TaintState::new();
        assert!(!taint.must_ask(ToolKind::Egress, true));
        taint.mark(Provenance::Mesh);
        assert!(taint.must_ask(ToolKind::Egress, false));
        assert!(taint.must_ask(ToolKind::Execute, true)); // network bash
        assert!(!taint.must_ask(ToolKind::Write, false));
    }

    #[test]
    fn subagents_inherit_taint_but_not_approvals() {
        let taint = TaintState::new();
        taint.mark(Provenance::Web);
        taint.approve_prefix("https://docs.rs/");
        assert!(taint.prefix_approved("https://docs.rs/regex"));
        let child = taint.for_subagent();
        assert!(child.is_tainted());
        assert!(!child.prefix_approved("https://docs.rs/regex"));
    }
}
