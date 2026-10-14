//! What an agent is told at wake: its definition, its memory, the team, the
//! board conventions, and why it was woken.

use ak_core::agent::Agent;
use ak_core::board::Column;

use crate::workspace::Workspace;

pub struct RosterLine {
    pub handle: String,
    pub kind: &'static str,
    pub description: String,
    pub state: String,
}

pub struct PromptContext<'a> {
    pub agent: &'a Agent,
    pub definition_body: &'a str,
    pub memory: &'a str,
    pub roster: &'a [RosterLine],
    pub columns: &'a [Column],
    pub workspace: &'a Workspace,
    pub push_allowed: bool,
}

/// The appended system prompt: definition body, memory, roster, conventions.
pub fn system_prompt(ctx: &PromptContext<'_>) -> String {
    let mut out = String::new();
    out.push_str(ctx.definition_body.trim());
    out.push_str("\n\n---\n\n## Your memory\n\n");
    if ctx.memory.trim().is_empty() {
        out.push_str("(empty: this is your first run on this project)\n");
    } else {
        out.push_str(ctx.memory.trim());
        out.push('\n');
    }
    out.push_str("\n## The team right now\n\n");
    for r in ctx.roster {
        out.push_str(&format!(
            "- @{} ({}{}){}\n",
            r.handle,
            r.kind,
            if r.state.is_empty() {
                String::new()
            } else {
                format!(", {}", r.state)
            },
            if r.description.is_empty() {
                String::new()
            } else {
                format!(": {}", r.description)
            }
        ));
    }
    let columns: Vec<String> = ctx
        .columns
        .iter()
        .map(|c| format!("{} ({})", c.name, c.kind.as_str()))
        .collect();
    out.push_str(&format!(
        "\n## How you work on this board\n\n\
- You are @{handle}. Everything you do on the board goes through the `board` tools \
(`mcp__board__*`); every action is attributed to you.\n\
- Start with inbox_list. It says why you were woken. Choose the order yourself; nothing \
is a queue.\n\
- Tickets carry the work. Read a ticket with ticket_get before acting on it. Comment when \
you start, when you are blocked, and when you finish. Move it as it progresses through the \
columns: {columns}. Hand work off by assigning the ticket and @mentioning the assignee.\n\
- @mention a teammate in a comment to notify them. Only mention people who need to act.\n\
- Keep comments short and factual: what changed, what you need, what you decided.\n\
- Your workspace is your own git worktree at `{path}`, currently on branch `{branch}`. \
Work for ticket KEY happens on branch `ticket/KEY` created from the default branch. Commit \
early, with the ticket key in the message. {push} Never touch files outside this worktree, \
and never the human's own checkout.\n\
- A frozen ticket rejects your changes. Only the human unfreezes it; comment if you need them to.\n\
- A tool call you are not allowed to make may ask the human for approval, which can take time. \
Prefer the tools you are allowed.\n\
- Before you finish: call inbox_list again and deal with what is pending; then memory_update \
with what your next run should know (short and concrete: decisions, where things live, what \
is unfinished); then stop. If there is nothing to do, say so in one line and stop.\n",
        handle = ctx.agent.handle,
        columns = columns.join(", "),
        path = ctx.workspace.path.display(),
        branch = ctx.workspace.branch,
        push = if ctx.push_allowed {
            "You may push your ticket branches."
        } else {
            "Never push."
        },
    ));
    out
}

/// The short prompt passed on the command line.
pub fn wake_prompt(trigger_lines: &[String], workspace: &Workspace) -> String {
    let mut out = String::new();
    if trigger_lines.is_empty() {
        out.push_str("You were started directly, with nothing pending.\n");
    } else {
        out.push_str("You were woken by:\n");
        for l in trigger_lines.iter().take(12) {
            out.push_str(&format!("- {l}\n"));
        }
        if trigger_lines.len() > 12 {
            out.push_str(&format!("- and {} more\n", trigger_lines.len() - 12));
        }
    }
    out.push_str(&format!(
        "Workspace: branch {} with {} uncommitted file(s).\n",
        workspace.branch, workspace.dirty_files
    ));
    if workspace.dirty_files > 0 {
        out.push_str("Uncommitted changes are probably your own unfinished work from a previous run: check git status before starting anything new.\n");
    }
    out.push_str("Call inbox_list first.");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ak_core::agent::AgentDefinition;
    use ak_core::board::{Column, ColumnKind};
    use ak_core::ids::{ActorId, ColumnId};

    #[test]
    fn system_prompt_has_every_section() {
        let def = AgentDefinition::template("backend", "Backend");
        let agent = Agent::from_definition(ActorId::new(), &def, "a".into(), "m".into());
        let roster = vec![RosterLine {
            handle: "me".into(),
            kind: "human",
            description: "the person running this board".into(),
            state: String::new(),
        }];
        let columns = vec![Column {
            id: ColumnId::new(),
            name: "To do".into(),
            position: 0,
            kind: ColumnKind::Todo,
        }];
        let ws = Workspace {
            path: "C:/w/backend".into(),
            branch: "agent/backend".into(),
            dirty_files: 0,
        };
        let text = system_prompt(&PromptContext {
            agent: &agent,
            definition_body: &def.body,
            memory: "- auth lives in src/auth",
            roster: &roster,
            columns: &columns,
            workspace: &ws,
            push_allowed: false,
        });
        for needle in [
            "**You own:**",
            "## Your memory",
            "auth lives in src/auth",
            "## The team right now",
            "@me (human)",
            "To do (todo)",
            "agent/backend",
            "Never push.",
            "memory_update",
        ] {
            assert!(text.contains(needle), "missing {needle:?}");
        }
    }

    #[test]
    fn wake_prompt_mentions_triggers_and_dirtiness() {
        let ws = Workspace {
            path: "x".into(),
            branch: "agent/qa".into(),
            dirty_files: 2,
        };
        let text = wake_prompt(&["@me assigned DEM-1 to you".into()], &ws);
        assert!(text.contains("You were woken by"));
        assert!(text.contains("DEM-1"));
        assert!(text.contains("2 uncommitted"));
        assert!(text.contains("git status"));
        assert!(wake_prompt(&[], &ws).contains("nothing pending"));
    }
}
