//! Configuration types.
//!
//! `HomeConfig` lives at `~/.agentic-kanban/config.toml` and registers
//! projects on this machine. `ProjectPolicy` lives at
//! `~/.agentic-kanban/projects/<id>/project.toml` and is what an export copies
//! into a repository, so it must contain nothing machine-specific.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::board::ColumnKind;
use crate::ids::ProjectId;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct HomeConfig {
    pub settings: GlobalSettings,
    pub projects: Vec<ProjectEntry>,
}

impl HomeConfig {
    pub fn project(&self, id: ProjectId) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| p.id == id)
    }

    pub fn project_by_path(&self, path: &std::path::Path) -> Option<&ProjectEntry> {
        self.projects.iter().find(|p| p.path == path)
    }
}

/// Which look the app uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThemeMode {
    /// Follow the operating system's setting.
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GlobalSettings {
    /// The human's handle, used for `@mentions` in every project.
    pub human_handle: String,
    pub human_display_name: String,
    /// Explicit path to the Claude Code binary. When absent the runtime
    /// searches PATH and the native installer's default location.
    pub claude_binary: Option<PathBuf>,
    /// Concurrent agent runs across all projects.
    pub max_concurrent_runs: u32,
    /// Concurrent Refine and Integrate calls.
    pub max_concurrent_utility_calls: u32,
    /// Spend cap across all projects per day, in USD.
    pub global_daily_budget_usd: f64,
    /// Model for Refine and Integrate when a project does not override it.
    pub authoring_model: Option<String>,
    /// Days to keep per-run directories.
    pub run_retention_days: u32,
    pub theme: ThemeMode,
}

impl Default for GlobalSettings {
    fn default() -> Self {
        Self {
            human_handle: "me".into(),
            human_display_name: "Me".into(),
            claude_binary: None,
            max_concurrent_runs: 3,
            max_concurrent_utility_calls: 4,
            global_daily_budget_usd: 100.0,
            authoring_model: None,
            run_retention_days: 30,
            theme: ThemeMode::System,
        }
    }
}

/// A project as registered on this machine.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProjectEntry {
    /// Machine-local id. Keys the project's directory under the home dir.
    pub id: ProjectId,
    pub name: String,
    /// The repository the agents work in.
    pub path: PathBuf,
    /// Where agent worktrees go. Defaults to a sibling of `path`.
    #[serde(default)]
    pub worktree_base: Option<PathBuf>,
}

/// Shareable team policy. Everything here may be exported into a repository.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ProjectPolicy {
    /// Stable across machines and clones. Generated once, exported with the
    /// snapshot, used by Import to match a snapshot to an existing project.
    pub shared_id: Uuid,
    pub name: String,
    /// Ticket keys are `<key_prefix>-<n>`.
    pub key_prefix: String,
    pub columns: Vec<ColumnSpec>,
    pub guardrails: GuardrailThresholds,
    pub push_policy: PushPolicy,
    /// Load the repository's own `.mcp.json` servers into agent runs.
    pub allow_project_mcp: bool,
    pub default_model: Option<String>,
    pub authoring_model: Option<String>,
    pub permission_profiles: Vec<PermissionProfile>,
    /// Threads with no activity for this many days are archived. Posting
    /// reopens them.
    pub thread_archive_days: u32,
    /// The branch ticket branches start from and the integrator merges into.
    /// None means the repository's default branch.
    pub integration_branch: Option<String>,
}

impl Default for ProjectPolicy {
    fn default() -> Self {
        Self::new("Project")
    }
}

impl ProjectPolicy {
    pub fn new(name: &str) -> Self {
        Self {
            shared_id: Uuid::new_v4(),
            name: name.to_string(),
            key_prefix: derive_key_prefix(name),
            columns: ColumnSpec::defaults(),
            guardrails: GuardrailThresholds::default(),
            push_policy: PushPolicy::Never,
            allow_project_mcp: false,
            thread_archive_days: 30,
            integration_branch: None,
            default_model: None,
            authoring_model: None,
            permission_profiles: PermissionProfile::defaults(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnSpec {
    pub name: String,
    pub kind: ColumnKind,
}

impl ColumnSpec {
    pub fn defaults() -> Vec<Self> {
        vec![
            ColumnSpec {
                name: "Backlog".into(),
                kind: ColumnKind::Backlog,
            },
            ColumnSpec {
                name: "To do".into(),
                kind: ColumnKind::Todo,
            },
            ColumnSpec {
                name: "In progress".into(),
                kind: ColumnKind::InProgress,
            },
            ColumnSpec {
                name: "Review".into(),
                kind: ColumnKind::Review,
            },
            ColumnSpec {
                name: "Done".into(),
                kind: ColumnKind::Done,
            },
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GuardrailThresholds {
    pub ping_pong_moves: u32,
    pub ping_pong_window_minutes: u32,
    pub comment_storm_comments: u32,
    pub comment_storm_window_minutes: u32,
    pub mention_loop_mentions: u32,
    pub mention_loop_window_minutes: u32,
    pub run_churn_runs: u32,
    pub run_churn_cooldown_minutes: u32,
    pub repeated_tool_calls: u32,
    pub max_turns_per_run: u32,
    pub max_usd_per_run: f64,
    pub max_usd_per_agent_per_day: f64,
    pub max_wall_clock_minutes: u32,
    pub wake_storm_per_hour: u32,
}

impl Default for GuardrailThresholds {
    fn default() -> Self {
        Self {
            ping_pong_moves: 6,
            ping_pong_window_minutes: 30,
            comment_storm_comments: 15,
            comment_storm_window_minutes: 30,
            mention_loop_mentions: 8,
            mention_loop_window_minutes: 30,
            run_churn_runs: 3,
            run_churn_cooldown_minutes: 30,
            repeated_tool_calls: 5,
            max_turns_per_run: 100,
            max_usd_per_run: 5.0,
            max_usd_per_agent_per_day: 25.0,
            max_wall_clock_minutes: 45,
            wake_storm_per_hour: 12,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PushPolicy {
    Never,
    Allowed,
}

/// A named permission profile in Claude Code's own rule syntax.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionProfile {
    pub name: String,
    /// A Claude Code permission mode: `default` or `acceptEdits`.
    /// `bypassPermissions` is refused by the runtime.
    pub mode: String,
    pub allowed_tools: Vec<String>,
    pub disallowed_tools: Vec<String>,
}

impl PermissionProfile {
    pub const SUPERVISED: &'static str = "supervised";
    pub const TRUSTED: &'static str = "trusted";
    pub const INTEGRATOR: &'static str = "integrator";

    /// Three presets. Supervised reads, builds and tests but asks before it
    /// changes anything; trusted edits and commits in its own worktree;
    /// integrator may also merge. Every part of a compound shell command must
    /// be covered, so the everyday shell verbs are listed too. Pushing is
    /// never listed: it always asks, and the deny list keeps it from being
    /// forced.
    pub fn defaults() -> Vec<Self> {
        let reads: Vec<String> = [
            // Claude Code's own bookkeeping and read tools.
            "Read",
            "Glob",
            "Grep",
            "ToolSearch",
            "TodoWrite",
            "TaskCreate",
            "TaskUpdate",
            "TaskList",
            "TaskGet",
            "BashOutput",
            "TaskOutput",
            "KillShell",
            "WebSearch",
            "WebFetch",
            "ListMcpResourcesTool",
            "ReadMcpResourceTool",
            // Everyday shell verbs that only read or print.
            "Bash(cd *)",
            "Bash(ls*)",
            "Bash(dir*)",
            "Bash(pwd*)",
            "Bash(cat *)",
            "Bash(head *)",
            "Bash(tail *)",
            "Bash(wc *)",
            "Bash(sort *)",
            "Bash(uniq *)",
            "Bash(grep *)",
            "Bash(rg *)",
            "Bash(find *)",
            "Bash(tree*)",
            "Bash(echo *)",
            "Bash(printf *)",
            "Bash(which *)",
            "Bash(where *)",
            "Bash(type *)",
            "Bash(env*)",
            "Bash(date*)",
            "Bash(true)",
            "Bash(sleep *)",
            // Building and testing write only to the target directory.
            "Bash(cargo *)",
            "Bash(rustc *)",
            "Bash(rustup *)",
            "Bash(npm test*)",
            "Bash(npm run*)",
            "Bash(npx tsc*)",
            "Bash(npx vitest*)",
            "Bash(npx eslint*)",
            // Git that reads.
            "Bash(git status*)",
            "Bash(git diff*)",
            "Bash(git log*)",
            "Bash(git show*)",
            "Bash(git rev-parse*)",
            "Bash(git merge-base*)",
            "Bash(git branch --list*)",
            "Bash(git branch -a*)",
            "Bash(git branch -r*)",
            "Bash(git branch -v*)",
            "Bash(git branch --show-current*)",
            "Bash(git remote*)",
            "Bash(git symbolic-ref*)",
            "Bash(git ls-files*)",
            "Bash(git ls-tree*)",
            "Bash(git blame*)",
            "Bash(git describe*)",
            "Bash(git fetch*)",
            "Bash(git worktree list*)",
            "Bash(git config --get*)",
            "Bash(git config --list*)",
            "Bash(git tag -l*)",
            "Bash(git tag --list*)",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let writes: Vec<String> = [
            "Edit",
            "Write",
            "NotebookEdit",
            "Agent",
            "Skill",
            "Bash(mkdir *)",
            "Bash(touch *)",
            "Bash(cp *)",
            "Bash(mv *)",
            "Bash(rm *)",
            "Bash(sed *)",
            "Bash(awk *)",
            "Bash(npm install*)",
            "Bash(npm ci*)",
            // Git that changes this worktree's own branches.
            "Bash(git add*)",
            "Bash(git commit*)",
            "Bash(git checkout*)",
            "Bash(git switch*)",
            "Bash(git restore*)",
            "Bash(git stash*)",
            "Bash(git rm*)",
            "Bash(git mv*)",
            "Bash(git branch*)",
            "Bash(git tag*)",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let merges: Vec<String> = [
            "Bash(git merge*)",
            "Bash(git cherry-pick*)",
            "Bash(git revert*)",
            "Bash(git worktree prune*)",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let destructive: Vec<String> = [
            "Bash(rm -r*)",
            "Bash(git push --force*)",
            "Bash(git push -f*)",
            "Bash(git reset --hard*)",
            "Bash(git clean*)",
        ]
        .into_iter()
        .map(String::from)
        .collect();
        let trusted: Vec<String> = reads.iter().chain(writes.iter()).cloned().collect();
        let integrator: Vec<String> = trusted.iter().chain(merges.iter()).cloned().collect();
        vec![
            PermissionProfile {
                name: Self::SUPERVISED.into(),
                mode: "default".into(),
                allowed_tools: reads,
                disallowed_tools: destructive.clone(),
            },
            PermissionProfile {
                name: Self::TRUSTED.into(),
                mode: "acceptEdits".into(),
                allowed_tools: trusted,
                disallowed_tools: destructive.clone(),
            },
            PermissionProfile {
                name: Self::INTEGRATOR.into(),
                mode: "acceptEdits".into(),
                allowed_tools: integrator,
                disallowed_tools: destructive,
            },
        ]
    }
}

/// Up to four uppercase letters from the initials of the name, falling back
/// to the first letters, so "Agentic Kanban" becomes "AK" and "zipos" becomes "ZIP".
pub fn derive_key_prefix(name: &str) -> String {
    let words: Vec<&str> = name
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    let prefix: String = if words.len() >= 2 {
        words
            .iter()
            .take(4)
            .filter_map(|w| w.chars().next())
            .collect()
    } else {
        name.chars()
            .filter(|c| c.is_alphanumeric())
            .take(3)
            .collect()
    };
    let prefix = prefix.to_uppercase();
    if prefix.is_empty() {
        "T".to_string()
    } else {
        prefix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_prefixes() {
        assert_eq!(derive_key_prefix("Agentic Kanban"), "AK");
        assert_eq!(derive_key_prefix("zipos"), "ZIP");
        assert_eq!(derive_key_prefix("my-big-data-platform-x"), "MBDP");
        assert_eq!(derive_key_prefix("!!!"), "T");
    }

    #[test]
    fn home_config_round_trips_through_toml() {
        let mut cfg = HomeConfig::default();
        cfg.projects.push(ProjectEntry {
            id: ProjectId::new(),
            name: "Demo".into(),
            path: PathBuf::from("C:/dev/demo"),
            worktree_base: None,
        });
        let text = toml::to_string_pretty(&cfg).unwrap();
        let back: HomeConfig = toml::from_str(&text).unwrap();
        assert_eq!(cfg, back);
    }

    #[test]
    fn policy_round_trips_and_tolerates_missing_fields() {
        let policy = ProjectPolicy::new("Demo");
        let text = toml::to_string_pretty(&policy).unwrap();
        let back: ProjectPolicy = toml::from_str(&text).unwrap();
        assert_eq!(policy, back);

        let minimal: ProjectPolicy = toml::from_str("name = \"X\"\n").unwrap();
        assert_eq!(minimal.name, "X");
        assert_eq!(minimal.columns.len(), 5);
        assert_eq!(minimal.guardrails.ping_pong_moves, 6);
    }

    #[test]
    fn empty_home_config_parses() {
        let cfg: HomeConfig = toml::from_str("").unwrap();
        assert!(cfg.projects.is_empty());
        assert_eq!(cfg.settings.max_concurrent_runs, 3);
    }
}
