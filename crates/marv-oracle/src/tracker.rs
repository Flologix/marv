//! The issue tracker MCP server the MCP scenarios run against. Ports Marv's `evals/mcp/tracker.ts`.
//!
//! Local, deterministic, no network: `marv-oracle tracker <data.json> <out.json>` serves the issues
//! in `data.json` over stdio and appends every comment added to `out.json`, which the runner copies
//! into the project (`__tracker_comments__.json`) so `expect/files/` records it, the way the evals'
//! runner copies it in for the check.
//!
//! 25 tools with realistic descriptions and parameters, of which a task needs one to three: their
//! specs together cost well over Marv's deferral threshold (#14), the way a real server's do. The
//! specs are the TypeScript tracker's own `tools/list` answer, byte for byte
//! (`tests/oracle/mcp/tracker-tools.json`, captured from zod's JSON Schema output), so both Marvs
//! are offered exactly what the evals offer.
//!
//! Answers are `JSON.stringify(value, null, 2)` of objects in the TypeScript's key order, hence the
//! small insertion-ordered [`J`] instead of `serde_json::Value` (which sorts keys; `preserve_order`
//! can't be turned on here, see `docs/subsystems/oracle.md`).
//!
//! Differences from the TypeScript: arguments aren't validated against the schemas (the SDK's zod
//! check); a missing or mistyped argument reads as absent. Scripted calls send valid arguments.

use std::fmt::Write as _;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Value, json};

/// The TypeScript tracker's `tools/list` result, as it sent it.
pub const TOOLS: &str = include_str!("../../../tests/oracle/mcp/tracker-tools.json");

/// The tools whose results `fields` can narrow (registered with `READ` in the TypeScript, which
/// includes `add_comment`: the evals run with no approver, which refuses anything else).
const READ: [&str; 14] = [
    "get_issue",
    "list_issues",
    "add_comment",
    "search_issues",
    "list_labels",
    "get_user",
    "list_users",
    "list_milestones",
    "get_milestone",
    "list_comments",
    "get_project",
    "list_projects",
    "get_activity",
    "export_issues",
];

/// JSON that keeps its keys in insertion order, like a JavaScript object.
#[derive(Debug, Clone, PartialEq)]
pub enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, key: &str) -> Option<&J> {
        match self {
            J::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut J> {
        match self {
            J::Obj(fields) => fields.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    /// `obj[key] = value`: replaced in place, or added at the end.
    fn set(&mut self, key: &str, value: J) {
        if let Some(slot) = self.get_mut(key) {
            *slot = value;
        } else if let J::Obj(fields) = self {
            fields.push((key.to_string(), value));
        }
    }

    fn remove(&mut self, key: &str) {
        if let J::Obj(fields) = self {
            fields.retain(|(k, _)| k != key);
        }
    }

    fn str(&self) -> Option<&str> {
        match self {
            J::Str(s) => Some(s),
            _ => None,
        }
    }

    fn int(&self) -> Option<i64> {
        match self {
            J::Num(n) => n.parse().ok(),
            _ => None,
        }
    }

    fn items(&self) -> &[J] {
        match self {
            J::Arr(items) => items,
            _ => &[],
        }
    }

    fn obj(fields: Vec<(&str, J)>) -> J {
        J::Obj(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    fn s(text: &str) -> J {
        J::Str(text.to_string())
    }

    fn n(number: i64) -> J {
        J::Num(number.to_string())
    }

    /// `JSON.stringify(value, null, 2)`.
    pub fn pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        let pad = |depth: usize| "  ".repeat(depth);
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(n),
            J::Str(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
            J::Arr(items) if items.is_empty() => out.push_str("[]"),
            J::Obj(fields) if fields.is_empty() => out.push_str("{}"),
            J::Arr(items) => {
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    out.push_str(&pad(depth + 1));
                    item.write(out, depth + 1);
                    out.push_str(if i + 1 < items.len() { ",\n" } else { "\n" });
                }
                let _ = write!(out, "{}]", pad(depth));
            }
            J::Obj(fields) => {
                out.push_str("{\n");
                for (i, (key, value)) in fields.iter().enumerate() {
                    let _ = write!(out, "{}{}: ", pad(depth + 1), serde_json::to_string(key).unwrap_or_default());
                    value.write(out, depth + 1);
                    out.push_str(if i + 1 < fields.len() { ",\n" } else { "\n" });
                }
                let _ = write!(out, "{}}}", pad(depth));
            }
        }
    }

    fn from_value(value: &Value) -> J {
        match value {
            Value::Null => J::Null,
            Value::Bool(b) => J::Bool(*b),
            Value::Number(n) => J::Num(n.to_string()),
            Value::String(s) => J::Str(s.clone()),
            Value::Array(items) => J::Arr(items.iter().map(J::from_value).collect()),
            // Only for call arguments, whose key order nothing depends on.
            Value::Object(map) => J::Obj(map.iter().map(|(k, v)| (k.clone(), J::from_value(v))).collect()),
        }
    }
}

impl<'de> Deserialize<'de> for J {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<J, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = J;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("JSON")
            }
            fn visit_unit<E>(self) -> Result<J, E> {
                Ok(J::Null)
            }
            fn visit_bool<E>(self, b: bool) -> Result<J, E> {
                Ok(J::Bool(b))
            }
            fn visit_i64<E>(self, n: i64) -> Result<J, E> {
                Ok(J::Num(n.to_string()))
            }
            fn visit_u64<E>(self, n: u64) -> Result<J, E> {
                Ok(J::Num(n.to_string()))
            }
            fn visit_f64<E>(self, n: f64) -> Result<J, E> {
                Ok(J::Num(serde_json::Number::from_f64(n).map_or("null".into(), |n| n.to_string())))
            }
            fn visit_str<E>(self, s: &str) -> Result<J, E> {
                Ok(J::Str(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> Result<J, E> {
                Ok(J::Str(s))
            }
            fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<J, A::Error> {
                let mut items = Vec::new();
                while let Some(item) = seq.next_element()? {
                    items.push(item);
                }
                Ok(J::Arr(items))
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<J, A::Error> {
                let mut fields: Vec<(String, J)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, J>()? {
                    // A repeated key keeps its first position and takes the last value, as JSON.parse does.
                    match fields.iter_mut().find(|(k, _)| *k == key) {
                        Some(slot) => slot.1 = value,
                        None => fields.push((key, value)),
                    }
                }
                Ok(J::Obj(fields))
            }
        }
        deserializer.deserialize_any(V)
    }
}

/// A tool's answer: text, and whether it's an error.
type Answer = (String, bool);

fn text(value: J) -> Answer {
    match value {
        J::Str(s) => (s, false),
        other => (other.pretty(), false),
    }
}

fn failure(message: String) -> Answer {
    (message, true)
}

/// `fields` applied to an answer: each object (or the one) keeps only those keys.
fn pick(value: J, keys: &[String]) -> J {
    match value {
        J::Arr(items) => J::Arr(items.into_iter().map(|v| pick(v, keys)).collect()),
        J::Obj(fields) => J::Obj(fields.into_iter().filter(|(k, _)| keys.contains(k)).collect()),
        other => other,
    }
}

/// The tracker's state, as `makeTracker` builds it.
pub struct Tracker {
    data: J,
    issues: Vec<J>,
    next_comment: i64,
    subscriptions: Vec<i64>,
    out: PathBuf,
}

impl Tracker {
    pub fn load(data_file: &Path, out: &Path) -> anyhow::Result<Tracker> {
        let data: J = serde_json::from_str(&std::fs::read_to_string(data_file)?)?;
        let mut next_comment = 1;
        let mut issues = Vec::new();
        for issue in data.get("issues").map(J::items).unwrap_or_default() {
            // `{ ...i, comments: (i.comments ?? []).map((c) => ({ id: nextComment++, issue: i.number, ...c })) }`
            let number = issue.get("number").cloned().unwrap_or(J::Null);
            let comments = issue
                .get("comments")
                .map(J::items)
                .unwrap_or_default()
                .iter()
                .map(|c| {
                    let mut fields = vec![("id".to_string(), J::n(next_comment)), ("issue".to_string(), number.clone())];
                    next_comment += 1;
                    if let J::Obj(rest) = c {
                        for (k, v) in rest {
                            match fields.iter_mut().find(|(key, _)| key == k) {
                                Some(slot) => slot.1 = v.clone(),
                                None => fields.push((k.clone(), v.clone())),
                            }
                        }
                    }
                    J::Obj(fields)
                })
                .collect();
            let mut issue = issue.clone();
            issue.set("comments", J::Arr(comments));
            issues.push(issue);
        }
        Ok(Tracker { data, issues, next_comment, subscriptions: Vec::new(), out: out.to_path_buf() })
    }

    fn find(&mut self, number: Option<i64>) -> Option<&mut J> {
        self.issues.iter_mut().find(|i| i.get("number").and_then(J::int) == number && number.is_some())
    }

    fn summary(issue: &J) -> J {
        let field = |k: &str| issue.get(k).cloned().unwrap_or(J::Null);
        J::obj(vec![
            ("number", field("number")),
            ("title", field("title")),
            ("state", field("state")),
            ("labels", field("labels")),
            ("assignee", field("assignee")),
        ])
    }

    fn data_list(&self, key: &str) -> Vec<J> {
        self.data.get(key).map(|v| v.items().to_vec()).unwrap_or_default()
    }

    /// The comments added so far (`out.json`).
    fn added(&self) -> Vec<J> {
        std::fs::read_to_string(&self.out)
            .ok()
            .and_then(|t| serde_json::from_str::<J>(&t).ok())
            .map(|j| j.items().to_vec())
            .unwrap_or_default()
    }

    /// Runs one tool, `fields` included.
    pub fn call(&mut self, name: &str, args: &Value) -> Answer {
        let answer = self.run(name, args);
        let only: Vec<String> = args
            .get("fields")
            .and_then(Value::as_array)
            .map(|f| f.iter().filter_map(|s| s.as_str().map(String::from)).collect())
            .unwrap_or_default();
        // A JSON answer is picked here; a tool that answers in another format (export_issues' CSV) picks its own.
        if !READ.contains(&name) || only.is_empty() || answer.1 {
            return answer;
        }
        match serde_json::from_str::<J>(&answer.0) {
            Ok(value) => text(pick(value, &only)),
            Err(_) => answer,
        }
    }

    fn run(&mut self, name: &str, args: &Value) -> Answer {
        let int = |key: &str| args.get(key).and_then(Value::as_i64);
        let string = |key: &str| args.get(key).and_then(Value::as_str).map(String::from);
        let number = int("number");
        let shown = |n: Option<i64>| n.map_or("undefined".to_string(), |n| n.to_string());
        match name {
            "get_issue" => {
                let include = args.get("include_comments").and_then(Value::as_bool);
                let Some(issue) = self.find(number) else { return failure(format!("No issue #{}.", shown(number))) };
                let mut issue = issue.clone();
                if include == Some(false) {
                    issue.remove("comments");
                }
                text(issue)
            }
            "list_issues" | "search_issues" => {
                let default_state = if name == "list_issues" { "open" } else { "all" };
                let state = string("state").unwrap_or_else(|| default_state.into());
                let label = string("label");
                let assignee = string("assignee");
                let limit = int("limit").unwrap_or(if name == "list_issues" { 30 } else { 10 }).max(0) as usize;
                let words: Vec<String> = string("query")
                    .unwrap_or_default()
                    .to_lowercase()
                    .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .filter(|w| !w.is_empty())
                    .map(String::from)
                    .collect();
                let mut found: Vec<&J> = self
                    .issues
                    .iter()
                    .filter(|i| state == "all" || i.get("state").and_then(J::str) == Some(state.as_str()))
                    .filter(|i| {
                        label
                            .as_ref()
                            .is_none_or(|l| i.get("labels").map(J::items).unwrap_or_default().iter().any(|x| x.str() == Some(l)))
                    })
                    .filter(|i| {
                        name == "search_issues" || assignee.as_ref().is_none_or(|a| i.get("assignee").and_then(J::str) == Some(a))
                    })
                    .filter(|i| {
                        name == "list_issues" || {
                            let hay = format!(
                                "{} {}",
                                i.get("title").and_then(J::str).unwrap_or(""),
                                i.get("body").and_then(J::str).unwrap_or("")
                            )
                            .to_lowercase();
                            words.iter().any(|w| hay.contains(w.as_str()))
                        }
                    })
                    .collect();
                if name == "list_issues" {
                    found.sort_by_key(|i| std::cmp::Reverse(i.get("number").and_then(J::int)));
                }
                text(J::Arr(found.into_iter().take(limit).map(Self::summary).collect()))
            }
            "add_comment" => {
                let body = string("body").unwrap_or_default();
                let id = self.next_comment;
                let Some(issue) = self.find(number) else { return failure(format!("No issue #{}.", shown(number))) };
                let issue_number = issue.get("number").cloned().unwrap_or(J::Null);
                let comment = J::obj(vec![
                    ("id", J::n(id)),
                    ("issue", issue_number.clone()),
                    ("author", J::s("you")),
                    ("body", J::s(&body)),
                ]);
                if let Some(J::Arr(comments)) = issue.get_mut("comments") {
                    comments.push(comment.clone());
                }
                self.next_comment += 1;
                let mut added = self.added();
                added.push(J::obj(vec![("issue", issue_number), ("body", J::s(&body))]));
                if let Err(err) = std::fs::write(&self.out, J::Arr(added).pretty()) {
                    return failure(format!("Couldn't record the comment: {err}"));
                }
                text(J::obj(vec![("ok", J::Bool(true)), ("comment", comment)]))
            }
            "create_issue" => {
                let next = self.issues.iter().filter_map(|i| i.get("number").and_then(J::int)).max().unwrap_or(0) + 1;
                let labels = args.get("labels").map(J::from_value).unwrap_or(J::Arr(Vec::new()));
                let mut issue = J::obj(vec![
                    ("number", J::n(next)),
                    ("title", J::s(&string("title").unwrap_or_default())),
                    ("state", J::s("open")),
                    ("author", J::s("you")),
                    ("body", J::s(&string("body").unwrap_or_default())),
                    ("labels", labels),
                ]);
                if let Some(assignee) = string("assignee") {
                    issue.set("assignee", J::s(&assignee));
                }
                issue.set("comments", J::Arr(Vec::new()));
                let summary = Self::summary(&issue);
                self.issues.push(issue);
                text(summary)
            }
            "close_issue" | "reopen_issue" => {
                let state = if name == "close_issue" { "closed" } else { "open" };
                let Some(issue) = self.find(number) else { return failure(format!("No issue #{}.", shown(number))) };
                issue.set("state", J::s(state));
                text(Self::summary(issue))
            }
            "assign_issue" => {
                let login = string("login");
                let Some(issue) = self.find(number) else { return failure(format!("No issue #{}.", shown(number))) };
                match login {
                    Some(login) => issue.set("assignee", J::s(&login)),
                    // `issue.assignee = undefined`: the key stays, JSON.stringify leaves it out.
                    None => issue.remove("assignee"),
                }
                text(Self::summary(issue))
            }
            "add_label" | "remove_label" => {
                let label = string("label").unwrap_or_default();
                let Some(issue) = self.find(number) else { return failure(format!("No issue #{}.", shown(number))) };
                let mut labels = issue.get("labels").map(|l| l.items().to_vec()).unwrap_or_default();
                if name == "add_label" {
                    if !labels.iter().any(|l| l.str() == Some(&label)) {
                        labels.push(J::s(&label));
                    }
                } else {
                    labels.retain(|l| l.str() != Some(&label));
                }
                issue.set("labels", J::Arr(labels));
                text(Self::summary(issue))
            }
            "list_labels" => text(J::Arr(self.data_list("labels"))),
            "get_user" => {
                let login = string("login");
                match self.data_list("users").into_iter().find(|u| u.get("login").and_then(J::str) == login.as_deref()) {
                    Some(user) => text(user),
                    None => failure(format!("No user \"{}\".", login.unwrap_or_else(|| "undefined".into()))),
                }
            }
            "list_users" => {
                let role = string("role");
                text(J::Arr(
                    self.data_list("users")
                        .into_iter()
                        .filter(|u| role.is_none() || u.get("role").and_then(J::str) == role.as_deref())
                        .collect(),
                ))
            }
            "list_milestones" => {
                let mut milestones = self.data_list("milestones");
                milestones.sort_by(|a, b| a.get("due").and_then(J::str).cmp(&b.get("due").and_then(J::str)));
                text(J::Arr(milestones))
            }
            "get_milestone" => {
                let title = string("title");
                let Some(mut milestone) =
                    self.data_list("milestones").into_iter().find(|m| m.get("title").and_then(J::str) == title.as_deref())
                else {
                    return failure(format!("No milestone \"{}\".", title.unwrap_or_else(|| "undefined".into())));
                };
                let issues = self
                    .issues
                    .iter()
                    .filter(|i| i.get("milestone").and_then(J::str) == title.as_deref())
                    .map(Self::summary)
                    .collect();
                milestone.set("issues", J::Arr(issues));
                text(milestone)
            }
            "link_pull_request" => {
                let pull_request = int("pull_request");
                if self.find(number).is_none() {
                    return failure(format!("No issue #{}.", shown(number)));
                }
                let n = |v: Option<i64>| v.map_or(J::Null, J::n);
                text(J::obj(vec![("ok", J::Bool(true)), ("issue", n(number)), ("pull_request", n(pull_request))]))
            }
            "list_comments" => {
                let since = int("since").unwrap_or(0);
                let limit = int("limit").unwrap_or(50).max(0) as usize;
                let Some(issue) = self.find(number) else { return failure(format!("No issue #{}.", shown(number))) };
                let comments = issue.get("comments").map(J::items).unwrap_or_default();
                text(J::Arr(
                    comments.iter().filter(|c| c.get("id").and_then(J::int).unwrap_or(0) > since).take(limit).cloned().collect(),
                ))
            }
            "edit_comment" => failure(format!("Comment {} isn't yours to edit.", shown(int("comment_id")))),
            "delete_comment" => failure(format!("Comment {} isn't yours to delete.", shown(int("comment_id")))),
            "get_project" => {
                let mut project = self.data.get("project").cloned().unwrap_or(J::Obj(Vec::new()));
                let count =
                    |state: &str| self.issues.iter().filter(|i| i.get("state").and_then(J::str) == Some(state)).count() as i64;
                project.set("open", J::n(count("open")));
                project.set("closed", J::n(count("closed")));
                text(project)
            }
            "list_projects" => {
                let project = self.data.get("project");
                let field = |k: &str| project.and_then(|p| p.get(k)).cloned().unwrap_or(J::Null);
                text(J::Arr(vec![J::obj(vec![("key", field("key")), ("name", field("name"))])]))
            }
            "subscribe" | "unsubscribe" => {
                let n = number.unwrap_or(0);
                if name == "subscribe" {
                    if !self.subscriptions.contains(&n) {
                        self.subscriptions.push(n);
                    }
                } else {
                    self.subscriptions.retain(|s| *s != n);
                }
                let key = if name == "subscribe" { "subscribed" } else { "unsubscribed" };
                text(J::obj(vec![("ok", J::Bool(true)), (key, number.map_or(J::Null, J::n))]))
            }
            "get_activity" => {
                let limit = int("limit").unwrap_or(20).max(0) as usize;
                let events: Vec<J> = self
                    .issues
                    .iter()
                    .flat_map(|i| {
                        let number = i.get("number").cloned().unwrap_or(J::Null);
                        let event = if i.get("state").and_then(J::str) == Some("closed") { "closed" } else { "opened" };
                        let by = i.get("author").cloned().unwrap_or(J::Null);
                        let first = J::obj(vec![("event", J::s(event)), ("issue", number.clone()), ("by", by)]);
                        let comments = i.get("comments").map(J::items).unwrap_or_default().iter().map(move |c| {
                            J::obj(vec![
                                ("event", J::s("commented")),
                                ("issue", number.clone()),
                                ("by", c.get("author").cloned().unwrap_or(J::Null)),
                            ])
                        });
                        std::iter::once(first).chain(comments.collect::<Vec<_>>())
                    })
                    .take(limit)
                    .collect();
                text(J::Arr(events))
            }
            "export_issues" => {
                if string("format").as_deref() == Some("json") {
                    let issues = self
                        .issues
                        .iter()
                        .map(|i| {
                            let mut i = i.clone();
                            i.remove("comments");
                            i
                        })
                        .collect();
                    return text(J::Arr(issues));
                }
                let known = ["number", "title", "state"];
                let wanted: Vec<&str> = args
                    .get("fields")
                    .and_then(Value::as_array)
                    .map(|f| f.iter().filter_map(Value::as_str).filter(|f| known.contains(f)).collect())
                    .unwrap_or_default();
                let columns = if wanted.is_empty() { known.to_vec() } else { wanted };
                let cell = |i: &J, c: &str| match c {
                    "number" => i.get("number").and_then(J::int).map_or(String::new(), |n| n.to_string()),
                    "title" => format!("\"{}\"", i.get("title").and_then(J::str).unwrap_or("").replace('"', "\"\"")),
                    _ => i.get("state").and_then(J::str).unwrap_or("").to_string(),
                };
                let mut lines = vec![columns.join(",")];
                lines.extend(self.issues.iter().map(|i| columns.iter().map(|c| cell(i, c)).collect::<Vec<_>>().join(",")));
                text(J::Str(lines.join("\n")))
            }
            _ => failure(format!("Tool {name} not found")),
        }
    }
}

/// Answers one JSON-RPC message (one line of JSON); `None` for a notification.
pub fn handle(tracker: &mut Tracker, message: &Value) -> Option<String> {
    let id = message.get("id")?.clone();
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => json!({
            "protocolVersion": params.get("protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
            "capabilities": {"tools": {"listChanged": true}},
            "serverInfo": {"name": "tracker", "version": "1.0.0"},
        }),
        "ping" => json!({}),
        // Spliced in as text: through a `Value` its schemas' keys would come out sorted.
        "tools/list" => return Some(format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":{}}}}}"#, TOOLS.trim())),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            let (text, is_error) = tracker.call(name, &args);
            let mut result = json!({"content": [{"type": "text", "text": text}]});
            if is_error {
                result["isError"] = json!(true);
            }
            result
        }
        _ => {
            return Some(
                json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": "Method not found"}}).to_string(),
            );
        }
    };
    Some(json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string())
}

/// `marv-oracle tracker <data.json> <out.json>`: serves MCP over stdio until stdin closes.
pub fn serve(data_file: &Path, out: &Path) -> anyhow::Result<()> {
    let mut tracker = Tracker::load(data_file, out)?;
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(message) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(reply) = handle(&mut tracker, &message) {
            writeln!(stdout, "{reply}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(name: &str) -> (Tracker, PathBuf) {
        let out = std::env::temp_dir().join(format!("marv-oracle-tracker-{name}-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/oracle/mcp/tracker.json");
        (Tracker::load(&data, &out).unwrap(), out)
    }

    #[test]
    fn the_specs_are_the_typescript_trackers_25() {
        let tools: Vec<Value> = serde_json::from_str(TOOLS).unwrap();
        assert_eq!(tools.len(), 25);
        assert_eq!(tools[0]["name"], "get_issue");
        assert_eq!(tools[2]["annotations"]["readOnlyHint"], true, "add_comment is marked read-only on purpose");
    }

    #[test]
    fn get_issue_is_json_stringify_in_the_datas_key_order() {
        let (mut t, _) = tracker("get");
        let (text, error) = t.call("get_issue", &json!({"number": 12}));
        assert!(!error);
        // What the TypeScript tracker answered (captured): keys in the data's order, comments with id and issue first.
        assert!(text.starts_with("{\n  \"number\": 12,\n  \"title\": \"range() with a negative step includes the end\",\n  \"state\": \"open\",\n  \"author\": \"lee\",\n  \"assignee\": \"you\",\n  \"labels\": [\n    \"bug\",\n    \"good first issue\"\n  ],"), "{text}");
        assert!(text.ends_with("\"comments\": [\n    {\n      \"id\": 4,\n      \"issue\": 12,\n      \"author\": \"dana\",\n      \"body\": \"Confirmed. Positive steps are fine.\"\n    }\n  ]\n}"), "{text}");
        assert_eq!(t.call("get_issue", &json!({"number": 99})), ("No issue #99.".to_string(), true));
    }

    #[test]
    fn fields_narrow_read_answers() {
        let (mut t, _) = tracker("fields");
        let (text, _) = t.call("list_issues", &json!({"fields": ["number"]}));
        assert_eq!(
            text,
            "[\n  {\n    \"number\": 14\n  },\n  {\n    \"number\": 12\n  },\n  {\n    \"number\": 11\n  },\n  {\n    \"number\": 7\n  }\n]"
        );
    }

    #[test]
    fn add_comment_appends_to_the_out_file() {
        let (mut t, out) = tracker("comment");
        let (text, error) = t.call("add_comment", &json!({"number": 7, "body": "Done: `--json`."}));
        assert!(!error && text.contains("\"id\": 5"), "{text}");
        t.call("add_comment", &json!({"number": 12, "body": "Fixed."}));
        let written = std::fs::read_to_string(&out).unwrap();
        std::fs::remove_file(&out).unwrap();
        assert_eq!(
            written,
            "[\n  {\n    \"issue\": 7,\n    \"body\": \"Done: `--json`.\"\n  },\n  {\n    \"issue\": 12,\n    \"body\": \"Fixed.\"\n  }\n]"
        );
    }

    #[test]
    fn json_rpc_round_trip() {
        let (mut t, _) = tracker("rpc");
        let mut handle = |m: Value| handle(&mut t, &m).map(|line| serde_json::from_str::<Value>(&line).unwrap());
        let init =
            handle(json!({"jsonrpc": "2.0", "id": 0, "method": "initialize", "params": {"protocolVersion": "2025-03-26"}}))
                .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert!(handle(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})).is_none());
        let list = handle(json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 25);
        // The specs go out as the TypeScript tracker sent them, keys unsorted.
        let raw = handle_raw_tools();
        assert!(raw.contains(r#""inputSchema":{"$schema":"http://json-schema.org/draft-07/schema#","type":"object""#), "{raw}");
        let call = handle(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/call", "params": {"name": "get_issue", "arguments": {"number": 99}}}))
            .unwrap();
        assert_eq!(call["result"], json!({"content": [{"type": "text", "text": "No issue #99."}], "isError": true}));
        let missing = handle(json!({"jsonrpc": "2.0", "id": 3, "method": "resources/list"})).unwrap();
        assert_eq!(missing["error"]["code"], -32601);
    }

    fn handle_raw_tools() -> String {
        let (mut t, _) = tracker("raw");
        handle(&mut t, &json!({"jsonrpc": "2.0", "id": 1, "method": "tools/list"})).unwrap()
    }

    #[test]
    fn ordered_json_keeps_insertion_order() {
        let j: J = serde_json::from_str(r#"{"b":1,"a":[true,null,"x"],"c":{}}"#).unwrap();
        assert_eq!(j.pretty(), "{\n  \"b\": 1,\n  \"a\": [\n    true,\n    null,\n    \"x\"\n  ],\n  \"c\": {}\n}");
    }
}
