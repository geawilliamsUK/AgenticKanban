# AgenticKanban — Design

Status: draft v0.4, 2026-10-13. Runtime: Claude Code (confirmed). UI: egui (confirmed). Store: SQLite behind a trait (confirmed). Agent authoring in the GUI with Refine and Integrate (v0.2, section 14). Multi-project from a home-directory config (v0.3). Agents, memory and policy live in the home directory, and the repository holds only an exported snapshot (v0.4, sections 1, 4, 14, 15).

## 1. Purpose

AgenticKanban is a native desktop application that turns a kanban board into a coordination harness for AI coding agents working alongside a human. Agents are full coding agents (Claude Code sessions) with filesystem, git and codebase access. They do not report to a parent orchestrator. They report the way a human teammate does: by updating tickets, commenting, mentioning the people who need to know, and moving on.

The human is a member of the team with the same verbs as the agents, plus the brakes: stop, halt, freeze, approve.

The app is a meta layer over any number of projects. Each project is a git repository registered in a config file in the user's home directory. The app holds the project's team, memory, board and run history in its own directory, so the repository is untouched unless the user exports the team into it for sharing. Agents work in the repository. They are not stored in it.

### Goals for v1

- Single human, many projects. Each project is one git repository with its own board, agents and chats, registered in the home-directory config.
- Agents defined by a markdown definition file (persona, role, team) and a memory file, both editable outside the app.
- Agents created and edited in the GUI, with two Claude-assisted actions: Refine rewrites a rough draft into a clear, structured definition, and Integrate updates every other agent's definition so the team knows the new agent's role and how to work with it (section 14).
- Export of a project's agents into the repository's `.agentic/` directory for sharing, and Import from one, with memory excluded by default (section 14).
- Event-driven wake: agents sleep by default and are started by notifications or direct activation.
- Full ticket lifecycle, comments and replies, @mentions, notifications.
- Direct and group chats organised as threads, searchable.
- Guardrails: ticket freeze, run budgets, loop detection, human kill switches.
- SQLite storage behind a store abstraction so Postgres can be added later.

### Non-goals for v1

- Multi-user or remote collaboration.
- Postgres implementation (abstraction only).
- In-app diff or PR review.
- Injecting messages into a running agent mid-session.
- Any agent runtime other than Claude Code.

## 2. Principles

1. **The board is the source of truth.** Every change to a ticket, chat or agent is an event. Events are persisted before anything reacts to them.
2. **Agents are teammates, not functions.** They receive the same notifications a human would and decide their own priorities. The system never imposes a FIFO queue on them.
3. **Sleep by default.** An agent runs only when there is something to do. One instance per agent, ever.
4. **The human sees everything and can stop anything.** No private channels, no unkillable runs.
5. **Context is a budget.** Agents wake with a summary and pull detail on demand. Threads keep old conversation out of the way without losing it.
6. **Reuse the coding agent.** The harness owns coordination. Claude Code owns coding.
7. **The app owns the team. The repo gets a snapshot.** Definitions, memory and policy live in the app's home directory, per project. Export publishes a copy into the repository's `.agentic/` directory for sharing, and Import brings one back. No agent ever commits a definition or a memory file, which is what keeps agent files out of merge conflicts.

## 3. System overview

One Rust process manages every registered project and hosts everything except the agent runs, which are child processes. Each project has its own store, actors and board. The scheduler's process pool, the bridge and the UI shell are shared across projects.

```
 human ──▶ egui UI ──┐
                     ▼
            ┌─────────────────┐  one transaction  ┌───────────────┐
            │ Command         │─────────────────▶ │ Store         │
            │ handlers        │  mutation + events│ SQLite + md   │
            └────────┬────────┘                   └───────────────┘
                     │ events published after commit
         ┌───────────┼───────────────┬──────────────┐
         ▼           ▼               ▼              ▼
   Notification   Guardrail       UI read        Replay
   router         engine          model          cursor
         │ wake
         ▼
     Scheduler ──▶ Supervisor ──spawn──▶ claude -p   (one process per run,
                        ▲                    │        cwd = agent worktree)
                        │ stdout stream-json │ MCP + hooks over localhost
                        └────────────────────┤
                                             ▼
                                   Bridge (MCP + hooks) ──▶ Command handlers
```

Components:

- **Project registry**: reads the home config, opens one store per project, watches each project's agent files in the home directory for edits and the repository's `.agentic/` directory for drift, and handles add, remove, export and import.
- **Store**: one SQLite database per project, beside that project's agent files in the home directory.
- **Command handlers**: the only way state changes. Used by the UI and by the MCP server, so human and agent actions share one code path.
- **Event bus**: append-only event log in the store with an in-process broadcast after commit.
- **Notification router**: turns events into per-actor notifications and decides which ones wake an agent.
- **Scheduler**: owns agent lifecycle state, the per-agent run lock and the concurrency cap, which is global across projects.
- **Supervisor**: spawns and monitors Claude Code processes, parses their output stream, enforces run limits.
- **Bridge**: one HTTP server on localhost, shared by all projects, that exposes board tools to agent runs over MCP and serves the hook endpoints Claude Code calls for approvals and the stop check. Every request carries a per-run token that resolves to a project, an agent and a run.
- **Workspace manager**: creates and maintains a git worktree per agent.
- **Guardrail engine**: subscribes to events and run telemetry, trips freezes, cooldowns and stops.
- **UI**: egui application. Board, tickets, team, chats, inbox, approvals, run inspector, settings.

## 4. Domain model

### Projects

- **Project**: registered in the home config with a machine-local `id`, a `name` and the repository `path`. Everything the app manages for the project lives under `~/.agentic-kanban/projects/<id>/`: its policy file (columns, guardrail thresholds, push policy, whether project MCP servers are allowed, default model, named permission profiles, authoring model), its agents and their memory, its store. Machine-specific overrides, such as the worktree base, live in the home config entry. The repository holds at most an exported snapshot (section 14). Every entity below belongs to exactly one project and lives in that project's store.

### Identity

- **Actor**: anyone who can act on the board. `id`, `kind` (Human | Agent | System), `handle` (unique within the project, used for @mentions), `display_name`. The human is an actor in every project. Every agent is an actor. Guardrail actions use the reserved `system` actor.

### Board

- **Column**: `id`, `name`, `position`, `kind` (Backlog | Todo | InProgress | Review | Done | Custom). Kind lets guardrails and agents reason about semantics regardless of display names.
- **Ticket**: `id`, `key` (e.g. `AK-42`), `title`, `description` (markdown), `column_id`, `position`, `assignee_id?`, `reporter_id`, `priority`, `labels[]`, `parent_id?`, `frozen`, `freeze_reason?`, `version` (optimistic lock), timestamps.
- **Comment**: `id`, `ticket_id`, `author_id`, `body`, `parent_comment_id?` (reply), `created_at`.
- **Watcher**: (`ticket_id`, `actor_id`). Reporter and assignee are implicit watchers. Anyone who comments becomes one.
- **Mention**: (`source_kind`, `source_id`, `mentioned_actor_id`). Extracted from `@handle` in descriptions, comments and messages.

### Agents

- **Agent**: `actor_id`, `definition_path`, `memory_path`, `role`, `description`, `model`, `permission_profile` and `budgets` (max turns per run, max USD per run, max USD per day, max wall-clock per run), all cached from the definition's frontmatter since the file is the source of truth (section 14), plus `state`, `current_run_id?`, `cooldown_until?` and `missing_since?`, set when the definition file disappears. A missing agent is retired, never deleted, because tickets and events reference it.
- **AgentRun**: `id`, `agent_id`, `trigger` (notification ids, or the actor who activated directly), `claude_session_id`, `workspace_path`, `started_at`, `ended_at?`, `exit_reason?` (Completed | Halted | BudgetExceeded | TimedOut | GuardrailTripped | Crashed), `num_turns`, `input_tokens`, `output_tokens`, `cost_usd`.
- **RunEvent**: `run_id`, `seq`, `kind` (ToolCall | ToolResult | AssistantText | PermissionRequest | SystemInit | Result), `payload`. The parsed transcript, used by the run inspector and loop detection.
- **MemorySnapshot**: `agent_id`, `run_id`, `content`, `created_at`. Taken at the end of every run so the human can diff what the agent learned.

### Authoring

- **UtilityCall**: `id`, `kind` (Refine | Integrate), `agent_id` (the agent being authored), `target_agent_id?` (for Integrate, the agent whose definition is proposed to change), `model`, `started_at`, `ended_at?`, `cost_usd`, `state` (Running | Proposed | Accepted | Rejected | Failed), `proposal` (the returned file), `warnings[]`. Proposals persist until the human accepts or rejects them.

### Events and notifications

- **Event**: `id`, `seq` (monotonic), `occurred_at`, `actor_id`, `kind`, `subject_kind`, `subject_id`, `payload` (json). Append-only, never deleted.
- **Notification**: `id`, `recipient_id`, `event_id`, `kind`, `wake` (bool), `state` (Pending | Seen | Handled | Dismissed), `created_at`, `handled_at?`, `handled_in_run_id?`.

### Chats

- **Chat**: `id`, `kind` (Direct | Group), `name?`, `created_by`. Direct chats have exactly two members. The human is an implicit observer of every chat.
- **ChatMember**: (`chat_id`, `actor_id`).
- **Thread**: `id`, `chat_id`, `title`, `created_by`, `created_at`, `last_activity_at`, `paused`, `archived`.
- **Message**: `id`, `thread_id`, `author_id`, `body`, `created_at`.

### Oversight

- **Approval**: `id`, `run_id`, `tool_name`, `input` (json), `summary`, `state` (Pending | Approved | Denied | Expired), `decided_by?`, `decided_at?`.
- **GuardrailTrip**: `id`, `rule`, `subject_kind`, `subject_id`, `details`, `action_taken`, `created_at`, `cleared_by?`, `cleared_at?`.

## 5. Event model

Every mutation goes through a command handler that, in one database transaction, applies the change and appends one or more events. After commit, the events are published on an in-process broadcast channel. Subscribers are the notification router, the guardrail engine, the scheduler and the UI. Each subscriber persists the last `seq` it processed, so a restart replays anything missed. This is the transactional outbox pattern, in-process.

| Area | Event kinds |
|---|---|
| Ticket | `ticket.created`, `ticket.updated` (field diff), `ticket.moved` (from, to), `ticket.assigned` (from, to), `ticket.commented`, `ticket.replied`, `ticket.frozen`, `ticket.unfrozen` |
| Mentions | `mention.created` (source, mentioned actor) |
| Chat | `chat.created`, `chat.member_added`, `thread.created`, `thread.message_posted`, `thread.paused`, `thread.unpaused` |
| Agent | `agent.created`, `agent.definition_updated` (by hand, Refine or Integrate), `agent.wake_requested`, `agent.run_started`, `agent.run_ended` (reason, stats), `agent.halted`, `agent.resumed`, `agent.memory_updated` |
| Oversight | `approval.requested`, `approval.decided`, `guardrail.tripped`, `guardrail.cleared` |

The event log is simultaneously the ticket activity timeline, the audit trail and the input to guardrails.

## 6. Notifications

The router maps each event to recipients and a wake flag.

| Event | Recipients | Wakes an agent? |
|---|---|---|
| `ticket.assigned` | new assignee | yes |
| `ticket.moved` | assignee, watchers (minus the mover) | assignee yes, watchers no |
| `ticket.commented` / `ticket.replied` | assignee, watchers, parent comment author (minus the author) | assignee and parent author yes, others no |
| `mention.created` | mentioned actor | yes |
| `ticket.unfrozen` | assignee | yes |
| `thread.message_posted` in a Direct chat | the other member | yes |
| `thread.message_posted` in a Group chat | thread participants (minus the author) | no, unless mentioned |
| `thread.message_posted` by the human | all agent participants of that thread (direct chat: both members) | yes |
| `thread.created` in a Group chat | chat members | no |
| `agent.wake_requested` (direct activation) | target agent | yes |
| `approval.requested`, `guardrail.tripped` | human | n/a |

Rules:

- An actor never receives a notification for their own action.
- Notifications about a frozen ticket are created but held, and released on unfreeze.
- Wake notifications request a run through the scheduler. Non-wake notifications accumulate and are presented the next time the agent runs for any reason.
- When an agent is already running, new notifications land in its inbox and are visible on its next `inbox.list` call. Nothing is injected mid-run in v1.
- A notification becomes Handled when the agent or human marks it so, or automatically when the agent mutates the subject ticket or posts in the subject thread during the same run.

The inbox is deliberately not a queue. `inbox.list` returns everything pending with enough context (ticket key, title, column, who did what, when) for the agent to triage in whatever order it judges best.

## 7. Agent lifecycle

### States

Idle, Scheduled, Starting, Running, Stopping, Errored, Cooldown, Halted.

| From | To | Trigger |
|---|---|---|
| Idle | Scheduled | wake notification, direct activation, or pending wake items at the previous exit |
| Scheduled | Starting | scheduler grants a global slot and the per-agent lock |
| Starting | Running | process spawned and its init message received |
| Starting | Errored | spawn failure, or no init within the startup timeout |
| Running | Idle | process exits normally, no pending wake items |
| Running | Scheduled | process exits normally, pending wake items exist |
| Running | Stopping | stop requested by the human, a budget, the wall clock or a guardrail |
| Running | Errored | process exits abnormally |
| Stopping | Idle | process confirmed gone after an automatic stop or a plain human stop |
| Stopping | Halted | process confirmed gone after a human halt |
| Errored | Scheduled | backoff elapsed, attempts remaining |
| Errored | Halted | attempt cap reached, human alerted |
| Idle / Scheduled | Cooldown | guardrail: run churn |
| Cooldown | Idle | cooldown elapsed (pending wake items re-schedule immediately) |
| any | Halted | human halt |
| Halted | Idle | human resume (pending wake items re-schedule immediately) |

Invariants:

- At most one AgentRun with `ended_at = null` per agent. Enforced by the scheduler's in-memory lock and a unique partial index in the store.
- A global cap on concurrent runs across all projects (default 3) so busy boards cannot fan out into a dozen simultaneous sessions.
- A Halted agent is never auto-woken. Its notifications still accumulate.
- A re-wake rate limit per agent (default 12 runs per hour). Excess wakes coalesce into the next permitted run.

### A run, step by step

1. The scheduler moves the agent to Starting and creates an AgentRun.
2. The workspace manager ensures the agent's worktree exists (section 10) and records its git status.
3. The supervisor composes the system prompt addition (persona, role, memory, team roster, board conventions) and the wake prompt (inbox summary plus workspace status), writes a per-run MCP config carrying a fresh run token, and spawns Claude Code with the worktree as its working directory.
4. The supervisor parses the output stream line by line into RunEvents, updates live telemetry (turns, tokens, cost) and forwards tool calls and text to the UI.
5. The agent works: reads tickets, edits code, commits on a ticket branch, comments, moves tickets, mentions teammates, posts in threads. Every board write arrives through the MCP server already attributed to this agent.
6. Before finishing, the agent calls `inbox.list`. If wake items remain it continues. Otherwise it calls `memory.update` and ends its turn.
7. The process exits. The supervisor records the result, takes a MemorySnapshot and releases the lock. The scheduler checks the inbox: pending wake items mean straight back to Scheduled, otherwise Idle.

If an agent ends without checking its inbox, step 7 still catches pending work. The only cost is a wasted process start. Section 8 covers how the runtime can enforce the check.

## 8. Runtime integration: Claude Code

Everything in this section was checked against the Claude Code documentation at code.claude.com on 2026-10-13. Flags marked as not used are listed at the end with the reason.

### Process model

One Claude Code process per run, in headless print mode, with a fresh session every wake. The persona, memory and inbox give the agent its continuity, not the session. Session ids are still recorded so the human can reopen any run interactively.

At startup the harness locates the binary (explicit setting, then PATH, then the native installer's default of `~/.local/bin/claude` or `%USERPROFILE%\.local\bin\claude.exe`), runs `claude --version` against a pinned minimum, and runs `claude auth status` to confirm a login. Both results show in Settings. The `.cmd` shim from the npm package is never used; the native binary is spawned directly.

### Invocation

```
claude -p "<wake prompt>"
  --output-format stream-json --verbose
  --session-id <run uuid>
  --append-system-prompt-file <run dir>/system.md
  --mcp-config <run dir>/mcp.json --strict-mcp-config
  --settings <run dir>/settings.json
  --permission-mode <from profile>
  --allowedTools <from profile> --disallowedTools <from profile>
  --max-turns <budget> --max-budget-usd <budget>
  --model <agent model>
```

Working directory: the agent's worktree. Each run gets a directory under the app's home directory (section 15) holding `system.md`, `mcp.json`, `settings.json`, and captured stdout and stderr. Run directories are pruned after a configurable number of days.

### Prompts

`system.md` is composed per run from: the body of the agent's definition file (section 14), the memory file, the live team roster (handles, descriptions, current states), and the shared board conventions. The conventions cover how to use the board tools, when to mention someone, chat etiquette, git branch and commit rules, and the finishing ritual: call `inbox.list`, then `memory.update`, then stop.

The wake prompt is deliberately short: why the agent was woken (notification ids with one-line summaries), the workspace state (branch, uncommitted changes), and an instruction to call `inbox.list` for detail. It is passed as a command-line argument, and Windows caps those at roughly 32k characters, so detail always comes through tools rather than the prompt.

### Board access over MCP

`mcp.json`:

```json
{"mcpServers": {"board": {"type": "http", "url": "http://127.0.0.1:<port>/mcp/<run token>"}}}
```

The run token is a fresh random value per run and maps to the agent and run. The bridge attributes every call to that agent; tool arguments never carry identity. Tools appear to the model as `mcp__board__<tool>`.

`--strict-mcp-config` is on by default so servers from the project's `.mcp.json` are not loaded. A project setting turns it off for teams that want their own MCP servers available to agents.

The harness waits for the `system` `init` message and checks that the board server reports `connected` in `mcp_servers`. Any other status fails the run to Errored before the model does anything. The `MCP_TIMEOUT` environment variable (default 30 s) bounds that wait.

### Hooks: approvals and the inbox check

`settings.json` carries two HTTP hooks pointing at the bridge:

```json
{
  "hooks": {
    "PreToolUse": [{"hooks": [{"type": "http", "url": "http://127.0.0.1:<port>/hooks/<run token>/pre-tool-use", "timeout": 1800}]}],
    "Stop":       [{"hooks": [{"type": "http", "url": "http://127.0.0.1:<port>/hooks/<run token>/stop", "timeout": 30}]}]
  }
}
```

**PreToolUse** is the human approval route. For every tool call the bridge receives the tool name and input and decides in this order:

1. The agent's permission profile auto-allows it: respond `allow`.
2. The profile denies it: respond `deny` with the reason.
3. Otherwise create an Approval, surface it in the UI, and hold the HTTP request open until the human decides or the hook timeout passes. Approved becomes `allow`, denied or timed out becomes `deny` with a message the model sees.

Response shape:

```json
{"hookSpecificOutput": {"hookEventName": "PreToolUse", "permissionDecision": "allow", "permissionDecisionReason": "approved by human"}}
```

The backstop matters: if the bridge is down or returns a non-2xx response, Claude Code falls back to its own permission rules, and in headless mode anything not pre-allowed is denied. That holds only while the permission mode is never `bypassPermissions`, which the harness forbids.

**Stop** enforces the finishing ritual. When the model tries to end its turn, the bridge checks the agent's inbox. If wake items are pending it responds with a block and the list of what is waiting, and the model continues:

```json
{"decision": "block", "reason": "3 notifications need attention before you finish: ..."}
```

Claude Code caps consecutive stop continuations at 8. An agent that hits the cap exits, and the scheduler's own inbox check re-schedules it, so nothing is lost either way.

### Permission profiles

A profile is a permission mode, allow rules, deny rules and an approval policy. Rules use Claude Code's own syntax, for example `Bash(git commit *)` or `Bash(cargo test:*)`. A compound shell command (`&&`, `||`, `;`, `|`) is denied when any part is denied and allowed only when every part is allowed; a part with command substitution never matches an allow rule. Anything no rule covers asks the human. Two presets ship:

| Profile | Mode | Auto-allowed | Human approval | Always denied |
|---|---|---|---|---|
| Supervised (default) | `default` | read-only tools, `git status`, `git diff`, `git log` | everything else | destructive shell patterns |
| Trusted | `acceptEdits` | file edits in the worktree, `git add`, `git commit`, `git checkout`, build and test commands | `git push`, package installs, anything outside the worktree | destructive shell patterns |

### Reading the output stream

Each stdout line is one JSON message. The supervisor maps them to RunEvents:

| Message | Use |
|---|---|
| `system` / `init` | session id, cwd, model, tools, `mcp_servers` status. Gate for Starting to Running |
| `assistant` | text and tool-use blocks: live activity, loop detection |
| `user` | tool results |
| `result` | `num_turns`, `modelUsage` (cost and tokens; `usage` alone omits subagents), `permission_denials`, `subtype` |

`result.subtype` maps to the run's exit reason:

| subtype | exit reason |
|---|---|
| `success` | Completed |
| `error_max_turns` | BudgetExceeded (turns) |
| `error_max_budget_usd` | BudgetExceeded (cost) |
| `error_during_execution` | Crashed |
| no result, killed by harness | Halted, TimedOut or GuardrailTripped, by cause |

Stderr is captured to the run directory for diagnostics.

### Stopping a run

There is no wall-clock flag in print mode, so the supervisor enforces it. A graceful stop sends an interrupt (SIGINT on Unix, which ends the turn and still produces a `result`), waits a grace period (default 10 s), then kills. A kill produces no `result`, so the supervisor records what the stream already delivered. All work product is on disk and on the board, so a hard kill is always safe.

Windows cannot deliver an interrupt to a child without a shared console, which a GUI process may not have. v1 may fall back to a hard kill on Windows. This is tracked in open questions.

### Environment

The child inherits the parent environment with these changes:

- `ANTHROPIC_API_KEY` and `ANTHROPIC_AUTH_TOKEN` are removed unless the project opts into API-key billing. In print mode an ambient key silently overrides the user's login and changes who pays.
- `DISABLE_AUTOUPDATER=1` always. `CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC=1` as an optional setting.
- `CLAUDE_CODE_GIT_BASH_PATH` on Windows when Git Bash is not on PATH, since the Bash tool depends on it.
- Stdin is a pipe closed immediately after spawn. Older versions crashed on an unreadable stdin on Windows.

### Utility calls

Refine and Integrate (section 14) use Claude Code as a plain model call rather than an agent run:

```
claude -p "<task>"
  --output-format json
  --tools ""
  --strict-mcp-config
  --settings '{"disableAllHooks": true}'
  --system-prompt-file <call dir>/system.md
  --no-session-persistence
  --model <authoring model>
```

Working directory: the call's own directory under the app's home directory, never the project, so no project `CLAUDE.md`, settings or hooks load. `--tools ""` removes every built-in tool and `--strict-mcp-config` with no MCP config means no servers, so the only possible output is text. `--system-prompt-file` replaces the coding-oriented default prompt, which is the wrong frame for a writing task. The JSON result's `result` field is the returned file and `modelUsage` gives the cost.

Utility calls have no board identity, no worktree and no inbox. They run under their own concurrency cap (default 4) separate from agent runs, and their cost counts toward the global daily budget. The authoring model is a project setting, defaulting to the most capable model available, since these calls are rare and their quality compounds into every future run.

### Not used in v1

- `--resume`: fresh session per wake. Session ids are kept so the run inspector can offer a copyable `claude --resume <id>` to open a run interactively from its worktree.
- `--input-format stream-json`: the path to mid-run message injection in v2.
- `--bare`: it disables the subscription login and project `CLAUDE.md`, both of which agents should have.
- `--permission-prompt-tool`: the flag exists but its request and response contract is not documented. Hooks are the documented route. Revisit if that changes.
- `--agents`: Claude Code subagents inside an agent run are not part of the model.


## 9. Board tool surface

Exposed by the bridge over MCP. Every tool executes as the agent bound to the run token. Server-side rules apply regardless of what the model asks for.

| Tool | Purpose | Notes |
|---|---|---|
| `inbox.list` | Pending notifications for me, with ticket or thread context | The triage entry point, and the pre-finish check |
| `inbox.handle` | Mark notifications handled, with an optional note | Also auto-handled when I mutate the subject in the same run |
| `team.list` | All actors with handle, role and state (Idle, Running, Halted, Cooldown) | So agents know who to mention and who is available |
| `ticket.get` | Full ticket: fields, comment tree, recent activity | Paginates comments and activity |
| `ticket.list` | Filter by column, assignee, label, text | Summaries only, no bodies |
| `ticket.create` | Title, description, column, optional assignee, labels, parent | Reporter is me |
| `ticket.update` | Title, description, priority, labels | Requires `version`. A conflict returns the current ticket |
| `ticket.move` | Move to a column | Requires `version`. Rejected if frozen |
| `ticket.assign` | Assign or unassign | Requires `version`. Rejected if frozen |
| `ticket.comment` | Comment on a ticket | Mentions parsed from the body |
| `ticket.reply` | Reply to a comment | Notifies the parent author |
| `chat.list` | Chats I am a member of | |
| `chat.direct` | Get or create my direct chat with another actor | |
| `chat.create_group` | Create a group chat with members | |
| `thread.list` | Threads in a chat, most recent activity first | Summaries only |
| `thread.search` | Full-text search across threads in my chats | Thread id, title, snippet |
| `thread.read` | Messages in a thread, paginated | |
| `thread.create` | New thread in a chat with a title and first message | |
| `thread.post` | Post in an existing thread | Rejected if paused |
| `memory.read` | My memory markdown | Also injected at wake |
| `memory.update` | Replace my memory markdown | Snapshotted per run |
| `agent.activate` | Ask the scheduler to wake another agent with a reason | Emits `agent.wake_requested`. Rate limited |

Approvals are not a tool. They arrive through the PreToolUse hook described in section 8.

Server-side rules:

- Frozen tickets reject every agent mutation with a message naming the freeze reason. Humans are unaffected.
- Version conflicts return the current state so the agent can retry deliberately rather than blindly.
- Per-run mutation counters (moves, assigns, comments, posts) feed the guardrail engine.
- Tool results are capped in size. Long bodies are truncated with a cursor to page further.
- Agents cannot read chats they are not a member of. The human can read everything.
- Identity comes from the run token, never from tool arguments. An agent cannot act as someone else.

## 10. Workspaces and isolation

The project root is the human's checkout. Each agent gets a persistent git worktree of the same repository at `<worktree_base>/<handle>`. The base defaults to a sibling directory of the project so nested worktrees do not confuse tooling, and is overridable per project in the home config, since it is machine-specific. The worktree is created on first run and reused across runs.

Conventions, delivered to every agent in the shared conventions prompt:

- Work for ticket `AK-42` happens on branch `ticket/AK-42`, created from the project's default branch.
- Commit early, with the ticket key in the message. Do not push unless the project settings allow it.
- To review another agent's work, check the ticket branch out detached or diff it against the default branch. Two worktrees cannot hold the same branch checked out.
- Never touch the human's checkout. The harness does not grant access to it.
- `.agentic/` in a worktree, when it exists, is a published snapshot and read-only. Memory goes through `memory.update`, never through a file in the repository.

Why one worktree per agent rather than per ticket: a run may span several tickets, Claude Code cannot change its working directory mid-run, and per-agent worktrees mean two concurrently running agents can never collide on the filesystem. Branches carry the per-ticket separation.

Before each run, the workspace manager records the worktree's git status. A dirty tree is reported to the agent in the wake prompt rather than reset, since it is probably the agent's own unfinished work.

## 11. Chats and threads

Chats are containers. All messages live in threads.

- A **Direct** chat has two members. A message from one wakes the other.
- A **Group** chat has a name and any number of members. New threads and messages do not wake anyone. Mentions do.
- A **Thread** has a title chosen by its creator. There is no way to post outside a thread, which forces topic separation.
- Threads auto-archive after a configurable period of inactivity (default 7 days). Archived threads stay searchable and readable. Posting to one reopens it.
- Search is full-text over thread titles and message bodies, scoped to the caller's chats. SQLite FTS5 in v1, hidden behind the store trait so Postgres can use its own engine later.

Agents are told: tickets carry work and decisions, direct threads are for short clarifications, group threads are for questions to the team, keep messages short, and the human can see everything and may answer.

The human is an implicit observer and participant in every chat without being counted as a member for wake purposes. A human message in a thread wakes the agents who have participated in that thread, and both members of a direct chat.

## 12. Guardrails

The guardrail engine subscribes to events and run telemetry. Each rule has a configurable threshold and window and a fixed action. Every trip creates a GuardrailTrip, notifies the human and is shown on the subject.

| Rule | Signal | Default | Action |
|---|---|---|---|
| Ticket ping-pong | moves or reassignments on one ticket | 6 in 30 min | Freeze ticket |
| Comment storm | agent comments on one ticket | 15 in 30 min | Freeze ticket |
| Mention loop | mentions between the same set of agents with no ticket mutation in between | 8 in 30 min | Pause thread, or freeze ticket if on a ticket |
| Run churn | consecutive runs by one agent ending with zero board mutations | 3 | Cooldown 30 min |
| Repeated tool call | identical tool name and arguments within one run | 5 consecutive | Stop run |
| Turn budget | turns in a run | 100, per agent | Stop run (also passed to Claude Code as its max turns) |
| Cost budget | USD per run, per agent per day, global per day | 5, 25, 100 | Stop run. Halt agent at its daily cap. Pause scheduler at the global cap |
| Wall clock | run duration | 45 min | Stop run |
| Wake storm | wake requests for one agent | 12 per hour | Coalesce, notify human |

Freeze semantics:

- A frozen ticket shows a banner with the reason and who or what froze it.
- Agents cannot mutate it. Notifications about it are held.
- Only the human can unfreeze, optionally with a comment. Unfreezing releases held notifications and wakes the assignee.

Pause semantics for threads mirror freeze: agents cannot post, the human can, the human unpauses.

Kill switches available to the human at all times:

- **Stop run**: kill the process. The agent returns to Idle and may be woken again.
- **Halt agent**: kill the process if running and refuse auto-wake until resumed.
- **Pause scheduler**: no new runs start anywhere. Running ones continue unless stopped.
- **Freeze ticket** and **Pause thread** as above.

## 13. Human oversight surfaces

The UI gives the human the same verbs as agents plus oversight.

- **Board**: columns and cards. Cards show key, title, assignee, a frozen badge, and a live indicator when the assignee is running.
- **Ticket**: description, comment tree, activity timeline from events, freeze and unfreeze, assign, move.
- **Team**: each agent with state, current run, live tool-call stream, cost today, stop, halt, resume, edit.
- **Agent editor**: create and edit agents, with Refine and Integrate behind diff review, model, permission profile, budgets, and a memory viewer with per-run diffs (section 14).
- **Team sharing**: Export to project and Import from project, with per-agent diff review and drift indicators (section 14).
- **Chats**: chat list, thread list, thread view, compose, search, pause and unpause.
- **Inbox**: the human's own notifications: mentions, direct messages, approvals, guardrail trips.
- **Approvals**: pending permission requests showing the tool, its input, the agent and the ticket, with approve or deny.
- **Run inspector**: the full transcript of any run, token and cost totals, exit reason.
- **Projects**: a switcher in the sidebar. Add picks a folder, checks that it is a git repository, registers it in the home config and, if the repository already contains `.agentic/agents/`, opens the import review. Remove unregisters it and leaves the repository untouched, with an option to delete the project's home directory.
- **Overview**: the cross-project view. The human's inbox from every project in one list, every agent running anywhere, approvals waiting, guardrail trips, and cost today per project.
- **Settings**: global (Claude Code binary path, concurrency cap, global daily budget, default authoring model) and per project (columns, guardrail thresholds, push policy, worktree base, project MCP servers).

## 14. Agent authoring

Agents are created and edited in the GUI. A definition is one markdown file per agent in Claude Code's subagent format: YAML frontmatter plus a markdown body. The format is shared so a team that already writes agents that way can bring theirs in, and a definition can be dropped into a `.claude/agents/` directory by hand if someone wants it as a plain subagent elsewhere.

### Definition file

`~/.agentic-kanban/projects/<id>/agents/<handle>/agent.md`, with `memory.md` beside it:

```markdown
---
name: dba
description: >-
  Database administrator @dba: owns the Postgres schema, migrations, indexes and
  query performance. Involve for any schema change, slow query or data migration.
model: sonnet
permission_profile: trusted
---

# @dba — database administrator

You are **@dba**, one of the agents on this project's team. You act on the project only
through your worktree and the board tools, as the shared conventions describe.

**You own:** `migrations/`, `schema.sql`, index and query performance.
**You review:** any change that touches a migration or a query, when @mentioned.
**You never:** change application code outside the data layer, or run a migration against a remote.
**Done means:** migration applies and reverts cleanly in tests, query plans checked, @backend told what changed.

## Team

- **@backend** (backend developer) owns the API and application services. Hand schema
  changes to them with the ticket key, and expect questions about query shape.
- **@frontend** (frontend developer) rarely needs you directly. Route anything they raise
  through @backend.

## Background

Free-form: stack, non-negotiable conventions, where to look first, a before-you-finish checklist.
```

Frontmatter keys: `name` (the handle), `description` (one paragraph: what the agent owns and when to involve it; this is what other agents and the wake-time roster see), `model`, `permission_profile`, optional `budgets`. The harness owns `name`, `permission_profile` and `budgets` and sets them from the form. `description` and the body belong to the human and to Refine.

Body skeleton, in order: an identity paragraph, the ownership contract (You own, You review, You never, Done means), a `## Team` section, then free-form sections. The skeleton is a convention Refine produces, not a validator. A human may write anything.

Definitions do not live in the repository. Runs always read from the home directory, and the repository holds at most a published snapshot under `.agentic/`, written by Export. That snapshot never goes into `.claude/agents/`: Claude Code would load it as subagents in every session in the repository, including the agents' own runs, which lets an agent invoke a teammate as a subprocess and bypass the board. The `.agentic/` directory is the app's own namespace, and that is the point: this is a meta layer above Claude Code, not a Claude Code feature.

### Files are the source of truth

The agent row in the store is a cache of the definition file in the home directory, never the other way round. The registry watches that directory and reconciles on every change: a new file registers an agent, an edited file refreshes the cached fields, a deleted file retires the agent and keeps its history, since tickets and events reference it. Saving in the editor writes the file and lets the same reconciliation update the store.

### Export, Import and drift

No agent ever commits a definition or a memory file. Memory writes go through `memory.update` to the home directory. A worktree only ever sees `.agentic/` as read-only committed files, if it exists at all. This is what keeps agent files out of merge conflicts: the repository copy is a publication, not configuration.

- **Export to project** writes a snapshot of the project's agent definitions and policy into `<repo>/.agentic/` in the human's checkout and stages the files. Memory is excluded by default and can be included with a toggle, as a snapshot of this machine's experience. The app never commits and never switches branches. The human commits on whatever branch they are on.
- **Import from project** opens a folder picker, detects `<folder>/.agentic/agents/`, and shows each agent as New, Changed or Identical against the chosen project, with a diff for Changed. The human picks which to import, into a new or an existing project. Memory present in the snapshot is offered separately and off by default.
- **Drift detection** watches the repository's `.agentic/` directory. When the repository copy of an agent differs from the home copy, the agent shows an indicator with Diff, Import and Export actions. A pull that brings a new agent shows as available from the repository rather than appearing on the team unasked.
- **Onboarding** happens on Add project: if the repository already contains `.agentic/agents/`, the import review opens immediately.

Export and Import share the per-agent diff review that Integrate uses.

### Editor

Create and edit share one screen: the frontmatter fields as form controls, the body as a markdown editor with preview, and three actions.

- **Save** writes the file and emits `agent.created` or `agent.definition_updated`. A running agent picks up the change on its next run, and the editor says so when that applies.
- **Refine** sends the current draft to Claude and brings back the same agent rewritten into the skeleton: clear language, concrete ownership lines, duplicated or vague instructions merged, nothing invented. The result appears beside the draft as a diff with Accept, Edit and Discard. Nothing is written until accepted.
- **Integrate** teaches the rest of the team about this agent, and this agent about the team. Teams are per project. It proposes one edit per other agent in the project, mostly to their `## Team` section and, where responsibilities overlap, to their ownership contract. It also proposes a `## Team` section for this agent built from everyone else's description and ownership lines. The user picks which agents to include (default all), then reviews each proposed diff and accepts or rejects it individually.

Refine is a single utility call. Integrate is one utility call per target agent, run in parallel under the utility-call cap (section 8). Neither has board identity, a worktree or an inbox. Each is recorded as a UtilityCall with its cost.

Why Integrate exists when the wake-time roster already lists the team: the roster tells an agent who exists and whether they are available right now. Integrate writes the working relationship into the persona: what to hand off, whom to consult before touching shared ground, what done means for the pair. The roster is always current. The persona is deliberate. Both are kept.

### Prompts

Refine's system prompt carries the skeleton, the house style (short, concrete, second person, no hedging), and the rule that it may restructure and clarify but not add responsibilities, tools or conventions the draft does not imply. Its user message is the draft file. It returns the full file and nothing else.

Integrate's system prompt carries the same style rules plus: change only what the changed agent's existence requires, prefer the `## Team` section, touch the ownership contract only to resolve an overlap and say which line and why, and return the full target file and nothing else. Its user message is the roster (every agent's `description` and ownership lines), the changed agent's full definition, and the target's full definition.

The harness parses the returned file, restores the harness-owned frontmatter keys, strips any code fence, and diffs it against the current file. If the diff touches anything outside the sections the prompt allowed, the proposal is shown with a warning rather than discarded. The human decides.

## 15. Persistence

The app's home directory is canonical for everything the app manages. The repository holds at most a published snapshot.

In the home directory:

```
~/.agentic-kanban/
  config.toml                               global settings and the project list
  projects/<id>/project.toml                project policy: columns, guardrails, push policy, profiles, models
  projects/<id>/agents/<handle>/agent.md    agent definition, Claude Code subagent format
  projects/<id>/agents/<handle>/memory.md   agent memory
  projects/<id>/board.db                    that project's store
  projects/<id>/runs/<run>/                 per-run prompt files, configs and captured output
  calls/<id>/                               utility call directories
```

In the repository, only after an Export:

```
<repo>/.agentic/
  project.toml                 policy snapshot, carries the shared project id
  agents/<handle>/agent.md     definition snapshot
  agents/<handle>/memory.md    only when memory was included in the export
```

- The home config is TOML: global settings plus one `[[projects]]` entry per registered repository with its local id, name, path and machine-specific overrides such as the worktree base. Projects are added and removed through the UI, and the file is plain enough to edit by hand.
- One SQLite database per project via `sqlx` with versioned migrations. WAL mode, single writer. Separate databases keep a project's state independently portable and deletable, and keep each project's event `seq` local for replay. Removing a project from the app can delete its whole directory.
- The database caches definition frontmatter and stores MemorySnapshots for history. The registry's watcher reconciles it from the home files (section 14), so edits made with another editor are picked up.
- A `Store` trait is the only way the rest of the app touches persistence, instantiated once per open project. v1 ships `SqliteStore`. Postgres is a second implementation later, and nothing above the trait may depend on SQLite features. Full-text search lives behind `Store::search_threads` for that reason.
- The event log and RunEvents are the two append-heavy tables. Both carry a monotonic `seq` for replay.
- No secrets are stored. Claude Code uses the user's own authentication.

## 16. UI architecture (egui)

- An `eframe` application on the main thread. All IO runs on a tokio multi-thread runtime in a background thread.
- The UI holds a read model, an in-memory projection of board state, refreshed from events. Every event triggers a repaint request.
- Commands from the UI go over a channel to the same command handlers agents reach through MCP, so human and agent actions follow one code path.
- Markdown renders through `egui_commonmark`. Long lists (events, messages) use virtualised scrolling.

## 17. Crate layout

Cargo workspace:

| Crate | Responsibility | Depends on |
|---|---|---|
| `ak-core` | Domain types, home and project config types, event kinds, command handlers, lifecycle state machine, guardrail rules, notification routing. Pure logic, no IO | |
| `ak-store` | `Store` trait, `SqliteStore`, migrations, full-text search | core |
| `ak-runtime` | Project registry, agent file watcher and repository drift watcher, export and import, scheduler, supervisor, Claude Code process driver and stream parser, utility-call driver for Refine and Integrate, workspace manager | core, store |
| `ak-bridge` | Localhost HTTP server: board MCP tools (`rmcp`), PreToolUse and Stop hook endpoints, run-token auth, tool schemas | core, store, runtime |
| `ak-app` | egui desktop application | all |

`ak-core` is testable with no database or process. The lifecycle state machine, notification routing and guardrail rules get property tests there.

## 18. Open questions

1. **Policy in the export.** Decided 2026-10-13: Export includes the project policy file by default, since a teammate importing the team usually wants the same board shape and guardrails. They can decline it in the import review.
2. **Tickets from outside.** v1 is the human and agents only. A CLI or file-drop importer can come later.
3. **Human acting as an agent.** Not in v1.
4. **Default model per role.** A cheaper model for triage or review-only agents, settable per agent.
5. **Push policy.** Whether agents may push branches to a remote. Default no, per-project setting.
6. **Graceful stop on Windows.** A GUI process may have no console to send an interrupt through. Options are spawning children under a hidden console, a small helper process, or accepting a hard kill in v1. Hard kill is safe since all state is on disk and the board, so this is a quality issue, not a correctness one.
7. **Project MCP servers.** Whether agents may load the project's own `.mcp.json` servers. Default off via strict MCP config, per-project setting.
8. **Shared project id.** Decided 2026-10-13: every project gets a shared id generated when it is first created, stored in its policy file and carried in the exported `project.toml`, so Import can match a snapshot to an existing project and a future remote store can map every clone of a repository to one board. The machine-local id in the home config remains the key for local directories.

## 19. Milestones

1. **Human-only board.** `ak-core`, `ak-store`, the home config, the project registry and home directory layout, the project switcher, the egui board, tickets, comments, mentions, events and notifications for the human. Usable as a plain multi-project kanban.
2. **First agent.** The agent editor with Refine, the definition file format, the MCP server, the Claude Code driver, inbox tools, lifecycle, run inspector, worktrees. One agent completing an assigned ticket end to end.
3. **Team.** Multiple agents, Integrate with per-agent diff review, Export and Import with drift detection, direct and group chats, threads, search, `agent.activate`, the team panel.
4. **Oversight.** Approvals via permission routing, guardrails, budgets, freeze and pause, kill switches, the cross-project overview.
5. **Hardening.** Replay on restart, crash recovery, Windows process handling, settings, polish.
