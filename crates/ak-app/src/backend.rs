//! The bridge between the UI thread and the async runtime.
//!
//! The UI sends `Request`s over a channel. The backend runs them against the
//! registry and stores on a tokio runtime, and sends `Response`s back. Every
//! open project has watchers on its event bus and its agents directory, and
//! a scheduler that wakes agents. Run notices from the supervisor and
//! approval events from the bridge are forwarded to the UI through an
//! internal channel, so the loop stays the single owner of the registry.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

use ak_bridge::{Bridge, BridgeEvent};
use ak_core::agent::{Agent, AgentDefinition, AgentState};
use ak_core::approval::Approval;
use ak_core::chat::{Thread, ThreadHit};
use ak_core::commands::{NewTicket, TicketPatch};
use ak_core::config::{GlobalSettings, ProjectEntry, ProjectPolicy};
use ak_core::ids::{
    ActorId, ApprovalId, CallId, ChatId, ColumnId, CommentId, NotificationId, ProjectId, RunId,
    ThreadId, TicketId,
};
use ak_core::run::{AgentRun, RunEvent};
use ak_core::utility::{UtilityCall, UtilityKind, UtilityState};
use ak_core::{Actor, ActorKind, Event, NotificationState, Timestamp};
use ak_runtime::agents::{self, AgentWatcher};
use ak_runtime::authoring::{self, RosterEntry, TeamMember};
use ak_runtime::claude::{self, ClaudeProbe};
use ak_runtime::snapshot::{
    self, DriftEntry, ExportOptions, ExportReport, ImportReview, ImportSelection,
};
use ak_runtime::{HomePaths, OpenProject, ProjectRegistry, RuntimeError, UtilityRunner};
use ak_store::Store;
use ak_supervisor::{GuardrailMonitor, RunNotice, Scheduler, Supervisor, SupervisorConfig};

use crate::model::{
    BoardModel, ChatSummary, InboxItem, ProjectOverview, ThreadDetail, TicketDetail,
};

/// Builds the utility runner for the current settings. Injected so tests can
/// supply a fake.
pub type RunnerFactory = Box<dyn Fn(&GlobalSettings) -> Arc<dyn UtilityRunner> + Send + Sync>;

#[allow(clippy::large_enum_variant)]
pub enum Request {
    AddProject {
        path: PathBuf,
        name: Option<String>,
    },
    RemoveProject {
        id: ProjectId,
        delete_data: bool,
    },
    OpenProject {
        id: ProjectId,
    },
    LoadTicket {
        project: ProjectId,
        id: TicketId,
    },
    CreateTicket {
        project: ProjectId,
        input: NewTicket,
    },
    UpdateTicket {
        project: ProjectId,
        id: TicketId,
        patch: TicketPatch,
        version: i64,
    },
    MoveTicket {
        project: ProjectId,
        id: TicketId,
        to: ColumnId,
        version: i64,
    },
    AssignTicket {
        project: ProjectId,
        id: TicketId,
        to: Option<ActorId>,
        version: i64,
    },
    Comment {
        project: ProjectId,
        ticket: TicketId,
        body: String,
        parent: Option<CommentId>,
    },
    Freeze {
        project: ProjectId,
        id: TicketId,
        reason: String,
    },
    Unfreeze {
        project: ProjectId,
        id: TicketId,
    },
    MarkInboxSeen {
        project: ProjectId,
    },
    SetNotificationState {
        project: ProjectId,
        id: NotificationId,
        state: NotificationState,
    },
    LoadAgent {
        project: ProjectId,
        handle: String,
    },
    /// Writes the definition file and reconciles. Creates the agent when the
    /// handle is new.
    SaveAgent {
        project: ProjectId,
        definition: AgentDefinition,
    },
    /// Deletes the agent's directory. Reconciliation retires the agent.
    RetireAgent {
        project: ProjectId,
        handle: String,
    },
    /// Runs Refine on a draft. Saves the draft first when the agent is new.
    Refine {
        project: ProjectId,
        draft: AgentDefinition,
    },
    /// Records the decision. With `save`, writes the accepted definition.
    AcceptProposal {
        project: ProjectId,
        call_id: CallId,
        definition: AgentDefinition,
        save: bool,
    },
    RejectProposal {
        project: ProjectId,
        call_id: CallId,
        handle: String,
    },
    /// Runs Integrate for a saved agent against the chosen teammates. The
    /// agent's own handle among the targets means its Team section.
    Integrate {
        project: ProjectId,
        handle: String,
        targets: Vec<String>,
    },
    // Chats.
    LoadThreads {
        project: ProjectId,
        chat: ChatId,
        include_archived: bool,
    },
    LoadThread {
        project: ProjectId,
        thread: ThreadId,
    },
    CreateDirectChat {
        project: ProjectId,
        with: ActorId,
    },
    CreateGroupChat {
        project: ProjectId,
        name: String,
        members: Vec<ActorId>,
    },
    CreateThread {
        project: ProjectId,
        chat: ChatId,
        title: String,
        body: String,
    },
    PostMessage {
        project: ProjectId,
        thread: ThreadId,
        body: String,
    },
    SetThreadPaused {
        project: ProjectId,
        thread: ThreadId,
        paused: bool,
    },
    SearchThreads {
        project: ProjectId,
        query: String,
    },
    // Snapshot: export to and import from a repository's .agentic/ directory.
    ExportAgents {
        project: ProjectId,
        options: ExportOptions,
    },
    ReviewImport {
        project: ProjectId,
        source: PathBuf,
    },
    /// Applies the review the human saw, so nothing is re-read behind them.
    ApplyImport {
        project: ProjectId,
        review: ImportReview,
        selection: ImportSelection,
    },
    CheckDrift {
        project: ProjectId,
    },
    /// The cross-project summary.
    LoadOverview,
    SaveGlobalSettings(GlobalSettings),
    SaveProjectPolicy {
        project: ProjectId,
        policy: ProjectPolicy,
    },
    /// Locates the Claude Code binary and asks it about its login.
    ProbeClaude,
    // Agent control.
    WakeAgent {
        project: ProjectId,
        handle: String,
    },
    StopAgent {
        project: ProjectId,
        handle: String,
    },
    HaltAgent {
        project: ProjectId,
        handle: String,
    },
    ResumeAgent {
        project: ProjectId,
        handle: String,
    },
    SetSchedulerPaused {
        project: ProjectId,
        paused: bool,
    },
    DecideApproval {
        project: ProjectId,
        id: ApprovalId,
        approve: bool,
    },
    LoadRunEvents {
        project: ProjectId,
        run_id: RunId,
    },
}

/// A proposal awaiting review, with the text it would replace.
#[derive(Clone)]
pub struct PendingProposal {
    pub call: UtilityCall,
    pub proposed: AgentDefinition,
    pub original: String,
}

#[allow(clippy::large_enum_variant)]
pub enum Response {
    Projects(Vec<ProjectEntry>),
    Settings {
        settings: GlobalSettings,
        runner_description: String,
        bridge_addr: Option<String>,
    },
    ProjectOpened {
        id: ProjectId,
        name: String,
        human: Actor,
        policy: ProjectPolicy,
    },
    ProjectPolicy {
        project: ProjectId,
        policy: ProjectPolicy,
    },
    ClaudeProbe(ClaudeProbe),
    Board {
        project: ProjectId,
        board: BoardModel,
    },
    TicketDetail {
        project: ProjectId,
        detail: TicketDetail,
    },
    Inbox {
        project: ProjectId,
        items: Vec<InboxItem>,
    },
    AgentLoaded {
        project: ProjectId,
        handle: String,
        /// None when the definition file is gone (the agent is retired).
        definition: Option<AgentDefinition>,
        memory: String,
        inbox: Vec<InboxItem>,
        retired_since: Option<Timestamp>,
        pending: Option<PendingProposal>,
        runs: Vec<AgentRun>,
        /// Pending Integrate proposals this agent authored, by target handle.
        integrations: Vec<(String, PendingProposal)>,
        cost_today: f64,
    },
    RefineStarted {
        project: ProjectId,
        handle: String,
    },
    Proposal {
        project: ProjectId,
        handle: String,
        proposal: PendingProposal,
    },
    RefineFailed {
        project: ProjectId,
        handle: String,
        error: String,
    },
    IntegrateStarted {
        project: ProjectId,
        handle: String,
        targets: Vec<String>,
    },
    IntegrateProposal {
        project: ProjectId,
        handle: String,
        target: String,
        proposal: PendingProposal,
    },
    IntegrateFailed {
        project: ProjectId,
        handle: String,
        target: String,
        error: String,
    },
    Chats {
        project: ProjectId,
        chats: Vec<ChatSummary>,
    },
    Threads {
        project: ProjectId,
        chat: ChatId,
        threads: Vec<Thread>,
    },
    ThreadDetail {
        project: ProjectId,
        detail: ThreadDetail,
    },
    ThreadSearch {
        project: ProjectId,
        query: String,
        hits: Vec<ThreadHit>,
    },
    /// A chat was opened or created from the UI; the view should select it.
    ChatOpened {
        project: ProjectId,
        chat: ChatId,
    },
    ThreadOpened {
        project: ProjectId,
        thread: ThreadId,
    },
    ExportDone {
        project: ProjectId,
        report: ExportReport,
    },
    ImportReview {
        project: ProjectId,
        review: ImportReview,
    },
    Drift {
        project: ProjectId,
        entries: Vec<DriftEntry>,
    },
    Overview(Vec<ProjectOverview>),
    RunStarted {
        project: ProjectId,
        run: AgentRun,
    },
    RunEvent {
        project: ProjectId,
        event: RunEvent,
    },
    RunEnded {
        project: ProjectId,
        run: AgentRun,
    },
    RunEvents {
        project: ProjectId,
        run_id: RunId,
        events: Vec<RunEvent>,
    },
    Approvals {
        project: ProjectId,
        items: Vec<Approval>,
    },
    SchedulerPaused {
        project: ProjectId,
        paused: bool,
    },
    Error(String),
    Info(String),
}

enum Internal {
    Run(RunNotice),
    Bridge(BridgeEvent),
}

struct ProjectWatchers {
    events: JoinHandle<()>,
    files: JoinHandle<()>,
    _agent_watcher: AgentWatcher,
    /// Watches the repository's `.agentic/agents`, once it exists.
    snapshot: Option<(AgentWatcher, JoinHandle<()>)>,
    scheduler: Option<Scheduler>,
    guardrails: GuardrailMonitor,
}

impl ProjectWatchers {
    fn stop(self) {
        self.events.abort();
        self.files.abort();
        if let Some((_, task)) = self.snapshot {
            task.abort();
        }
        self.guardrails.shutdown();
        if let Some(s) = self.scheduler {
            s.shutdown();
        }
    }
}

pub struct Backend {
    registry: ProjectRegistry,
    runner_factory: RunnerFactory,
    runner: Arc<dyn UtilityRunner>,
    settings: GlobalSettings,
    rx: UnboundedReceiver<Request>,
    tx: Sender<Response>,
    ctx: egui::Context,
    watchers: HashMap<ProjectId, ProjectWatchers>,
    bridge: Option<Bridge>,
    supervisor: Option<Arc<Supervisor>>,
    slots: Arc<Semaphore>,
    utility_slots: Arc<Semaphore>,
    internal_tx: UnboundedSender<Internal>,
    internal_rx: UnboundedReceiver<Internal>,
}

impl Backend {
    pub fn new(
        registry: ProjectRegistry,
        runner_factory: RunnerFactory,
        rx: UnboundedReceiver<Request>,
        tx: Sender<Response>,
        ctx: egui::Context,
    ) -> Self {
        let settings = registry.config().settings.clone();
        let runner = runner_factory(&settings);
        let slots = Arc::new(Semaphore::new(settings.max_concurrent_runs.max(1) as usize));
        let utility_slots = Arc::new(Semaphore::new(
            settings.max_concurrent_utility_calls.max(1) as usize
        ));
        let (internal_tx, internal_rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            registry,
            runner_factory,
            runner,
            settings,
            rx,
            tx,
            ctx,
            watchers: HashMap::new(),
            bridge: None,
            supervisor: None,
            slots,
            utility_slots,
            internal_tx,
            internal_rx,
        }
    }

    pub async fn run(mut self) {
        self.start_agent_runtime().await;
        // Every registered project runs: agents wake in all of them, and
        // the overview sees all of them.
        let ids: Vec<ProjectId> = self.registry.projects().iter().map(|e| e.id).collect();
        for id in ids {
            if let Err(e) = self.ensure_open(id).await {
                tracing::warn!(project = %id, "could not open project: {e}");
            }
        }
        self.send(Response::Projects(self.registry.projects().to_vec()));
        self.send_settings();
        let mut housekeeping = tokio::time::interval(Duration::from_secs(3600));
        loop {
            tokio::select! {
                req = self.rx.recv() => match req {
                    Some(req) => {
                        if let Err(msg) = self.handle(req).await {
                            self.send(Response::Error(msg));
                        }
                    }
                    None => break,
                },
                msg = self.internal_rx.recv() => {
                    if let Some(msg) = msg {
                        self.handle_internal(msg).await;
                    }
                }
                _ = housekeeping.tick() => {
                    let ids: Vec<ProjectId> = self.registry.projects().iter().map(|e| e.id).collect();
                    for id in ids {
                        if let Some(p) = self.registry.opened(id) {
                            archive_idle_threads(&p).await;
                            prune_runs(&p, self.registry.paths(), self.settings.run_retention_days).await;
                        }
                    }
                }
            }
        }
        for (_, w) in self.watchers.drain() {
            w.stop();
        }
    }

    /// Starts the bridge and the supervisor. Without them the app still
    /// works as a board; agents just cannot run.
    async fn start_agent_runtime(&mut self) {
        match Bridge::start().await {
            Ok(bridge) => {
                let supervisor = Arc::new(Supervisor::new(
                    bridge.clone(),
                    self.registry.paths().clone(),
                    self.supervisor_config(),
                ));
                let mut notices = supervisor.subscribe();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    loop {
                        match notices.recv().await {
                            Ok(n) => {
                                if tx.send(Internal::Run(n)).is_err() {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        }
                    }
                });
                let mut events = bridge.subscribe();
                let tx = self.internal_tx.clone();
                tokio::spawn(async move {
                    loop {
                        match events.recv().await {
                            Ok(e) => {
                                if tx.send(Internal::Bridge(e)).is_err() {
                                    return;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
                        }
                    }
                });
                self.bridge = Some(bridge);
                self.supervisor = Some(supervisor);
            }
            Err(e) => {
                tracing::error!("bridge failed to start: {e}");
                self.send(Response::Error(format!(
                    "agent bridge failed to start, agents cannot run: {e}"
                )));
            }
        }
    }

    /// Saves a policy, reports it back, and keeps the project list's name
    /// in step with the policy's.
    fn apply_policy(&mut self, project: ProjectId, policy: ProjectPolicy) -> Result<(), String> {
        let p = self.project(project)?;
        let name = policy.name.trim().to_string();
        if name.is_empty() {
            return Err("project name cannot be empty".into());
        }
        self.registry
            .save_project_policy(project, &policy)
            .map_err(|e| e.to_string())?;
        self.send(Response::ProjectPolicy {
            project,
            policy: p.policy(),
        });
        let renamed = self.registry.entry(project).is_some_and(|e| e.name != name);
        if renamed {
            self.registry
                .rename_project(project, &name)
                .map_err(|e| e.to_string())?;
            self.send(Response::Projects(self.registry.projects().to_vec()));
        }
        Ok(())
    }

    /// Starts watching the repository's snapshot directory when it exists
    /// and is not watched yet. Never creates it.
    fn ensure_snapshot_watch(&mut self, id: ProjectId) {
        let Some(p) = self.registry.opened(id) else {
            return;
        };
        let Some(w) = self.watchers.get_mut(&id) else {
            return;
        };
        if w.snapshot.is_some() {
            return;
        }
        let dir = snapshot::SnapshotPaths::new(&p.entry.path).agents_dir();
        let (file_tx, file_rx) = tokio::sync::mpsc::unbounded_channel();
        match agents::watch_if_exists(&dir, file_tx) {
            Ok(Some(watcher)) => {
                let task = tokio::spawn(watch_snapshot(
                    p.clone(),
                    file_rx,
                    self.tx.clone(),
                    self.ctx.clone(),
                ));
                w.snapshot = Some((watcher, task));
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("snapshot watcher failed: {e}"),
        }
    }

    /// One line per open project for the overview.
    async fn overview(&self) -> Vec<ProjectOverview> {
        let midnight = start_of_today();
        let mut out = Vec::new();
        for entry in self.registry.projects() {
            let Some(p) = self.registry.opened(entry.id) else {
                continue;
            };
            let store = p.store();
            let (
                Ok(agents),
                Ok(active),
                Ok(approvals),
                Ok(inbox),
                Ok(run_cost),
                Ok(utility_cost),
                Ok(trips),
            ) = (
                store.agents().await,
                store.active_runs().await,
                store.pending_approvals().await,
                store.notifications(p.human.id, true).await,
                store.run_cost_since(midnight, None).await,
                store.utility_cost_since(midnight).await,
                store.guardrail_trips_since(midnight).await,
            )
            else {
                continue;
            };
            let live: Vec<Agent> = agents.into_iter().filter(|a| !a.is_retired()).collect();
            let running = live
                .iter()
                .filter(|a| a.state.is_active())
                .map(|a| {
                    let started = active
                        .iter()
                        .find(|r| r.agent_id == a.actor_id)
                        .map(|r| r.started_at)
                        .unwrap_or_else(Utc::now);
                    (a.handle.clone(), started)
                })
                .collect();
            let paused = self
                .watchers
                .get(&entry.id)
                .and_then(|w| w.scheduler.as_ref())
                .map(|s| s.paused())
                .unwrap_or(true);
            out.push(ProjectOverview {
                id: entry.id,
                name: entry.name.clone(),
                agents: live.len(),
                running,
                halted: live
                    .iter()
                    .filter(|a| a.state == AgentState::Halted)
                    .count(),
                approvals: approvals.len(),
                unread: inbox
                    .iter()
                    .filter(|n| n.state == NotificationState::Pending)
                    .count(),
                cost_today: run_cost + utility_cost,
                paused,
                trips,
            });
        }
        out
    }

    fn supervisor_config(&self) -> SupervisorConfig {
        SupervisorConfig {
            binary: claude::locate(self.settings.claude_binary.as_deref())
                .map_err(|e| e.to_string()),
            pass_api_key: false,
            startup_timeout: Duration::from_secs(90),
        }
    }

    fn send(&self, response: Response) {
        if self.tx.send(response).is_ok() {
            self.ctx.request_repaint();
        }
    }

    fn send_settings(&self) {
        self.send(Response::Settings {
            settings: self.settings.clone(),
            runner_description: self.runner.describe(),
            bridge_addr: self.bridge.as_ref().map(|b| b.addr().to_string()),
        });
    }

    fn project(&self, id: ProjectId) -> Result<Arc<OpenProject>, String> {
        self.registry
            .opened(id)
            .ok_or_else(|| "project is not open".to_string())
    }

    fn scheduler(&self, id: ProjectId) -> Result<Scheduler, String> {
        self.watchers
            .get(&id)
            .and_then(|w| w.scheduler.clone())
            .ok_or_else(|| "agents cannot run: the agent runtime is not available".to_string())
    }

    async fn agent_actor(&self, p: &OpenProject, handle: &str) -> Result<ActorId, String> {
        p.store()
            .agent_by_handle(handle)
            .await
            .map_err(|e| e.to_string())?
            .map(|a| a.actor_id)
            .ok_or_else(|| format!("no agent @{handle}"))
    }

    async fn handle_internal(&mut self, msg: Internal) {
        match msg {
            Internal::Run(RunNotice::Started { project, run }) => {
                if let Some(p) = self.registry.opened(project) {
                    let _ = push_board(&p, &self.tx, &self.ctx).await;
                }
                self.send(Response::RunStarted { project, run });
            }
            Internal::Run(RunNotice::Event { project, event }) => {
                self.send(Response::RunEvent { project, event });
            }
            Internal::Run(RunNotice::Ended { project, run }) => {
                if let Some(p) = self.registry.opened(project) {
                    let _ = push_board(&p, &self.tx, &self.ctx).await;
                    let _ = push_inbox(&p, &self.tx, &self.ctx).await;
                }
                self.send(Response::RunEnded { project, run });
            }
            Internal::Bridge(BridgeEvent::ApprovalRequested(a)) => {
                if let Some(p) = self.project_of_agent(a.agent_id).await {
                    let _ = push_approvals(&p, &self.tx, &self.ctx).await;
                    self.send(Response::Info(format!("Approval needed: {}", a.summary)));
                }
            }
            Internal::Bridge(BridgeEvent::RunGuardrail { reason, .. }) => {
                self.send(Response::Error(format!(
                    "Guardrail stopped a run: {reason}"
                )));
            }
            Internal::Bridge(BridgeEvent::ApprovalDecided(a)) => {
                if let Some(p) = self.project_of_agent(a.agent_id).await {
                    let _ = push_approvals(&p, &self.tx, &self.ctx).await;
                }
            }
        }
    }

    async fn project_of_agent(&self, agent: ActorId) -> Option<Arc<OpenProject>> {
        for entry in self.registry.projects() {
            if let Some(p) = self.registry.opened(entry.id) {
                if p.store().agent(agent).await.is_ok() {
                    return Some(p);
                }
            }
        }
        None
    }

    async fn handle(&mut self, req: Request) -> Result<(), String> {
        match req {
            Request::AddProject { path, name } => {
                let id = self
                    .registry
                    .add_project(&path, name)
                    .map_err(|e| e.to_string())?;
                self.send(Response::Projects(self.registry.projects().to_vec()));
                self.send(Response::Info("Project added".into()));
                self.open_project(id).await?;
                // Onboarding: a repository that already carries a snapshot
                // offers its agents straight away.
                if let Some(p) = self.registry.opened(id) {
                    if snapshot::SnapshotPaths::new(&p.entry.path).exists() {
                        match snapshot::review_import(&p.entry.path, &p.agents, &p.policy()) {
                            Ok(review) => self.send(Response::ImportReview {
                                project: id,
                                review,
                            }),
                            Err(e) => tracing::warn!("onboarding review failed: {e}"),
                        }
                    }
                }
            }
            Request::RemoveProject { id, delete_data } => {
                if let Some(w) = self.watchers.remove(&id) {
                    w.stop();
                }
                self.registry
                    .remove_project(id, delete_data)
                    .await
                    .map_err(|e| e.to_string())?;
                self.send(Response::Projects(self.registry.projects().to_vec()));
                self.send(Response::Info("Project removed".into()));
            }
            Request::OpenProject { id } => self.open_project(id).await?,
            Request::LoadTicket { project, id } => {
                let p = self.project(project)?;
                let store = p.store();
                let detail = TicketDetail {
                    ticket: store.ticket(id).await.map_err(|e| e.to_string())?,
                    comments: store.comments(id).await.map_err(|e| e.to_string())?,
                    events: store
                        .events_for_ticket(id)
                        .await
                        .map_err(|e| e.to_string())?,
                    watchers: store.watchers(id).await.map_err(|e| e.to_string())?,
                };
                self.send(Response::TicketDetail { project, detail });
            }
            Request::CreateTicket { project, input } => {
                let p = self.project(project)?;
                p.store()
                    .create_ticket(input, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::UpdateTicket {
                project,
                id,
                patch,
                version,
            } => {
                let p = self.project(project)?;
                p.store()
                    .update_ticket(id, patch, version, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::MoveTicket {
                project,
                id,
                to,
                version,
            } => {
                let p = self.project(project)?;
                p.store()
                    .move_ticket(id, to, version, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::AssignTicket {
                project,
                id,
                to,
                version,
            } => {
                let p = self.project(project)?;
                p.store()
                    .assign_ticket(id, to, version, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::Comment {
                project,
                ticket,
                body,
                parent,
            } => {
                let p = self.project(project)?;
                p.store()
                    .comment(ticket, body, parent, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::Freeze {
                project,
                id,
                reason,
            } => {
                let p = self.project(project)?;
                p.store()
                    .freeze_ticket(id, reason, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::Unfreeze { project, id } => {
                let p = self.project(project)?;
                p.store()
                    .unfreeze_ticket(id, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Request::MarkInboxSeen { project } => {
                let p = self.project(project)?;
                p.store()
                    .mark_notifications_seen(p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
                push_inbox(&p, &self.tx, &self.ctx).await?;
            }
            Request::SetNotificationState { project, id, state } => {
                let p = self.project(project)?;
                p.store()
                    .set_notification_state(id, state)
                    .await
                    .map_err(|e| e.to_string())?;
                push_inbox(&p, &self.tx, &self.ctx).await?;
            }
            Request::LoadAgent { project, handle } => {
                let p = self.project(project)?;
                push_agent(&p, &handle, &self.tx, &self.ctx).await?;
            }
            Request::SaveAgent {
                project,
                definition,
            } => {
                let p = self.project(project)?;
                save_definition(&p, &definition).await?;
                push_board(&p, &self.tx, &self.ctx).await?;
                push_agent(&p, definition.handle(), &self.tx, &self.ctx).await?;
                self.send(Response::Info(format!("Saved @{}", definition.handle())));
            }
            Request::RetireAgent { project, handle } => {
                let p = self.project(project)?;
                p.agents.remove(&handle).map_err(|e| e.to_string())?;
                agents::reconcile(&p.agents, p.store.as_ref())
                    .await
                    .map_err(|e| e.to_string())?;
                push_board(&p, &self.tx, &self.ctx).await?;
                push_agent(&p, &handle, &self.tx, &self.ctx).await?;
                self.send(Response::Info(format!("Retired @{handle}")));
            }
            Request::LoadThreads {
                project,
                chat,
                include_archived,
            } => {
                let p = self.project(project)?;
                let threads = p
                    .store()
                    .threads(chat, include_archived)
                    .await
                    .map_err(|e| e.to_string())?;
                self.send(Response::Threads {
                    project,
                    chat,
                    threads,
                });
            }
            Request::LoadThread { project, thread } => {
                let p = self.project(project)?;
                push_thread(&p, thread, &self.tx, &self.ctx).await?;
            }
            Request::CreateDirectChat { project, with } => {
                let p = self.project(project)?;
                let chat = p
                    .store()
                    .direct_chat(p.human.id, with, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
                push_chats(&p, &self.tx, &self.ctx).await?;
                self.send(Response::ChatOpened {
                    project,
                    chat: chat.id,
                });
            }
            Request::CreateGroupChat {
                project,
                name,
                members,
            } => {
                let p = self.project(project)?;
                let chat = p
                    .store()
                    .create_group_chat(name, members, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
                push_chats(&p, &self.tx, &self.ctx).await?;
                self.send(Response::ChatOpened {
                    project,
                    chat: chat.id,
                });
            }
            Request::CreateThread {
                project,
                chat,
                title,
                body,
            } => {
                let p = self.project(project)?;
                let (thread, _) = p
                    .store()
                    .create_thread(chat, title, body, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
                let threads = p
                    .store()
                    .threads(chat, false)
                    .await
                    .map_err(|e| e.to_string())?;
                self.send(Response::Threads {
                    project,
                    chat,
                    threads,
                });
                self.send(Response::ThreadOpened {
                    project,
                    thread: thread.id,
                });
            }
            Request::PostMessage {
                project,
                thread,
                body,
            } => {
                let p = self.project(project)?;
                p.store()
                    .post_message(thread, body, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
                push_thread(&p, thread, &self.tx, &self.ctx).await?;
            }
            Request::SetThreadPaused {
                project,
                thread,
                paused,
            } => {
                let p = self.project(project)?;
                p.store()
                    .set_thread_paused(thread, paused, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
                push_thread(&p, thread, &self.tx, &self.ctx).await?;
                self.send(Response::Info(if paused {
                    "Thread paused: agents cannot post until you unpause it".into()
                } else {
                    "Thread unpaused".into()
                }));
            }
            Request::SearchThreads { project, query } => {
                let p = self.project(project)?;
                let hits = p
                    .store()
                    .search_threads(None, &query, 30)
                    .await
                    .map_err(|e| e.to_string())?;
                self.send(Response::ThreadSearch {
                    project,
                    query,
                    hits,
                });
            }
            Request::ExportAgents { project, options } => {
                let p = self.project(project)?;
                let report = snapshot::export(&p.agents, &p.policy(), &p.entry.path, &options)
                    .map_err(|e| e.to_string())?;
                self.ensure_snapshot_watch(project);
                push_drift(&p, &self.tx, &self.ctx).await?;
                self.send(Response::ExportDone { project, report });
            }
            Request::ReviewImport { project, source } => {
                let p = self.project(project)?;
                let review = snapshot::review_import(&source, &p.agents, &p.policy())
                    .map_err(|e| e.to_string())?;
                self.send(Response::ImportReview { project, review });
            }
            Request::ApplyImport {
                project,
                review,
                selection,
            } => {
                let p = self.project(project)?;
                let (written, policy) = snapshot::apply_import(&review, &selection, &p.agents)
                    .map_err(|e| e.to_string())?;
                let report = agents::reconcile(&p.agents, p.store.as_ref())
                    .await
                    .map_err(|e| e.to_string())?;
                for (handle, msg) in &report.errors {
                    tracing::warn!(handle, "imported definition rejected: {msg}");
                }
                if let Some(policy) = policy {
                    self.apply_policy(project, policy)?;
                }
                push_board(&p, &self.tx, &self.ctx).await?;
                push_drift(&p, &self.tx, &self.ctx).await?;
                self.send(Response::Info(format!(
                    "Imported {} agent(s)",
                    written.len()
                )));
            }
            Request::LoadOverview => {
                let list = self.overview().await;
                self.send(Response::Overview(list));
            }
            Request::CheckDrift { project } => {
                let p = self.project(project)?;
                push_drift(&p, &self.tx, &self.ctx).await?;
            }
            Request::Integrate {
                project,
                handle,
                targets,
            } => {
                let p = self.project(project)?;
                self.start_integrate(p, handle, targets).await?;
            }
            Request::Refine { project, draft } => {
                let p = self.project(project)?;
                self.start_refine(p, draft).await?;
            }
            Request::AcceptProposal {
                project,
                call_id,
                definition,
                save,
            } => {
                let p = self.project(project)?;
                let store = p.store();
                let mut call = store
                    .utility_call(call_id)
                    .await
                    .map_err(|e| e.to_string())?;
                call.state = UtilityState::Accepted;
                store
                    .update_utility_call(&call)
                    .await
                    .map_err(|e| e.to_string())?;
                if save {
                    save_definition(&p, &definition).await?;
                    push_board(&p, &self.tx, &self.ctx).await?;
                    push_agent(&p, definition.handle(), &self.tx, &self.ctx).await?;
                    self.send(Response::Info(format!("Saved @{}", definition.handle())));
                }
            }
            Request::RejectProposal {
                project,
                call_id,
                handle,
            } => {
                let p = self.project(project)?;
                let store = p.store();
                let mut call = store
                    .utility_call(call_id)
                    .await
                    .map_err(|e| e.to_string())?;
                call.state = UtilityState::Rejected;
                store
                    .update_utility_call(&call)
                    .await
                    .map_err(|e| e.to_string())?;
                push_agent(&p, &handle, &self.tx, &self.ctx).await?;
            }
            Request::SaveGlobalSettings(settings) => self.save_global_settings(settings).await?,
            Request::SaveProjectPolicy { project, policy } => {
                self.apply_policy(project, policy)?;
                self.send(Response::Info("Project settings saved".into()));
            }
            Request::ProbeClaude => {
                let probe = claude::probe(self.settings.claude_binary.as_deref()).await;
                self.send(Response::ClaudeProbe(probe));
            }
            Request::WakeAgent { project, handle } => {
                let p = self.project(project)?;
                let actor = self.agent_actor(&p, &handle).await?;
                let scheduler = self.scheduler(project)?;
                scheduler.activate(actor, p.human.id, "woken from the Team view".into());
                self.send(Response::Info(format!("Waking @{handle}")));
            }
            Request::StopAgent { project, handle } => {
                let p = self.project(project)?;
                let actor = self.agent_actor(&p, &handle).await?;
                self.scheduler(project)?.stop(actor);
                self.send(Response::Info(format!("Stopping @{handle}")));
            }
            Request::HaltAgent { project, handle } => {
                let p = self.project(project)?;
                let actor = self.agent_actor(&p, &handle).await?;
                self.scheduler(project)?
                    .halt(actor)
                    .await
                    .map_err(|e| e.to_string())?;
                push_board(&p, &self.tx, &self.ctx).await?;
                self.send(Response::Info(format!("Halted @{handle}")));
            }
            Request::ResumeAgent { project, handle } => {
                let p = self.project(project)?;
                let actor = self.agent_actor(&p, &handle).await?;
                self.scheduler(project)?
                    .resume(actor)
                    .await
                    .map_err(|e| e.to_string())?;
                push_board(&p, &self.tx, &self.ctx).await?;
                self.send(Response::Info(format!("Resumed @{handle}")));
            }
            Request::SetSchedulerPaused { project, paused } => {
                let scheduler = self.scheduler(project)?;
                scheduler.set_paused(paused);
                self.send(Response::SchedulerPaused { project, paused });
            }
            Request::DecideApproval {
                project,
                id,
                approve,
            } => {
                let p = self.project(project)?;
                let bridge = self
                    .bridge
                    .clone()
                    .ok_or_else(|| "the agent runtime is not available".to_string())?;
                bridge
                    .decide(p.store().as_ref(), id, approve, p.human.id)
                    .await
                    .map_err(|e| e.to_string())?;
                push_approvals(&p, &self.tx, &self.ctx).await?;
            }
            Request::LoadRunEvents { project, run_id } => {
                let p = self.project(project)?;
                let events = p
                    .store()
                    .run_events(run_id)
                    .await
                    .map_err(|e| e.to_string())?;
                self.send(Response::RunEvents {
                    project,
                    run_id,
                    events,
                });
            }
        }
        Ok(())
    }

    async fn save_global_settings(&mut self, settings: GlobalSettings) -> Result<(), String> {
        Actor::validate_handle(&settings.human_handle).map_err(|e| e.to_string())?;
        if settings.human_display_name.trim().is_empty() {
            return Err("display name cannot be empty".into());
        }
        let human_changed = settings.human_handle != self.settings.human_handle
            || settings.human_display_name != self.settings.human_display_name;

        // The handle must be free in every open project before anything is written.
        let open: Vec<Arc<OpenProject>> = self
            .registry
            .projects()
            .iter()
            .filter_map(|e| self.registry.opened(e.id))
            .collect();
        if human_changed {
            for p in &open {
                if let Some(other) = p
                    .store()
                    .actor_by_handle(&settings.human_handle)
                    .await
                    .map_err(|e| e.to_string())?
                {
                    if other.id != p.human.id {
                        return Err(format!(
                            "@{} is already an agent in {}",
                            settings.human_handle, p.entry.name
                        ));
                    }
                }
            }
        }

        self.registry
            .update_settings(settings.clone())
            .map_err(|e| e.to_string())?;
        self.settings = settings.clone();
        self.runner = (self.runner_factory)(&self.settings);
        self.utility_slots = Arc::new(Semaphore::new(
            self.settings.max_concurrent_utility_calls.max(1) as usize,
        ));
        if let Some(s) = &self.supervisor {
            s.set_config(self.supervisor_config());
        }
        for w in self.watchers.values() {
            if let Some(s) = &w.scheduler {
                s.update_settings(self.settings.clone());
            }
        }
        self.send_settings();

        if human_changed {
            for p in &open {
                let actor = Actor {
                    id: p.human.id,
                    kind: ActorKind::Human,
                    handle: settings.human_handle.clone(),
                    display_name: settings.human_display_name.clone(),
                };
                p.store()
                    .upsert_actor(&actor)
                    .await
                    .map_err(|e| e.to_string())?;
                push_board(p, &self.tx, &self.ctx).await?;
            }
        }
        self.send(Response::Info("Settings saved".into()));
        Ok(())
    }

    /// Opens the project's store and starts its watchers, scheduler and
    /// guardrail monitor if they are not running. Quiet: nothing is pushed.
    async fn ensure_open(&mut self, id: ProjectId) -> Result<Arc<OpenProject>, String> {
        let p = self.registry.open(id).await.map_err(|e| e.to_string())?;
        if !self.watchers.contains_key(&id) {
            // Runs a previous session left open ended with that session.
            match p
                .store()
                .recover_stale_runs("the app was not running when this run ended")
                .await
            {
                Ok(n) if n > 0 => {
                    tracing::warn!(project = %p.entry.name, "closed {n} run(s) left open by a previous session")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("run recovery failed: {e}"),
            }
            let events = tokio::spawn(watch_events(p.clone(), self.tx.clone(), self.ctx.clone()));
            let (file_tx, file_rx) = tokio::sync::mpsc::unbounded_channel();
            let agent_watcher =
                agents::watch(p.agents.dir(), file_tx).map_err(|e| e.to_string())?;
            let files = tokio::spawn(watch_agent_files(
                p.clone(),
                file_rx,
                self.tx.clone(),
                self.ctx.clone(),
            ));
            let scheduler = self.supervisor.as_ref().map(|s| {
                Scheduler::spawn(
                    p.clone(),
                    s.clone(),
                    self.slots.clone(),
                    self.settings.clone(),
                )
            });
            let guardrails = GuardrailMonitor::spawn(p.clone());
            self.watchers.insert(
                id,
                ProjectWatchers {
                    events,
                    files,
                    _agent_watcher: agent_watcher,
                    snapshot: None,
                    scheduler,
                    guardrails,
                },
            );
            self.ensure_snapshot_watch(id);
            archive_idle_threads(&p).await;
        }
        Ok(p)
    }

    /// Opens the project if needed and announces it to the UI with its state.
    async fn open_project(&mut self, id: ProjectId) -> Result<(), String> {
        let p = self.ensure_open(id).await?;
        self.send(Response::ProjectOpened {
            id,
            name: p.entry.name.clone(),
            human: p.human.clone(),
            policy: p.policy(),
        });
        push_board(&p, &self.tx, &self.ctx).await?;
        push_inbox(&p, &self.tx, &self.ctx).await?;
        push_approvals(&p, &self.tx, &self.ctx).await?;
        push_chats(&p, &self.tx, &self.ctx).await?;
        push_drift(&p, &self.tx, &self.ctx).await?;
        let paused = self
            .watchers
            .get(&id)
            .and_then(|w| w.scheduler.as_ref())
            .map(|s| s.paused())
            .unwrap_or(true);
        self.send(Response::SchedulerPaused {
            project: id,
            paused,
        });
        Ok(())
    }

    /// Refuses a utility call once today's spend has reached the global cap.
    async fn check_utility_budget(&self, store: &dyn Store) -> Result<(), String> {
        let since_midnight = Utc::now()
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .map(|t| t.and_utc())
            .unwrap_or_else(Utc::now);
        let spent = store
            .utility_cost_since(since_midnight)
            .await
            .map_err(|e| e.to_string())?;
        let limit = self.settings.global_daily_budget_usd;
        if spent >= limit {
            return Err(RuntimeError::BudgetExceeded { limit, spent }.to_string());
        }
        Ok(())
    }

    /// One call per target, in parallel under the utility cap. Each answer
    /// arrives as its own proposal; the call row is recorded first.
    async fn start_integrate(
        &self,
        p: Arc<OpenProject>,
        handle: String,
        targets: Vec<String>,
    ) -> Result<(), String> {
        if targets.is_empty() {
            return Err("pick at least one agent to integrate with".into());
        }
        let store = p.store();
        let source = store
            .agent_by_handle(&handle)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("no agent @{handle}"))?;
        let changed = p
            .agents
            .read_definition(&handle)
            .map_err(|e| e.to_string())?;
        self.check_utility_budget(store.as_ref()).await?;

        // The roster comes from the files: descriptions and contracts.
        let mut team = Vec::new();
        let mut defs: HashMap<String, (ak_core::agent::Agent, AgentDefinition)> = HashMap::new();
        for a in store.agents().await.map_err(|e| e.to_string())? {
            if a.is_retired() {
                continue;
            }
            match p.agents.read_definition(&a.handle) {
                Ok(d) => {
                    team.push(TeamMember::from_definition(&d));
                    defs.insert(a.handle.clone(), (a, d));
                }
                Err(e) => tracing::warn!(handle = %a.handle, "definition skipped: {e}"),
            }
        }
        let model = p
            .policy()
            .authoring_model
            .clone()
            .or_else(|| self.settings.authoring_model.clone());

        let mut started = Vec::new();
        let mut jobs = Vec::new();
        for target in targets {
            let Some((agent, def)) = defs.get(&target) else {
                return Err(format!("no agent @{target} to integrate with"));
            };
            let call = UtilityCall::start(
                UtilityKind::Integrate,
                source.actor_id,
                Some(agent.actor_id),
                model.clone(),
                Utc::now(),
            );
            store
                .insert_utility_call(&call)
                .await
                .map_err(|e| e.to_string())?;
            let request = authoring::integrate_request(&changed, def, &team, model.clone());
            started.push(target.clone());
            jobs.push((target, def.clone(), call, request));
        }
        self.send(Response::IntegrateStarted {
            project: p.id(),
            handle: handle.clone(),
            targets: started,
        });

        for (target, def, mut call, request) in jobs {
            let runner = self.runner.clone();
            let slots = self.utility_slots.clone();
            let tx = self.tx.clone();
            let ctx = self.ctx.clone();
            let p = p.clone();
            let handle = handle.clone();
            tokio::spawn(async move {
                let _permit = slots.acquire_owned().await.ok();
                let outcome = runner.call(request).await.and_then(|out| {
                    authoring::apply_integrate(&def, &out.text).map(|prop| (prop, out.cost_usd))
                });
                let now = Utc::now();
                let project = p.id();
                let response = match outcome {
                    Ok((proposal, cost)) => {
                        call.propose(
                            proposal.definition.render(),
                            proposal.warnings.clone(),
                            cost,
                            now,
                        );
                        if let Err(e) = p.store().update_utility_call(&call).await {
                            tracing::warn!("could not record proposal: {e}");
                        }
                        Response::IntegrateProposal {
                            project,
                            handle,
                            target,
                            proposal: PendingProposal {
                                call,
                                proposed: proposal.definition,
                                original: def.render(),
                            },
                        }
                    }
                    Err(e) => {
                        call.fail(e.to_string(), None, now);
                        if let Err(e) = p.store().update_utility_call(&call).await {
                            tracing::warn!("could not record failure: {e}");
                        }
                        Response::IntegrateFailed {
                            project,
                            handle,
                            target,
                            error: e.to_string(),
                        }
                    }
                };
                if tx.send(response).is_ok() {
                    ctx.request_repaint();
                }
            });
        }
        Ok(())
    }

    /// Records the call, answers immediately, and runs the model call in its
    /// own task so the loop stays responsive.
    async fn start_refine(
        &self,
        p: Arc<OpenProject>,
        draft: AgentDefinition,
    ) -> Result<(), String> {
        let handle = draft.handle().to_string();
        let store = p.store();

        // A brand-new agent has no row yet: save the draft first.
        let agent = match store
            .agent_by_handle(&handle)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(a) => a,
            None => {
                save_definition(&p, &draft).await?;
                push_board(&p, &self.tx, &self.ctx).await?;
                store
                    .agent_by_handle(&handle)
                    .await
                    .map_err(|e| e.to_string())?
                    .ok_or_else(|| "the agent could not be created".to_string())?
            }
        };

        self.check_utility_budget(store.as_ref()).await?;

        // Project override, then the global setting, then Claude Code's own default.
        let model = p
            .policy()
            .authoring_model
            .clone()
            .or_else(|| self.settings.authoring_model.clone());
        let mut call = UtilityCall::start(
            UtilityKind::Refine,
            agent.actor_id,
            None,
            model.clone(),
            Utc::now(),
        );
        store
            .insert_utility_call(&call)
            .await
            .map_err(|e| e.to_string())?;
        self.send(Response::RefineStarted {
            project: p.id(),
            handle: handle.clone(),
        });

        let roster: Vec<RosterEntry> = store
            .agents()
            .await
            .map_err(|e| e.to_string())?
            .into_iter()
            .filter(|a| !a.is_retired())
            .map(|a| RosterEntry {
                handle: a.handle,
                description: a.description,
            })
            .collect();
        let request = authoring::refine_request(&draft, &roster, model);

        let runner = self.runner.clone();
        let tx = self.tx.clone();
        let ctx = self.ctx.clone();
        let project = p.id();
        tokio::spawn(async move {
            let outcome = runner.call(request).await.and_then(|out| {
                authoring::apply_refine(&draft, &out.text).map(|prop| (prop, out.cost_usd))
            });
            let now = Utc::now();
            let response = match outcome {
                Ok((proposal, cost)) => {
                    call.propose(
                        proposal.definition.render(),
                        proposal.warnings.clone(),
                        cost,
                        now,
                    );
                    if let Err(e) = p.store().update_utility_call(&call).await {
                        tracing::warn!("could not record proposal: {e}");
                    }
                    Response::Proposal {
                        project,
                        handle,
                        proposal: PendingProposal {
                            call,
                            proposed: proposal.definition,
                            original: draft.render(),
                        },
                    }
                }
                Err(e) => {
                    call.fail(e.to_string(), None, now);
                    if let Err(e) = p.store().update_utility_call(&call).await {
                        tracing::warn!("could not record failure: {e}");
                    }
                    Response::RefineFailed {
                        project,
                        handle,
                        error: e.to_string(),
                    }
                }
            };
            if tx.send(response).is_ok() {
                ctx.request_repaint();
            }
        });
        Ok(())
    }
}

/// Writes a definition and reconciles, reporting a rejected file as an error.
async fn save_definition(p: &OpenProject, definition: &AgentDefinition) -> Result<(), String> {
    p.agents
        .write_definition(definition)
        .map_err(|e| e.to_string())?;
    let report = agents::reconcile(&p.agents, p.store.as_ref())
        .await
        .map_err(|e| e.to_string())?;
    if let Some((_, msg)) = report.errors.iter().find(|(h, _)| h == definition.handle()) {
        return Err(format!("saved, but the definition was rejected: {msg}"));
    }
    Ok(())
}

/// Pushes a fresh board and inbox whenever the project's store publishes
/// events. Bursts are coalesced with a short debounce.
async fn watch_events(p: Arc<OpenProject>, tx: Sender<Response>, ctx: egui::Context) {
    let mut rx = p.store.subscribe();
    loop {
        match rx.recv().await {
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        }
        tokio::time::sleep(Duration::from_millis(40)).await;
        while rx.try_recv().is_ok() {}
        if push_board(&p, &tx, &ctx).await.is_err() {
            return;
        }
        let _ = push_inbox(&p, &tx, &ctx).await;
        let _ = push_chats(&p, &tx, &ctx).await;
    }
}

/// Reconciles agent rows whenever a definition file changes on disk, so an
/// edit in another editor or a restored file shows up without a restart.
async fn watch_agent_files(
    p: Arc<OpenProject>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    tx: Sender<Response>,
    ctx: egui::Context,
) {
    while rx.recv().await.is_some() {
        while rx.try_recv().is_ok() {}
        match agents::reconcile(&p.agents, p.store.as_ref()).await {
            Ok(report) => {
                for (handle, msg) in &report.errors {
                    tracing::warn!(handle, "agent definition skipped: {msg}");
                }
                if report.changed() && push_board(&p, &tx, &ctx).await.is_err() {
                    return;
                }
                let _ = push_drift(&p, &tx, &ctx).await;
            }
            Err(e) => tracing::warn!("agent reconciliation failed: {e}"),
        }
    }
}

async fn push_board(
    p: &OpenProject,
    tx: &Sender<Response>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let store = p.store();
    let agents = store.agents().await.map_err(|e| e.to_string())?;
    let mut counts = HashMap::new();
    for a in &agents {
        let open = store
            .notifications(a.actor_id, true)
            .await
            .map_err(|e| e.to_string())?;
        counts.insert(a.actor_id, open.len());
    }
    let board = BoardModel::new(
        store.columns().await.map_err(|e| e.to_string())?,
        store.tickets().await.map_err(|e| e.to_string())?,
        store.actors().await.map_err(|e| e.to_string())?,
        agents,
        counts,
    );
    tx.send(Response::Board {
        project: p.id(),
        board,
    })
    .map_err(|_| "ui gone".to_string())?;
    ctx.request_repaint();
    Ok(())
}

/// Every notification for an actor, joined with its event.
async fn load_inbox(
    store: &dyn Store,
    actor: ActorId,
    open_only: bool,
) -> Result<Vec<InboxItem>, String> {
    let notifications = store
        .notifications(actor, open_only)
        .await
        .map_err(|e| e.to_string())?;
    let recent = store
        .events_since(0, 5000)
        .await
        .map_err(|e| e.to_string())?;
    let by_id: HashMap<_, Event> = recent.into_iter().map(|e| (e.id, e)).collect();
    Ok(notifications
        .into_iter()
        .map(|n| InboxItem {
            event: by_id.get(&n.event_id).cloned(),
            notification: n,
        })
        .collect())
}

async fn push_inbox(
    p: &OpenProject,
    tx: &Sender<Response>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let items = load_inbox(p.store.as_ref(), p.human.id, false).await?;
    tx.send(Response::Inbox {
        project: p.id(),
        items,
    })
    .map_err(|_| "ui gone".to_string())?;
    ctx.request_repaint();
    Ok(())
}

fn start_of_today() -> Timestamp {
    Utc::now()
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .map(|t| t.and_utc())
        .unwrap_or_else(Utc::now)
}

/// Drops transcripts and run directories older than the retention period.
/// Run rows stay, so cost history does.
async fn prune_runs(p: &OpenProject, paths: &HomePaths, days: u32) {
    let before = Utc::now() - chrono::Duration::days(days.max(1) as i64);
    match p.store().prune_run_events(before).await {
        Ok(ids) => {
            for id in &ids {
                let dir = paths.run_dir(p.id(), *id);
                if dir.is_dir() {
                    if let Err(e) = std::fs::remove_dir_all(&dir) {
                        tracing::warn!("could not remove {}: {e}", dir.display());
                    }
                }
            }
            if !ids.is_empty() {
                tracing::info!(project = %p.entry.name, "pruned {} run transcript(s)", ids.len());
            }
        }
        Err(e) => tracing::warn!("run pruning failed: {e}"),
    }
}

/// Archives threads idle for longer than the project's period. Quiet: no
/// events, nothing to notify.
async fn archive_idle_threads(p: &OpenProject) {
    let days = p.policy().thread_archive_days.max(1) as i64;
    let since = Utc::now() - chrono::Duration::days(days);
    match p.store().archive_idle_threads(since).await {
        Ok(n) if n > 0 => tracing::info!(project = %p.entry.name, "archived {n} idle thread(s)"),
        Ok(_) => {}
        Err(e) => tracing::warn!("could not archive idle threads: {e}"),
    }
}

async fn push_chats(
    p: &OpenProject,
    tx: &Sender<Response>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let store = p.store();
    let mut chats = Vec::new();
    for chat in store.chats().await.map_err(|e| e.to_string())? {
        let members = store
            .chat_members(chat.id)
            .await
            .map_err(|e| e.to_string())?;
        chats.push(ChatSummary { chat, members });
    }
    tx.send(Response::Chats {
        project: p.id(),
        chats,
    })
    .map_err(|_| "ui gone".to_string())?;
    ctx.request_repaint();
    Ok(())
}

async fn push_thread(
    p: &OpenProject,
    id: ThreadId,
    tx: &Sender<Response>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let store = p.store();
    let detail = ThreadDetail {
        thread: store.thread(id).await.map_err(|e| e.to_string())?,
        messages: store.messages(id).await.map_err(|e| e.to_string())?,
        participants: store
            .thread_participants(id)
            .await
            .map_err(|e| e.to_string())?,
    };
    tx.send(Response::ThreadDetail {
        project: p.id(),
        detail,
    })
    .map_err(|_| "ui gone".to_string())?;
    ctx.request_repaint();
    Ok(())
}

/// Recomputes drift whenever the repository's snapshot changes on disk, so
/// a pull that brings new or changed agents shows up without a restart.
async fn watch_snapshot(
    p: Arc<OpenProject>,
    mut rx: tokio::sync::mpsc::UnboundedReceiver<()>,
    tx: Sender<Response>,
    ctx: egui::Context,
) {
    while rx.recv().await.is_some() {
        while rx.try_recv().is_ok() {}
        if push_drift(&p, &tx, &ctx).await.is_err() {
            return;
        }
    }
}

async fn push_drift(
    p: &OpenProject,
    tx: &Sender<Response>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let entries = snapshot::drift(&p.agents, &p.entry.path).map_err(|e| e.to_string())?;
    tx.send(Response::Drift {
        project: p.id(),
        entries,
    })
    .map_err(|_| "ui gone".to_string())?;
    ctx.request_repaint();
    Ok(())
}

async fn push_approvals(
    p: &OpenProject,
    tx: &Sender<Response>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let items = p
        .store()
        .pending_approvals()
        .await
        .map_err(|e| e.to_string())?;
    tx.send(Response::Approvals {
        project: p.id(),
        items,
    })
    .map_err(|_| "ui gone".to_string())?;
    ctx.request_repaint();
    Ok(())
}

/// Pending Integrate proposals this agent authored, with each target's
/// handle and current file. Retired targets and unparseable proposals are
/// skipped.
async fn load_integrations(
    p: &OpenProject,
    source: ActorId,
) -> Result<Vec<(String, PendingProposal)>, String> {
    let store = p.store();
    let mut out = Vec::new();
    for call in store
        .pending_proposals(source, UtilityKind::Integrate)
        .await
        .map_err(|e| e.to_string())?
    {
        let Some(target_id) = call.target_id else {
            continue;
        };
        let Ok(target) = store.agent(target_id).await else {
            continue;
        };
        if target.is_retired() {
            continue;
        }
        let Some(proposed) = call
            .proposal
            .as_deref()
            .and_then(|t| AgentDefinition::parse(t).ok())
        else {
            continue;
        };
        let original = p
            .agents
            .read_definition(&target.handle)
            .map(|d| d.render())
            .unwrap_or_default();
        out.push((
            target.handle.clone(),
            PendingProposal {
                call,
                proposed,
                original,
            },
        ));
    }
    Ok(out)
}

async fn push_agent(
    p: &OpenProject,
    handle: &str,
    tx: &Sender<Response>,
    ctx: &egui::Context,
) -> Result<(), String> {
    let store = p.store();
    let agent = store
        .agent_by_handle(handle)
        .await
        .map_err(|e| e.to_string())?;
    let definition = if p.agents.definition_path(handle).is_file() {
        Some(
            p.agents
                .read_definition(handle)
                .map_err(|e| e.to_string())?,
        )
    } else {
        None
    };
    let memory = p.agents.read_memory(handle).map_err(|e| e.to_string())?;
    let (inbox, retired_since, pending, runs, integrations, cost_today) = match &agent {
        Some(a) => {
            let pending = store
                .pending_proposal(a.actor_id, UtilityKind::Refine)
                .await
                .map_err(|e| e.to_string())?
                .and_then(|call| {
                    let proposed = AgentDefinition::parse(call.proposal.as_deref()?).ok()?;
                    let original = definition.as_ref().map(|d| d.render()).unwrap_or_default();
                    Some(PendingProposal {
                        call,
                        proposed,
                        original,
                    })
                });
            let runs = store
                .runs_for_agent(a.actor_id, 20)
                .await
                .map_err(|e| e.to_string())?;
            let integrations = load_integrations(p, a.actor_id).await?;
            let cost_today = store
                .run_cost_since(start_of_today(), Some(a.actor_id))
                .await
                .map_err(|e| e.to_string())?;
            (
                load_inbox(p.store.as_ref(), a.actor_id, true).await?,
                a.missing_since,
                pending,
                runs,
                integrations,
                cost_today,
            )
        }
        None => (Vec::new(), None, None, Vec::new(), Vec::new(), 0.0),
    };
    tx.send(Response::AgentLoaded {
        project: p.id(),
        handle: handle.to_string(),
        definition,
        memory,
        inbox,
        retired_since,
        pending,
        runs,
        integrations,
        cost_today,
    })
    .map_err(|_| "ui gone".to_string())?;
    ctx.request_repaint();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    use ak_core::ids::ColumnId;
    use ak_runtime::{FakeRunner, HomePaths};

    /// Waits for the first response matching `pred`, discarding others.
    fn next<F: Fn(&Response) -> bool>(rx: &mpsc::Receiver<Response>, pred: F) -> Response {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let r = rx
                .recv_timeout(remaining)
                .expect("timed out waiting for a response");
            if pred(&r) {
                return r;
            }
        }
    }

    /// A fake Refine: returns the draft with a rewritten description, and
    /// records which model it was asked for.
    fn fake_factory() -> RunnerFactory {
        Box::new(|_settings| {
            Arc::new(FakeRunner::new(|req| {
                // The last file in the input is the one to rewrite: the draft
                // for Refine, the target for Integrate.
                let start = req
                    .input
                    .rfind(
                        "---
name:",
                    )
                    .ok_or("no definition in input")?;
                let base =
                    AgentDefinition::parse(&req.input[start..]).map_err(|e| e.to_string())?;
                let mut refined = base;
                if req.instruction.contains("target") {
                    let changed = req
                        .label
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("@?")
                        .to_string();
                    refined.body.push_str(&format!(
                        "
## Team

- **{changed}**: integrated by the fake.
"
                    ));
                } else {
                    refined.frontmatter.description = format!(
                        "Refined description. model={}",
                        req.model.clone().unwrap_or_default()
                    );
                    refined.body = "# refined body

**You own:** everything
"
                    .into();
                }
                Ok(format!("```markdown\n{}\n```", refined.render()))
            }))
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn backend_round_trip_through_registry_store_and_watcher() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let registry = ProjectRegistry::load(HomePaths::at(tmp.path().join("home"))).unwrap();

        let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = mpsc::channel();
        tokio::spawn(
            Backend::new(
                registry,
                fake_factory(),
                req_rx,
                resp_tx,
                egui::Context::default(),
            )
            .run(),
        );

        match next(&resp_rx, |r| matches!(r, Response::Projects(_))) {
            Response::Projects(p) => assert!(p.is_empty()),
            _ => unreachable!(),
        }
        match next(&resp_rx, |r| matches!(r, Response::Settings { .. })) {
            Response::Settings {
                settings,
                bridge_addr,
                ..
            } => {
                assert_eq!(settings.human_handle, "me");
                assert!(bridge_addr.is_some(), "the bridge is listening");
            }
            _ => unreachable!(),
        }

        // Adding a project opens it and pushes a board and an inbox.
        req_tx
            .send(Request::AddProject {
                path: repo.clone(),
                name: Some("Demo".into()),
            })
            .unwrap();
        let project = match next(&resp_rx, |r| matches!(r, Response::ProjectOpened { .. })) {
            Response::ProjectOpened {
                id,
                name,
                human,
                policy,
            } => {
                assert_eq!(name, "Demo");
                assert_eq!(human.handle, "me");
                assert_eq!(policy.permission_profiles.len(), 2);
                assert!(policy.authoring_model.is_none());
                id
            }
            _ => unreachable!(),
        };
        let columns: Vec<ColumnId> = match next(&resp_rx, |r| matches!(r, Response::Board { .. })) {
            Response::Board { board, .. } => board.columns.iter().map(|c| c.id).collect(),
            _ => unreachable!(),
        };
        assert_eq!(columns.len(), 5);
        next(&resp_rx, |r| matches!(r, Response::Inbox { .. }));
        match next(&resp_rx, |r| matches!(r, Response::SchedulerPaused { .. })) {
            Response::SchedulerPaused { paused, .. } => assert!(!paused),
            _ => unreachable!(),
        }

        // Pause the scheduler for the rest of the test so no run starts
        // against a binary that is not there.
        req_tx
            .send(Request::SetSchedulerPaused {
                project,
                paused: true,
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::SchedulerPaused { .. })) {
            Response::SchedulerPaused { paused, .. } => assert!(paused),
            _ => unreachable!(),
        }

        // A mutation is not acknowledged directly; the watcher pushes a fresh board.
        req_tx
            .send(Request::CreateTicket {
                project,
                input: NewTicket {
                    title: "Hello".into(),
                    column_id: columns[1],
                    ..Default::default()
                },
            })
            .unwrap();
        let ticket = match next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if !board.tickets.is_empty()),
        ) {
            Response::Board { board, .. } => board.tickets[0].clone(),
            _ => unreachable!(),
        };
        assert_eq!(ticket.key, "DEM-1");

        req_tx
            .send(Request::MoveTicket {
                project,
                id: ticket.id,
                to: columns[2],
                version: ticket.version,
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if board.tickets[0].column_id == columns[2]),
        );

        req_tx
            .send(Request::LoadTicket {
                project,
                id: ticket.id,
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::TicketDetail { .. })) {
            Response::TicketDetail { detail, .. } => {
                let names: Vec<&str> = detail.events.iter().map(|e| e.kind.name()).collect();
                assert_eq!(names, vec!["ticket.created", "ticket.moved"]);
                assert_eq!(detail.ticket.version, 2);
            }
            _ => unreachable!(),
        }

        // Client errors come back as Error responses rather than killing the backend.
        req_tx
            .send(Request::MoveTicket {
                project,
                id: ticket.id,
                to: columns[3],
                version: 999,
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::Error(_))) {
            Response::Error(msg) => assert!(msg.contains("version conflict"), "{msg}"),
            _ => unreachable!(),
        }

        // Project settings: a Refine model override is saved and reported back,
        // and a rename reaches the project list.
        let mut policy = ProjectPolicy::new("Demo");
        policy.authoring_model = Some("sonnet".into());
        policy.name = "Demo Renamed".into();
        req_tx
            .send(Request::SaveProjectPolicy { project, policy })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::ProjectPolicy { .. })) {
            Response::ProjectPolicy { policy, .. } => {
                assert_eq!(policy.authoring_model.as_deref(), Some("sonnet"));
            }
            _ => unreachable!(),
        }
        match next(&resp_rx, |r| matches!(r, Response::Projects(_))) {
            Response::Projects(p) => assert_eq!(p[0].name, "Demo Renamed"),
            _ => unreachable!(),
        }

        // Saving an agent definition creates the agent and makes it assignable.
        req_tx
            .send(Request::SaveAgent {
                project,
                definition: AgentDefinition::template("backend", "Backend developer"),
            })
            .unwrap();
        let backend_id = match next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if !board.agents.is_empty()),
        ) {
            Response::Board { board, .. } => {
                assert_eq!(board.agents[0].handle, "backend");
                assert!(board.assignable().any(|a| a.handle == "backend"));
                board.agents[0].actor_id
            }
            _ => unreachable!(),
        };
        let saved = match next(&resp_rx, |r| matches!(r, Response::AgentLoaded { .. })) {
            Response::AgentLoaded {
                handle,
                definition,
                pending,
                runs,
                ..
            } => {
                assert_eq!(handle, "backend");
                assert!(pending.is_none());
                assert!(runs.is_empty());
                definition.unwrap()
            }
            _ => unreachable!(),
        };

        // Refine runs through the (fake) runner with the project's model and
        // comes back as a proposal.
        req_tx
            .send(Request::Refine {
                project,
                draft: saved.clone(),
            })
            .unwrap();
        next(&resp_rx, |r| matches!(r, Response::RefineStarted { .. }));
        let proposal = match next(&resp_rx, |r| matches!(r, Response::Proposal { .. })) {
            Response::Proposal { proposal, .. } => proposal,
            _ => unreachable!(),
        };
        assert_eq!(
            proposal.proposed.frontmatter.description,
            "Refined description. model=sonnet"
        );
        assert_eq!(proposal.call.model.as_deref(), Some("sonnet"));
        assert_eq!(proposal.proposed.handle(), "backend");
        assert!(
            proposal.call.warnings.is_empty(),
            "{:?}",
            proposal.call.warnings
        );
        assert_eq!(proposal.call.state, UtilityState::Proposed);
        assert_eq!(proposal.original, saved.render());

        // Reloading the agent surfaces the pending proposal until it is decided.
        req_tx
            .send(Request::LoadAgent {
                project,
                handle: "backend".into(),
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::AgentLoaded { .. })) {
            Response::AgentLoaded { pending, .. } => {
                assert_eq!(pending.unwrap().call.id, proposal.call.id);
            }
            _ => unreachable!(),
        }

        // Accepting with save writes the file and the reload shows the new text.
        req_tx
            .send(Request::AcceptProposal {
                project,
                call_id: proposal.call.id,
                definition: proposal.proposed.clone(),
                save: true,
            })
            .unwrap();
        match next(
            &resp_rx,
            |r| matches!(r, Response::AgentLoaded { definition: Some(d), .. } if d.frontmatter.description.starts_with("Refined description.")),
        ) {
            Response::AgentLoaded { pending, .. } => assert!(pending.is_none()),
            _ => unreachable!(),
        }

        // Chats: a direct chat with the agent, a thread in it, and the agent's
        // wake; pausing and search go through the same channel.
        req_tx
            .send(Request::CreateDirectChat {
                project,
                with: backend_id,
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::Chats { .. })) {
            Response::Chats { chats, .. } => {
                assert_eq!(chats.len(), 1);
                assert!(chats[0].members.contains(&backend_id));
            }
            _ => unreachable!(),
        }
        let chat_id = match next(&resp_rx, |r| matches!(r, Response::ChatOpened { .. })) {
            Response::ChatOpened { chat, .. } => chat,
            _ => unreachable!(),
        };
        req_tx
            .send(Request::CreateThread {
                project,
                chat: chat_id,
                title: "Hello".into(),
                body: "Are you there?".into(),
            })
            .unwrap();
        let thread_id = match next(&resp_rx, |r| matches!(r, Response::ThreadOpened { .. })) {
            Response::ThreadOpened { thread, .. } => thread,
            _ => unreachable!(),
        };
        // The message woke the agent: its inbox count rises.
        next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if board.agent_inbox_counts.get(&backend_id) == Some(&1)),
        );
        req_tx
            .send(Request::LoadThread {
                project,
                thread: thread_id,
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::ThreadDetail { .. })) {
            Response::ThreadDetail { detail, .. } => {
                assert_eq!(detail.thread.title, "Hello");
                assert_eq!(detail.messages.len(), 1);
                assert!(!detail.thread.paused);
            }
            _ => unreachable!(),
        }
        req_tx
            .send(Request::SetThreadPaused {
                project,
                thread: thread_id,
                paused: true,
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::ThreadDetail { detail, .. } if detail.thread.paused),
        );
        req_tx
            .send(Request::SearchThreads {
                project,
                query: "there".into(),
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::ThreadSearch { .. })) {
            Response::ThreadSearch { hits, .. } => {
                assert_eq!(hits.len(), 1);
                assert_eq!(hits[0].thread.id, thread_id);
            }
            _ => unreachable!(),
        }

        // Integrate: one proposal per chosen target, including the agent's own
        // Team section, each reviewed on its own.
        req_tx
            .send(Request::SaveAgent {
                project,
                definition: AgentDefinition::template("frontend", "Frontend developer"),
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::AgentLoaded { handle, .. } if handle == "frontend"),
        );
        req_tx
            .send(Request::Integrate {
                project,
                handle: "backend".into(),
                targets: vec!["frontend".into(), "backend".into()],
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::IntegrateStarted { .. })) {
            Response::IntegrateStarted { targets, .. } => {
                assert_eq!(targets, vec!["frontend", "backend"]);
            }
            _ => unreachable!(),
        }
        let mut proposals = Vec::new();
        while proposals.len() < 2 {
            match next(&resp_rx, |r| {
                matches!(
                    r,
                    Response::IntegrateProposal { .. } | Response::IntegrateFailed { .. }
                )
            }) {
                Response::IntegrateProposal {
                    target, proposal, ..
                } => proposals.push((target, proposal)),
                Response::IntegrateFailed { target, error, .. } => {
                    panic!("integrate @{target} failed: {error}")
                }
                _ => unreachable!(),
            }
        }
        let (_, for_frontend) = proposals
            .iter()
            .find(|(t, _)| t == "frontend")
            .expect("a proposal for @frontend");
        assert!(for_frontend
            .proposed
            .body
            .contains("**@backend**: integrated by the fake"));
        assert!(
            for_frontend.call.warnings.is_empty(),
            "{:?}",
            for_frontend.call.warnings
        );
        assert!(for_frontend.call.target_id.is_some());
        assert_eq!(for_frontend.proposed.handle(), "frontend");
        let (_, for_self) = proposals
            .iter()
            .find(|(t, _)| t == "backend")
            .expect("a proposal for @backend itself");
        assert!(for_self.proposed.body.contains("integrated by the fake"));

        // Accepting the @frontend one with save changes its file.
        req_tx
            .send(Request::AcceptProposal {
                project,
                call_id: for_frontend.call.id,
                definition: for_frontend.proposed.clone(),
                save: true,
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::AgentLoaded { handle, definition: Some(d), .. } if handle == "frontend" && d.body.contains("integrated by the fake")),
        );

        // The other one still waits on @backend's editor.
        req_tx
            .send(Request::LoadAgent {
                project,
                handle: "backend".into(),
            })
            .unwrap();
        match next(
            &resp_rx,
            |r| matches!(r, Response::AgentLoaded { handle, .. } if handle == "backend"),
        ) {
            Response::AgentLoaded { integrations, .. } => {
                assert_eq!(integrations.len(), 1);
                assert_eq!(integrations[0].0, "backend");
            }
            _ => unreachable!(),
        }

        // The overview sees every open project.
        req_tx.send(Request::LoadOverview).unwrap();
        match next(&resp_rx, |r| matches!(r, Response::Overview(_))) {
            Response::Overview(list) => {
                assert_eq!(list.len(), 1);
                assert_eq!(list[0].name, "Demo Renamed");
                assert_eq!(list[0].agents, 2);
                assert!(list[0].running.is_empty());
                assert!(list[0].paused, "paused earlier in this test");
            }
            _ => unreachable!(),
        }

        // Export writes the repository snapshot; editing the snapshot copy shows
        // as drift, reviews as changed, and imports back.
        req_tx
            .send(Request::ExportAgents {
                project,
                options: ExportOptions::default(),
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::ExportDone { .. })) {
            Response::ExportDone { report, .. } => {
                assert_eq!(report.exported, vec!["backend", "frontend"]);
                assert!(report.policy_written);
            }
            _ => unreachable!(),
        }
        let snapshot_files = snapshot::SnapshotPaths::new(&repo).files();
        assert!(snapshot_files.definition_path("backend").is_file());
        assert!(!repo.join(".claude").exists());
        let mut changed = snapshot_files.read_definition("frontend").unwrap();
        changed.frontmatter.description = "Changed in the repository".into();
        snapshot_files.write_definition(&changed).unwrap();
        req_tx.send(Request::CheckDrift { project }).unwrap();
        match next(
            &resp_rx,
            |r| matches!(r, Response::Drift { entries, .. } if !entries.is_empty()),
        ) {
            Response::Drift { entries, .. } => {
                assert_eq!(entries.len(), 1);
                assert_eq!(entries[0].handle, "frontend");
            }
            _ => unreachable!(),
        }
        req_tx
            .send(Request::ReviewImport {
                project,
                source: repo.clone(),
            })
            .unwrap();
        let review = match next(&resp_rx, |r| matches!(r, Response::ImportReview { .. })) {
            Response::ImportReview { review, .. } => review,
            _ => unreachable!(),
        };
        assert_eq!(review.same_project, Some(true));
        let for_frontend = review
            .candidates
            .iter()
            .find(|c| c.handle == "frontend")
            .unwrap();
        assert_eq!(
            for_frontend.status,
            ak_runtime::snapshot::ImportStatus::Changed
        );
        req_tx
            .send(Request::ApplyImport {
                project,
                review: review.clone(),
                selection: ImportSelection {
                    handles: vec!["frontend".into()],
                    memory_for: Vec::new(),
                    policy: false,
                },
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if board.agents.iter().any(|a| a.handle == "frontend" && a.description == "Changed in the repository")),
        );
        next(
            &resp_rx,
            |r| matches!(r, Response::Drift { entries, .. } if entries.is_empty()),
        );

        // Global settings: the human's handle is renamed across the open project.
        let settings = GlobalSettings {
            human_handle: "george".into(),
            human_display_name: "George".into(),
            authoring_model: Some("haiku".into()),
            ..Default::default()
        };
        req_tx.send(Request::SaveGlobalSettings(settings)).unwrap();
        match next(&resp_rx, |r| matches!(r, Response::Settings { .. })) {
            Response::Settings { settings, .. } => {
                assert_eq!(settings.human_handle, "george");
                assert_eq!(settings.authoring_model.as_deref(), Some("haiku"));
            }
            _ => unreachable!(),
        }
        next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if board.actors.iter().any(|a| a.handle == "george")),
        );
        // A handle an agent already uses is refused before anything is written.
        let clash = GlobalSettings {
            human_handle: "backend".into(),
            ..Default::default()
        };
        req_tx.send(Request::SaveGlobalSettings(clash)).unwrap();
        match next(&resp_rx, |r| matches!(r, Response::Error(_))) {
            Response::Error(msg) => assert!(msg.contains("already an agent"), "{msg}"),
            _ => unreachable!(),
        }

        // Assigning the ticket to the agent lands in the agent's inbox, which
        // the Team view shows as a count on the board model.
        req_tx
            .send(Request::AssignTicket {
                project,
                id: ticket.id,
                to: Some(backend_id),
                version: 2,
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if board.agent_inbox_counts.get(&backend_id) == Some(&2)),
        );

        // Halting and resuming go through the scheduler and refresh the board.
        req_tx
            .send(Request::HaltAgent {
                project,
                handle: "backend".into(),
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if board.agents[0].state == ak_core::agent::AgentState::Halted),
        );
        req_tx
            .send(Request::ResumeAgent {
                project,
                handle: "backend".into(),
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if board.agents[0].state == ak_core::agent::AgentState::Idle),
        );

        // Retiring deletes the file; the agent row stays, marked missing.
        req_tx
            .send(Request::RetireAgent {
                project,
                handle: "backend".into(),
            })
            .unwrap();
        next(
            &resp_rx,
            |r| matches!(r, Response::Board { board, .. } if board.agents[0].is_retired()),
        );

        req_tx
            .send(Request::RemoveProject {
                id: project,
                delete_data: true,
            })
            .unwrap();
        match next(&resp_rx, |r| matches!(r, Response::Projects(_))) {
            Response::Projects(p) => assert!(p.is_empty()),
            _ => unreachable!(),
        }
        assert!(!tmp
            .path()
            .join("home")
            .join("projects")
            .join(project.to_string())
            .exists());
        assert!(repo.join(".git").exists(), "repository untouched");
    }
}
