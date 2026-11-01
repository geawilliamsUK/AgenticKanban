//! The Desk's tools for running the team itself: agents' definitions,
//! memory and state, and the project's settings. Only a Desk session sees
//! them; the agents never do. They go through the same files and reconcile
//! as the app's own Team and Settings views, so every change shows there.

use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use ak_core::agent::{AgentDefinition, Budgets};
use ak_core::config::ProjectPolicy;
use ak_core::ActorKind;
use ak_runtime::agents;

use crate::tools::{parse_args, schema, ToolError, ToolSpec};
use crate::{Bridge, BridgeEvent, RunContext};

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HandleInput {
    /// The agent's handle, without the @.
    pub handle: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentCreateInput {
    /// Lowercase letters, digits and hyphens, without the @.
    pub handle: String,
    /// One or two sentences: what it owns and when to involve it. Teammates
    /// read this to decide who gets what.
    pub description: String,
    /// The definition's markdown body: identity, the four contract lines
    /// (**You own:**, **You review:**, **You never:**, **Done means:**), a
    /// `## Team` section and anything else it must know. Left out, the house
    /// skeleton (or the preset) is used.
    pub body: Option<String>,
    /// "lead" starts from the team lead preset.
    pub preset: Option<String>,
    /// opus, sonnet, haiku, fable or a full model id; left out, the project
    /// default.
    pub model: Option<String>,
    /// A permission profile from project_settings_get; left out, supervised.
    pub permission_profile: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentUpdateInput {
    pub handle: String,
    pub description: Option<String>,
    /// Replaces the whole markdown body.
    pub body: Option<String>,
    /// A model, or "default" for the project default.
    pub model: Option<String>,
    pub permission_profile: Option<String>,
    /// Per-agent budgets; 0 clears one back to the project's guardrail.
    pub max_turns_per_run: Option<u32>,
    pub max_usd_per_run: Option<f64>,
    pub max_usd_per_day: Option<f64>,
    pub max_wall_clock_minutes: Option<u32>,
    /// The share of the 5-hour subscription window, in percent, its runs may
    /// use per window; 0 clears it.
    pub max_five_hour_pct: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentMessageInput {
    pub handle: String,
    /// What the agent should know or do differently.
    pub text: String,
    /// Stop the step in progress first, for a change of plan that cannot
    /// wait. Otherwise the agent reads it at its next step.
    #[serde(default)]
    pub interrupt: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AgentMemoryInput {
    pub handle: String,
    /// The agent's whole memory file, replacing what is there.
    pub content: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SettingsUpdateInput {
    /// Fields to change, in the shape project_settings_get shows: e.g.
    /// {"guardrails": {"max_usd_per_run": 3}, "default_model": "sonnet"}.
    /// Objects merge; anything else replaces.
    pub changes: Value,
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
pub struct Empty {}

pub(crate) const SPECS: &[ToolSpec] = &[
    ToolSpec {
        name: "agent_get",
        description: "An agent's full definition file, its memory and its state.",
        schema: schema::<HandleInput>,
    },
    ToolSpec {
        name: "agent_create",
        description: "Add an agent to the team. It can be assigned and woken at once. Use preset \"lead\" for a team lead.",
        schema: schema::<AgentCreateInput>,
    },
    ToolSpec {
        name: "agent_update",
        description: "Change an agent's description, body, model, permission profile or budgets. Fields left out stay as they are.",
        schema: schema::<AgentUpdateInput>,
    },
    ToolSpec {
        name: "agent_message",
        description: "Tell a running agent something mid-run: it reads it at its next step, or at once with interrupt. For an agent that is not running, comment on its ticket or agent_activate it instead.",
        schema: schema::<AgentMessageInput>,
    },
    ToolSpec {
        name: "agent_memory_update",
        description: "Replace an agent's memory file, which it reads at the start of every run.",
        schema: schema::<AgentMemoryInput>,
    },
    ToolSpec {
        name: "agent_retire",
        description: "Retire an agent: it stops being woken or assigned. Its history stays on the board.",
        schema: schema::<HandleInput>,
    },
    ToolSpec {
        name: "agent_halt",
        description: "Stop an agent's run if it has one and keep it from waking until agent_resume.",
        schema: schema::<HandleInput>,
    },
    ToolSpec {
        name: "agent_resume",
        description: "Let a halted or cooling-down agent wake again.",
        schema: schema::<HandleInput>,
    },
    ToolSpec {
        name: "project_settings_get",
        description: "The project's settings: name, columns, guardrails, models, permission profiles, the Desk's profile, push policy, the analyst cadence.",
        schema: schema::<Empty>,
    },
    ToolSpec {
        name: "project_settings_update",
        description: "Change project settings. The shared id cannot change, and every permission profile an agent uses must still exist.",
        schema: schema::<SettingsUpdateInput>,
    },
];

/// Runs a Desk team tool, or None when `name` is not one.
pub(crate) async fn run(
    bridge: &Bridge,
    ctx: &RunContext,
    name: &str,
    args: Value,
) -> Option<Result<String, ToolError>> {
    if !ctx.is_desk() || !SPECS.iter().any(|s| s.name == name) {
        return None;
    }
    Some(run_inner(bridge, ctx, name, args).await)
}

async fn run_inner(
    bridge: &Bridge,
    ctx: &RunContext,
    name: &str,
    args: Value,
) -> Result<String, ToolError> {
    let project = &ctx.project;
    let store = ctx.store();
    match name {
        "agent_get" => {
            let input: HandleInput = parse_args(args)?;
            let handle = bare(&input.handle);
            let def = project
                .agents
                .read_definition(&handle)
                .map_err(|e| format!("no agent @{handle}: {e}"))?;
            let memory = project.agents.read_memory(&handle).unwrap_or_default();
            let state = store
                .agent_by_handle(&handle)
                .await?
                .map(|a| a.state.label().to_string())
                .unwrap_or_else(|| "unknown".into());
            Ok(format!(
                "@{handle} is {state}.\n\n----- agent.md -----\n{}\n----- memory.md -----\n{}",
                def.render(),
                if memory.trim().is_empty() {
                    "(empty)"
                } else {
                    memory.trim()
                }
            ))
        }
        "agent_create" => {
            let input: AgentCreateInput = parse_args(args)?;
            let handle = bare(&input.handle);
            if project.agents.definition_path(&handle).is_file() {
                return Err(format!("@{handle} already exists; use agent_update").into());
            }
            if store.agent_by_handle(&handle).await?.is_none()
                && store
                    .actors()
                    .await?
                    .iter()
                    .any(|a| a.handle == handle && a.kind != ActorKind::Agent)
            {
                return Err(format!("@{handle} is taken by someone who is not an agent").into());
            }
            let mut def = match input.preset.as_deref() {
                Some("lead") => {
                    let mut d = AgentDefinition::lead();
                    d.body = d.body.replace("@lead", &format!("@{handle}"));
                    d.frontmatter.name = handle.clone();
                    d
                }
                Some(other) => {
                    return Err(format!("no preset {other:?}; the one preset is \"lead\"").into())
                }
                None => AgentDefinition::template(&handle, &input.description),
            };
            def.frontmatter.description = input.description.trim().to_string();
            if let Some(body) = input.body {
                def.body = body;
            }
            if let Some(m) = input.model {
                def.frontmatter.model = model(&m);
            }
            if let Some(p) = input.permission_profile {
                check_profile(&project.policy(), &p)?;
                def.frontmatter.permission_profile = p;
            }
            save(ctx, &def).await?;
            Ok(format!(
                "created @{handle} ({}); it can be assigned now. Consider updating teammates' ## Team sections so they know about it.",
                def.frontmatter.description
            ))
        }
        "agent_update" => {
            let input: AgentUpdateInput = parse_args(args)?;
            let handle = bare(&input.handle);
            let mut def = project
                .agents
                .read_definition(&handle)
                .map_err(|e| format!("no agent @{handle}: {e}"))?;
            if let Some(d) = input.description {
                def.frontmatter.description = d.trim().to_string();
            }
            if let Some(b) = input.body {
                def.body = b;
            }
            if let Some(m) = input.model {
                def.frontmatter.model = model(&m);
            }
            if let Some(p) = input.permission_profile {
                check_profile(&project.policy(), &p)?;
                def.frontmatter.permission_profile = p;
            }
            let mut budgets = def.frontmatter.budgets.clone().unwrap_or_default();
            let positive_u = |v: Option<u32>, old: Option<u32>| match v {
                Some(0) => None,
                Some(n) => Some(n),
                None => old,
            };
            let positive_f = |v: Option<f64>, old: Option<f64>| match v {
                Some(n) if n <= 0.0 => None,
                Some(n) => Some(n),
                None => old,
            };
            budgets = Budgets {
                max_turns_per_run: positive_u(input.max_turns_per_run, budgets.max_turns_per_run),
                max_usd_per_run: positive_f(input.max_usd_per_run, budgets.max_usd_per_run),
                max_usd_per_day: positive_f(input.max_usd_per_day, budgets.max_usd_per_day),
                max_wall_clock_minutes: positive_u(
                    input.max_wall_clock_minutes,
                    budgets.max_wall_clock_minutes,
                ),
                max_five_hour_pct: positive_u(input.max_five_hour_pct, budgets.max_five_hour_pct),
            };
            def.frontmatter.budgets = (!budgets.is_empty()).then_some(budgets);
            save(ctx, &def).await?;
            Ok(format!("updated @{handle}"))
        }
        "agent_message" => {
            let input: AgentMessageInput = parse_args(args)?;
            let handle = bare(&input.handle);
            let agent = store
                .agent_by_handle(&handle)
                .await?
                .filter(|a| !a.is_retired())
                .ok_or_else(|| format!("no agent @{handle}"))?;
            if !agent.state.is_active() {
                return Err(format!(
                    "@{handle} is not running ({}); comment on its ticket, or agent_activate it with this as the reason",
                    agent.state.label().to_lowercase()
                )
                .into());
            }
            bridge.message_agent(
                project.id(),
                agent.actor_id,
                ctx.agent.handle.clone(),
                input.text,
                input.interrupt,
            );
            Ok(if input.interrupt {
                format!("@{handle}'s current step is being interrupted; it reads your message next")
            } else {
                format!("@{handle} reads your message at its next step")
            })
        }
        "agent_memory_update" => {
            let input: AgentMemoryInput = parse_args(args)?;
            let handle = bare(&input.handle);
            if !project.agents.definition_path(&handle).is_file() {
                return Err(format!("no agent @{handle}").into());
            }
            project
                .agents
                .write_memory(&handle, &input.content)
                .map_err(|e| e.to_string())?;
            Ok(format!(
                "@{handle}'s memory saved ({} characters)",
                input.content.chars().count()
            ))
        }
        "agent_retire" => {
            let input: HandleInput = parse_args(args)?;
            let handle = bare(&input.handle);
            let agent = store
                .agent_by_handle(&handle)
                .await?
                .ok_or_else(|| format!("no agent @{handle}"))?;
            bridge.control_agent(project.id(), agent.actor_id, true);
            project.agents.remove(&handle).map_err(|e| e.to_string())?;
            agents::reconcile(&project.agents, project.store.as_ref())
                .await
                .map_err(|e| e.to_string())?;
            Ok(format!("retired @{handle}; its run, if any, was stopped"))
        }
        "agent_halt" | "agent_resume" => {
            let input: HandleInput = parse_args(args)?;
            let handle = bare(&input.handle);
            let agent = store
                .agent_by_handle(&handle)
                .await?
                .filter(|a| !a.is_retired())
                .ok_or_else(|| format!("no agent @{handle}"))?;
            let halt = name == "agent_halt";
            bridge.control_agent(project.id(), agent.actor_id, halt);
            Ok(if halt {
                format!("halting @{handle}: its run, if any, stops and it will not wake until agent_resume")
            } else {
                format!("@{handle} may wake again")
            })
        }
        "project_settings_get" => {
            let policy = project.policy();
            let text = toml::to_string_pretty(&policy).map_err(|e| e.to_string())?;
            Ok(format!(
                "Project settings (project.toml). Change them with project_settings_update.\n\n{text}"
            ))
        }
        "project_settings_update" => {
            let input: SettingsUpdateInput = parse_args(args)?;
            let Some(file) = &ctx.policy_file else {
                return Err("the project's settings file is not known to this session".into());
            };
            let current = project.policy();
            let next = apply_changes(&current, &input.changes)?;
            check_policy(ctx, &current, &next).await?;
            ak_runtime::config::save_policy(file, &next).map_err(|e| e.to_string())?;
            *project.policy.write().expect("policy lock") = next.clone();
            bridge.emit(BridgeEvent::PolicyChanged {
                project: project.id(),
            });
            let changed: Vec<String> = input
                .changes
                .as_object()
                .map(|o| o.keys().cloned().collect())
                .unwrap_or_default();
            Ok(format!("settings saved: {}", changed.join(", ")))
        }
        _ => Err(format!("unknown tool {name}").into()),
    }
}

fn bare(handle: &str) -> String {
    handle.trim().trim_start_matches('@').to_lowercase()
}

/// A model choice: "default" or empty means the project default.
fn model(m: &str) -> Option<String> {
    let m = m.trim();
    (!m.is_empty() && m != "default").then(|| m.to_string())
}

fn check_profile(policy: &ProjectPolicy, profile: &str) -> Result<(), ToolError> {
    if policy.permission_profiles.iter().any(|p| p.name == profile) {
        Ok(())
    } else {
        let names: Vec<&str> = policy
            .permission_profiles
            .iter()
            .map(|p| p.name.as_str())
            .collect();
        Err(format!(
            "no permission profile {profile:?}; there are {}",
            names.join(", ")
        )
        .into())
    }
}

/// Writes a definition and reconciles, as the Team view's Save does. A
/// definition the parser rejects is reported, and the old file is restored.
async fn save(ctx: &RunContext, def: &AgentDefinition) -> Result<(), ToolError> {
    let project = &ctx.project;
    let rendered = def.render();
    AgentDefinition::parse(&rendered).map_err(|e| format!("the definition is not valid: {e}"))?;
    let previous = std::fs::read_to_string(project.agents.definition_path(def.handle())).ok();
    project
        .agents
        .write_definition(def)
        .map_err(|e| e.to_string())?;
    let report = agents::reconcile(&project.agents, project.store.as_ref())
        .await
        .map_err(|e| e.to_string())?;
    if let Some((_, msg)) = report.errors.iter().find(|(h, _)| h == def.handle()) {
        match previous {
            Some(text) => {
                let _ = std::fs::write(project.agents.definition_path(def.handle()), text);
            }
            None => {
                let _ = project.agents.remove(def.handle());
            }
        }
        let _ = agents::reconcile(&project.agents, project.store.as_ref()).await;
        return Err(format!("the definition was rejected: {msg}").into());
    }
    Ok(())
}

/// The policy with `changes` merged in: objects merge key by key, anything
/// else replaces.
fn apply_changes(policy: &ProjectPolicy, changes: &Value) -> Result<ProjectPolicy, ToolError> {
    if !changes.is_object() {
        return Err("changes must be an object of fields to change".into());
    }
    let mut value = serde_json::to_value(policy).map_err(|e| e.to_string())?;
    merge(&mut value, changes);
    serde_json::from_value(value).map_err(|e| format!("those settings do not fit: {e}").into())
}

fn merge(into: &mut Value, changes: &Value) {
    match (into, changes) {
        (Value::Object(a), Value::Object(b)) => {
            for (k, v) in b {
                match a.get_mut(k) {
                    Some(existing) if existing.is_object() && v.is_object() => merge(existing, v),
                    _ => {
                        a.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (slot, v) => *slot = v.clone(),
    }
}

/// What the Settings view would refuse.
async fn check_policy(
    ctx: &RunContext,
    current: &ProjectPolicy,
    next: &ProjectPolicy,
) -> Result<(), ToolError> {
    if next.shared_id != current.shared_id {
        return Err(
            "the shared id cannot change; it ties exported snapshots to this project".into(),
        );
    }
    if next.name.trim().is_empty() {
        return Err("the project name cannot be empty".into());
    }
    if next.columns.is_empty() {
        return Err("a board needs at least one column".into());
    }
    for agent in ctx.store().agents().await? {
        if agent.is_retired() {
            continue;
        }
        if !next
            .permission_profiles
            .iter()
            .any(|p| p.name == agent.permission_profile)
        {
            return Err(format!(
                "@{} uses permission profile {:?}, which these settings remove",
                agent.handle, agent.permission_profile
            )
            .into());
        }
    }
    if !next
        .permission_profiles
        .iter()
        .any(|p| p.name == next.desk_permission_profile)
    {
        return Err(format!(
            "the Desk works under permission profile {:?}, which these settings remove",
            next.desk_permission_profile
        )
        .into());
    }
    if next
        .permission_profiles
        .iter()
        .any(|p| p.mode == "bypassPermissions")
    {
        return Err(
            "bypassPermissions is refused: every profile must keep the hooks in charge".into(),
        );
    }
    Ok(())
}

impl From<&str> for ToolError {
    fn from(m: &str) -> Self {
        ToolError::Message(m.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn changes_merge_into_the_policy() {
        let policy = ProjectPolicy::new("Demo");
        let next = apply_changes(
            &policy,
            &json!({"guardrails": {"max_usd_per_run": 3.5}, "default_model": "sonnet"}),
        )
        .unwrap();
        assert_eq!(next.guardrails.max_usd_per_run, 3.5);
        assert_eq!(
            next.guardrails.max_turns_per_run, policy.guardrails.max_turns_per_run,
            "untouched fields stay"
        );
        assert_eq!(next.default_model.as_deref(), Some("sonnet"));
        assert!(apply_changes(&policy, &json!({"guardrails": "nope"})).is_err());
        assert!(apply_changes(&policy, &json!(3)).is_err());
    }
}
