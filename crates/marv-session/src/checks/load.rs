//! The project's own checks: rules from its instructions, as code that runs after the model's edits (issue #41).
//! Ports Marv's `src/checks/load.ts`.
//!
//! One file per rule in `.marv/checks/`: declarative (`.yaml`/`.yml`/`.json`: `glob`, optional `exclude`, exactly one
//! of `forbid`/`require`, `flags` from `imsu`, `message`, `why`) or a script (`.ts`, run with bun in the sandbox; its
//! leading `// glob:`/`// exclude:` comments are read as text, never run). Reading them runs nothing.
//!
//! Port notes:
//! - Patterns are JavaScript regular expressions, compiled here only to validate (matching happens in the sandbox,
//!   [`super::runner`]). Rust's `regex` has another dialect (no lookaround, no backreferences), so both use
//!   `regress`, an ECMAScript engine: a rule that works in Marv works here.
//! - Globs follow Bun.Glob (`globset` with `literal_separator`: `*` stays in its folder, `**` crosses folders). A glob
//!   `globset` can't read matches nothing.

use std::path::Path;

use serde_json::Value;

pub const CHECKS_DIR: &str = ".marv/checks";
/// Larger declarative files are refused (a rule is a few lines).
pub const MAX_RULE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckRule {
    /// A JavaScript regex: each match is a violation, at its line.
    Forbid(String),
    /// A JavaScript regex: a matching file without one is a violation.
    Require(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckBody {
    Pattern {
        rule: CheckRule,
        flags: String,
        message: String,
        why: Option<String>,
    },
    /// A `.ts` file run with bun from the project root; prints violations as JSON lines.
    Script,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectCheck {
    pub name: String,
    /// Project-relative, e.g. `.marv/checks/imports.yaml`.
    pub file: String,
    pub glob: Vec<String>,
    pub exclude: Vec<String>,
    pub body: CheckBody,
}

/// What a check reported: a rule broken somewhere, or the check itself failing (`failed`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Violation {
    pub check: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failed: bool,
}

fn strings(value: &Value) -> Option<Vec<String>> {
    match value {
        Value::String(s) => Some(vec![s.clone()]),
        Value::Array(items) => items.iter().map(|v| v.as_str().map(String::from)).collect(),
        _ => None,
    }
}

/// A declarative check's settings, or why they're wrong.
pub fn parse_pattern_check(name: &str, file: &str, text: &str) -> Result<ProjectCheck, String> {
    let data: Value = serde_norway::from_str(text).map_err(|e| format!("isn't valid YAML or JSON ({e})"))?;
    let Value::Object(d) = data else {
        return Err("should be an object with glob, forbid or require, message and why".into());
    };
    let glob = d.get("glob").and_then(strings).filter(|g| !g.is_empty()).ok_or("needs a glob (a string or a list of them)")?;
    let exclude = match d.get("exclude") {
        None => Vec::new(),
        Some(v) => strings(v).ok_or("has an exclude that isn't a string or a list of them")?,
    };
    let rule = match (d.get("forbid"), d.get("require")) {
        (Some(Value::String(p)), None) if !p.is_empty() => CheckRule::Forbid(p.clone()),
        (None, Some(Value::String(p))) if !p.is_empty() => CheckRule::Require(p.clone()),
        (Some(_), None) | (None, Some(_)) => {
            return Err("has a forbid or require that isn't a regular expression".into());
        }
        _ => return Err("needs exactly one of forbid and require".into()),
    };
    let flags = match d.get("flags") {
        None => String::new(),
        Some(Value::String(f)) if f.chars().all(|c| "imsu".contains(c)) => f.clone(),
        Some(_) => return Err("has flags other than i, m, s and u".into()),
    };
    let pattern = match &rule {
        CheckRule::Forbid(p) | CheckRule::Require(p) => p,
    };
    // Compiled, never run, here: matching happens in the sandbox.
    compile(pattern, &flags).map_err(|e| format!("has a regular expression that doesn't compile ({e})"))?;
    let message = d.get("message").and_then(Value::as_str).map(str::trim).filter(|m| !m.is_empty()).ok_or("needs a message")?;
    let why = match d.get("why") {
        None => None,
        Some(Value::String(w)) => Some(w.trim().to_string()).filter(|w| !w.is_empty()),
        Some(_) => return Err("has a why that isn't a string".into()),
    };
    Ok(ProjectCheck {
        name: name.into(),
        file: file.into(),
        glob,
        exclude,
        body: CheckBody::Pattern { rule, flags, message: message.into(), why },
    })
}

/// A check's pattern as JavaScript compiles it (`flags` from `imsu`; the runner adds nothing: `regress` iterates
/// matches itself, like `g`).
pub fn compile(pattern: &str, flags: &str) -> Result<regress::Regex, regress::Error> {
    regress::Regex::with_flags(pattern, flags)
}

/// Globs as Bun.Glob matches them; `None` when one can't be read.
pub fn glob_set(patterns: &[String]) -> Option<globset::GlobSet> {
    let mut set = globset::GlobSetBuilder::new();
    for pattern in patterns {
        set.add(globset::GlobBuilder::new(pattern).literal_separator(true).build().ok()?);
    }
    set.build().ok()
}

/// Glob patterns separated by commas or spaces; a comma inside braces (`*.{ts,tsx}`) belongs to its pattern.
pub fn split_patterns(text: &str) -> Vec<String> {
    let (mut patterns, mut current, mut depth) = (Vec::new(), String::new(), 0usize);
    for c in text.chars() {
        if c == '{' {
            depth += 1;
        } else if c == '}' && depth > 0 {
            depth -= 1;
        } else if depth == 0 && (c == ',' || c.is_whitespace()) {
            if !current.is_empty() {
                patterns.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(c);
    }
    if !current.is_empty() {
        patterns.push(current);
    }
    patterns
}

/// A script check's settings, from its leading `//` comments. Without a `glob:` it runs after any change.
pub fn parse_script_check(name: &str, file: &str, text: &str) -> ProjectCheck {
    let (mut glob, mut exclude) = (Vec::new(), Vec::new());
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with("#!") {
            continue;
        }
        let Some(comment) = trimmed.strip_prefix("//") else {
            break;
        };
        let Some((key, value)) = comment.split_once(':') else {
            continue;
        };
        match key.trim() {
            "glob" => glob.extend(split_patterns(value)),
            "exclude" => exclude.extend(split_patterns(value)),
            _ => {}
        }
    }
    if glob.is_empty() {
        glob.push("**".into());
    }
    ProjectCheck { name: name.into(), file: file.into(), glob, exclude, body: CheckBody::Script }
}

/// The project's checks, sorted by name, and the files that couldn't be used. Nothing when there's no folder.
pub fn load_checks(root: &Path) -> (Vec<ProjectCheck>, Vec<String>) {
    let dir = root.join(CHECKS_DIR);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return (Vec::new(), Vec::new());
    };
    let mut names: Vec<String> = entries.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    names.sort();
    let (mut checks, mut problems): (Vec<ProjectCheck>, Vec<String>) = (Vec::new(), Vec::new());
    for entry in names {
        let Some((name, ext)) = entry.rsplit_once('.') else {
            continue;
        };
        let declarative = matches!(ext, "yaml" | "yml" | "json");
        if !declarative && ext != "ts" {
            continue;
        }
        let file = format!("{CHECKS_DIR}/{entry}");
        if checks.iter().any(|c| c.name == name) {
            problems.push(format!("{file}: another check is already named \"{name}\""));
            continue;
        }
        let path = dir.join(&entry);
        // marv-rs: never read through a symlink out of the checks folder (`symlink_metadata`: a link isn't a file).
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        if declarative && meta.len() > MAX_RULE_BYTES {
            problems.push(format!("{file}: larger than {} KB", MAX_RULE_BYTES / 1024));
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                problems.push(format!("{file}: couldn't be read ({e})"));
                continue;
            }
        };
        if !declarative {
            checks.push(parse_script_check(name, &file, &text));
            continue;
        }
        match parse_pattern_check(name, &file, &text) {
            Ok(check) => checks.push(check),
            Err(why) => problems.push(format!("{file}: {why}")),
        }
    }
    (checks, problems)
}

/// Whether a change to `path` (project-relative) concerns the check: its glob matches and its exclude doesn't, or
/// it's the check's own file.
pub fn concerns(check: &ProjectCheck, path: &str) -> bool {
    if path == check.file {
        return true;
    }
    let matches = |patterns: &[String]| glob_set(patterns).is_some_and(|set| set.is_match(path));
    matches(&check.glob) && !matches(&check.exclude)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declarative_checks_are_validated() {
        let ok = parse_pattern_check(
            "imports",
            ".marv/checks/imports.yaml",
            "glob: src/**/*.ts\nforbid: 'from \"\\.\\./'\nmessage: No parent imports\nwhy: Use aliases.\n",
        )
        .unwrap();
        assert!(matches!(ok.body, CheckBody::Pattern { rule: CheckRule::Forbid(_), .. }));
        assert_eq!(ok.glob, vec!["src/**/*.ts"]);
        let err = |t: &str| parse_pattern_check("x", "f", t).unwrap_err();
        assert_eq!(err("forbid: a\nmessage: m"), "needs a glob (a string or a list of them)");
        assert_eq!(err("glob: a\nmessage: m"), "needs exactly one of forbid and require");
        assert_eq!(err("glob: a\nforbid: a\nrequire: b\nmessage: m"), "needs exactly one of forbid and require");
        assert_eq!(err("glob: a\nforbid: a\nflags: g\nmessage: m"), "has flags other than i, m, s and u");
        assert_eq!(err("glob: a\nforbid: a"), "needs a message");
    }

    #[test]
    fn script_settings_come_from_leading_comments_only() {
        let c = parse_script_check(
            "s",
            "f",
            "#!/usr/bin/env bun\n// glob: src/**/*.{ts,tsx}, tests/**\n// exclude: dist/**\nconsole.log(1)\n// glob: never/**\n",
        );
        assert_eq!(c.glob, vec!["src/**/*.{ts,tsx}", "tests/**"]);
        assert_eq!(c.exclude, vec!["dist/**"]);
        assert_eq!(parse_script_check("s", "f", "run()").glob, vec!["**"]);
    }

    use crate::checks::Scratch;

    const NO_DEFAULT: &str =
        "glob: 'src/**/*.ts'\nforbid: 'export default'\nmessage: no default exports\nwhy: named exports are easier to find\n";

    fn script(name: &str, glob: &[&str], exclude: &[&str]) -> ProjectCheck {
        ProjectCheck {
            name: name.into(),
            file: format!(".marv/checks/{name}.ts"),
            glob: glob.iter().map(|s| s.to_string()).collect(),
            exclude: exclude.iter().map(|s| s.to_string()).collect(),
            body: CheckBody::Script,
        }
    }

    #[test]
    fn no_folder_no_checks_no_problems() {
        let dir = Scratch::new();
        assert_eq!(load_checks(&dir.0), (vec![], vec![]));
    }

    #[test]
    fn declarative_checks_and_scripts_with_their_leading_glob_comments() {
        let dir = Scratch::new();
        dir.put(".marv/checks/no-default.yaml", NO_DEFAULT);
        dir.put(
            ".marv/checks/license.json",
            r#"{"glob":["src/**/*.ts"],"exclude":"src/gen/**","require":"^// SPDX","flags":"m","message":"no license header"}"#,
        );
        dir.put(
            ".marv/checks/custom.ts",
            "#!/usr/bin/env bun\n// glob: src/**/*.ts, lib/*.ts\n// exclude: src/gen/**\nconsole.log('x');\n// glob: ignored.ts\n",
        );
        dir.put(".marv/checks/anywhere.ts", "console.log('x');\n");
        dir.put(".marv/checks/README.md", "notes, not a check");
        let (checks, problems) = load_checks(&dir.0);
        assert_eq!(problems, Vec::<String>::new());
        assert_eq!(checks.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["anywhere", "custom", "license", "no-default"]);
        let find = |name: &str| checks.iter().find(|c| c.name == name).unwrap().clone();
        assert_eq!(
            find("no-default"),
            ProjectCheck {
                name: "no-default".into(),
                file: ".marv/checks/no-default.yaml".into(),
                glob: vec!["src/**/*.ts".into()],
                exclude: vec![],
                body: CheckBody::Pattern {
                    rule: CheckRule::Forbid("export default".into()),
                    flags: String::new(),
                    message: "no default exports".into(),
                    why: Some("named exports are easier to find".into()),
                },
            }
        );
        let license = find("license");
        assert_eq!(license.exclude, vec!["src/gen/**"]);
        assert!(matches!(
            license.body,
            CheckBody::Pattern { rule: CheckRule::Require(ref p), ref flags, .. } if p == "^// SPDX" && flags == "m"
        ));
        assert_eq!(find("custom"), script("custom", &["src/**/*.ts", "lib/*.ts"], &["src/gen/**"]));
        assert_eq!(find("anywhere").glob, vec!["**"]);
    }

    #[test]
    fn broken_ones_are_skipped_and_say_why() {
        let dir = Scratch::new();
        dir.put(".marv/checks/a-no-glob.yaml", "forbid: x\nmessage: m\n");
        dir.put(".marv/checks/b-both.yaml", "glob: x\nforbid: a\nrequire: b\nmessage: m\n");
        dir.put(".marv/checks/c-bad-regex.yaml", "glob: x\nforbid: '('\nmessage: m\n");
        dir.put(".marv/checks/d-no-message.yaml", "glob: x\nforbid: a\n");
        dir.put(".marv/checks/e-flags.yaml", "glob: x\nforbid: a\nflags: g\nmessage: m\n");
        dir.put(".marv/checks/f-yaml.yaml", "glob: [unclosed\n");
        dir.put(".marv/checks/g.yaml", NO_DEFAULT);
        dir.put(".marv/checks/g.ts", "");
        let (checks, problems) = load_checks(&dir.0);
        let kinds: Vec<String> = checks
            .iter()
            .map(|c| format!("{}:{}", c.name, if c.body == CheckBody::Script { "script" } else { "pattern" }))
            .collect();
        assert_eq!(kinds, ["g:script"]);
        assert_eq!(problems.len(), 7);
        let all = problems.join("\n");
        for expected in [
            "a-no-glob.yaml: needs a glob",
            "b-both.yaml: needs exactly one of forbid and require",
            "c-bad-regex.yaml: has a regular expression that doesn't compile",
            "d-no-message.yaml: needs a message",
            "e-flags.yaml: has flags other than",
            "f-yaml.yaml: isn't valid YAML",
            "g.yaml: another check is already named \"g\"",
        ] {
            assert!(all.contains(expected), "{expected} not in {all}");
        }
    }

    #[test]
    fn javascript_regular_expressions_are_accepted() {
        // Lookaround and backreferences: `regex` would refuse these; Marv's checks may use them.
        assert!(parse_pattern_check("x", "f", "glob: a\nforbid: '(?<!\\.)import(?= )'\nmessage: m\n").is_ok());
        assert!(parse_pattern_check("x", "f", "glob: a\nforbid: '([\"''])x\\1'\nmessage: m\n").is_ok());
    }

    #[test]
    fn a_scripts_globs_may_hold_brace_lists() {
        let check = parse_script_check(
            "c",
            ".marv/checks/c.ts",
            "// glob: src/**/*.{ts,tsx}, lib/*.ts\n// exclude: src/{gen,vendor}/**\n",
        );
        assert_eq!(check.glob, vec!["src/**/*.{ts,tsx}", "lib/*.ts"]);
        assert_eq!(check.exclude, vec!["src/{gen,vendor}/**"]);
        assert!(concerns(&check, "src/a.tsx"));
        assert!(concerns(&check, "src/a.ts"));
        assert!(!concerns(&check, "src/vendor/a.ts"));
    }

    #[test]
    fn a_change_concerns_a_check_when_its_glob_matches_and_its_exclude_doesnt_or_its_own_file() {
        let check = script("c", &["src/**/*.ts"], &["src/gen/**"]);
        assert!(concerns(&check, "src/a/b.ts"));
        assert!(concerns(&check, "src/a.ts"));
        assert!(!concerns(&check, "src/gen/x.ts"));
        assert!(!concerns(&check, "README.md"));
        assert!(concerns(&check, ".marv/checks/c.ts"));
        // `*` stays in its folder, as in Bun.Glob.
        assert!(!concerns(&script("c", &["lib/*.ts"], &[]), "lib/a/b.ts"));
    }
}
