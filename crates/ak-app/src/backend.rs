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
use ak_core::agent::AgentDefinition;
use ak_core::approval::Approval;
use ak_core::commands::{NewTicket, TicketPatch};
use ak_core::config::{GlobalSettings, ProjectEntry, ProjectPolicy};
use ak_core::ids::{
    ActorId, ApprovalId, CallId, ColumnId, CommentId, NotificationId, ProjectId, RunId, TicketId,
};
use ak_core::run::{AgentRun, RunEvent};
use ak_core::utility::{UtilityCall, UtilityKind, UtilityState};
use ak_core::{Actor, ActorKind, Event, NotificationState, Timestamp};
use ak_runtime::agents::{self, AgentWatcher};
use ak_runtime::authoring::{self, RosterEntry};
use ak_runtime::claude::{self, ClaudeProbe};
use ak_runtime::{OpenProject, ProjectRegistry, RuntimeError, UtilityRunner};
use ak_store::Store;
use ak_supervisor::{RunNotice, Scheduler, Supervisor, SupervisorConfig};

use crate::model::{BoardModel, InboxItem, TicketDetail};

/// Builds the utility runner for the current settings. Injected so tests can
/// supply a fake.
pub type RunnerFactory = Box<dyn Fn(&GlobalSettings) -> Arc<dyn UtilityRunner> + Send + Sync>;

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
    scheduler: Option<Scheduler>,
}

impl ProjectWatchers {
    fn stop(self) {
        self.events.abort();
        self.files.abort();
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
            internal_tx,
            internal_rx,
        }
    }

    pub async fn run(mut self) {
        self.start_agent_runtime().await;
        self.send(Response::Projects(self.registry.projects().to_vec()));
        self.send_settings();
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

    async fn open_project(&mut self, id: ProjectId) -> Result<(), String> {
        let p = self.registry.open(id).await.map_err(|e| e.to_string())?;
        self.send(Response::ProjectOpened {
            id,
            name: p.entry.name.clone(),
            human: p.human.clone(),
            policy: p.policy(),
        });
        push_board(&p, &self.tx, &self.ctx).await?;
        push_inbox(&p, &self.tx, &self.ctx).await?;
        push_approvals(&p, &self.tx, &self.ctx).await?;
        if !self.watchers.contains_key(&id) {
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
            self.watchers.insert(
                id,
                ProjectWatchers {
                    events,
                    files,
                    _agent_watcher: agent_watcher,
                    scheduler,
                },
            );
        }
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
    let (inbox, retired_since, pending, runs) = match &agent {
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
            (
                load_inbox(p.store.as_ref(), a.actor_id, true).await?,
                a.missing_since,
                pending,
                runs,
            )
        }
        None => (Vec::new(), None, None, Vec::new()),
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
                let marker = "# Draft definition file to refine\n\n";
                let start = req.input.find(marker).ok_or("no draft in input")? + marker.len();
                let draft =
                    AgentDefinition::parse(&req.input[start..]).map_err(|e| e.to_string())?;
                let mut refined = draft;
                refined.frontmatter.description = format!(
                    "Refined description. model={}",
                    req.model.clone().unwrap_or_default()
                );
                refined.body = "# refined body\n\n**You own:** everything\n".into();
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
            |r| matches!(r, Response::Board { board, .. } if board.agent_inbox_counts.get(&backend_id) == Some(&1)),
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
