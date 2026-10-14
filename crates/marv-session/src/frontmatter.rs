//! YAML frontmatter: the `---` block at the top of `SKILL.md` and agent files (name, description, …), followed by
//! the instructions. Ports Marv's `src/frontmatter.ts`.
//!
//! Port note: Marv parsed with `Bun.YAML`; here `serde_norway` reads the YAML straight into JSON values, so callers
//! check fields the way Marv did (`typeof fields.name === "string"`). YAML error wording differs from Bun's.

use std::sync::LazyLock;

use regex::Regex;
use serde_json::{Map, Value};

static BLOCK: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^---\r?\n((?s:.*?))\r?\n---\r?\n?").unwrap());

#[derive(Debug, Clone, PartialEq)]
pub struct Frontmatter {
    pub fields: Map<String, Value>,
    /// The text after the block, trimmed.
    pub body: String,
}

/// Splits a file into its frontmatter fields and body. `shown` names the file in problems
/// (`~/.marv/skills/x/SKILL.md`). The error is a whole sentence for the startup notice.
pub fn parse_frontmatter(text: &str, shown: &str) -> Result<Frontmatter, String> {
    let Some(found) = BLOCK.captures(text) else {
        return Err(format!("{shown} starts without a --- frontmatter block (name and description)."));
    };
    // The block's own last line break is part of the YAML (a `|` scalar keeps it, as Bun.YAML did).
    let yaml = format!("{}\n", found.get(1).map_or("", |m| m.as_str()));
    let meta: Value = match serde_norway::from_str::<Option<Value>>(&yaml) {
        Ok(v) => v.unwrap_or(Value::Null),
        Err(err) => {
            return Err(format!("{shown}: the frontmatter isn't valid YAML ({err})."));
        }
    };
    let fields = match meta {
        Value::Object(map) => map,
        _ => Map::new(),
    };
    let end = found.get(0).map_or(0, |m| m.end());
    Ok(Frontmatter { fields, body: text[end..].trim().to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn splits_the_yaml_fields_from_the_body() {
        let parsed = parse_frontmatter("---\nname: review\ndescription: Look for bugs.\n---\n\n# Review\n", "x.md").unwrap();
        assert_eq!(Value::Object(parsed.fields), json!({"name": "review", "description": "Look for bugs."}));
        assert_eq!(parsed.body, "# Review");
    }

    #[test]
    fn a_multi_line_description_is_one_string() {
        let parsed =
            parse_frontmatter("---\nname: r\ndescription: |\n  Line one.\n  <example>two</example>\n---\nBody", "x.md").unwrap();
        assert_eq!(parsed.fields["description"], json!("Line one.\n<example>two</example>\n"));
        assert_eq!(parsed.body, "Body");
    }

    #[test]
    fn no_frontmatter_or_broken_yaml_is_an_error_naming_the_file() {
        assert!(parse_frontmatter("# Just text", "a/SKILL.md").unwrap_err().contains("a/SKILL.md starts without"));
        assert!(
            parse_frontmatter("---\nname: [unclosed\n---\n", "b.md")
                .unwrap_err()
                .contains("b.md: the frontmatter isn't valid YAML")
        );
    }

    #[test]
    fn crlf_files_work() {
        let parsed = parse_frontmatter("---\r\nname: a\r\n---\r\nBody\r\n", "c.md").unwrap();
        assert_eq!(parsed.fields["name"], json!("a"));
        assert_eq!(parsed.body, "Body");
    }
}
