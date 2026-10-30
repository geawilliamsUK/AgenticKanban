//! The team analyst: a digest of what the team did over a window, the
//! utility call that reads it and proposes changes, and the means to apply a
//! proposal.
//!
//! The analyst is not a team member. It has no handle, no worktree and no
//! board tools; it reads a bounded digest plus the agents' definitions and
//! returns structured recommendations. The human applies or dismisses each
//! one; nothing it returns is applied by itself.

use std::collections::{BTreeMap, HashMap};

use chrono::Utc;
use serde::{Deserialize, Serialize};

use ak_core::agent::AgentDefinition;
use ak_core::approval::ApprovalState;
use ak_core::config::{GuardrailThresholds, PermissionProfile, ProjectPolicy};
use ak_core::event::{EventKind, Subject};
use ak_core::ids::ActorId;
use ak_core::permissions::PermissionRule;
use ak_core::run::{ExitReason, RunTrigger};
use ak_core::utility::UtilityKind;
use ak_core::{ActorKind, Timestamp};

use crate::error::RuntimeError;
use crate::registry::OpenProject;
use crate::utility::UtilityRequest;

/// How many events the digest reads at most.
const EVENT_LIMIT: u32 = 20_000;
/// Distinct approval summaries kept per agent.
const APPROVAL_EXAMPLES: usize = 8;
/// Runs listed per agent as the ones worth a look.
const WORST_RUNS: usize = 3;
/// Previous analyses the digest reminds the model of.
const PREVIOUS_ANALYSES: u32 = 5;
/// The most recommendations a reply may carry.
const MAX_RECOMMENDATIONS: usize = 12;

// ---------------------------------------------------------------- digest

/// What the team did over a window, per agent and for the project.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Digest {
    pub project: String,
    pub since: Timestamp,
    pub until: Timestamp,
    /// Live agents, highest cost first.
    pub agents: Vec<AgentStats>,
    /// Every guardrail trip in the window, oldest first.
    pub trips: Vec<TripLine>,
    pub frozen_tickets: Vec<String>,
    pub thresholds: GuardrailThresholds,
    pub profiles: Vec<PermissionProfile>,
    /// Earlier analyses, newest first.
    pub previous: Vec<PreviousAnalysis>,
}

impl Digest {
    pub fn total_cost(&self) -> f64 {
        self.agents.iter().map(|a| a.cost_usd).sum()
    }

    pub fn total_runs(&self) -> u32 {
        self.agents.iter().map(|a| a.runs).sum()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct AgentStats {
    pub handle: String,
    pub model: Option<String>,
    pub profile: String,
    pub state: String,
    pub runs: u32,
    /// Exit reason label to count.
    pub exits: BTreeMap<String, u32>,
    pub turns: u32,
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub tool_calls: u32,
    pub board_calls: u32,
    pub tool_errors: u32,
    pub commits: u32,
    pub merges: u32,
    pub ticket_moves: u32,
    pub comments: u32,
    pub messages: u32,
    /// Handle mentioned to count.
    pub mentions_sent: BTreeMap<String, u32>,
    pub mentions_received: u32,
    /// Completed runs that produced no commit, move, comment or message.
    pub idle_runs: u32,
    pub approvals: u32,
    pub approvals_denied: u32,
    pub approvals_expired: u32,
    /// Total wait of the decided approvals.
    pub approval_wait_secs: u64,
    pub approval_examples: Vec<String>,
    /// Skip reason label to count.
    pub skips: BTreeMap<String, u32>,
    /// Guardrail trips whose subject is this agent.
    pub trips: u32,
    pub worst_runs: Vec<RunBrief>,
}

impl AgentStats {
    pub fn produced(&self) -> u32 {
        self.commits + self.merges + self.ticket_moves + self.comments + self.messages
    }

    pub fn decided_approvals(&self) -> u32 {
        self.approvals_denied + self.approvals_expired + self.approvals_approved()
    }

    /// Approvals the human allowed: everything asked that was not denied,
    /// expired or (still) pending is counted at render time, so this is the
    /// best the digest can say without a pending count.
    fn approvals_approved(&self) -> u32 {
        self.approvals
            .saturating_sub(self.approvals_denied)
            .saturating_sub(self.approvals_expired)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunBrief {
    pub started_at: Timestamp,
    pub trigger: String,
    pub exit: String,
    pub cost_usd: f64,
    pub turns: u32,
    pub error: Option<String>,
    pub result: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TripLine {
    pub at: Timestamp,
    pub rule: String,
    pub action: String,
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PreviousAnalysis {
    pub at: Timestamp,
    pub summary: String,
    pub applied: Vec<String>,
    pub dismissed: Vec<String>,
    pub undecided: u32,
}

/// Reads the window from the store and tallies it.
pub async fn build_digest(project: &OpenProject, since: Timestamp) -> Result<Digest, RuntimeError> {
    let store = project.store();
    let until = Utc::now();
    let policy = project.policy();

    let actors = store.actors().await?;
    let handles: HashMap<ActorId, String> =
        actors.iter().map(|a| (a.id, a.handle.clone())).collect();
    let is_agent: HashMap<ActorId, bool> = actors
        .iter()
        .map(|a| (a.id, a.kind == ActorKind::Agent))
        .collect();
    let agents = store.agents().await?;
    let runs = store.runs_since(since).await?;
    let approvals = store.approvals_since(since).await?;
    let events = store.events_after(since, EVENT_LIMIT).await?;
    let decisions = store.scheduler_decisions_since(since).await?;
    let tickets = store.tickets().await?;
    let calls = store
        .utility_calls_of_kind(UtilityKind::Analyse, PREVIOUS_ANALYSES)
        .await?;

    let mut stats: HashMap<ActorId, AgentStats> = agents
        .iter()
        .filter(|a| !a.is_retired())
        .map(|a| {
            (
                a.actor_id,
                AgentStats {
                    handle: a.handle.clone(),
                    model: a.model.clone(),
                    profile: a.permission_profile.clone(),
                    state: a.state.label().to_lowercase(),
                    ..Default::default()
                },
            )
        })
        .collect();

    // Runs and their outcomes.
    let mut per_agent_runs: HashMap<ActorId, Vec<&ak_core::run::AgentRun>> = HashMap::new();
    for run in &runs {
        let Some(s) = stats.get_mut(&run.agent_id) else {
            continue;
        };
        s.runs += 1;
        let exit = run
            .exit_reason
            .map(|e| e.label().to_string())
            .unwrap_or_else(|| "running".into());
        *s.exits.entry(exit).or_insert(0) += 1;
        s.turns += run.num_turns;
        s.cost_usd += run.cost_usd.unwrap_or(0.0);
        s.input_tokens += run.input_tokens;
        s.output_tokens += run.output_tokens;
        if let Some(o) = &run.outcome {
            s.tool_calls += o.total_tool_calls();
            s.board_calls += o.board_tool_calls();
            s.tool_errors += o.tool_errors;
            s.commits += o.commits;
            s.merges += o.merges;
            let produced = o.commits + o.merges + o.ticket_moves + o.comments + o.messages;
            if run.exit_reason == Some(ExitReason::Completed) && produced == 0 {
                s.idle_runs += 1;
            }
        }
        per_agent_runs.entry(run.agent_id).or_default().push(run);
    }
    for (agent, mut list) in per_agent_runs {
        // Failures first, then the dearest.
        list.sort_by(|a, b| {
            let fa = a.exit_reason != Some(ExitReason::Completed);
            let fb = b.exit_reason != Some(ExitReason::Completed);
            fb.cmp(&fa).then(
                b.cost_usd
                    .unwrap_or(0.0)
                    .partial_cmp(&a.cost_usd.unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
        });
        if let Some(s) = stats.get_mut(&agent) {
            s.worst_runs = list
                .iter()
                .take(WORST_RUNS)
                .map(|r| RunBrief {
                    started_at: r.started_at,
                    trigger: describe_trigger(&r.trigger),
                    exit: r
                        .exit_reason
                        .map(|e| e.label().to_string())
                        .unwrap_or_else(|| "running".into()),
                    cost_usd: r.cost_usd.unwrap_or(0.0),
                    turns: r.num_turns,
                    error: r.error.clone(),
                    result: r.outcome.as_ref().and_then(|o| o.result.clone()),
                })
                .collect();
        }
    }

    // Approvals.
    for a in &approvals {
        let Some(s) = stats.get_mut(&a.agent_id) else {
            continue;
        };
        s.approvals += 1;
        match a.state {
            ApprovalState::Denied => s.approvals_denied += 1,
            ApprovalState::Expired => s.approvals_expired += 1,
            ApprovalState::Approved | ApprovalState::Pending => {}
        }
        if let Some(decided) = a.decided_at {
            s.approval_wait_secs += (decided - a.created_at).num_seconds().max(0) as u64;
        }
        let example: String = a.summary.chars().take(120).collect();
        if s.approval_examples.len() < APPROVAL_EXAMPLES && !s.approval_examples.contains(&example)
        {
            s.approval_examples.push(example);
        }
    }

    // Board activity and mentions, from the event log.
    let mut trips = Vec::new();
    for e in &events {
        match &e.kind {
            EventKind::TicketMoved { .. } => {
                if let Some(s) = stats.get_mut(&e.actor_id) {
                    s.ticket_moves += 1;
                }
            }
            EventKind::TicketCommented { .. } | EventKind::TicketReplied { .. } => {
                if let Some(s) = stats.get_mut(&e.actor_id) {
                    s.comments += 1;
                }
            }
            EventKind::ThreadCreated { .. } | EventKind::ThreadMessagePosted { .. } => {
                if let Some(s) = stats.get_mut(&e.actor_id) {
                    s.messages += 1;
                }
            }
            EventKind::MentionCreated { mentioned_id, .. } => {
                let target = handles
                    .get(mentioned_id)
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                if let Some(s) = stats.get_mut(&e.actor_id) {
                    *s.mentions_sent.entry(target).or_insert(0) += 1;
                }
                if is_agent.get(mentioned_id).copied().unwrap_or(false) {
                    if let Some(s) = stats.get_mut(mentioned_id) {
                        s.mentions_received += 1;
                    }
                }
            }
            EventKind::GuardrailTripped {
                rule,
                action,
                subject,
                detail,
            } => {
                trips.push(TripLine {
                    at: e.occurred_at,
                    rule: rule.label().to_string(),
                    action: action.label().to_string(),
                    detail: detail.clone(),
                });
                if let Subject::Agent(id) = subject {
                    if let Some(s) = stats.get_mut(id) {
                        s.trips += 1;
                    }
                }
            }
            _ => {}
        }
    }

    for d in &decisions {
        if let Some(s) = stats.get_mut(&d.agent_id) {
            *s.skips.entry(d.reason.label().to_string()).or_insert(0) += 1;
        }
    }

    let mut agents: Vec<AgentStats> = stats.into_values().collect();
    agents.sort_by(|a, b| {
        b.cost_usd
            .partial_cmp(&a.cost_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.handle.cmp(&b.handle))
    });

    let previous = calls
        .iter()
        .filter_map(|c| {
            let analysis = Analysis::from_json(c.proposal.as_deref()?).ok()?;
            Some(PreviousAnalysis {
                at: c.started_at,
                summary: analysis.summary.clone(),
                applied: analysis.titles_with(Decision::Applied),
                dismissed: analysis.titles_with(Decision::Dismissed),
                undecided: analysis
                    .recommendations
                    .iter()
                    .filter(|r| r.decision.is_none())
                    .count() as u32,
            })
        })
        .collect();

    Ok(Digest {
        project: policy.name.clone(),
        since,
        until,
        agents,
        trips,
        frozen_tickets: tickets
            .iter()
            .filter(|t| t.frozen)
            .map(|t| t.key.clone())
            .collect(),
        thresholds: policy.guardrails,
        profiles: policy.permission_profiles.clone(),
        previous,
    })
}

fn describe_trigger(trigger: &RunTrigger) -> String {
    match trigger {
        RunTrigger::Notifications { ids } => format!("{} wake item(s)", ids.len()),
        RunTrigger::Activated { reason, .. } => format!("activation: {reason}"),
        RunTrigger::Manual => "manual".into(),
    }
}

/// The digest as markdown: what the model reads, and what the human can
/// read to see what the model saw.
pub fn render_digest(d: &Digest) -> String {
    let mut out = String::new();
    let hours = (d.until - d.since).num_minutes() as f64 / 60.0;
    out.push_str(&format!(
        "# Team digest: {}\n\nWindow: {} to {} ({hours:.0} hours). Costs are Claude Code's own estimates at API list prices. Totals: {} run(s), ${:.2}.\n\n",
        d.project,
        d.since.format("%Y-%m-%d %H:%M"),
        d.until.format("%Y-%m-%d %H:%M"),
        d.total_runs(),
        d.total_cost()
    ));

    out.push_str("## Agents\n\n");
    if d.agents.is_empty() {
        out.push_str("(no agents)\n\n");
    } else {
        out.push_str("| agent | model | profile | runs | completed | turns | cost | tool calls (board) | tool errors | commits | merges | moves | comments | messages | approvals (denied) | trips | not started |\n");
        out.push_str("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|\n");
        for a in &d.agents {
            let completed = a.exits.get("Completed").copied().unwrap_or(0);
            let skips: u32 = a.skips.values().sum();
            out.push_str(&format!(
                "| @{} | {} | {} | {} | {} | {} | ${:.2} | {} ({}) | {} | {} | {} | {} | {} | {} | {} ({}) | {} | {} |\n",
                a.handle,
                a.model.as_deref().unwrap_or("default"),
                a.profile,
                a.runs,
                completed,
                a.turns,
                a.cost_usd,
                a.tool_calls,
                a.board_calls,
                a.tool_errors,
                a.commits,
                a.merges,
                a.ticket_moves,
                a.comments,
                a.messages,
                a.approvals,
                a.approvals_denied,
                a.trips,
                skips
            ));
        }
        out.push('\n');
        for a in &d.agents {
            out.push_str(&format!("### @{}\n\n", a.handle));
            if a.runs == 0 {
                out.push_str(&format!("- No runs in the window (state: {}).\n", a.state));
            } else {
                let exits: Vec<String> = a
                    .exits
                    .iter()
                    .map(|(k, v)| format!("{} {v}", k.to_lowercase()))
                    .collect();
                out.push_str(&format!("- Exits: {}.\n", exits.join(", ")));
                out.push_str(&format!(
                    "- Idle runs (completed with no commit, move, comment or message): {} of {}.\n",
                    a.idle_runs, a.runs
                ));
                out.push_str(&format!(
                    "- Tokens: {} in, {} out; ${:.2} per run on average.\n",
                    a.input_tokens,
                    a.output_tokens,
                    a.cost_usd / a.runs as f64
                ));
            }
            if !a.mentions_sent.is_empty() || a.mentions_received > 0 {
                let sent: Vec<String> = a
                    .mentions_sent
                    .iter()
                    .map(|(h, n)| format!("@{h} {n}"))
                    .collect();
                out.push_str(&format!(
                    "- Mentions sent: {}; received {}.\n",
                    if sent.is_empty() {
                        "none".to_string()
                    } else {
                        sent.join(", ")
                    },
                    a.mentions_received
                ));
            }
            if a.approvals > 0 {
                let decided = a.decided_approvals();
                let wait = if decided > 0 {
                    format!(
                        "; average wait {:.0} s",
                        a.approval_wait_secs as f64 / decided as f64
                    )
                } else {
                    String::new()
                };
                out.push_str(&format!(
                    "- Approvals asked: {}, denied {}, expired {}{wait}. Examples: {}.\n",
                    a.approvals,
                    a.approvals_denied,
                    a.approvals_expired,
                    a.approval_examples.join("; ")
                ));
            }
            if !a.skips.is_empty() {
                let skips: Vec<String> = a.skips.iter().map(|(k, v)| format!("{k} {v}")).collect();
                out.push_str(&format!(
                    "- Ticks that could not start it: {}.\n",
                    skips.join(", ")
                ));
            }
            if !a.worst_runs.is_empty() {
                out.push_str("- Runs worth a look:\n");
                for r in &a.worst_runs {
                    out.push_str(&format!(
                        "  - {} ({}): {}, {} turns, ${:.2}{}{}\n",
                        r.started_at.format("%m-%d %H:%M"),
                        r.trigger,
                        r.exit,
                        r.turns,
                        r.cost_usd,
                        r.error
                            .as_deref()
                            .map(|e| format!("; error: {}", one_line(e, 200)))
                            .unwrap_or_default(),
                        r.result
                            .as_deref()
                            .map(|t| format!("; final text: {}", one_line(t, 300)))
                            .unwrap_or_default()
                    ));
                }
            }
            out.push('\n');
        }
    }

    out.push_str("## Guardrail trips\n\n");
    if d.trips.is_empty() {
        out.push_str("None.\n\n");
    } else {
        for t in &d.trips {
            out.push_str(&format!(
                "- {} {} ({}): {}\n",
                t.at.format("%m-%d %H:%M"),
                t.rule,
                t.action,
                t.detail
            ));
        }
        out.push('\n');
    }

    out.push_str("## Frozen tickets\n\n");
    if d.frozen_tickets.is_empty() {
        out.push_str("None.\n\n");
    } else {
        out.push_str(&format!("{}\n\n", d.frozen_tickets.join(", ")));
    }

    out.push_str("## Current guardrails\n\n");
    for field in GUARDRAIL_FIELDS {
        if let Some(v) = guardrail_value(&d.thresholds, field) {
            out.push_str(&format!("- {field} = {}\n", trim_float(v)));
        }
    }
    out.push('\n');

    out.push_str("## Permission profiles\n\n");
    for p in &d.profiles {
        out.push_str(&format!("### {} (mode {})\n\n", p.name, p.mode));
        out.push_str(&format!("- allow: {}\n", p.allowed_tools.join(", ")));
        out.push_str(&format!("- deny: {}\n\n", p.disallowed_tools.join(", ")));
    }

    out.push_str("## Previous analyses\n\n");
    if d.previous.is_empty() {
        out.push_str("None yet.\n");
    } else {
        for p in &d.previous {
            out.push_str(&format!(
                "- {}: {} Applied: {}. Dismissed: {}.{}\n",
                p.at.format("%Y-%m-%d %H:%M"),
                one_line(&p.summary, 300),
                if p.applied.is_empty() {
                    "none".to_string()
                } else {
                    p.applied.join("; ")
                },
                if p.dismissed.is_empty() {
                    "none".to_string()
                } else {
                    p.dismissed.join("; ")
                },
                if p.undecided > 0 {
                    format!(" {} still undecided.", p.undecided)
                } else {
                    String::new()
                }
            ));
        }
    }
    out
}

fn one_line(s: &str, max: usize) -> String {
    let joined: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > max {
        joined
            .chars()
            .take(max.saturating_sub(1))
            .collect::<String>()
            + "…"
    } else {
        joined
    }
}

fn trim_float(v: f64) -> String {
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

// ------------------------------------------------------------ guardrails

/// The threshold fields the analyst may name, in the order Settings shows them.
pub const GUARDRAIL_FIELDS: [&str; 14] = [
    "ping_pong_moves",
    "ping_pong_window_minutes",
    "comment_storm_comments",
    "comment_storm_window_minutes",
    "mention_loop_mentions",
    "mention_loop_window_minutes",
    "run_churn_runs",
    "run_churn_cooldown_minutes",
    "repeated_tool_calls",
    "max_turns_per_run",
    "max_usd_per_run",
    "max_usd_per_agent_per_day",
    "max_wall_clock_minutes",
    "wake_storm_per_hour",
];

pub fn guardrail_value(t: &GuardrailThresholds, field: &str) -> Option<f64> {
    Some(match field {
        "ping_pong_moves" => t.ping_pong_moves as f64,
        "ping_pong_window_minutes" => t.ping_pong_window_minutes as f64,
        "comment_storm_comments" => t.comment_storm_comments as f64,
        "comment_storm_window_minutes" => t.comment_storm_window_minutes as f64,
        "mention_loop_mentions" => t.mention_loop_mentions as f64,
        "mention_loop_window_minutes" => t.mention_loop_window_minutes as f64,
        "run_churn_runs" => t.run_churn_runs as f64,
        "run_churn_cooldown_minutes" => t.run_churn_cooldown_minutes as f64,
        "repeated_tool_calls" => t.repeated_tool_calls as f64,
        "max_turns_per_run" => t.max_turns_per_run as f64,
        "max_usd_per_run" => t.max_usd_per_run,
        "max_usd_per_agent_per_day" => t.max_usd_per_agent_per_day,
        "max_wall_clock_minutes" => t.max_wall_clock_minutes as f64,
        "wake_storm_per_hour" => t.wake_storm_per_hour as f64,
        _ => return None,
    })
}

/// Sets one threshold. Counts and minutes must be whole and at least one;
/// dollar amounts must be positive.
pub fn set_guardrail(t: &mut GuardrailThresholds, field: &str, value: f64) -> Result<(), String> {
    if !value.is_finite() || value <= 0.0 {
        return Err(format!("{field} must be a positive number, not {value}"));
    }
    let whole = || -> Result<u32, String> {
        if value.fract() != 0.0 || value > u32::MAX as f64 {
            return Err(format!("{field} must be a whole number, not {value}"));
        }
        Ok(value as u32)
    };
    match field {
        "ping_pong_moves" => t.ping_pong_moves = whole()?,
        "ping_pong_window_minutes" => t.ping_pong_window_minutes = whole()?,
        "comment_storm_comments" => t.comment_storm_comments = whole()?,
        "comment_storm_window_minutes" => t.comment_storm_window_minutes = whole()?,
        "mention_loop_mentions" => t.mention_loop_mentions = whole()?,
        "mention_loop_window_minutes" => t.mention_loop_window_minutes = whole()?,
        "run_churn_runs" => t.run_churn_runs = whole()?,
        "run_churn_cooldown_minutes" => t.run_churn_cooldown_minutes = whole()?,
        "repeated_tool_calls" => t.repeated_tool_calls = whole()?,
        "max_turns_per_run" => t.max_turns_per_run = whole()?,
        "max_usd_per_run" => t.max_usd_per_run = value,
        "max_usd_per_agent_per_day" => t.max_usd_per_agent_per_day = value,
        "max_wall_clock_minutes" => t.max_wall_clock_minutes = whole()?,
        "wake_storm_per_hour" => t.wake_storm_per_hour = whole()?,
        other => return Err(format!("unknown guardrail {other:?}")),
    }
    Ok(())
}

// --------------------------------------------------------------- the call

pub const ANALYST_SYSTEM_PROMPT: &str = r#"You are the team analyst for AgenticKanban, a kanban board where Claude Code agents work as teammates alongside a human. You do not run on the board. You read a digest of what the team did over a window, with the agents' definition files, and you recommend changes to the agents' definitions, the guardrail thresholds and the permission profiles that would make the team produce more with less waste. The human decides on each recommendation; nothing you return is applied by itself.

What to look for:
- Agents that spend runs without producing: many turns or high cost with no commits, moves, comments or messages; runs that end crashed, over budget or timed out; repeated tool errors.
- Chatter: messages and mentions out of proportion to work; mention loops; comment storms; agents waiting on each other.
- Friction the harness causes: approvals asked again and again for the same safe command; runs not started because of cooldowns, wake storms, budgets or slots (a threshold may be too tight, or an agent wakes too often); guardrail trips that froze tickets.
- Definitions that are unclear, contradictory, or that send work to the wrong teammate.

How the harness works, so the fix lands in the right place:
- An approval is raised by the app's permission hook, not by the agent: when a tool call matches no allow rule of the agent's profile, the run pauses until the human answers. An agent cannot avoid approvals by being told to. If the same safe command keeps asking, the fix is a profile_allow rule for it. If the command looks allowed already, the exact form matters: a compound shell command is judged part by part, and a rule such as `Bash(cargo *)` covers `cargo doc` but not `cd x && cargo doc` unless `cd` is allowed too, so propose the missing part.
- Guardrail trips and "not started" counts come from the thresholds listed under "Current guardrails"; a guardrail change moves one of them.
- Behaviour (chatter, idle runs, wrong hand-offs, unclear ownership) lives in the definitions; a definition change rewrites one section.

Rules:
- Every recommendation cites numbers from the digest as its evidence. No evidence, no recommendation.
- Prefer few, specific changes over many vague ones. At most eight.
- A definition change replaces one `## section` of one agent's body with text you write in full, in the file's house style: short sentences, second person, concrete nouns, no filler. Name the section exactly as it appears in the file; a new section name adds a section at the end. Never touch the frontmatter, the identity paragraph or the bold ownership contract lines.
- A guardrail change names one field from the "Current guardrails" list and a new numeric value.
- A profile change adds allow rules in Claude Code's syntax, for example `Bash(cargo doc *)`, to one named profile. Never propose removing a deny rule, allowing `git push`, or allowing anything destructive.
- Anything only the human can do (split a ticket, add a teammate, answer a question, unfreeze a ticket) is a note with target kind "human".
- Confidence is "high" when the evidence is direct and the change is small, "medium" when it is a reasonable inference, "low" when it is a guess worth considering.
- If the window holds too little activity to say anything, return an empty recommendations list and say so in the summary.

Return exactly one JSON object, with no code fences and no commentary:
{
  "summary": "two or three sentences on how the team is doing and where the waste is",
  "recommendations": [
    {
      "title": "short imperative title",
      "evidence": "the numbers and events behind it",
      "change": "what to change, in one or two sentences",
      "expected_effect": "what should improve and how the next digest would show it",
      "confidence": "high",
      "target": {"kind": "agent_section", "handle": "lead", "section": "Team", "text": "the full new section text, without the heading"}
    },
    {"title": "...", "evidence": "...", "change": "...", "expected_effect": "...", "confidence": "medium", "target": {"kind": "guardrail", "field": "comment_storm_comments", "value": 25}},
    {"title": "...", "evidence": "...", "change": "...", "expected_effect": "...", "confidence": "high", "target": {"kind": "profile_allow", "profile": "trusted", "rules": ["Bash(cargo doc *)"]}},
    {"title": "...", "evidence": "...", "change": "...", "expected_effect": "...", "confidence": "low", "target": {"kind": "human"}}
  ]
}"#;

pub const ANALYST_INSTRUCTION: &str = "Read the team digest and the agent definitions supplied as input. Return your analysis as the JSON object the system prompt describes, and nothing else.";

/// Builds the call: the digest, every live definition, and the human's notes.
pub fn analyse_request(
    digest_markdown: &str,
    definitions: &[AgentDefinition],
    notes: &str,
    model: Option<String>,
) -> UtilityRequest {
    let mut input = String::new();
    let notes = notes.trim();
    if !notes.is_empty() {
        input.push_str(&format!("# Notes from the human\n\n{notes}\n\n"));
    }
    input.push_str(digest_markdown);
    input.push_str("\n\n# Agent definitions\n\n");
    if definitions.is_empty() {
        input.push_str("(none)\n");
    }
    for d in definitions {
        input.push_str(&format!(
            "## File for @{}\n\n```\n{}```\n\n",
            d.handle(),
            d.render()
        ));
    }
    UtilityRequest {
        system_prompt: ANALYST_SYSTEM_PROMPT.to_string(),
        instruction: ANALYST_INSTRUCTION.to_string(),
        input,
        model,
        label: "analyse team".to_string(),
    }
}

// ------------------------------------------------------- the reply

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Target {
    /// Replace, or add, one `## section` of an agent's body.
    AgentSection {
        handle: String,
        section: String,
        text: String,
    },
    Guardrail {
        field: String,
        value: f64,
    },
    ProfileAllow {
        profile: String,
        rules: Vec<String>,
    },
    /// Only the human can do this.
    Human,
}

impl Target {
    pub fn label(&self) -> String {
        match self {
            Target::AgentSection {
                handle, section, ..
            } => format!("@{handle}: section {section}"),
            Target::Guardrail { field, value } => {
                format!("guardrail {field} = {}", trim_float(*value))
            }
            Target::ProfileAllow { profile, rules } => {
                format!("profile {profile}: allow {}", rules.join(", "))
            }
            Target::Human => "for you".to_string(),
        }
    }

    pub fn is_human(&self) -> bool {
        matches!(self, Target::Human)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Confidence {
    High,
    #[default]
    Medium,
    Low,
}

impl Confidence {
    pub fn label(&self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    Applied,
    Dismissed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Recommendation {
    pub title: String,
    #[serde(default)]
    pub evidence: String,
    #[serde(default)]
    pub change: String,
    #[serde(default)]
    pub expected_effect: String,
    #[serde(default)]
    pub confidence: Confidence,
    pub target: Target,
    /// The human's decision, recorded on the call.
    #[serde(default)]
    pub decision: Option<Decision>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Analysis {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub recommendations: Vec<Recommendation>,
    /// Problems found while checking the reply, for the human.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Analysis {
    pub fn from_json(text: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(text)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_default()
    }

    pub fn titles_with(&self, decision: Decision) -> Vec<String> {
        self.recommendations
            .iter()
            .filter(|r| r.decision == Some(decision))
            .map(|r| r.title.clone())
            .collect()
    }

    pub fn undecided(&self) -> usize {
        self.recommendations
            .iter()
            .filter(|r| r.decision.is_none())
            .count()
    }
}

/// Turns the model's reply into an analysis the UI can show. Recommendations
/// that name an unknown agent, guardrail or profile, or an unsafe rule, are
/// kept as notes for the human with a warning, so nothing is applied blind.
pub fn apply_analysis(
    returned: &str,
    handles: &[String],
    policy: &ProjectPolicy,
) -> Result<Analysis, RuntimeError> {
    let json = extract_json(returned);
    let mut analysis = Analysis::from_json(json)
        .map_err(|e| RuntimeError::Utility(format!("the reply was not the expected JSON: {e}")))?;
    if analysis.recommendations.len() > MAX_RECOMMENDATIONS {
        analysis.warnings.push(format!(
            "the reply had {} recommendations; only the first {MAX_RECOMMENDATIONS} are shown",
            analysis.recommendations.len()
        ));
        analysis.recommendations.truncate(MAX_RECOMMENDATIONS);
    }
    for (i, r) in analysis.recommendations.iter_mut().enumerate() {
        r.decision = None;
        if r.title.trim().is_empty() {
            r.title = format!("Recommendation {}", i + 1);
        }
        let problem = match &r.target {
            Target::AgentSection {
                handle,
                section,
                text,
            } => {
                if !handles.iter().any(|h| h == handle) {
                    Some(format!("names an unknown agent @{handle}"))
                } else if section.trim().is_empty() || section.trim().starts_with('#') {
                    Some("names no section".to_string())
                } else if text.trim().is_empty() {
                    Some("has no replacement text".to_string())
                } else {
                    None
                }
            }
            Target::Guardrail { field, value } => {
                let mut probe = policy.guardrails.clone();
                set_guardrail(&mut probe, field, *value).err()
            }
            Target::ProfileAllow { profile, rules } => {
                if !policy
                    .permission_profiles
                    .iter()
                    .any(|p| &p.name == profile)
                {
                    Some(format!("names an unknown profile {profile:?}"))
                } else if rules.is_empty() {
                    Some("adds no rules".to_string())
                } else {
                    rules.iter().find_map(|rule| unsafe_rule(rule))
                }
            }
            Target::Human => None,
        };
        if let Some(problem) = problem {
            analysis
                .warnings
                .push(format!("{:?} {problem}; kept as a note for you", r.title));
            r.target = Target::Human;
        }
    }
    Ok(analysis)
}

/// Why a rule may not be added to a profile by the analyst, if any.
fn unsafe_rule(rule: &str) -> Option<String> {
    let parsed = PermissionRule::parse(rule)?;
    let lower = rule.to_lowercase();
    let forbidden = [
        "git push",
        "rm -r",
        "rm -f",
        "--force",
        "-f ",
        "reset --hard",
        "git clean",
        "sudo",
        "curl",
        "wget",
    ];
    if parsed.tool == "*" || parsed.tool.is_empty() {
        return Some(format!("rule {rule:?} would allow every tool"));
    }
    if parsed.tool == "Bash" && parsed.pattern.as_deref().is_none_or(|p| p.trim() == "*") {
        return Some(format!("rule {rule:?} would allow every shell command"));
    }
    if let Some(bad) = forbidden.iter().find(|f| lower.contains(*f)) {
        return Some(format!("rule {rule:?} would allow {}", bad.trim()));
    }
    None
}

/// The JSON object in a reply that may carry fences or commentary.
fn extract_json(s: &str) -> &str {
    let start = s.find('{');
    let end = s.rfind('}');
    match (start, end) {
        (Some(a), Some(b)) if b >= a => &s[a..=b],
        _ => s.trim(),
    }
}

/// Replaces the content of `## section` in a body, or appends the section
/// when the body has none of that name. Headings compare without case.
pub fn replace_section(body: &str, section: &str, text: &str) -> String {
    let wanted = section.trim().to_lowercase();
    let lines: Vec<&str> = body.lines().collect();
    let start = lines.iter().position(|l| {
        l.strip_prefix("## ")
            .is_some_and(|rest| rest.trim().to_lowercase() == wanted)
    });
    let text = text.trim_end();
    let mut out = String::new();
    match start {
        Some(start) => {
            let end = lines[start + 1..]
                .iter()
                .position(|l| l.starts_with("## "))
                .map(|i| start + 1 + i)
                .unwrap_or(lines.len());
            for l in &lines[..=start] {
                out.push_str(l);
                out.push('\n');
            }
            out.push('\n');
            out.push_str(text);
            out.push('\n');
            if end < lines.len() {
                out.push('\n');
                for l in &lines[end..] {
                    out.push_str(l);
                    out.push('\n');
                }
            }
        }
        None => {
            out.push_str(body.trim_end());
            out.push_str(&format!("\n\n## {}\n\n{text}\n", section.trim()));
        }
    }
    out
}

/// Adds the rules a profile does not have yet. Returns how many were added.
pub fn add_allow_rules(profile: &mut PermissionProfile, rules: &[String]) -> usize {
    let mut added = 0;
    for r in rules {
        let r = r.trim();
        if r.is_empty() || profile.allowed_tools.iter().any(|x| x == r) {
            continue;
        }
        profile.allowed_tools.push(r.to_string());
        added += 1;
    }
    added
}

#[cfg(test)]
mod tests {
    use super::*;
    use ak_core::agent::AgentDefinition;
    use ak_core::approval::Approval;
    use ak_core::commands::NewTicket;
    use ak_core::run::{AgentRun, RunOutcome, SchedulerDecision, SkipReason};
    use ak_core::utility::UtilityCall;
    use chrono::Duration;
    use serde_json::json;

    fn policy() -> ProjectPolicy {
        ProjectPolicy::new("Demo")
    }

    #[test]
    fn replace_section_replaces_or_appends() {
        let body = "# @a — a\n\nIdentity.\n\n## Team\n\nOld team.\n\n## Background\n\nStuff.\n";
        let out = replace_section(body, "team", "New team.\n- @b does x");
        assert_eq!(
            out,
            "# @a — a\n\nIdentity.\n\n## Team\n\nNew team.\n- @b does x\n\n## Background\n\nStuff.\n"
        );
        let out = replace_section(body, "Background", "Different.");
        assert!(out.ends_with("## Background\n\nDifferent.\n"));
        assert!(out.contains("Old team."));
        let out = replace_section(body, "Checklist", "- run tests");
        assert!(out.ends_with("## Background\n\nStuff.\n\n## Checklist\n\n- run tests\n"));
    }

    #[test]
    fn guardrails_set_and_read() {
        let mut t = GuardrailThresholds::default();
        set_guardrail(&mut t, "comment_storm_comments", 25.0).unwrap();
        assert_eq!(t.comment_storm_comments, 25);
        set_guardrail(&mut t, "max_usd_per_run", 7.5).unwrap();
        assert_eq!(t.max_usd_per_run, 7.5);
        assert!(set_guardrail(&mut t, "wake_storm_per_hour", 2.5).is_err());
        assert!(set_guardrail(&mut t, "wake_storm_per_hour", 0.0).is_err());
        assert!(set_guardrail(&mut t, "nope", 1.0).is_err());
        for f in GUARDRAIL_FIELDS {
            assert!(guardrail_value(&t, f).is_some(), "{f}");
        }
        assert_eq!(guardrail_value(&t, "comment_storm_comments"), Some(25.0));
    }

    #[test]
    fn apply_analysis_checks_targets() {
        let reply = r#"Here is my analysis:
```json
{"summary": "Busy but wasteful.", "recommendations": [
  {"title": "Tighten lead's standup", "evidence": "12 runs, 0 commits", "change": "x", "expected_effect": "y", "confidence": "high",
   "target": {"kind": "agent_section", "handle": "lead", "section": "A standup", "text": "Do less."}},
  {"title": "Unknown agent", "evidence": "e", "change": "c", "expected_effect": "f", "confidence": "low",
   "target": {"kind": "agent_section", "handle": "ghost", "section": "Team", "text": "t"}},
  {"title": "Raise comment storm", "evidence": "3 trips", "change": "c", "expected_effect": "f",
   "target": {"kind": "guardrail", "field": "comment_storm_comments", "value": 25}},
  {"title": "Bad guardrail", "evidence": "e", "change": "c", "expected_effect": "f",
   "target": {"kind": "guardrail", "field": "comment_storm_comments", "value": -1}},
  {"title": "Allow cargo doc", "evidence": "9 approvals", "change": "c", "expected_effect": "f", "confidence": "high",
   "target": {"kind": "profile_allow", "profile": "trusted", "rules": ["Bash(cargo doc *)"]}},
  {"title": "Allow push", "evidence": "e", "change": "c", "expected_effect": "f",
   "target": {"kind": "profile_allow", "profile": "trusted", "rules": ["Bash(git push *)"]}},
  {"title": "", "evidence": "e", "change": "c", "expected_effect": "f", "target": {"kind": "human"}}
]}
```"#;
        let a = apply_analysis(reply, &["lead".into()], &policy()).unwrap();
        assert_eq!(a.summary, "Busy but wasteful.");
        assert_eq!(a.recommendations.len(), 7);
        assert!(matches!(
            a.recommendations[0].target,
            Target::AgentSection { .. }
        ));
        assert_eq!(a.recommendations[0].confidence, Confidence::High);
        assert!(
            a.recommendations[1].target.is_human(),
            "unknown agent becomes a note"
        );
        assert!(matches!(
            a.recommendations[2].target,
            Target::Guardrail { .. }
        ));
        assert_eq!(a.recommendations[2].confidence, Confidence::Medium);
        assert!(
            a.recommendations[3].target.is_human(),
            "bad value becomes a note"
        );
        assert!(matches!(
            a.recommendations[4].target,
            Target::ProfileAllow { .. }
        ));
        assert!(
            a.recommendations[5].target.is_human(),
            "push stays forbidden"
        );
        assert_eq!(a.recommendations[6].title, "Recommendation 7");
        assert_eq!(a.warnings.len(), 3, "{:?}", a.warnings);
        assert!(a.warnings.iter().any(|w| w.contains("ghost")));
        assert!(a.warnings.iter().any(|w| w.contains("git push")));
        assert!(a.recommendations.iter().all(|r| r.decision.is_none()));

        assert!(apply_analysis("Sorry.", &[], &policy()).is_err());
        let empty = apply_analysis(r#"{"summary": "Too little to say."}"#, &[], &policy()).unwrap();
        assert!(empty.recommendations.is_empty());
    }

    #[test]
    fn unsafe_rules_are_named() {
        assert!(unsafe_rule("Bash(cargo doc *)").is_none());
        assert!(unsafe_rule("WebFetch").is_none());
        assert!(unsafe_rule("Bash(*)").is_some());
        assert!(unsafe_rule("Bash").is_some());
        assert!(unsafe_rule("*").is_some());
        assert!(unsafe_rule("Bash(rm -rf *)").is_some());
        assert!(unsafe_rule("Bash(git push origin *)").is_some());
        assert!(unsafe_rule("Bash(git reset --hard *)").is_some());
    }

    #[test]
    fn add_allow_rules_skips_duplicates() {
        let mut p = PermissionProfile::defaults().remove(1);
        let before = p.allowed_tools.len();
        let added = add_allow_rules(
            &mut p,
            &["Bash(cargo doc *)".into(), "Edit".into(), " ".into()],
        );
        assert_eq!(added, 1, "Edit is already allowed in trusted");
        assert_eq!(p.allowed_tools.len(), before + 1);
    }

    #[test]
    fn request_carries_digest_definitions_and_notes() {
        let d = AgentDefinition::template("lead", "Lead");
        let req = analyse_request(
            "# Team digest: Demo\n\n| x |",
            &[d],
            "Focus on cost.",
            Some("opus".into()),
        );
        assert!(req
            .input
            .starts_with("# Notes from the human\n\nFocus on cost."));
        assert!(req.input.contains("# Team digest: Demo"));
        assert!(req.input.contains("## File for @lead"));
        assert!(req.input.contains("name: lead"));
        assert_eq!(req.model.as_deref(), Some("opus"));
        assert_eq!(req.label, "analyse team");
        assert!(req.system_prompt.contains("\"kind\": \"agent_section\""));
    }

    #[tokio::test]
    async fn digest_tallies_a_window() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let ok = std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(ok, "git init");
        let paths = crate::HomePaths::at(tmp.path().join("home"));
        let mut registry = crate::ProjectRegistry::load(paths).unwrap();
        let id = registry.add_project(&repo, Some("Demo".into())).unwrap();
        let project = registry.open(id).await.unwrap();
        for (h, role) in [("lead", "Lead"), ("dev", "Developer")] {
            project
                .agents
                .write_definition(&AgentDefinition::template(h, role))
                .unwrap();
        }
        crate::agents::reconcile(&project.agents, project.store.as_ref())
            .await
            .unwrap();
        let store = project.store();
        let lead = store.agent_by_handle("lead").await.unwrap().unwrap();
        let dev = store.agent_by_handle("dev").await.unwrap().unwrap();
        let now = Utc::now();
        let since = now - Duration::hours(24);

        // Two lead runs: one that produced nothing, one that crashed. One dev
        // run that committed. One old run outside the window.
        let mut r1 = AgentRun::start(
            lead.actor_id,
            RunTrigger::Manual,
            "ws".into(),
            now - Duration::hours(3),
        );
        r1.ended_at = Some(now - Duration::hours(3) + Duration::minutes(2));
        r1.exit_reason = Some(ExitReason::Completed);
        r1.num_turns = 9;
        r1.cost_usd = Some(0.40);
        let mut o1 = RunOutcome::default();
        o1.note_tool_call("mcp__board__inbox_list", &json!({}));
        o1.result = Some("Nothing to do.".into());
        r1.outcome = Some(o1);
        store.insert_run(&r1).await.unwrap();
        let mut r2 = AgentRun::start(
            lead.actor_id,
            RunTrigger::Activated {
                by: project.human.id,
                reason: "wake".into(),
            },
            "ws".into(),
            now - Duration::hours(2),
        );
        r2.ended_at = Some(now - Duration::hours(2) + Duration::minutes(1));
        r2.exit_reason = Some(ExitReason::Crashed);
        r2.error = Some("boom".into());
        r2.cost_usd = Some(0.10);
        store.insert_run(&r2).await.unwrap();
        let mut r3 = AgentRun::start(
            dev.actor_id,
            RunTrigger::Manual,
            "ws".into(),
            now - Duration::hours(1),
        );
        r3.ended_at = Some(now - Duration::minutes(30));
        r3.exit_reason = Some(ExitReason::Completed);
        r3.num_turns = 20;
        r3.cost_usd = Some(0.90);
        let mut o3 = RunOutcome::default();
        o3.note_tool_call("Bash", &json!({"command": "git commit -m x"}));
        o3.note_tool_call("Edit", &json!({"file_path": "a.rs"}));
        o3.note_tool_result(true);
        r3.outcome = Some(o3);
        store.insert_run(&r3).await.unwrap();
        let mut old = AgentRun::start(
            dev.actor_id,
            RunTrigger::Manual,
            "ws".into(),
            now - Duration::days(3),
        );
        old.ended_at = Some(now - Duration::days(3));
        old.cost_usd = Some(5.0);
        store.insert_run(&old).await.unwrap();

        // An approval the human denied after a minute.
        let a = Approval::new(
            r3.id,
            dev.actor_id,
            "Bash",
            json!({"command": "cargo publish"}),
            now - Duration::minutes(50),
        );
        store.insert_approval(&a).await.unwrap();
        store
            .decide_approval(a.id, ApprovalState::Denied, Some(project.human.id))
            .await
            .unwrap();

        // Board activity: dev moves a ticket and comments with a mention of lead.
        let columns = store.columns().await.unwrap();
        let t = store
            .create_ticket(
                NewTicket {
                    title: "T".into(),
                    column_id: columns[0].id,
                    ..Default::default()
                },
                project.human.id,
            )
            .await
            .unwrap();
        let t = store
            .move_ticket(t.id, columns[1].id, t.version, dev.actor_id)
            .await
            .unwrap();
        store
            .comment(t.id, "@lead please look".into(), None, dev.actor_id)
            .await
            .unwrap();
        store
            .freeze_ticket(t.id, "test".into(), project.human.id)
            .await
            .unwrap();

        // The scheduler could not start lead twice.
        for _ in 0..2 {
            store
                .record_scheduler_decision(&SchedulerDecision {
                    at: now - Duration::minutes(10),
                    agent_id: lead.actor_id,
                    reason: SkipReason::Cooldown,
                    detail: "cooling".into(),
                })
                .await
                .unwrap();
        }

        // A previous analysis with one applied and one undecided recommendation.
        let mut call = UtilityCall::start(
            UtilityKind::Analyse,
            ActorId::SYSTEM,
            None,
            None,
            now - Duration::hours(5),
        );
        let mut prior = Analysis {
            summary: "Earlier.".into(),
            ..Default::default()
        };
        prior.recommendations.push(Recommendation {
            title: "Did this".into(),
            evidence: String::new(),
            change: String::new(),
            expected_effect: String::new(),
            confidence: Confidence::High,
            target: Target::Human,
            decision: Some(Decision::Applied),
        });
        prior.recommendations.push(Recommendation {
            title: "Not yet".into(),
            evidence: String::new(),
            change: String::new(),
            expected_effect: String::new(),
            confidence: Confidence::Low,
            target: Target::Human,
            decision: None,
        });
        call.propose(prior.to_json(), vec![], Some(0.2), now - Duration::hours(5));
        store.insert_utility_call(&call).await.unwrap();

        let d = build_digest(&project, since).await.unwrap();
        assert_eq!(d.project, "Demo");
        assert_eq!(d.total_runs(), 3, "the old run is outside the window");
        assert!((d.total_cost() - 1.40).abs() < 1e-9);
        assert_eq!(d.agents[0].handle, "dev", "highest cost first");
        let dev_s = &d.agents[0];
        assert_eq!(dev_s.runs, 1);
        assert_eq!(dev_s.commits, 1);
        assert_eq!(dev_s.tool_calls, 2);
        assert_eq!(dev_s.tool_errors, 1);
        assert_eq!(dev_s.ticket_moves, 1);
        assert_eq!(dev_s.comments, 1);
        assert_eq!(dev_s.mentions_sent.get("lead"), Some(&1));
        assert_eq!(dev_s.approvals, 1);
        assert_eq!(dev_s.approvals_denied, 1);
        assert!(
            dev_s.approval_wait_secs >= 60 * 49,
            "{}",
            dev_s.approval_wait_secs
        );
        assert_eq!(
            dev_s.approval_examples,
            vec!["Bash: cargo publish".to_string()]
        );
        assert_eq!(dev_s.idle_runs, 0);
        let lead_s = &d.agents[1];
        assert_eq!(lead_s.runs, 2);
        assert_eq!(lead_s.idle_runs, 1);
        assert_eq!(lead_s.exits.get("Crashed"), Some(&1));
        assert_eq!(lead_s.mentions_received, 1);
        assert_eq!(lead_s.skips.get("cooling down"), Some(&2));
        assert_eq!(lead_s.worst_runs.len(), 2);
        assert_eq!(lead_s.worst_runs[0].exit, "Crashed", "failures first");
        assert_eq!(lead_s.worst_runs[0].trigger, "activation: wake");
        assert_eq!(
            lead_s.worst_runs[1].result.as_deref(),
            Some("Nothing to do.")
        );
        assert_eq!(d.frozen_tickets, vec![t.key.clone()]);
        assert_eq!(d.previous.len(), 1);
        assert_eq!(d.previous[0].applied, vec!["Did this".to_string()]);
        assert_eq!(d.previous[0].undecided, 1);
        assert_eq!(d.profiles.len(), 3);

        let md = render_digest(&d);
        assert!(md.contains("# Team digest: Demo"));
        assert!(md.contains("| @dev |"));
        assert!(md.contains("### @lead"));
        assert!(
            md.contains("Idle runs (completed with no commit, move, comment or message): 1 of 2.")
        );
        assert!(md.contains("cooling down 2"));
        assert!(md.contains("Approvals asked: 1, denied 1, expired 0; average wait"));
        assert!(md.contains("Bash: cargo publish"));
        assert!(md.contains(&format!("## Frozen tickets\n\n{}", t.key)));
        assert!(md.contains("- comment_storm_comments = 15"));
        assert!(md.contains("### trusted (mode acceptEdits)"));
        assert!(md.contains("Applied: Did this. Dismissed: none. 1 still undecided."));
        assert!(md.contains("; error: boom"));
        assert!(md.contains("; final text: Nothing to do."));
    }
}
