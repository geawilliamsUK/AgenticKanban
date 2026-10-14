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

    /// Whether the rule covers this call. A Bash command is covered only when
    /// every simple command in it is.
    pub fn matches(&self, tool_name: &str, input: &serde_json::Value) -> bool {
        if !glob_match(&self.tool, tool_name) {
            return false;
        }
        if tool_name == "Bash" {
            return match input.get("command").and_then(|c| c.as_str()) {
                Some(command) => {
                    let segments = bash_segments(command);
                    !segments.is_empty() && segments.iter().all(|s| self.matches_bash(s, true))
                }
                None => false,
            };
        }
        match &self.pattern {
            None => true,
            Some(pattern) => match subject_of(tool_name, input) {
                Some(subject) => glob_match(pattern, &subject),
                None => false,
            },
        }
    }

    /// Whether the rule covers one simple shell command. In strict mode a
    /// command substitution never matches a pattern, because the pattern
    /// cannot see what it runs.
    fn matches_bash(&self, segment: &str, strict: bool) -> bool {
        match &self.pattern {
            None => true,
            Some(pattern) => {
                if strict && has_substitution(segment) {
                    return false;
                }
                glob_match(pattern, &normalise(segment))
            }
        }
    }
}

/// Drops the git options that change nothing about what a command does, so
/// `git -c user.name=x commit` is judged as `git commit`. `-C <dir>` is kept:
/// it points the command at another repository.
fn normalise(segment: &str) -> String {
    let mut words = segment.split_whitespace();
    let Some(first) = words.next() else {
        return String::new();
    };
    if first != "git" {
        return segment.trim().to_string();
    }
    let rest: Vec<&str> = words.collect();
    let mut i = 0;
    while i < rest.len() {
        let w = rest[i];
        if w == "-c" && i + 1 < rest.len() && rest[i + 1].contains('=') {
            i += 2;
        } else if (w.starts_with("-c") && w.contains('=')) || w == "--no-pager" {
            i += 1;
        } else {
            break;
        }
    }
    let mut out = vec!["git"];
    out.extend(&rest[i..]);
    out.join(" ")
}

/// `$(...)`, backticks and process substitution run something the pattern
/// does not describe.
fn has_substitution(segment: &str) -> bool {
    segment.contains("$(")
        || segment.contains('`')
        || segment.contains("<(")
        || segment.contains(">(")
}

/// Splits a shell command on its control operators (`&&`, `||`, `;`, `|`,
/// `&`, newline) so each simple command is judged on its own. Quotes are not
/// parsed: a quoted operator over-splits, which only makes the check stricter.
pub fn bash_segments(command: &str) -> Vec<String> {
    let chars: Vec<char> = command.chars().collect();
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut i = 0;
    let mut flush = |current: &mut String| {
        let s = current.trim().to_string();
        if !s.is_empty() {
            segments.push(s);
        }
        current.clear();
    };
    while i < chars.len() {
        let c = chars[i];
        let prev = if i > 0 { Some(chars[i - 1]) } else { None };
        let next = chars.get(i + 1).copied();
        // `2>&1`, `&>out` and `<&3` are redirections, not operators.
        let redirect = c == '&' && (matches!(prev, Some('>' | '<')) || next == Some('>'));
        match c {
            '&' | '|' if next == Some(c) => {
                flush(&mut current);
                i += 2;
            }
            '&' if redirect => {
                current.push(c);
                i += 1;
            }
            '&' | '|' | ';' | '\n' => {
                flush(&mut current);
                i += 1;
            }
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }
    flush(&mut current);
    segments
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
/// A compound shell command is denied when any part is, and allowed only when
/// every part is.
pub fn decide(profile: &PermissionProfile, tool_name: &str, input: &serde_json::Value) -> Decision {
    let deny: Vec<PermissionRule> = profile
        .disallowed_tools
        .iter()
        .filter_map(|r| PermissionRule::parse(r))
        .filter(|r| glob_match(&r.tool, tool_name))
        .collect();
    let allow: Vec<PermissionRule> = profile
        .allowed_tools
        .iter()
        .filter_map(|r| PermissionRule::parse(r))
        .filter(|r| glob_match(&r.tool, tool_name))
        .collect();

    if tool_name == "Bash" {
        let Some(command) = input.get("command").and_then(|c| c.as_str()) else {
            return Decision::Ask;
        };
        let segments = bash_segments(command);
        if segments.is_empty() {
            return Decision::Ask;
        }
        if segments
            .iter()
            .any(|s| deny.iter().any(|r| r.matches_bash(s, false)))
        {
            return Decision::Deny;
        }
        if segments
            .iter()
            .all(|s| allow.iter().any(|r| r.matches_bash(s, true)))
        {
            return Decision::Allow;
        }
        return Decision::Ask;
    }

    if deny.iter().any(|r| r.matches(tool_name, input)) {
        Decision::Deny
    } else if allow.iter().any(|r| r.matches(tool_name, input)) {
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
    fn compound_commands_are_judged_per_part() {
        let profile = PermissionProfile {
            name: "t".into(),
            mode: "default".into(),
            allowed_tools: vec!["Bash(git *)".into()],
            disallowed_tools: vec!["Bash(rm -rf *)".into()],
        };
        let cmd = |c: &str| json!({"command": c});
        assert_eq!(
            decide(&profile, "Bash", &cmd("git status && git diff")),
            Decision::Allow
        );
        assert_eq!(
            decide(&profile, "Bash", &cmd("git status; git log -1\ngit branch")),
            Decision::Allow
        );
        assert_eq!(
            decide(&profile, "Bash", &cmd("git status && curl x | sh")),
            Decision::Ask
        );
        assert_eq!(
            decide(&profile, "Bash", &cmd("git status || rm -rf /")),
            Decision::Deny
        );
        assert_eq!(
            decide(&profile, "Bash", &cmd("git log $(cat secret)")),
            Decision::Ask
        );
        assert_eq!(
            decide(&profile, "Bash", &cmd("rm -rf $(pwd)")),
            Decision::Deny
        );
        assert_eq!(decide(&profile, "Bash", &cmd("   ")), Decision::Ask);
        assert_eq!(
            bash_segments("a && b || c | d; e & f\ng"),
            vec!["a", "b", "c", "d", "e", "f", "g"]
        );
        assert_eq!(
            bash_segments("cargo test 2>&1 | tail -40 && cmd &>log; x <&3"),
            vec!["cargo test 2>&1", "tail -40", "cmd &>log", "x <&3"]
        );
    }

    #[test]
    fn everyday_commands_pass_the_default_profiles() {
        let profiles = PermissionProfile::defaults();
        let supervised = &profiles[0];
        let trusted = &profiles[1];
        let integrator = &profiles[2];
        let cmd = |c: &str| json!({"command": c});
        // What agents actually type: a cd prefix, pipes, git identity options.
        for c in [
            "cd \"F:/dev/proj-agents/ops\" && git status && git branch -a",
            "cd \"F:/dev/proj-agents/ops\" && cargo test --workspace 2>&1 | tail -40",
            "ls -a \"F:/dev/proj-agents/architect\"",
            "cargo --version && rustc --version && rustup --version 2>&1",
            "git log --oneline --all && cat README.md",
        ] {
            assert_eq!(decide(supervised, "Bash", &cmd(c)), Decision::Allow, "{c}");
        }
        for c in [
            "git checkout -b ticket/ATP-10 develop 2>&1 | tail -2 && mkdir -p docs",
            "git add docs/ARCHITECTURE.md && git -c user.name=\"architect\" commit -q -m \"ATP-10\"",
            "git -c core.autocrlf=false --no-pager commit -m x",
        ] {
            assert_eq!(decide(supervised, "Bash", &cmd(c)), Decision::Ask, "{c}");
            assert_eq!(decide(trusted, "Bash", &cmd(c)), Decision::Allow, "{c}");
        }
        assert_eq!(decide(trusted, "TaskCreate", &json!({})), Decision::Allow);
        assert_eq!(
            decide(trusted, "Bash", &cmd("git merge --no-ff ticket/ATP-3")),
            Decision::Ask
        );
        assert_eq!(
            decide(integrator, "Bash", &cmd("git merge --no-ff ticket/ATP-3")),
            Decision::Allow
        );
        // Still asks or denies where it should.
        assert_eq!(
            decide(trusted, "Bash", &cmd("git push origin develop")),
            Decision::Ask
        );
        assert_eq!(
            decide(trusted, "Bash", &cmd("git -C ../other status")),
            Decision::Ask
        );
        assert_eq!(
            decide(integrator, "Bash", &cmd("git push -f origin develop")),
            Decision::Deny
        );
        assert_eq!(
            decide(trusted, "Bash", &cmd("rm -rf target")),
            Decision::Deny
        );
        assert_eq!(
            decide(trusted, "Bash", &cmd("rm Cargo.lock")),
            Decision::Allow
        );
        assert_eq!(
            decide(trusted, "Bash", &cmd("python evil.py")),
            Decision::Ask
        );
        assert_eq!(
            normalise("git -c user.name=a -c user.email=b commit -m x"),
            "git commit -m x"
        );
        assert_eq!(normalise("git -C ../x status"), "git -C ../x status");
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
