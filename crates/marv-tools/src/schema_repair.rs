//! Schema-guided argument repair (Marv #65): the generic mistakes weaker and local models make against a tool's
//! JSON schema, fixed before the arguments are parsed, where the schema leaves exactly one reading. Anything else is
//! left as it came, so it fails with the ordinary error.
//!
//! New in marv-rs (Marv's `repair.ts` planned it; opencode does the same before running a tool). Seen in the
//! GLM-5.3-flash bench (#73): `get_issue` called with `{"number": "12"}` (the server answered "No issue #undefined"),
//! `grep` with another harness's `head_limit` (refused as an unknown field), `add_comment` without its required
//! `body` (posted an empty comment).
//!
//! [`repair_args`] walks the schema (`type`, also a list of types, `properties`, `required`, `items`,
//! `additionalProperties`) and repairs:
//! - a string where the schema wants a number, integer or boolean, when the whole string is one (`"20"`, `"true"`);
//!   `"20abc"`, `" 20"` and `"1.5"` for an integer are left;
//! - a JSON string where it wants an object or array (`"{\"a\":1}"`);
//! - a lone value where it wants an array, when the value fits the items (`"a.ts"` for `["a.ts"]`);
//! - [`RepairMode::Builtin`] only: `null` for an optional field (left out) and fields the schema doesn't name
//!   (dropped, unless `additionalProperties` allows them).
//!
//! A schema with `anyOf`, `oneOf` or `allOf`, or without a `type`, isn't read: what it wants isn't one thing. No
//! full validator: the tool's own parse (serde) or the MCP server still decides.
//!
//! MCP tools ([`RepairMode::Mcp`]) get only the type coercions: a server may accept keys its schema doesn't list, and
//! `null` may mean something to it, so unknown keys are dropped only under `additionalProperties: false`. Before an
//! MCP server is called, [`check_required`] refuses a missing required field or a string under its `minLength`: the
//! server may not check (the bench's empty comment was posted).
//!
//! A built-in tool's arguments that its parse already accepts aren't repaired at all (`registry::schema_repaired`):
//! `null` for an `Option` field works, and a note there would be noise.
//!
//! Prompt cache: this repairs the value `run_tool` parses, never the `ToolCall` in the history.

use serde_json::{Map, Value};

/// How far a tool's arguments are repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RepairMode {
    /// Marv's own tools: their schema is exactly what they accept (serde refuses unknown fields).
    #[default]
    Builtin,
    /// An MCP server's tool: type coercions only; unknown keys dropped only under `additionalProperties: false`.
    Mcp,
}

/// `args` repaired against `schema`, and what was repaired (one phrase per repair; [`repair_note`] makes the note).
/// Valid arguments come back unchanged, with nothing listed.
pub fn repair_args(schema: &Value, args: Value, mode: RepairMode) -> (Value, Vec<String>) {
    let mut fixes = Vec::new();
    let value = repair_value(schema, args, "", mode, &mut fixes);
    (value, fixes)
}

/// The `Marv:` note for the repairs [`repair_args`] listed; None when there were none.
pub fn repair_note(fixes: &[String]) -> Option<String> {
    if fixes.is_empty() {
        return None;
    }
    Some(format!(
        "Marv: the arguments didn't match the tool's schema and were repaired before the call: {}. Send them as the schema says next time.",
        fixes.join("; ")
    ))
}

/// Before an MCP server is called: every required field present (and not null), every string at least its
/// `minLength`, in the arguments and in the objects inside them. The error names each field.
pub fn check_required(schema: &Value, args: &Value) -> Result<(), String> {
    let mut errors = Vec::new();
    check_object(schema, args, "", &mut errors);
    if errors.is_empty() { Ok(()) } else { Err(format!("{}. The MCP server was not called.", errors.join("; "))) }
}

fn check_object(schema: &Value, value: &Value, path: &str, errors: &mut Vec<String>) {
    let Some(schema) = schema.as_object() else { return };
    // A string under its minLength, wherever it sits (a field, or a list's item).
    if let (Some(text), Some(min)) = (value.as_str(), schema.get("minLength").and_then(Value::as_u64))
        && (text.chars().count() as u64) < min
    {
        let unit = if min == 1 { "character" } else { "characters" };
        errors.push(format!("field \"{path}\" must be at least {min} {unit} long"));
    }
    match value {
        Value::Object(object) => {
            let properties = schema.get("properties").and_then(Value::as_object);
            for name in schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                // `null` is missing only where the field's schema says it can't be null: `["string", "null"]`, or a
                // field we can't read (no type, a combinator), may take it.
                let null_refused = || {
                    properties
                        .and_then(|p| p.get(name))
                        .and_then(Value::as_object)
                        .and_then(types_of)
                        .is_some_and(|ts| !ts.is_empty() && !ts.contains(&"null"))
                };
                match object.get(name) {
                    None => errors.push(format!("missing required field \"{}\"", join(path, name))),
                    Some(Value::Null) if null_refused() => {
                        errors.push(format!("missing required field \"{}\"", join(path, name)))
                    }
                    _ => {}
                }
            }
            for (name, field) in properties.into_iter().flatten() {
                if let Some(value) = object.get(name) {
                    check_object(field, value, &join(path, name), errors);
                }
            }
        }
        // A list's items are checked as well: a batch tool's `comments: [{"body": ""}]` (#65 review).
        Value::Array(items) => {
            if let Some(item_schema) = schema.get("items").filter(|s| s.is_object()) {
                for (i, item) in items.iter().enumerate() {
                    check_object(item_schema, item, &format!("{path}[{i}]"), errors);
                }
            }
        }
        _ => {}
    }
}

/// `a.b` for a field inside an object; the top level's own name.
fn join(path: &str, name: &str) -> String {
    if path.is_empty() { name.to_string() } else { format!("{path}.{name}") }
}

/// How a repair names a value's place: "limit", "edits[0].old", or the whole call.
fn place(path: &str) -> String {
    if path.is_empty() { "the whole call".into() } else { format!("\"{path}\"") }
}

/// The schema's types, or None when it doesn't say one thing (no `type`, or `anyOf`/`oneOf`/`allOf`).
fn types_of(schema: &Map<String, Value>) -> Option<Vec<&str>> {
    if ["anyOf", "oneOf", "allOf"].iter().any(|k| schema.contains_key(*k)) {
        return None;
    }
    match schema.get("type")? {
        Value::String(t) => Some(vec![t.as_str()]),
        Value::Array(ts) => Some(ts.iter().filter_map(Value::as_str).collect()),
        _ => None,
    }
}

/// Whether `value` already is of the JSON schema type `ty`.
fn is_type(ty: &str, value: &Value) -> bool {
    match ty {
        "string" => value.is_string(),
        "integer" => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|f| f.fract() == 0.0),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => true, // a type we don't know: not ours to judge
    }
}

fn repair_value(schema: &Value, value: Value, path: &str, mode: RepairMode, fixes: &mut Vec<String>) -> Value {
    let Some(schema) = schema.as_object() else { return value };
    let Some(types) = types_of(schema) else {
        // No single type: still walk an object's named fields (a schema with `properties` and no `type`).
        return if schema.contains_key("type") || ["anyOf", "oneOf", "allOf"].iter().any(|k| schema.contains_key(*k)) {
            value
        } else {
            descend(schema, value, path, mode, fixes)
        };
    };
    if types.is_empty() || types.iter().any(|t| is_type(t, &value)) {
        return descend(schema, value, path, mode, fixes);
    }
    let Some((repaired, how)) = coerce(schema, &types, &value) else { return value };
    fixes.push(format!("{} {how}", place(path)));
    descend(schema, repaired, path, mode, fixes)
}

/// The one value of a wanted type that `value` (of none of them) stands for, and how the repair reads; None when there
/// is no such value, or more than one.
fn coerce(schema: &Map<String, Value>, types: &[&str], value: &Value) -> Option<(Value, String)> {
    if let Value::String(text) = value {
        // A JSON parser accepts whitespace around the value; "exactly" doesn't.
        let parsed = (!text.is_empty() && text.trim() == text).then(|| serde_json::from_str::<Value>(text).ok()).flatten();
        let mut found: Vec<(Value, String)> = Vec::new();
        for ty in types {
            let candidate = match (*ty, &parsed) {
                ("integer", Some(n @ Value::Number(num))) if num.is_i64() || num.is_u64() => {
                    Some((n.clone(), format!("was the string \"{text}\": read as the integer {n}")))
                }
                ("number", Some(n @ Value::Number(_))) => {
                    Some((n.clone(), format!("was the string \"{text}\": read as the number {n}")))
                }
                ("boolean", Some(b @ Value::Bool(_))) => {
                    Some((b.clone(), format!("was the string \"{text}\": read as the boolean {b}")))
                }
                ("object", Some(o @ Value::Object(_))) => {
                    Some((o.clone(), "was an object sent as a JSON string: read as the object".into()))
                }
                ("array", Some(a @ Value::Array(_))) => {
                    Some((a.clone(), "was a list sent as a JSON string: read as the list".into()))
                }
                _ => None,
            };
            if let Some(candidate) = candidate
                && !found.iter().any(|(v, _)| *v == candidate.0)
            {
                found.push(candidate);
            }
        }
        match found.len() {
            1 => return found.pop(),
            0 => {}
            _ => return None, // two readings: not ours to choose
        }
    }
    // A lone value where a list is wanted, when it is what the list holds (never null: that's "no value").
    if types.contains(&"array") && !value.is_null() && !value.is_array() {
        let fits = match schema.get("items").and_then(Value::as_object) {
            None => true,
            Some(items) => match types_of(items) {
                Some(item_types) => item_types.is_empty() || item_types.iter().any(|t| is_type(t, value)),
                // `items: {}` takes anything; a combinator doesn't say.
                None => items.is_empty(),
            },
        };
        if fits {
            return Some((Value::Array(vec![value.clone()]), "was a single value: read as a one-item list".into()));
        }
    }
    None
}

/// Repairs inside an object's fields and an array's items.
fn descend(schema: &Map<String, Value>, value: Value, path: &str, mode: RepairMode, fixes: &mut Vec<String>) -> Value {
    match value {
        Value::Object(object) => {
            let Some(properties) = schema.get("properties").and_then(Value::as_object) else {
                return Value::Object(object);
            };
            let required: Vec<&str> =
                schema.get("required").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
            // Built-in schemas are exactly what serde accepts: absent means closed. An MCP schema is open unless it says.
            // `patternProperties` names keys by pattern: a key outside `properties` may still be valid, so none is dropped.
            let closed = !schema.contains_key("patternProperties")
                && matches!(
                    (mode, schema.get("additionalProperties")),
                    (_, Some(Value::Bool(false))) | (RepairMode::Builtin, None)
                );
            let mut out = Map::new();
            for (name, field) in object {
                let at = join(path, &name);
                let Some(field_schema) = properties.get(&name) else {
                    if closed {
                        fixes.push(format!("unknown field \"{at}\" was dropped"));
                    } else {
                        out.insert(name, field);
                    }
                    continue;
                };
                if field.is_null()
                    && mode == RepairMode::Builtin
                    && !required.contains(&name.as_str())
                    && field_schema.as_object().and_then(types_of).is_some_and(|ts| !ts.is_empty() && !ts.contains(&"null"))
                {
                    fixes.push(format!("\"{at}\" was null: left out (it's optional)"));
                    continue;
                }
                let repaired = repair_value(field_schema, field, &at, mode, fixes);
                out.insert(name, repaired);
            }
            Value::Object(out)
        }
        Value::Array(items) => {
            let Some(item_schema) = schema.get("items").filter(|s| s.is_object()) else { return Value::Array(items) };
            Value::Array(
                items
                    .into_iter()
                    .enumerate()
                    .map(|(i, item)| repair_value(item_schema, item, &format!("{path}[{i}]"), mode, fixes))
                    .collect(),
            )
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A built-in tool's schema in the shape Marv writes them.
    fn grep_like() -> Value {
        json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string"},
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "replace_all": {"type": "boolean"},
                "paths": {"type": "array", "items": {"type": "string"}},
                "options": {"type": "object", "properties": {"depth": {"type": "integer"}}, "additionalProperties": false},
                "edits": {"type": "array", "items": {"type": "object", "properties": {"old": {"type": "string"}, "count": {"type": "integer"}}, "required": ["old"], "additionalProperties": false}},
                "maybe": {"type": ["string", "null"]},
                "either": {"anyOf": [{"type": "integer"}, {"type": "string"}]}
            },
            "required": ["pattern"],
            "additionalProperties": false
        })
    }

    #[test]
    fn each_unambiguous_mistake_is_repaired_with_a_phrase() {
        let cases: Vec<(Value, Value, &str)> = vec![
            (
                json!({"pattern": "x", "limit": "20"}),
                json!({"pattern": "x", "limit": 20}),
                "\"limit\" was the string \"20\": read as the integer 20",
            ),
            (json!({"pattern": "x", "limit": "-3"}), json!({"pattern": "x", "limit": -3}), "read as the integer -3"),
            (json!({"pattern": "x", "ratio": "1.5"}), json!({"pattern": "x", "ratio": 1.5}), "read as the number 1.5"),
            (
                json!({"pattern": "x", "replace_all": "true"}),
                json!({"pattern": "x", "replace_all": true}),
                "\"replace_all\" was the string \"true\": read as the boolean true",
            ),
            (
                json!({"pattern": "x", "replace_all": "false"}),
                json!({"pattern": "x", "replace_all": false}),
                "read as the boolean false",
            ),
            (
                json!({"pattern": "x", "options": "{\"depth\":2}"}),
                json!({"pattern": "x", "options": {"depth": 2}}),
                "\"options\" was an object sent as a JSON string",
            ),
            (
                json!({"pattern": "x", "paths": "[\"a\",\"b\"]"}),
                json!({"pattern": "x", "paths": ["a", "b"]}),
                "\"paths\" was a list sent as a JSON string",
            ),
            (
                json!({"pattern": "x", "paths": "a.ts"}),
                json!({"pattern": "x", "paths": ["a.ts"]}),
                "\"paths\" was a single value: read as a one-item list",
            ),
            (json!({"pattern": "x", "limit": null}), json!({"pattern": "x"}), "\"limit\" was null: left out (it's optional)"),
            (json!({"pattern": "x", "head_limit": 30}), json!({"pattern": "x"}), "unknown field \"head_limit\" was dropped"),
            // Nested: inside an object, inside a list, and a lone object wrapped and then repaired inside.
            (
                json!({"pattern": "x", "options": {"depth": "4"}}),
                json!({"pattern": "x", "options": {"depth": 4}}),
                "\"options.depth\" was the string \"4\"",
            ),
            (
                json!({"pattern": "x", "edits": [{"old": "a", "count": "2"}]}),
                json!({"pattern": "x", "edits": [{"old": "a", "count": 2}]}),
                "\"edits[0].count\" was the string \"2\"",
            ),
            (
                json!({"pattern": "x", "edits": {"old": "a", "extra": 1}}),
                json!({"pattern": "x", "edits": [{"old": "a"}]}),
                "unknown field \"edits[0].extra\" was dropped",
            ),
            // The whole arguments sent as a JSON string.
            (json!("{\"pattern\":\"x\"}"), json!({"pattern": "x"}), "the whole call was an object sent as a JSON string"),
        ];
        for (input, want, phrase) in cases {
            let (got, fixes) = repair_args(&grep_like(), input.clone(), RepairMode::Builtin);
            assert_eq!(got, want, "repairing {input}");
            assert!(fixes.iter().any(|f| f.contains(phrase)), "{input}: {fixes:?} lacks {phrase:?}");
        }
    }

    #[test]
    fn valid_arguments_are_left_alone_with_no_note() {
        for input in [
            json!({"pattern": "x"}),
            json!({"pattern": "20", "limit": 20, "ratio": 2, "replace_all": false, "paths": ["a"], "options": {"depth": 1}}),
            json!({"pattern": "x", "edits": [{"old": "a", "count": 1}], "maybe": null, "either": "5"}),
        ] {
            let (got, fixes) = repair_args(&grep_like(), input.clone(), RepairMode::Builtin);
            assert_eq!(got, input);
            assert!(fixes.is_empty(), "{input}: {fixes:?}");
            assert_eq!(repair_note(&fixes), None);
        }
    }

    #[test]
    fn ambiguous_values_are_left_for_the_ordinary_error() {
        for input in [
            json!({"pattern": "x", "limit": "20abc"}),
            json!({"pattern": "x", "limit": " 20"}),
            json!({"pattern": "x", "limit": "1.5"}), // a number, not an integer
            json!({"pattern": "x", "limit": ""}),
            json!({"pattern": "x", "replace_all": "yes"}),
            json!({"pattern": "x", "replace_all": "True"}),
            json!({"pattern": "x", "options": "{depth: 2}"}),
            json!({"pattern": "x", "options": "[1]"}), // a list is not the object wanted
            json!({"pattern": "x", "paths": 5}),       // not what the list holds
            json!({"pattern": "x", "either": 5.5}),    // anyOf: not read
            json!({"pattern": 5}),                     // a number where a string is wanted: not one of ours
        ] {
            let (got, fixes) = repair_args(&grep_like(), input.clone(), RepairMode::Builtin);
            assert_eq!(got, input);
            assert!(fixes.is_empty(), "{input}: {fixes:?}");
        }
        // Two readings of one string (an integer or a boolean can't both parse, but a list or an object type list can).
        let either = json!({"type": "object", "properties": {"v": {"type": ["integer", "number"]}}});
        assert_eq!(repair_args(&either, json!({"v": "3"}), RepairMode::Builtin).0, json!({"v": 3})); // same value: one
        // A required field's null stays (serde names it), and a nullable one is valid.
        let (got, fixes) = repair_args(&grep_like(), json!({"pattern": null}), RepairMode::Builtin);
        assert_eq!((got, fixes.len()), (json!({"pattern": null}), 0));
    }

    #[test]
    fn mcp_mode_only_coerces_types() {
        let open = json!({
            "type": "object",
            "properties": {"number": {"type": "integer"}, "labels": {"type": "array", "items": {"type": "string"}}, "body": {"type": "string"}},
            "required": ["number"]
        });
        // #73: get_issue with {"number": "12"}.
        let (got, fixes) = repair_args(&open, json!({"number": "12"}), RepairMode::Mcp);
        assert_eq!(got, json!({"number": 12}));
        assert_eq!(fixes, vec!["\"number\" was the string \"12\": read as the integer 12".to_string()]);
        // Unknown keys and nulls pass through to the server: its schema doesn't close the object.
        let input = json!({"number": 1, "extra": true, "body": null});
        assert_eq!(repair_args(&open, input.clone(), RepairMode::Mcp), (input, vec![]));
        assert_eq!(
            repair_args(&open, json!({"number": 1, "labels": "bug"}), RepairMode::Mcp).0,
            json!({"number": 1, "labels": ["bug"]})
        );
        // additionalProperties: false closes it.
        let mut closed = open.clone();
        closed["additionalProperties"] = json!(false);
        let (got, fixes) = repair_args(&closed, json!({"number": 1, "extra": true}), RepairMode::Mcp);
        assert_eq!(got, json!({"number": 1}));
        assert_eq!(fixes, vec!["unknown field \"extra\" was dropped".to_string()]);
        // patternProperties names more keys: closed or not, nothing outside `properties` is dropped.
        closed["patternProperties"] = json!({"^label_": {"type": "string"}});
        let input = json!({"number": 1, "label_name": "x"});
        assert_eq!(repair_args(&closed, input.clone(), RepairMode::Mcp), (input, vec![]));
    }

    #[test]
    fn schemas_that_dont_say_one_thing_are_not_read() {
        let loose = json!({"type": "object"});
        assert_eq!(repair_args(&loose, json!({"a": "1"}), RepairMode::Builtin), (json!({"a": "1"}), vec![]));
        let open_args = json!({"type": "object", "properties": {"arguments": {"type": "object", "additionalProperties": {}}}});
        let input = json!({"arguments": {"anything": "1"}});
        assert_eq!(repair_args(&open_args, input.clone(), RepairMode::Builtin), (input, vec![]));
        let built_in_open = json!({"type": "object", "properties": {"a": {"type": "string"}}, "additionalProperties": true});
        let input = json!({"a": "x", "b": 1});
        assert_eq!(repair_args(&built_in_open, input.clone(), RepairMode::Builtin), (input, vec![]));
        assert_eq!(repair_args(&Value::Null, json!({"a": 1}), RepairMode::Builtin), (json!({"a": 1}), vec![]));
    }

    #[test]
    fn the_note_lists_every_repair() {
        let note = repair_note(&["a".into(), "b".into()]).unwrap();
        assert_eq!(
            note,
            "Marv: the arguments didn't match the tool's schema and were repaired before the call: a; b. Send them as the schema says next time."
        );
    }

    #[test]
    fn missing_required_fields_and_short_strings_are_refused_by_name() {
        let schema = json!({
            "type": "object",
            "properties": {
                "number": {"type": "integer"},
                "body": {"type": "string", "minLength": 1},
                "meta": {"type": "object", "properties": {"tag": {"type": "string", "minLength": 2}}, "required": ["tag"]}
            },
            "required": ["number", "body"]
        });
        assert_eq!(check_required(&schema, &json!({"number": 1, "body": "hi"})), Ok(()));
        // #73: add_comment without a body.
        assert_eq!(
            check_required(&schema, &json!({"number": 1})),
            Err("missing required field \"body\". The MCP server was not called.".into())
        );
        assert_eq!(
            check_required(&schema, &json!({"number": 1, "body": ""})),
            Err("field \"body\" must be at least 1 character long. The MCP server was not called.".into())
        );
        let err = check_required(&schema, &json!({"body": null, "meta": {}})).unwrap_err();
        assert!(err.contains("missing required field \"number\"") && err.contains("missing required field \"body\""));
        assert!(err.contains("missing required field \"meta.tag\""));
        let err = check_required(&schema, &json!({"number": 1, "body": "x", "meta": {"tag": "a"}})).unwrap_err();
        assert!(err.contains("field \"meta.tag\" must be at least 2 characters long"));
        // A required field whose schema allows null may be null; one that doesn't is missing.
        let nullable = json!({"type": "object", "properties": {"v": {"type": ["string", "null"]}, "w": {"type": "string"}}, "required": ["v", "w"]});
        assert_eq!(check_required(&nullable, &json!({"v": null, "w": "x"})), Ok(()));
        assert!(check_required(&nullable, &json!({"v": "a", "w": null})).unwrap_err().contains("missing required field \"w\""));
        // Inside a list's items too (a batch-comment tool).
        let batch = json!({"type": "object", "properties": {"comments": {"type": "array", "items": schema.clone(), "minItems": 1}, "tags": {"type": "array", "items": {"type": "string", "minLength": 1}}}});
        let err = check_required(
            &batch,
            &json!({"comments": [{"number": 1, "body": "ok"}, {"number": 2, "body": ""}, {}], "tags": ["a", ""]}),
        )
        .unwrap_err();
        assert!(err.contains("field \"comments[1].body\" must be at least 1 character long"), "{err}");
        assert!(err.contains("missing required field \"comments[2].body\""), "{err}");
        assert!(err.contains("field \"tags[1]\" must be"), "{err}");
        assert!(!err.contains("comments[0]"), "{err}");
        // An open schema has nothing to check.
        assert_eq!(check_required(&json!({"type": "object", "properties": {}}), &json!({})), Ok(()));
    }
}
