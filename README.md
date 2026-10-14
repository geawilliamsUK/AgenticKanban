# AgenticKanban

A native desktop kanban board that acts as a coordination harness for AI coding agents working alongside a human. Agents are Claude Code sessions with full filesystem and git access. They report the way a teammate does: by updating tickets, commenting, and mentioning the people who need to know. The human has the same verbs plus the brakes.

The design is in [docs/DESIGN.md](docs/DESIGN.md).

## Status

Milestones 1 and 2 are implemented: the human-only board, then agents. Agents are defined in the GUI (Claude Code subagent format) with Refine and Integrate, run as headless Claude Code processes in their own git worktrees, reach the board through a localhost MCP bridge with per-agent permission profiles and human approvals, and are scheduled from wake notifications under budgets and guardrails. Direct and group chats with threads and search, Export and Import of agent definitions through a repository's `.agentic/` snapshot, an Overview across projects, and a run inspector are in. Progress, known gaps and what comes next are kept in [docs/STATUS.md](docs/STATUS.md).

Two ignored tests exercise the real Claude Code binary and cost real money (about half a dollar each): `cargo test -p ak-supervisor --test live -- --ignored --nocapture` has an agent complete a ticket end to end, and `--test live_chat` has an agent answer a question in a direct thread.

## Build and run

Requires a stable Rust toolchain (1.85 or newer) and, on Windows, the MSVC build tools that rustup's default toolchain already depends on.

```bash
cargo run -p agentic-kanban
```

```bash
cargo test --workspace
```

The app keeps all of its state under `~/.agentic-kanban/`. Set `AGENTIC_KANBAN_HOME` to use a different directory. Repositories you register are never modified.

## Layout

| Crate | What it is |
|---|---|
| `crates/ak-core` | Pure domain logic: entities, events, the decide functions behind every command, notification routing, permission rules, guardrail rules, config types. No IO. |
| `crates/ak-store` | The `Store` trait and its SQLite implementation. One database per project. Commands are transactional and publish events after commit. |
| `crates/ak-runtime` | Filesystem and process side: home directory layout, config IO, the project registry, agent definition files, utility calls (Refine, Integrate), the `.agentic/` snapshot, the instance lock. |
| `crates/ak-bridge` | The localhost MCP server that exposes the board to a run, plus the permission and stop hooks. |
| `crates/ak-supervisor` | Spawns and watches one Claude Code process per run, schedules runs, manages worktrees, composes prompts, applies guardrails. |
| `crates/ak-app` | The egui desktop application. A backend task bridges the UI thread and the async runtime; watchers per project push fresh state whenever events land. |
