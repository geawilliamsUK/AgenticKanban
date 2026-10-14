# Status

Last updated 2026-10-13. The design is [DESIGN.md](DESIGN.md); milestones are its section 19.

## Done

**Milestone 1, human-only multi-project kanban.** Workspace of four crates (`ak-core`, `ak-store`, `ak-runtime`, `ak-app`), 44 tests, clippy clean. Projects register from `~/.agentic-kanban/config.toml`, one SQLite store per project, event log, notification routing, inbox, board with drag and drop, ticket panel with editing, comments with replies, freeze and unfreeze, `@mention` completion and highlighting.

Verified by the user in the running app: creating, opening, editing and moving tickets, commenting, mentions.

**Milestone 2, slice 1: agent definitions and editor.** Definition files in Claude Code's subagent format under `~/.agentic-kanban/projects/<id>/agents/<handle>/` (`agent.md` plus `memory.md`), parsed and rendered by `ak-core` with unknown frontmatter keys preserved. An `agents` table caches each file plus runtime state. The runtime reconciles rows from files on project open, on save, and whenever a file watcher sees a change: new directories add agents, edits refresh, deletions retire (history kept). The app has a Team view with a list, an editor (handle, description, model, permission profile, budgets, body with `@` completion), memory viewer, per-agent inbox, and Retire. Agents are assignable and receive notifications. Refine and Integrate buttons are present but disabled until slice 2.

**Milestone 2, slice 2: utility calls and Refine.** `ak-runtime` locates the Claude Code binary (configured path, native installer default, PATH; never the npm `.cmd` shim) and runs tool-less calls: `-p` with the instruction, material via stdin, `--tools ""`, strict MCP config, hooks disabled, replaced system prompt, no session, cwd in a per-call directory under the home dir with the system prompt, input, stdout and stderr kept. The runner is a trait, so tests use a fake. Refine builds its request from the draft and the roster, strips fences from the reply, restores the harness-owned frontmatter keys with warnings, and records every call (proposal, cost, decision) in a `utility_calls` table. A daily global budget check runs before each call. The editor shows a running state, then the proposal as a line diff with Accept and save, Edit first, and Discard. Pending proposals survive closing the editor. Refine on an unsaved new agent saves the draft first. Verified against the real binary on 2026-10-14 (about three cents per call). The CLI binary has its own login, separate from the desktop app; the runner checks `claude auth status` before each call and reports the login hint. Replies that put a bare `: ` inside an unquoted frontmatter value are repaired by the parser, and the prompt asks for a block-scalar description. Live test: `cargo test -p ak-runtime --test live -- --ignored --nocapture`.

**Milestone 2, slice 3: the bridge.** New crate `ak-bridge`: an axum server on an ephemeral loopback port. `POST /mcp/{token}` is a small MCP Streamable HTTP server (JSON responses only, no streams) exposing the board tools as the run's agent: inbox_list, inbox_handle, team_list, ticket_get, ticket_list, ticket_create, ticket_update, ticket_move, ticket_assign, ticket_comment, ticket_reply, memory_read, memory_update, and agent_activate (which reports that direct activation is not available until the scheduler lands). Tickets are addressed by key, columns by name or kind, actors by handle. Mutating a ticket auto-handles the agent's notifications about it. `POST /hooks/{token}/pre-tool-use` evaluates the agent's permission profile in Claude Code's rule syntax (core `permissions` module); uncovered calls become `approvals` rows and the request waits for the human's decision or times out to deny. `POST /hooks/{token}/stop` blocks the stop with a reason while wake notifications are pending. Tool names use underscores, not the dots in design section 9, because they are exposed as `mcp__board__<name>`. The app does not start the bridge yet; that comes with the supervisor.

## Known gaps and deliberate choices

- Dragging a card moves a small ghost label, not the card itself. Cosmetic; egui's built-in drag source broke clicking, so the card uses its own click-and-drag handling.
- Your own actions never reach your inbox. Design, not a bug. The inbox stays empty until a second actor exists.
- Nothing is committed yet. The user has not asked for commits.
- The desktop-control tools cannot target the dev build in a session that started before its Start menu shortcut existed. The shortcut is at `%APPDATA%\Microsoft\Windows\Start Menu\Programs\AgenticKanban.lnk`; request access to "AgenticKanban" at the start of a new session.
- Settings view (added 2026-10-14): global handle and display name, Claude Code binary path with a Check that reports version and login, Refine model, concurrency caps, daily budget, run retention; per project: name, Refine model override, agent run model, push policy, project MCP servers. Per-agent model is a picker in the agent editor. Model resolution for Refine: project override, then global, then Claude Code's own default. Global settings are re-read by the backend on save; the runner is rebuilt from them.

## Next: milestone 2, first agent

In order, each slice shippable on its own:

1. ~~Agent definitions and editor.~~ Done.
2. ~~Utility calls and Refine.~~ Done, pending a live run.
3. ~~Bridge.~~ Done.
4. **Supervisor and lifecycle.** Spawning Claude Code per run, stream-json parsing into RunEvents, the scheduler state machine, worktree per agent, run inspector in the UI.
5. **One agent completing an assigned ticket end to end.**
