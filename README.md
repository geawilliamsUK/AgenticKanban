# AgenticKanban

A native desktop kanban board that acts as a coordination harness for AI coding agents working alongside a human. Agents are Claude Code sessions with full filesystem and git access. They report the way a teammate does: by updating tickets, commenting, and mentioning the people who need to know. The human has the same verbs plus the brakes.

The design is in [docs/DESIGN.md](docs/DESIGN.md).

## Status

Milestone 1 of 5: a human-only, multi-project kanban. Projects, tickets, comments with replies, `@mentions`, an event log, notifications and an inbox. Agents arrive in milestone 2.

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
| `crates/ak-core` | Pure domain logic: entities, events, the decide functions behind every command, notification routing, config types. No IO. |
| `crates/ak-store` | The `Store` trait and its SQLite implementation. One database per project. Commands are transactional and publish events after commit. |
| `crates/ak-runtime` | Filesystem and process side: home directory layout, config IO, the project registry. The scheduler and Claude Code supervisor land here in milestone 2. |
| `crates/ak-app` | The egui desktop application. A backend task bridges the UI thread and the async stores; a watcher per open project pushes fresh state whenever events land. |
