//! The eframe application: state, the request/response loop, the shell
//! layout, and dispatch of UI actions to the backend.

use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::mpsc::UnboundedSender;

use std::collections::HashMap;

use ak_core::approval::Approval;
use ak_core::chat::{Thread, ThreadHit};
use ak_core::config::{GlobalSettings, ProjectEntry, ProjectPolicy};
use ak_core::ids::{ActorId, ChatId, ProjectId, RunId, ThreadId, TicketId};
use ak_core::run::RunEvent;
use ak_core::{Actor, NotificationState};
use ak_runtime::snapshot::DriftEntry;
use ak_runtime::{ClaudeCodeRunner, ClaudeProbe, HomePaths, ProjectRegistry, UtilityRunner};

use crate::backend::{Backend, Request, Response, RunnerFactory};
use crate::chats_view::ChatsState;
use crate::dialogs::{AddProjectDialog, NewTicketDialog};
use crate::markdown::Markdown;
use crate::model::{
    BoardModel, ChatSummary, InboxItem, LiveRun, ProjectOverview, ThreadDetail, TicketDetail,
};
use crate::settings_view::SettingsEditor;
use crate::snapshot_view::{ExportDialog, ImportDialog};
use crate::team_view::{AgentEditor, ProposalView, RefineStatus};
use crate::ticket_view::TicketPanelState;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum View {
    Board,
    Inbox,
    Chats,
    Overview,
    Team,
    Settings,
}

/// Something the UI wants done. Views push these; `App::update` dispatches
/// them after drawing, which keeps borrows simple.
#[allow(clippy::large_enum_variant)]
pub enum Action {
    Request(Request),
    SelectTicket(Option<TicketId>),
    SwitchView(View),
    OpenProject(ProjectId),
    /// `None` starts a new agent; `Some(handle)` loads an existing one.
    OpenAgent(Option<String>),
    CloseAgent,
    SelectChat(Option<ChatId>),
    /// Selecting a thread also switches to the Chats view.
    SelectThread(Option<ThreadId>),
    ProbeClaude,
    Toast(String, bool),
}

pub struct App {
    tx: UnboundedSender<Request>,
    rx: Receiver<Response>,
    startup_error: Option<String>,

    pub projects: Vec<ProjectEntry>,
    pub current: Option<ProjectId>,
    pub project_name: String,
    pub human: Option<Actor>,

    pub board: BoardModel,
    pub selected: Option<TicketId>,
    pub detail: Option<TicketDetail>,
    pub inbox: Vec<InboxItem>,
    pub view: View,

    pub chats: Vec<ChatSummary>,
    pub selected_chat: Option<ChatId>,
    pub threads: Vec<Thread>,
    pub selected_thread: Option<ThreadId>,
    pub thread_detail: Option<ThreadDetail>,
    pub thread_hits: Vec<ThreadHit>,
    pub chats_ui: ChatsState,

    /// How the home directory and the repository snapshot differ.
    pub drift: Vec<DriftEntry>,
    pub export_dialog: Option<ExportDialog>,
    pub import_dialog: Option<ImportDialog>,

    pub overview: Vec<ProjectOverview>,
    pub overview_requested: Option<Instant>,

    /// Permission profile names and default model from the open project's policy.
    pub profiles: Vec<String>,
    pub default_model: Option<String>,
    pub team: Option<AgentEditor>,
    /// What Refine and Integrate will run, for tooltips.
    pub runner_description: String,

    pub settings: Option<GlobalSettings>,
    pub policy: Option<ProjectPolicy>,
    pub settings_editor: Option<SettingsEditor>,
    pub claude_probe: Option<ClaudeProbe>,
    pub probing: bool,
    pub bridge_addr: Option<String>,

    /// The run in progress or the last one, per agent.
    pub live_runs: HashMap<ActorId, LiveRun>,
    /// A transcript loaded on demand from the run history.
    pub run_events: Option<(RunId, Vec<RunEvent>)>,
    pub approvals: Vec<Approval>,
    pub scheduler_paused: bool,

    pub new_ticket: Option<NewTicketDialog>,
    pub add_project: Option<AddProjectDialog>,
    pub remove_confirm: Option<ProjectId>,
    pub ticket_panel: TicketPanelState,
    pub markdown: Markdown,

    pub actions: Vec<Action>,
    toast: Option<(Instant, String, bool)>,
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let (req_tx, req_rx) = tokio::sync::mpsc::unbounded_channel();
        let (resp_tx, resp_rx) = std::sync::mpsc::channel();
        let ctx = cc.egui_ctx.clone();

        let startup_error = match HomePaths::resolve().and_then(ProjectRegistry::load) {
            Ok(registry) => {
                let paths = registry.paths().clone();
                let factory: RunnerFactory = Box::new(move |settings| {
                    let runner: Arc<dyn UtilityRunner> =
                        Arc::new(ClaudeCodeRunner::from_settings(settings, &paths));
                    runner
                });
                std::thread::Builder::new()
                    .name("backend".into())
                    .spawn(move || {
                        let rt = tokio::runtime::Builder::new_multi_thread()
                            .enable_all()
                            .build()
                            .expect("tokio runtime");
                        rt.block_on(Backend::new(registry, factory, req_rx, resp_tx, ctx).run());
                    })
                    .expect("backend thread");
                None
            }
            Err(e) => Some(e.to_string()),
        };

        Self {
            tx: req_tx,
            rx: resp_rx,
            startup_error,
            projects: Vec::new(),
            current: None,
            project_name: String::new(),
            human: None,
            board: BoardModel::default(),
            selected: None,
            detail: None,
            inbox: Vec::new(),
            chats: Vec::new(),
            selected_chat: None,
            threads: Vec::new(),
            selected_thread: None,
            thread_detail: None,
            thread_hits: Vec::new(),
            chats_ui: ChatsState::default(),
            drift: Vec::new(),
            export_dialog: None,
            import_dialog: None,
            overview: Vec::new(),
            overview_requested: None,
            view: View::Board,
            profiles: Vec::new(),
            default_model: None,
            team: None,
            runner_description: String::new(),
            settings: None,
            policy: None,
            settings_editor: None,
            claude_probe: None,
            probing: false,
            bridge_addr: None,
            live_runs: HashMap::new(),
            run_events: None,
            approvals: Vec::new(),
            scheduler_paused: true,
            new_ticket: None,
            add_project: None,
            remove_confirm: None,
            ticket_panel: TicketPanelState::default(),
            markdown: Markdown::default(),
            actions: Vec::new(),
            toast: None,
        }
    }

    pub fn send(&self, req: Request) {
        let _ = self.tx.send(req);
    }

    pub fn toast(&mut self, text: impl Into<String>, is_error: bool) {
        self.toast = Some((Instant::now(), text.into(), is_error));
    }

    pub fn unread_count(&self) -> usize {
        self.inbox
            .iter()
            .filter(|i| i.notification.state == NotificationState::Pending)
            .count()
    }

    fn drain_responses(&mut self) {
        loop {
            match self.rx.try_recv() {
                Ok(resp) => self.apply(resp),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    if self.startup_error.is_none() {
                        self.startup_error = Some("backend stopped".into());
                    }
                    break;
                }
            }
        }
    }

    fn apply(&mut self, resp: Response) {
        match resp {
            Response::Projects(projects) => {
                let first = projects.first().map(|p| p.id);
                self.projects = projects;
                if let Some(cur) = self.current {
                    if let Some(p) = self.projects.iter().find(|p| p.id == cur) {
                        self.project_name = p.name.clone();
                    }
                }
                if self.current.is_none() {
                    if let Some(id) = first {
                        self.send(Request::OpenProject { id });
                    }
                }
                if let Some(cur) = self.current {
                    if !self.projects.iter().any(|p| p.id == cur) {
                        self.current = None;
                        self.board = BoardModel::default();
                        self.inbox.clear();
                        self.selected = None;
                        self.detail = None;
                        self.team = None;
                        if let Some(id) = first {
                            self.send(Request::OpenProject { id });
                        }
                    }
                }
            }
            Response::ProjectOpened {
                id,
                name,
                human,
                policy,
            } => {
                self.current = Some(id);
                self.project_name = name;
                self.human = Some(human);
                self.apply_policy(policy);
                self.selected = None;
                self.detail = None;
                self.team = None;
                self.live_runs.clear();
                self.run_events = None;
                self.approvals.clear();
                self.chats.clear();
                self.selected_chat = None;
                self.threads.clear();
                self.selected_thread = None;
                self.thread_detail = None;
                self.thread_hits.clear();
                self.chats_ui = ChatsState::default();
                self.drift.clear();
                self.export_dialog = None;
                self.import_dialog = None;
                self.ticket_panel = TicketPanelState::default();
                if self.view == View::Overview {
                    self.view = View::Board;
                }
            }
            Response::RunStarted { project, run } => {
                if Some(project) == self.current {
                    let handle = self.board.handle(run.agent_id);
                    self.live_runs.insert(run.agent_id, LiveRun::new(run));
                    self.toast(format!("{handle} started a run"), false);
                }
            }
            Response::RunEvent { project, event } => {
                if Some(project) == self.current {
                    if let Some(live) = self
                        .live_runs
                        .values_mut()
                        .find(|l| l.run.id == event.run_id)
                    {
                        live.push(event);
                    }
                }
            }
            Response::RunEnded { project, run } => {
                if Some(project) == self.current {
                    let handle = self.board.handle(run.agent_id);
                    let label = run.exit_reason.map(|r| r.label()).unwrap_or("ended");
                    let is_error =
                        !matches!(run.exit_reason, Some(ak_core::run::ExitReason::Completed));
                    match self.live_runs.get_mut(&run.agent_id) {
                        Some(live) if live.run.id == run.id => live.run = run.clone(),
                        _ => {
                            self.live_runs
                                .insert(run.agent_id, LiveRun::new(run.clone()));
                        }
                    }
                    self.toast(format!("{handle}: run {label}"), is_error);
                    if let Some(e) = self.team.as_mut() {
                        if self.board.agent_by_handle(&e.handle).map(|a| a.actor_id)
                            == Some(run.agent_id)
                        {
                            e.runs.insert(0, run);
                        }
                    }
                }
            }
            Response::RunEvents {
                project,
                run_id,
                events,
            } => {
                if Some(project) == self.current {
                    self.run_events = Some((run_id, events));
                }
            }
            Response::Approvals { project, items } => {
                if Some(project) == self.current {
                    self.approvals = items;
                }
            }
            Response::SchedulerPaused { project, paused } => {
                if Some(project) == self.current {
                    self.scheduler_paused = paused;
                }
            }
            Response::ProjectPolicy { project, policy } => {
                if Some(project) == self.current {
                    self.apply_policy(policy);
                }
            }
            Response::Settings {
                settings,
                runner_description,
                bridge_addr,
            } => {
                self.runner_description = runner_description;
                self.bridge_addr = bridge_addr;
                if let Some(e) = self.settings_editor.as_mut() {
                    e.refresh_global(&settings);
                }
                self.settings = Some(settings);
            }
            Response::ClaudeProbe(probe) => {
                self.claude_probe = Some(probe);
                self.probing = false;
            }
            Response::AgentLoaded {
                project,
                handle,
                definition,
                memory,
                inbox,
                retired_since,
                pending,
                runs,
                integrations,
                cost_today,
            } => {
                if Some(project) != self.current {
                    return;
                }
                // Never clobber unsaved edits or a running Refine with a refresh.
                let applies = match &self.team {
                    Some(e) => {
                        e.handle == handle
                            && (!e.dirty || !e.loaded)
                            && !matches!(e.refine, RefineStatus::Running { .. })
                            && e.integrate.running.is_empty()
                    }
                    None => false,
                };
                if applies {
                    self.team = Some(AgentEditor::from_definition(
                        &handle,
                        definition,
                        memory,
                        inbox,
                        runs,
                        retired_since,
                        pending,
                        integrations,
                        cost_today,
                    ));
                }
            }
            Response::RefineStarted { project, handle } => {
                if Some(project) == self.current {
                    if let Some(e) = self.team.as_mut() {
                        if e.handle == handle && !matches!(e.refine, RefineStatus::Running { .. }) {
                            e.refine = RefineStatus::Running {
                                since: Instant::now(),
                            };
                        }
                    }
                }
            }
            Response::Proposal {
                project,
                handle,
                proposal,
            } => {
                if Some(project) == self.current {
                    if let Some(e) = self.team.as_mut() {
                        if e.handle == handle {
                            e.refine = RefineStatus::Proposed(ProposalView::new(proposal));
                        }
                    }
                }
                self.toast(format!("Refine proposal ready for @{handle}"), false);
            }
            Response::IntegrateStarted {
                project,
                handle,
                targets,
            } => {
                if Some(project) == self.current {
                    if let Some(e) = self.team.as_mut() {
                        if e.handle == handle {
                            e.integrate.start(targets);
                        }
                    }
                }
            }
            Response::IntegrateProposal {
                project,
                handle,
                target,
                proposal,
            } => {
                if Some(project) == self.current {
                    if let Some(e) = self.team.as_mut() {
                        if e.handle == handle {
                            e.integrate.propose(target.clone(), proposal);
                        }
                    }
                }
                self.toast(format!("Integrate proposal for @{target} ready"), false);
            }
            Response::IntegrateFailed {
                project,
                handle,
                target,
                error,
            } => {
                if Some(project) == self.current {
                    if let Some(e) = self.team.as_mut() {
                        if e.handle == handle {
                            e.integrate.fail(target.clone(), error.clone());
                        }
                    }
                }
                self.toast(format!("Integrate @{target} failed: {error}"), true);
            }
            Response::RefineFailed {
                project,
                handle,
                error,
            } => {
                if Some(project) == self.current {
                    if let Some(e) = self.team.as_mut() {
                        if e.handle == handle {
                            e.refine = RefineStatus::Failed(error.clone());
                        }
                    }
                }
                self.toast(format!("Refine failed: {error}"), true);
            }
            Response::Board { project, board } => {
                if Some(project) == self.current {
                    self.board = board;
                    // The human's handle can change from Settings.
                    if let Some(h) = &self.human {
                        if let Some(a) = self.board.actor(h.id) {
                            self.human = Some(a.clone());
                        }
                    }
                    if let Some(id) = self.selected {
                        if self.board.ticket(id).is_some() {
                            self.send(Request::LoadTicket { project, id });
                        } else {
                            self.selected = None;
                            self.detail = None;
                        }
                    }
                }
            }
            Response::TicketDetail { project, detail } => {
                if Some(project) == self.current && Some(detail.ticket.id) == self.selected {
                    self.detail = Some(detail);
                }
            }
            Response::Inbox { project, items } => {
                if Some(project) == self.current {
                    self.inbox = items;
                }
            }
            Response::Chats { project, chats } => {
                if Some(project) == self.current {
                    self.chats = chats;
                    // Chats are pushed on every event; keep what is open fresh.
                    if let Some(chat) = self.selected_chat {
                        self.send(Request::LoadThreads {
                            project,
                            chat,
                            include_archived: self.chats_ui.show_archived,
                        });
                    }
                    if let Some(thread) = self.selected_thread {
                        self.send(Request::LoadThread { project, thread });
                    }
                }
            }
            Response::Threads {
                project,
                chat,
                threads,
            } => {
                if Some(project) == self.current && Some(chat) == self.selected_chat {
                    self.threads = threads;
                }
            }
            Response::ThreadDetail { project, detail } => {
                if Some(project) == self.current && Some(detail.thread.id) == self.selected_thread {
                    // Opened from the inbox: the thread may live in another chat.
                    if self.selected_chat != Some(detail.thread.chat_id) {
                        self.selected_chat = Some(detail.thread.chat_id);
                        self.threads.clear();
                        self.send(Request::LoadThreads {
                            project,
                            chat: detail.thread.chat_id,
                            include_archived: self.chats_ui.show_archived,
                        });
                    }
                    self.thread_detail = Some(detail);
                }
            }
            Response::ThreadSearch {
                project,
                query,
                hits,
            } => {
                if Some(project) == self.current && query == self.chats_ui.search {
                    self.thread_hits = hits;
                }
            }
            Response::ChatOpened { project, chat } => {
                if Some(project) == self.current {
                    self.view = View::Chats;
                    self.actions.push(Action::SelectChat(Some(chat)));
                }
            }
            Response::ThreadOpened { project, thread } => {
                if Some(project) == self.current {
                    self.actions.push(Action::SelectThread(Some(thread)));
                }
            }
            Response::ExportDone { project, report } => {
                if Some(project) == self.current {
                    let mut msg = format!(
                        "Exported {} agent(s) to {}",
                        report.exported.len(),
                        report.root.display()
                    );
                    if !report.removed.is_empty() {
                        msg.push_str(&format!(", removed {}", report.removed.len()));
                    }
                    if report.staged {
                        msg.push_str("; staged, commit when ready");
                    }
                    match report.stage_error {
                        Some(e) => self.toast(format!("{msg}; staging failed: {e}"), true),
                        None => self.toast(msg, false),
                    }
                }
            }
            Response::ImportReview { project, review } => {
                if Some(project) == self.current {
                    self.import_dialog = Some(ImportDialog::new(review));
                    self.view = View::Team;
                }
            }
            Response::Overview(list) => self.overview = list,
            Response::Drift { project, entries } => {
                if Some(project) == self.current {
                    self.drift = entries;
                }
            }
            Response::Error(msg) => self.toast(msg, true),
            Response::Info(msg) => self.toast(msg, false),
        }
    }

    fn apply_policy(&mut self, policy: ProjectPolicy) {
        self.profiles = policy
            .permission_profiles
            .iter()
            .map(|p| p.name.clone())
            .collect();
        self.default_model = policy.default_model.clone();
        self.policy = Some(policy);
        if let Some(e) = self.settings_editor.as_mut() {
            e.refresh_project(self.policy.as_ref());
        }
    }

    fn dispatch_actions(&mut self) {
        let actions = std::mem::take(&mut self.actions);
        for action in actions {
            match action {
                Action::Request(req) => self.send(req),
                Action::SelectTicket(id) => {
                    self.selected = id;
                    self.detail = None;
                    self.ticket_panel = TicketPanelState::default();
                    if let (Some(project), Some(id)) = (self.current, id) {
                        self.send(Request::LoadTicket { project, id });
                    }
                }
                Action::SwitchView(view) => {
                    self.view = view;
                    if view == View::Overview {
                        self.overview_requested = None;
                    }
                    if view == View::Team {
                        if let Some(project) = self.current {
                            self.send(Request::CheckDrift { project });
                        }
                    }
                    if view == View::Inbox {
                        if let Some(project) = self.current {
                            self.send(Request::MarkInboxSeen { project });
                        }
                    }
                }
                Action::OpenProject(id) => {
                    if Some(id) != self.current {
                        self.send(Request::OpenProject { id });
                    }
                }
                Action::OpenAgent(None) => {
                    let profile = self
                        .profiles
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "supervised".into());
                    self.team = Some(AgentEditor::new_agent(&profile));
                    self.view = View::Team;
                }
                Action::OpenAgent(Some(handle)) => {
                    self.team = Some(AgentEditor::pending(&handle));
                    self.view = View::Team;
                    if let Some(project) = self.current {
                        self.send(Request::LoadAgent { project, handle });
                    }
                }
                Action::CloseAgent => self.team = None,
                Action::SelectChat(chat) => {
                    self.selected_chat = chat;
                    self.selected_thread = None;
                    self.thread_detail = None;
                    self.threads.clear();
                    self.chats_ui.compose.clear();
                    self.chats_ui.new_thread = None;
                    if let (Some(project), Some(chat)) = (self.current, chat) {
                        self.send(Request::LoadThreads {
                            project,
                            chat,
                            include_archived: self.chats_ui.show_archived,
                        });
                    }
                }
                Action::SelectThread(thread) => {
                    self.selected_thread = thread;
                    self.thread_detail = None;
                    self.chats_ui.compose.clear();
                    self.view = View::Chats;
                    if let (Some(project), Some(thread)) = (self.current, thread) {
                        self.send(Request::LoadThread { project, thread });
                    }
                }
                Action::ProbeClaude => {
                    if !self.probing {
                        self.probing = true;
                        self.send(Request::ProbeClaude);
                    }
                }
                Action::Toast(text, is_error) => self.toast(text, is_error),
            }
        }
    }

    fn draw_sidebar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.heading("AgenticKanban");
        ui.add_space(8.0);

        ui.horizontal(|ui| {
            ui.label(egui::RichText::new("Projects").strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("Add…")
                    .on_hover_text("Register a git repository")
                    .clicked()
                {
                    if let Some(path) = rfd::FileDialog::new()
                        .set_title("Choose a git repository")
                        .pick_folder()
                    {
                        self.add_project = Some(AddProjectDialog::new(path));
                    }
                }
            });
        });

        let current = self.current;
        let projects = self.projects.clone();
        if projects.is_empty() {
            ui.weak("No projects yet. Add a repository to start.");
        }
        for p in &projects {
            let selected = Some(p.id) == current;
            let resp = ui
                .selectable_label(selected, &p.name)
                .on_hover_text(p.path.display().to_string());
            if resp.clicked() {
                self.actions.push(Action::OpenProject(p.id));
            }
            resp.context_menu(|ui| {
                if ui.button("Remove from app…").clicked() {
                    self.remove_confirm = Some(p.id);
                    ui.close();
                }
            });
        }

        ui.add_space(12.0);
        ui.separator();
        ui.add_space(4.0);

        if ui
            .selectable_label(self.view == View::Overview, "Overview")
            .clicked()
        {
            self.actions.push(Action::SwitchView(View::Overview));
        }
        let enabled = self.current.is_some();
        ui.add_enabled_ui(enabled, |ui| {
            if ui
                .selectable_label(self.view == View::Board, "Board")
                .clicked()
            {
                self.actions.push(Action::SwitchView(View::Board));
            }
            let unread = self.unread_count() + self.approvals.len();
            let label = if !self.approvals.is_empty() {
                format!("Inbox  ({unread}) ⚠")
            } else if unread > 0 {
                format!("Inbox  ({unread})")
            } else {
                "Inbox".to_string()
            };
            if ui
                .selectable_label(self.view == View::Inbox, label)
                .clicked()
            {
                self.actions.push(Action::SwitchView(View::Inbox));
            }
            if ui
                .selectable_label(self.view == View::Chats, "Chats")
                .clicked()
            {
                self.actions.push(Action::SwitchView(View::Chats));
            }
            let agents = self.board.active_agents().count();
            let label = if agents > 0 {
                format!("Team  ({agents})")
            } else {
                "Team".to_string()
            };
            if ui
                .selectable_label(self.view == View::Team, label)
                .clicked()
            {
                self.actions.push(Action::SwitchView(View::Team));
            }
        });

        ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
            ui.add_space(6.0);
            if let Some(h) = &self.human {
                ui.weak(format!("You are @{}", h.handle));
            }
            if ui
                .selectable_label(self.view == View::Settings, "Settings")
                .clicked()
            {
                self.actions.push(Action::SwitchView(View::Settings));
            }
        });
    }

    fn draw_toast(&mut self, ctx: &egui::Context) {
        let Some((at, text, is_error)) = self.toast.clone() else {
            return;
        };
        if at.elapsed() > Duration::from_secs(if is_error { 8 } else { 4 }) {
            self.toast = None;
            return;
        }
        ctx.request_repaint_after(Duration::from_millis(500));
        egui::TopBottomPanel::bottom("toast").show(ctx, |ui| {
            ui.horizontal(|ui| {
                let color = if is_error {
                    egui::Color32::from_rgb(220, 80, 80)
                } else {
                    egui::Color32::from_rgb(80, 160, 90)
                };
                ui.colored_label(color, if is_error { "Error" } else { "OK" });
                ui.label(&text);
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Dismiss").clicked() {
                        self.toast = None;
                    }
                });
            });
        });
    }

    fn draw_remove_confirm(&mut self, ctx: &egui::Context) {
        let Some(id) = self.remove_confirm else {
            return;
        };
        let name = self
            .projects
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.name.clone())
            .unwrap_or_default();
        let mut open = true;
        let mut decided: Option<Option<bool>> = None;
        egui::Window::new("Remove project")
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(format!(
                    "Remove \"{name}\" from the app? The repository is never touched."
                ));
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.button("Remove, keep local data").clicked() {
                        decided = Some(Some(false));
                    }
                    if ui
                        .button(
                            egui::RichText::new("Remove and delete local board")
                                .color(egui::Color32::from_rgb(220, 80, 80)),
                        )
                        .clicked()
                    {
                        decided = Some(Some(true));
                    }
                    if ui.button("Cancel").clicked() {
                        decided = Some(None);
                    }
                });
            });
        if !open {
            decided = Some(None);
        }
        if let Some(choice) = decided {
            self.remove_confirm = None;
            if let Some(delete_data) = choice {
                self.send(Request::RemoveProject { id, delete_data });
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.drain_responses();

        if self.view == View::Overview
            && self
                .overview_requested
                .is_none_or(|t| t.elapsed() > std::time::Duration::from_secs(5))
        {
            self.overview_requested = Some(Instant::now());
            self.send(Request::LoadOverview);
            ctx.request_repaint_after(std::time::Duration::from_secs(5));
        }

        if let Some(err) = self.startup_error.clone() {
            egui::CentralPanel::default().show(ctx, |ui| {
                ui.heading("AgenticKanban could not start");
                ui.label(err);
            });
            return;
        }

        egui::SidePanel::left("sidebar")
            .resizable(true)
            .default_width(220.0)
            .show(ctx, |ui| {
                self.draw_sidebar(ui);
            });

        if self.current.is_some() && self.selected.is_some() {
            egui::SidePanel::right("ticket")
                .resizable(true)
                .default_width(460.0)
                .width_range(360.0..=820.0)
                .show(ctx, |ui| {
                    crate::ticket_view::draw(self, ui);
                });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if self.view == View::Settings {
                crate::settings_view::draw(self, ui);
                return;
            }
            if self.view == View::Overview {
                crate::overview_view::draw(self, ui);
                return;
            }
            if self.current.is_none() {
                ui.centered_and_justified(|ui| {
                    ui.label("Add a project from the sidebar to get started.");
                });
                return;
            }
            match self.view {
                View::Board => crate::board_view::draw(self, ui),
                View::Inbox => crate::inbox_view::draw(self, ui),
                View::Chats => crate::chats_view::draw(self, ui),
                View::Team => crate::team_view::draw(self, ui),
                View::Settings | View::Overview => unreachable!(),
            }
        });

        crate::dialogs::draw(self, ctx);
        crate::snapshot_view::draw(self, ctx);
        self.draw_remove_confirm(ctx);
        self.draw_toast(ctx);
        self.dispatch_actions();
    }
}
