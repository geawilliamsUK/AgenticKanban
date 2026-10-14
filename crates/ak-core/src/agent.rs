//! Agent definitions and the agent entity.
//!
//! A definition is one markdown file in Claude Code's subagent format: YAML
//! frontmatter followed by a markdown body. The file is the source of truth;
//! the `Agent` row in the store is a cache of it plus runtime state.

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::actor::Actor;
use crate::error::CoreError;
use crate::ids::{ActorId, RunId};
use crate::Timestamp;

pub const DEFINITION_FILE: &str = "agent.md";
pub const MEMORY_FILE: &str = "memory.md";

/// Per-agent overrides of the project's guardrail budgets. `None` means use
/// the project default.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Budgets {
    pub max_turns_per_run: Option<u32>,
    pub max_usd_per_run: Option<f64>,
    pub max_usd_per_day: Option<f64>,
    pub max_wall_clock_minutes: Option<u32>,
}

impl Budgets {
    pub fn is_empty(&self) -> bool {
        *self == Budgets::default()
    }
}

/// The YAML frontmatter. `name` is the handle. Keys Claude Code uses for its
/// own subagents, such as `tools`, are preserved in `extra` so an imported file
/// renders back out unchanged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Frontmatter {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default = "default_profile")]
    pub permission_profile: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub budgets: Option<Budgets>,
    #[serde(flatten)]
    pub extra: BTreeMap<String, serde_norway::Value>,
}

fn default_profile() -> String {
    crate::config::PermissionProfile::SUPERVISED.to_string()
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgentDefinition {
    pub frontmatter: Frontmatter,
    /// Markdown, without the frontmatter.
    pub body: String,
}

impl AgentDefinition {
    pub fn handle(&self) -> &str {
        &self.frontmatter.name
    }

    /// Parses a definition file. The frontmatter must be present and must
    /// name a valid handle.
    pub fn parse(text: &str) -> Result<Self, CoreError> {
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let (yaml, body) = split_frontmatter(text)
            .ok_or_else(|| CoreError::validation("missing YAML frontmatter (--- ... ---)"))?;
        // Models and humans both write `description: Owns X: involve for Y`,
        // which YAML rejects. Retry once with such values quoted.
        let frontmatter: Frontmatter = match serde_norway::from_str(yaml) {
            Ok(fm) => fm,
            Err(first) => serde_norway::from_str(&repair_frontmatter(yaml))
                .map_err(|_| CoreError::validation(format!("invalid frontmatter: {first}")))?,
        };
        Actor::validate_handle(&frontmatter.name)?;
        Ok(Self {
            frontmatter,
            body: body.to_string(),
        })
    }

    pub fn render(&self) -> String {
        let yaml = serde_norway::to_string(&self.frontmatter).unwrap_or_default();
        let body = self.body.trim_end();
        format!("---\n{yaml}---\n\n{body}\n")
    }

    /// The house skeleton for a new agent: identity line, ownership contract,
    /// a Team section for Integrate to fill, and room for conventions.
    pub fn template(handle: &str, role: &str) -> Self {
        let role = if role.trim().is_empty() {
            "team member"
        } else {
            role.trim()
        };
        let body = format!(
            "# @{handle} — {role}\n\n\
             You are **@{handle}**, one of the agents on this project's team. You act on the \
             project only through your worktree and the board tools, as the shared conventions \
             describe.\n\n\
             **You own:** \n\
             **You review:** \n\
             **You never:** \n\
             **Done means:** \n\n\
             ## Team\n\n\
             Integrate fills this in once other agents exist.\n\n\
             ## Background\n\n\
             Stack, non-negotiable conventions, where to look first, a before-you-finish checklist.\n"
        );
        Self {
            frontmatter: Frontmatter {
                name: handle.to_string(),
                description: if role == "team member" {
                    "Describe what this agent owns and when to involve it.".to_string()
                } else {
                    role.to_string()
                },
                model: None,
                permission_profile: default_profile(),
                budgets: None,
                extra: BTreeMap::new(),
            },
            body,
        }
    }
}

/// Quotes top-level plain scalar values that YAML would misread: ones that
/// contain `: ` or ` #`, or end with `:`. Quoted values, block scalars, flow
/// collections and indented continuation lines are left alone.
fn repair_frontmatter(yaml: &str) -> String {
    let mut out = String::with_capacity(yaml.len() + 16);
    for line in yaml.lines() {
        out.push_str(&repair_line(line));
        out.push('\n');
    }
    out
}

fn repair_line(line: &str) -> String {
    if line.starts_with([' ', '\t', '#']) {
        return line.to_string();
    }
    let Some((key, value)) = line.split_once(": ") else {
        return line.to_string();
    };
    let value = value.trim();
    let Some(first) = value.chars().next() else {
        return line.to_string();
    };
    if matches!(first, '"' | '\'' | '>' | '|' | '[' | '{' | '&' | '*' | '!') {
        return line.to_string();
    }
    let risky = value.contains(": ") || value.ends_with(':') || value.contains(" #");
    if !risky {
        return line.to_string();
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    format!("{key}: \"{escaped}\"")
}

/// Splits `---\n<yaml>\n---\n<body>`. Returns `(yaml, body)`.
fn split_frontmatter(text: &str) -> Option<(&str, &str)> {
    let rest = text.strip_prefix("---")?;
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))?;
    // Find the closing delimiter on its own line.
    let mut offset = 0;
    for line in rest.split_inclusive('\n') {
        if line.trim_end_matches(['\r', '\n']) == "---" {
            let yaml = &rest[..offset];
            let body = &rest[offset + line.len()..];
            return Some((yaml, body.trim_start_matches(['\r', '\n'])));
        }
        offset += line.len();
    }
    None
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentState {
    Idle,
    Scheduled,
    Starting,
    Running,
    Stopping,
    Errored,
    Cooldown,
    Halted,
}

impl AgentState {
    pub fn as_str(&self) -> &'static str {
        match self {
            AgentState::Idle => "idle",
            AgentState::Scheduled => "scheduled",
            AgentState::Starting => "starting",
            AgentState::Running => "running",
            AgentState::Stopping => "stopping",
            AgentState::Errored => "errored",
            AgentState::Cooldown => "cooldown",
            AgentState::Halted => "halted",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "idle" => Some(AgentState::Idle),
            "scheduled" => Some(AgentState::Scheduled),
            "starting" => Some(AgentState::Starting),
            "running" => Some(AgentState::Running),
            "stopping" => Some(AgentState::Stopping),
            "errored" => Some(AgentState::Errored),
            "cooldown" => Some(AgentState::Cooldown),
            "halted" => Some(AgentState::Halted),
            _ => None,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            AgentState::Idle => "Idle",
            AgentState::Scheduled => "Scheduled",
            AgentState::Starting => "Starting",
            AgentState::Running => "Running",
            AgentState::Stopping => "Stopping",
            AgentState::Errored => "Errored",
            AgentState::Cooldown => "Cooldown",
            AgentState::Halted => "Halted",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(
            self,
            AgentState::Scheduled
                | AgentState::Starting
                | AgentState::Running
                | AgentState::Stopping
        )
    }
}

/// The store's view of an agent: cached definition fields plus runtime state.
#[derive(Debug, Clone, PartialEq)]
pub struct Agent {
    pub actor_id: ActorId,
    pub handle: String,
    pub description: String,
    pub model: Option<String>,
    pub permission_profile: String,
    pub budgets: Option<Budgets>,
    pub state: AgentState,
    pub current_run_id: Option<RunId>,
    pub cooldown_until: Option<Timestamp>,
    /// Set when the definition file disappeared. The agent is retired, not
    /// deleted, because tickets and events reference it.
    pub missing_since: Option<Timestamp>,
    pub definition_path: PathBuf,
    pub memory_path: PathBuf,
}

impl Agent {
    pub fn from_definition(
        actor_id: ActorId,
        def: &AgentDefinition,
        definition_path: PathBuf,
        memory_path: PathBuf,
    ) -> Self {
        Self {
            actor_id,
            handle: def.frontmatter.name.clone(),
            description: def.frontmatter.description.clone(),
            model: def.frontmatter.model.clone(),
            permission_profile: def.frontmatter.permission_profile.clone(),
            budgets: def.frontmatter.budgets.clone().filter(|b| !b.is_empty()),
            state: AgentState::Idle,
            current_run_id: None,
            cooldown_until: None,
            missing_since: None,
            definition_path,
            memory_path,
        }
    }

    pub fn is_retired(&self) -> bool {
        self.missing_since.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "---\nname: dba\ndescription: >-\n  Database administrator @dba: owns the schema.\n  Involve for schema changes.\nmodel: sonnet\npermission_profile: trusted\ntools: Read, Write, Bash\n---\n\n# @dba — database administrator\n\n**You own:** `migrations/`\n";

    #[test]
    fn parses_frontmatter_and_body() {
        let def = AgentDefinition::parse(SAMPLE).unwrap();
        assert_eq!(def.handle(), "dba");
        assert_eq!(
            def.frontmatter.description,
            "Database administrator @dba: owns the schema. Involve for schema changes."
        );
        assert_eq!(def.frontmatter.model.as_deref(), Some("sonnet"));
        assert_eq!(def.frontmatter.permission_profile, "trusted");
        assert!(def.body.starts_with("# @dba"));
        assert!(
            def.frontmatter.extra.contains_key("tools"),
            "Claude Code keys are preserved"
        );
    }

    #[test]
    fn render_round_trips() {
        let def = AgentDefinition::parse(SAMPLE).unwrap();
        let text = def.render();
        let again = AgentDefinition::parse(&text).unwrap();
        assert_eq!(def, again);
        assert!(text.contains("tools: Read, Write, Bash"));
    }

    #[test]
    fn defaults_apply_to_minimal_frontmatter() {
        let def = AgentDefinition::parse("---\nname: qa\n---\nbody\n").unwrap();
        assert_eq!(def.frontmatter.permission_profile, "supervised");
        assert!(def.frontmatter.model.is_none());
        assert_eq!(def.body, "body\n");
    }

    #[test]
    fn rejects_missing_frontmatter_and_bad_handles() {
        assert!(AgentDefinition::parse("# no frontmatter\n").is_err());
        assert!(AgentDefinition::parse("---\nname: Bad Handle\n---\nx").is_err());
        assert!(AgentDefinition::parse("---\ndescription: no name\n---\nx").is_err());
    }

    #[test]
    fn handles_crlf_files() {
        let text = SAMPLE.replace('\n', "\r\n");
        let def = AgentDefinition::parse(&text).unwrap();
        assert_eq!(def.handle(), "dba");
    }

    #[test]
    fn template_parses_and_has_the_skeleton() {
        let def = AgentDefinition::template("backend", "Backend developer");
        let text = def.render();
        let parsed = AgentDefinition::parse(&text).unwrap();
        assert_eq!(parsed.handle(), "backend");
        for section in [
            "**You own:**",
            "**You review:**",
            "**You never:**",
            "**Done means:**",
            "## Team",
        ] {
            assert!(parsed.body.contains(section), "{section}");
        }
    }

    #[test]
    fn agent_from_definition_drops_empty_budgets() {
        let mut def = AgentDefinition::template("ops", "Ops");
        def.frontmatter.budgets = Some(Budgets::default());
        let agent = Agent::from_definition(ActorId::new(), &def, "a".into(), "m".into());
        assert!(agent.budgets.is_none());
        assert_eq!(agent.state, AgentState::Idle);
    }

    #[test]
    fn repairs_unquoted_colons_in_plain_values() {
        let text = "---\nname: backend\ndescription: Owns the API: routes, auth. Involve @backend for \"server\" work.\npermission_profile: trusted\n---\nbody\n";
        let def = AgentDefinition::parse(text).unwrap();
        assert_eq!(
            def.frontmatter.description,
            "Owns the API: routes, auth. Involve @backend for \"server\" work."
        );
        assert_eq!(def.frontmatter.permission_profile, "trusted");
        // Renders back out as valid YAML and parses again.
        let again = AgentDefinition::parse(&def.render()).unwrap();
        assert_eq!(again, def);
    }

    #[test]
    fn repair_leaves_well_formed_lines_alone() {
        assert_eq!(repair_line("name: dba"), "name: dba");
        assert_eq!(repair_line("description: >-"), "description: >-");
        assert_eq!(
            repair_line("  continued: text: here"),
            "  continued: text: here"
        );
        assert_eq!(repair_line("tools: \"a: b\""), "tools: \"a: b\"");
        assert_eq!(repair_line("tags: [a, b]"), "tags: [a, b]");
        assert_eq!(repair_line("# a comment: yes"), "# a comment: yes");
        assert_eq!(repair_line("k: v # note"), "k: \"v # note\"");
        assert_eq!(repair_line("k: ends with:"), "k: \"ends with:\"");
    }

    #[test]
    fn genuinely_broken_frontmatter_still_fails() {
        assert!(AgentDefinition::parse("---\nname: [unclosed\n---\nbody\n").is_err());
    }
}
