//! Permission rules in Claude Code's own syntax, evaluated for the PreToolUse
//! hook. A rule is a tool name, optionally with a pattern in parentheses:
//!
//! - `Read`, `Edit`, `mcp__board__ticket_move`: the tool by name.
//! - `mcp__board__*`: a glob on the tool name.
//! - `Bash(git commit *)`, `Bash(cargo *)`: a glob on the shell command.
//! - `Bash(git commit:*)`: Claude Code's trailing-wildcard form, same as a
//!   glob ending in `*`.
//!
//! Patterns on tools other than `Bash` are matched against the tool input's
//! `command`, `file_path` or `path` field when present, else ignored.

use serde::{Deserialize, Serialize};

use crate::config::PermissionProfile;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionRule {
    pub tool: String,
    pub pattern: Option<String>,
}

impl PermissionRule {
    pub fn parse(rule: &str) -> Option<Self> {
        let rule = rule.trim();
        if rule.is_empty() {
            return None;
        }
        match rule.split_once('(') {
            Some((tool, rest)) => {
                let pattern = rest.strip_suffix(')')?;
                // `git commit:*` means `git commit*`, and `git commit *` stays as is.
                let pattern = pattern
                    .strip_suffix(":*")
                    .map(|p| format!("{p}*"))
                    .unwrap_or_else(|| pattern.to_string());
                Some(Self {
                    tool: tool.trim().to_string(),
                    pattern: Some(pattern),
                })
            }
            None => Some(Self {
                tool: rule.to_string(),
                pattern: None,
            }),
        }
    }

    /// Whether the rule covers this call.
    pub fn matches(&self, tool_name: &str, input: &serde_json::Value) -> bool {
        if !glob_match(&self.tool, tool_name) {
            return false;
        }
        match &self.pattern {
            None => true,
            Some(pattern) => match subject_of(tool_name, input) {
                Some(subject) => glob_match(pattern, &subject),
                None => false,
            },
        }
    }
}

/// The part of a tool call a pattern applies to.
fn subject_of(tool_name: &str, input: &serde_json::Value) -> Option<String> {
    let key = if tool_name == "Bash" {
        "command"
    } else if input.get("file_path").is_some() {
        "file_path"
    } else if input.get("path").is_some() {
        "path"
    } else {
        "command"
    };
    input.get(key)?.as_str().map(|s| s.trim().to_string())
}

/// `*` matches any run of characters, including none. Everything else is literal.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Classic iterative wildcard matching with backtracking on the last star.
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark): (Option<usize>, usize) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Allow,
    Deny,
    /// Not covered by any rule: the human decides.
    Ask,
}

/// Evaluates a profile. Deny rules win over allow rules, as in Claude Code.
pub fn decide(profile: &PermissionProfile, tool_name: &str, input: &serde_json::Value) -> Decision {
    let denied = profile
        .disallowed_tools
        .iter()
        .filter_map(|r| PermissionRule::parse(r))
        .any(|r| r.matches(tool_name, input));
    if denied {
        return Decision::Deny;
    }
    let allowed = profile
        .allowed_tools
        .iter()
        .filter_map(|r| PermissionRule::parse(r))
        .any(|r| r.matches(tool_name, input));
    if allowed {
        Decision::Allow
    } else {
        Decision::Ask
    }
}

/// A one-line description of a tool call for the approval card.
pub fn summarise(tool_name: &str, input: &serde_json::Value) -> String {
    let subject = subject_of(tool_name, input).unwrap_or_default();
    let subject = if subject.chars().count() > 160 {
        let cut: String = subject.chars().take(157).collect();
        format!("{cut}…")
    } else {
        subject
    };
    if subject.is_empty() {
        tool_name.to_string()
    } else {
        format!("{tool_name}: {subject}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn globs() {
        assert!(glob_match("git commit *", "git commit -m x"));
        assert!(glob_match("git commit*", "git commit"));
        assert!(!glob_match("git commit *", "git commit"));
        assert!(glob_match("mcp__board__*", "mcp__board__ticket_move"));
        assert!(!glob_match("mcp__board__*", "mcp__other__x"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXXbYYc"));
        assert!(!glob_match("a*b*c", "aXXbYY"));
    }

    #[test]
    fn parses_claude_code_rule_forms() {
        let r = PermissionRule::parse("Bash(git commit:*)").unwrap();
        assert_eq!(r.tool, "Bash");
        assert_eq!(r.pattern.as_deref(), Some("git commit*"));
        let r = PermissionRule::parse("Bash(git diff *)").unwrap();
        assert_eq!(r.pattern.as_deref(), Some("git diff *"));
        let r = PermissionRule::parse("Read").unwrap();
        assert!(r.pattern.is_none());
        assert!(PermissionRule::parse("Bash(unclosed").is_none());
        assert!(PermissionRule::parse("  ").is_none());
    }

    #[test]
    fn rules_match_commands_and_paths() {
        let bash = PermissionRule::parse("Bash(git commit *)").unwrap();
        assert!(bash.matches("Bash", &json!({"command": "git commit -m 'x'"})));
        assert!(!bash.matches("Bash", &json!({"command": "git push"})));
        assert!(!bash.matches("Edit", &json!({"command": "git commit -m 'x'"})));

        let edit = PermissionRule::parse("Edit(src/*)").unwrap();
        assert!(edit.matches("Edit", &json!({"file_path": "src/main.rs"})));
        assert!(!edit.matches("Edit", &json!({"file_path": "docs/x.md"})));

        let any_board = PermissionRule::parse("mcp__board__*").unwrap();
        assert!(any_board.matches("mcp__board__ticket_get", &json!({})));
    }

    #[test]
    fn deny_beats_allow_and_unmatched_asks() {
        let profile = PermissionProfile {
            name: "t".into(),
            mode: "default".into(),
            allowed_tools: vec!["Bash(git *)".into(), "Read".into()],
            disallowed_tools: vec!["Bash(git push --force*)".into()],
        };
        assert_eq!(decide(&profile, "Read", &json!({})), Decision::Allow);
        assert_eq!(
            decide(&profile, "Bash", &json!({"command": "git status"})),
            Decision::Allow
        );
        assert_eq!(
            decide(
                &profile,
                "Bash",
                &json!({"command": "git push --force origin main"})
            ),
            Decision::Deny
        );
        assert_eq!(
            decide(&profile, "Bash", &json!({"command": "rm -rf /"})),
            Decision::Ask
        );
        assert_eq!(decide(&profile, "Write", &json!({})), Decision::Ask);
    }

    #[test]
    fn default_profiles_behave() {
        let profiles = PermissionProfile::defaults();
        let supervised = &profiles[0];
        let trusted = &profiles[1];
        let commit = json!({"command": "git commit -m 'feat'"});
        assert_eq!(decide(supervised, "Bash", &commit), Decision::Ask);
        assert_eq!(decide(trusted, "Bash", &commit), Decision::Allow);
        assert_eq!(
            decide(trusted, "Bash", &json!({"command": "git push"})),
            Decision::Ask
        );
        assert_eq!(
            decide(trusted, "Bash", &json!({"command": "rm -rf target"})),
            Decision::Deny
        );
    }

    #[test]
    fn summaries_are_short() {
        assert_eq!(
            summarise("Read", &json!({"file_path": "a.rs"})),
            "Read: a.rs"
        );
        assert_eq!(
            summarise("mcp__board__inbox_list", &json!({})),
            "mcp__board__inbox_list"
        );
        let long = "x".repeat(500);
        assert!(summarise("Bash", &json!({"command": long})).chars().count() < 200);
    }
}
