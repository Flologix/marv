//! Numbers from trajectory logs, to see whether a change (a model, a prompt, a Marv version) made runs better: how
//! turns ended, what they cost, and how the user rated them. Ports Marv's `src/trajectory-stats.ts` (`bun run stats`;
//! in marv-rs a future `marv stats`).
//!
//! An explicit rating (`/good`, `/bad`) wins over what the next message implied; the latest explicit one counts.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::usage::tokens;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GroupStats {
    pub turns: usize,
    /// How the main agent's runs ended: end, declined, aborted, error, max_steps…
    pub ended: BTreeMap<String, usize>,
    /// All requests in the turn, subagents' included.
    pub tokens_per_turn: u64,
    /// The main agent's tool calls (a subagent counts as one).
    pub tools_per_turn: f64,
    pub median_seconds: f64,
    pub rated: usize,
    pub good: usize,
    pub bad: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupBy {
    Model,
    Marv,
}

#[derive(Default)]
struct Turn {
    group: String,
    tokens: u64,
    end: Option<(String, f64, f64)>, // reason, tools, ms
    explicit: Option<i64>,
    implicit: Option<i64>,
}

/// Groups turns by model (as of the turn) or by Marv version (as of its session), in first-seen order.
pub fn summarize(records: &[Value], by: GroupBy) -> Vec<(String, GroupStats)> {
    let mut marv_of: BTreeMap<String, String> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    let mut turns: BTreeMap<String, Turn> = BTreeMap::new();
    let s = |r: &Value, k: &str| r.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
    for r in records {
        let kind = s(r, "type");
        if kind == "session" {
            marv_of.insert(s(r, "session"), s(r, "marv"));
            continue;
        }
        if kind == "turn_start" {
            let group = match by {
                GroupBy::Model => s(r, "model"),
                GroupBy::Marv => marv_of.get(&s(r, "session")).cloned().unwrap_or_else(|| "unknown".into()),
            };
            let id = s(r, "turn");
            if !turns.contains_key(&id) {
                order.push(id.clone());
            }
            turns.insert(id, Turn { group, ..Turn::default() });
            continue;
        }
        let Some(t) = turns.get_mut(&s(r, "turn")) else {
            continue;
        };
        let num = |v: &Value, k: &str| v.get(k).and_then(Value::as_f64).unwrap_or(0.0);
        match kind.as_str() {
            "request" => t.tokens += (num(&r["usage"], "promptTokens") + num(&r["usage"], "completionTokens")) as u64,
            "agent_end" if s(r, "agent") == "main" => t.end = Some((s(r, "reason"), num(r, "tools"), num(r, "ms"))),
            "feedback" => {
                let score = r.get("score").and_then(Value::as_i64).unwrap_or(0);
                if score != 0 {
                    if s(r, "source") == "explicit" { t.explicit = Some(score) } else { t.implicit = Some(score) }
                }
            }
            _ => {}
        }
    }
    let mut groups: Vec<(String, Vec<&Turn>)> = Vec::new();
    for id in &order {
        let t = &turns[id];
        match groups.iter_mut().find(|(g, _)| *g == t.group) {
            Some((_, list)) => list.push(t),
            None => groups.push((t.group.clone(), vec![t])),
        }
    }
    groups
        .into_iter()
        .map(|(group, list)| {
            let mut ended = BTreeMap::new();
            for t in &list {
                if let Some((reason, _, _)) = &t.end {
                    *ended.entry(reason.clone()).or_insert(0) += 1;
                }
            }
            let scores: Vec<i64> = list.iter().filter_map(|t| t.explicit.or(t.implicit)).collect();
            let finished: Vec<_> = list.iter().filter_map(|t| t.end.as_ref()).collect();
            let total: u64 = list.iter().map(|t| t.tokens).sum();
            let stats = GroupStats {
                turns: list.len(),
                ended,
                tokens_per_turn: (total as f64 / list.len() as f64).round() as u64,
                tools_per_turn: if finished.is_empty() {
                    0.0
                } else {
                    finished.iter().map(|e| e.1).sum::<f64>() / finished.len() as f64
                },
                median_seconds: median(finished.iter().map(|e| e.2 / 1000.0).collect()),
                rated: scores.len(),
                good: scores.iter().filter(|s| **s > 0).count(),
                bad: scores.iter().filter(|s| **s < 0).count(),
            };
            (group, stats)
        })
        .collect()
}

/// The mesh radio's traffic in a trajectory (marv-rs #124): messages per kind each way, refusals, and the turns the
/// radio started.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RadioStats {
    pub sent: BTreeMap<String, usize>,
    pub received: BTreeMap<String, usize>,
    /// Sent or received, refused (by this Marv or, sent, by its own limits).
    pub refused: usize,
    pub wakes: usize,
}

/// Counts the radio's records: `mesh` records with `radio: true`, and `turn_start`s with a `radioWake`.
pub fn radio_stats(records: &[Value]) -> RadioStats {
    let mut stats = RadioStats::default();
    for r in records {
        match r.get("type").and_then(Value::as_str) {
            Some("mesh") if r.get("radio").and_then(Value::as_bool) == Some(true) => {
                let kind = r.get("kind").and_then(Value::as_str).unwrap_or("unknown").to_string();
                match r.get("direction").and_then(Value::as_str) {
                    Some("sent") => *stats.sent.entry(kind).or_insert(0) += 1,
                    _ => *stats.received.entry(kind).or_insert(0) += 1,
                }
                if r.get("outcome").and_then(Value::as_str) == Some("refused") {
                    stats.refused += 1;
                }
            }
            Some("turn_start") if r.get("radioWake").is_some_and(|w| !w.is_null()) => stats.wakes += 1,
            _ => {}
        }
    }
    stats
}

/// #63's eval metric: what a compaction made the model do again.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepeatedReads {
    /// `compact` records.
    pub compactions: usize,
    /// The main agent's `read_file` calls, within [`REPEAT_WINDOW`] steps after a compaction, of a path (and an
    /// overlapping range) it had already read before it.
    pub repeated: usize,
}

/// #63: the steps after a compaction in which a read counts as repeated.
pub const REPEAT_WINDOW: usize = 10;

/// The lines a `read_file` call covers: `offset` (1-based, default 1) and `limit` (default: to the end).
fn read_range(args: &Value) -> (u64, u64) {
    let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(1).max(1);
    let end = args.get("limit").and_then(Value::as_u64).map_or(u64::MAX, |l| offset.saturating_add(l.max(1) - 1));
    (offset, end)
}

/// #63: counts repeated reads after each compaction in a trajectory (records in file order; the main agent's only).
pub fn repeated_reads(records: &[Value]) -> RepeatedReads {
    let mut stats = RepeatedReads::default();
    let mut reads: Vec<(String, (u64, u64))> = Vec::new();
    let mut before: Vec<(String, (u64, u64))> = Vec::new();
    // Steps left in the window after the last compaction (`request` records of the main agent).
    let mut window: Option<usize> = None;
    let main = |r: &Value| r.get("agent").and_then(Value::as_str).is_none_or(|a| a == "main");
    for r in records {
        match r.get("type").and_then(Value::as_str) {
            Some("compact") => {
                stats.compactions += 1;
                before = reads.clone();
                window = Some(REPEAT_WINDOW);
            }
            Some("request") if main(r) => {
                window = window.and_then(|w| w.checked_sub(1)).filter(|w| *w > 0);
            }
            Some("tool") if main(r) && r["call"]["name"] == "read_file" => {
                let args: Value = r["call"]["arguments"].as_str().and_then(|a| serde_json::from_str(a).ok()).unwrap_or_default();
                let Some(path) = args.get("path").and_then(Value::as_str) else { continue };
                let range = read_range(&args);
                if window.is_some() && before.iter().any(|(p, (start, end))| p == path && range.0 <= *end && *start <= range.1) {
                    stats.repeated += 1;
                }
                reads.push((path.to_string(), range));
            }
            _ => {}
        }
    }
    stats
}

fn median(mut values: Vec<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(f64::total_cmp);
    let mid = values.len() / 2;
    if values.len() % 2 == 1 { values[mid] } else { (values[mid - 1] + values[mid]) / 2.0 }
}

/// A plain-text table, one row per group.
pub fn format_stats(stats: &[(String, GroupStats)]) -> String {
    let header: Vec<String> =
        ["", "turns", "good", "rated", "tokens/turn", "tools/turn", "median", "ended"].map(String::from).to_vec();
    let rows: Vec<Vec<String>> = stats
        .iter()
        .map(|(group, s)| {
            vec![
                group.clone(),
                s.turns.to_string(),
                if s.rated > 0 { format!("{}%", (100 * s.good * 2 + s.rated) / (2 * s.rated)) } else { "-".into() },
                format!("{} ({}+ {}-)", s.rated, s.good, s.bad),
                tokens(s.tokens_per_turn),
                format!("{:.1}", s.tools_per_turn),
                format!("{:.1}s", s.median_seconds),
                s.ended.iter().map(|(r, n)| format!("{r} {n}")).collect::<Vec<_>>().join(", "),
            ]
        })
        .collect();
    let all: Vec<&Vec<String>> = std::iter::once(&header).chain(rows.iter()).collect();
    let widths: Vec<usize> = (0..header.len()).map(|i| all.iter().map(|r| r[i].chars().count()).max().unwrap_or(0)).collect();
    all.iter()
        .map(|row| {
            row.iter()
                .enumerate()
                .map(|(i, c)| format!("{c:<w$}", w = widths[i]))
                .collect::<Vec<_>>()
                .join("  ")
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn groups_turns_and_prefers_explicit_ratings() {
        let records = vec![
            json!({"type": "session", "session": "s", "marv": "0.3.0"}),
            json!({"type": "turn_start", "session": "s", "turn": "a", "model": "m1"}),
            json!({"type": "request", "turn": "a", "usage": {"promptTokens": 100, "completionTokens": 20}}),
            json!({"type": "agent_end", "turn": "a", "agent": "main", "reason": "end", "tools": 2, "ms": 3000}),
            json!({"type": "feedback", "turn": "a", "score": -1, "source": "implicit"}),
            json!({"type": "feedback", "turn": "a", "score": 1, "source": "explicit"}),
            json!({"type": "turn_start", "session": "s", "turn": "b", "model": "m1"}),
            json!({"type": "agent_end", "turn": "b", "agent": "main", "reason": "max_steps", "tools": 4, "ms": 5000}),
        ];
        let stats = summarize(&records, GroupBy::Model);
        let (group, s) = &stats[0];
        assert_eq!(group, "m1");
        assert_eq!((s.turns, s.rated, s.good, s.bad, s.tokens_per_turn), (2, 1, 1, 0, 60));
        assert_eq!(s.tools_per_turn, 3.0);
        assert_eq!(s.median_seconds, 4.0);
        assert_eq!(summarize(&records, GroupBy::Marv)[0].0, "0.3.0");
        assert!(format_stats(&stats).contains("end 1, max_steps 1"));
    }

    fn read(path: &str, args: Value) -> Value {
        let mut args = args;
        args["path"] = path.into();
        json!({"type": "tool", "turn": "a", "agent": "main", "call": {"id": "c", "name": "read_file", "arguments": args.to_string()}})
    }

    #[test]
    fn repeated_reads_count_reads_of_what_was_read_before_a_compaction_within_ten_steps() {
        let request = json!({"type": "request", "turn": "a", "agent": "main"});
        let mut records = vec![
            read("a.rs", json!({})),
            read("b.rs", json!({"offset": 1, "limit": 10})),
            json!({"type": "compact", "turn": "a"}),
            request.clone(),
            read("a.rs", json!({})),                          // repeated
            read("b.rs", json!({"offset": 50, "limit": 10})), // another range
            read("c.rs", json!({})),                          // new
            json!({"type": "tool", "turn": "a", "agent": "a.1", "call": {"name": "read_file", "arguments": "{\"path\":\"a.rs\"}"}}),
        ];
        records.extend((0..10).map(|_| request.clone()));
        records.push(read("a.rs", json!({}))); // past the window
        assert_eq!(repeated_reads(&records), RepeatedReads { compactions: 1, repeated: 1 });
    }

    #[test]
    fn radio_traffic_is_counted_per_kind_with_refusals_and_wakes() {
        let records = vec![
            json!({"type": "mesh", "radio": true, "direction": "sent", "kind": "msg", "outcome": "delivered"}),
            json!({"type": "mesh", "radio": true, "direction": "sent", "kind": "msg", "outcome": "refused", "reason": "rate"}),
            json!({"type": "mesh", "radio": true, "direction": "received", "kind": "msg", "outcome": "delivered"}),
            json!({"type": "mesh", "sender": "worker-1", "outcome": "delivered"}),
            json!({"type": "turn_start", "turn": "a", "radioWake": {"id": "b:1", "from": "b", "hop": 1}}),
            json!({"type": "turn_start", "turn": "b"}),
        ];
        let stats = radio_stats(&records);
        assert_eq!((stats.sent["msg"], stats.received["msg"], stats.refused, stats.wakes), (2, 1, 1, 1));
    }
}
