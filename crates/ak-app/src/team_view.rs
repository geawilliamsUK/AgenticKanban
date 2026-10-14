//! The Team view: the agents on this project and the editor for one of them,
//! including Refine proposals, live runs and run history.

use std::collections::BTreeMap;
use std::time::Instant;

use ak_core::agent::{AgentDefinition, AgentState, Budgets, Frontmatter};
use ak_core::run::{AgentRun, RunEventKind};
use ak_core::Timestamp;

use crate::app::{Action, App};
use crate::backend::{PendingProposal, Request};
use crate::model::{describe_event, format_time, kind_label, InboxItem};

#[allow(clippy::large_enum_variant)]
pub enum RefineStatus {
    Idle,
    Running { since: Instant },
    Proposed(ProposalView),
    Failed(String),
}

pub struct ProposalView {
    pub pending: PendingProposal,
    pub proposed_text: String,
}

impl ProposalView {
    pub fn new(pending: PendingProposal) -> Self {
        let proposed_text = pending.proposed.render();
        Self {
            pending,
            proposed_text,
        }
    }
}

/// Integrate in progress or awaiting review: one proposal per target agent.
#[derive(Default)]
pub struct IntegrateState {
    /// The targets on offer with their checkboxes, while the picker is open.
    pub picker: Option<Vec<(String, bool)>>,
    pub running: Vec<String>,
    pub since: Option<Instant>,
    pub proposals: Vec<IntegrationView>,
    pub failures: Vec<(String, String)>,
}

pub struct IntegrationView {
    pub target: String,
    pub view: ProposalView,
}

impl IntegrateState {
    pub fn from_pending(pending: Vec<(String, PendingProposal)>) -> Self {
        Self {
            proposals: pending
                .into_iter()
                .map(|(target, p)| IntegrationView {
                    target,
                    view: ProposalView::new(p),
                })
                .collect(),
            ..Default::default()
        }
    }

    pub fn start(&mut self, targets: Vec<String>) {
        self.running = targets;
        self.since = Some(Instant::now());
        self.failures.clear();
        self.picker = None;
    }

    pub fn propose(&mut self, target: String, proposal: PendingProposal) {
        self.running.retain(|t| t != &target);
        self.proposals.retain(|v| v.target != target);
        self.proposals.push(IntegrationView {
            target,
            view: ProposalView::new(proposal),
        });
        if self.running.is_empty() {
            self.since = None;
        }
    }

    pub fn fail(&mut self, target: String, error: String) {
        self.running.retain(|t| t != &target);
        self.failures.push((target, error));
        if self.running.is_empty() {
            self.since = None;
        }
    }
}

/// Editor state for one agent. Built from a loaded definition, or from the
/// template for a new one.
pub struct AgentEditor {
    pub handle: String,
    pub is_new: bool,
    pub loaded: bool,
    pub description: String,
    /// Empty means the project default.
    pub model: String,
    pub permission_profile: String,
    pub max_turns: String,
    pub max_usd_run: String,
    pub max_usd_day: String,
    pub wall_clock: String,
    pub extra: BTreeMap<String, ak_core::serde_norway::Value>,
    pub body: String,
    pub memory: String,
    pub inbox: Vec<InboxItem>,
    pub runs: Vec<AgentRun>,
    pub retired_since: Option<Timestamp>,
    pub dirty: bool,
    pub confirm_retire: bool,
    pub refine: RefineStatus,
    pub integrate: IntegrateState,
    /// Spent on this agent's runs since midnight.
    pub cost_today: f64,
}

impl AgentEditor {
    pub fn new_agent(profile: &str) -> Self {
        let def = AgentDefinition::template("", "");
        let mut editor = Self::from_definition(
            "",
            Some(def),
            String::new(),
            Vec::new(),
            Vec::new(),
            None,
            None,
            Vec::new(),
            0.0,
        );
        editor.is_new = true;
        editor.loaded = true;
        editor.permission_profile = profile.to_string();
        editor.description.clear();
        editor
    }

    pub fn pending(handle: &str) -> Self {
        let mut editor = Self::from_definition(
            handle,
            None,
            String::new(),
            Vec::new(),
            Vec::new(),
            None,
            None,
            Vec::new(),
            0.0,
        );
        editor.loaded = false;
        editor
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_definition(
        handle: &str,
        definition: Option<AgentDefinition>,
        memory: String,
        inbox: Vec<InboxItem>,
        runs: Vec<AgentRun>,
        retired_since: Option<Timestamp>,
        pending: Option<PendingProposal>,
        integrations: Vec<(String, PendingProposal)>,
        cost_today: f64,
    ) -> Self {
        let def = definition.unwrap_or_else(|| AgentDefinition::template(handle, ""));
        let budgets = def.frontmatter.budgets.clone().unwrap_or_default();
        let num = |v: Option<u32>| v.map(|n| n.to_string()).unwrap_or_default();
        let money = |v: Option<f64>| v.map(|n| n.to_string()).unwrap_or_default();
        Self {
            handle: handle.to_string(),
            is_new: false,
            loaded: true,
            description: def.frontmatter.description.clone(),
            model: def.frontmatter.model.clone().unwrap_or_default(),
            permission_profile: def.frontmatter.permission_profile.clone(),
            max_turns: num(budgets.max_turns_per_run),
            max_usd_run: money(budgets.max_usd_per_run),
            max_usd_day: money(budgets.max_usd_per_day),
            wall_clock: num(budgets.max_wall_clock_minutes),
            extra: def.frontmatter.extra.clone(),
            body: def.body.clone(),
            memory,
            inbox,
            runs,
            retired_since,
            dirty: false,
            confirm_retire: false,
            refine: match pending {
                Some(p) => RefineStatus::Proposed(ProposalView::new(p)),
                None => RefineStatus::Idle,
            },
            integrate: IntegrateState::from_pending(integrations),
            cost_today,
        }
    }

    /// Replaces the editable fields with a definition, keeping handle and
    /// runtime context.
    pub fn load_fields(&mut self, def: &AgentDefinition) {
        let budgets = def.frontmatter.budgets.clone().unwrap_or_default();
        self.description = def.frontmatter.description.clone();
        self.model = def.frontmatter.model.clone().unwrap_or_default();
        self.permission_profile = def.frontmatter.permission_profile.clone();
        self.max_turns = budgets
            .max_turns_per_run
            .map(|n| n.to_string())
            .unwrap_or_default();
        self.max_usd_run = budgets
            .max_usd_per_run
            .map(|n| n.to_string())
            .unwrap_or_default();
        self.max_usd_day = budgets
            .max_usd_per_day
            .map(|n| n.to_string())
            .unwrap_or_default();
        self.wall_clock = budgets
            .max_wall_clock_minutes
            .map(|n| n.to_string())
            .unwrap_or_default();
        self.extra = def.frontmatter.extra.clone();
        self.body = def.body.clone();
    }

    /// Builds the definition to save, or explains why it cannot be saved.
    pub fn to_definition(&self) -> Result<AgentDefinition, String> {
        let handle = self.handle.trim().to_lowercase();
        ak_core::Actor::validate_handle(&handle).map_err(|e| e.to_string())?;
        let parse_u32 = |s: &str, what: &str| -> Result<Option<u32>, String> {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            s.parse::<u32>()
                .map(Some)
                .map_err(|_| format!("{what} must be a whole number"))
        };
        let parse_f64 = |s: &str, what: &str| -> Result<Option<f64>, String> {
            let s = s.trim();
            if s.is_empty() {
                return Ok(None);
            }
            s.parse::<f64>()
                .ok()
                .filter(|v| *v >= 0.0)
                .map(Some)
                .ok_or_else(|| format!("{what} must be a non-negative number"))
        };
        let budgets = Budgets {
            max_turns_per_run: parse_u32(&self.max_turns, "Max turns per run")?,
            max_usd_per_run: parse_f64(&self.max_usd_run, "Max USD per run")?,
            max_usd_per_day: parse_f64(&self.max_usd_day, "Max USD per day")?,
            max_wall_clock_minutes: parse_u32(&self.wall_clock, "Max minutes per run")?,
        };
        let model = self.model.trim();
        Ok(AgentDefinition {
            frontmatter: Frontmatter {
                name: handle,
                description: self.description.trim().to_string(),
                model: if model.is_empty() {
                    None
                } else {
                    Some(model.to_string())
                },
                permission_profile: self.permission_profile.clone(),
                budgets: if budgets.is_empty() {
                    None
                } else {
                    Some(budgets)
                },
                extra: self.extra.clone(),
            },
            body: self.body.clone(),
        })
    }
}

fn state_color(state: AgentState) -> egui::Color32 {
    match state {
        AgentState::Idle => egui::Color32::from_rgb(130, 140, 150),
        AgentState::Scheduled | AgentState::Starting => egui::Color32::from_rgb(230, 190, 80),
        AgentState::Running => egui::Color32::from_rgb(120, 220, 130),
        AgentState::Stopping => egui::Color32::from_rgb(230, 150, 80),
        AgentState::Errored | AgentState::Halted => egui::Color32::from_rgb(240, 120, 120),
        AgentState::Cooldown => egui::Color32::from_rgb(90, 160, 230),
    }
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else { return };

    egui::SidePanel::left("team_list")
        .resizable(true)
        .default_width(260.0)
        .show_inside(ui, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Team");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("New agent").clicked() {
                        app.actions.push(Action::OpenAgent(None));
                    }
                    if ui
                        .small_button("Import…")
                        .on_hover_text(
                            "Reads .agentic/agents/ from a repository and shows each agent as new, changed or identical before anything is written.",
                        )
                        .clicked()
                    {
                        if let Some(dir) = rfd::FileDialog::new()
                            .set_title("Repository with an .agentic/ snapshot")
                            .pick_folder()
                        {
                            app.actions.push(Action::Request(Request::ReviewImport {
                                project,
                                source: dir,
                            }));
                        }
                    }
                    if ui
                        .small_button("Export…")
                        .on_hover_text(
                            "Writes this project's agents into .agentic/ in the repository, for sharing through git.",
                        )
                        .clicked()
                    {
                        app.export_dialog = Some(crate::snapshot_view::ExportDialog::default());
                    }
                });
            });
            ui.add_space(4.0);
            if !app.drift.is_empty() {
                let n = app.drift.len();
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::from_rgb(230, 190, 80),
                        format!("{n} agent(s) differ from the repository snapshot."),
                    );
                    if ui
                        .small_button("Review…")
                        .on_hover_text("Compare with, and import from, this repository's .agentic/")
                        .clicked()
                    {
                        if let Some(path) = app
                            .projects
                            .iter()
                            .find(|p| p.id == project)
                            .map(|p| p.path.clone())
                        {
                            app.actions.push(Action::Request(Request::ReviewImport {
                                project,
                                source: path,
                            }));
                        }
                    }
                });
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                let paused = app.scheduler_paused;
                let label = if paused { "Agents paused" } else { "Agents active" };
                let color = if paused {
                    egui::Color32::from_rgb(240, 120, 120)
                } else {
                    egui::Color32::from_rgb(120, 220, 130)
                };
                ui.colored_label(color, label);
                let button = if paused { "Resume all" } else { "Pause all" };
                if ui
                    .small_button(button)
                    .on_hover_text("Paused: no new runs start anywhere in this project. Running agents finish or are stopped by you.")
                    .clicked()
                {
                    app.actions.push(Action::Request(Request::SetSchedulerPaused {
                        project,
                        paused: !paused,
                    }));
                }
            });
            ui.add_space(6.0);
            if let Some(h) = &app.human {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(format!("@{}", h.handle)).strong());
                    ui.weak("you");
                });
                ui.add_space(4.0);
            }
            if app.board.agents.is_empty() {
                ui.weak("No agents yet. Create one to give the board its first teammate.");
            }
            let selected = app.team.as_ref().map(|e| e.handle.clone());
            let agents = app.board.agents.clone();
            for a in &agents {
                let is_selected = selected.as_deref() == Some(a.handle.as_str());
                let open = app
                    .board
                    .agent_inbox_counts
                    .get(&a.actor_id)
                    .copied()
                    .unwrap_or(0);
                let mut text = format!("@{}", a.handle);
                if open > 0 {
                    text.push_str(&format!("   ({open})"));
                }
                let drift = app
                    .drift
                    .iter()
                    .find(|d| d.handle == a.handle)
                    .map(|d| d.status);
                if drift.is_some() {
                    text.push_str("  ⇄");
                }
                let rich = if a.is_retired() {
                    egui::RichText::new(text).weak().strikethrough()
                } else {
                    egui::RichText::new(text)
                };
                let resp = ui.horizontal(|ui| {
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                    if !a.is_retired() {
                        ui.painter().circle_filled(rect.center(), 4.0, state_color(a.state));
                    }
                    ui.selectable_label(is_selected, rich)
                });
                let label = resp.inner;
                if label.clicked() {
                    app.actions.push(Action::OpenAgent(Some(a.handle.clone())));
                }
                label.on_hover_text(if a.is_retired() {
                    "Retired: the definition file was removed".to_string()
                } else {
                    let mut text = format!("{}\nState: {}", a.description, a.state.label());
                    if let Some(status) = drift {
                        text.push_str(&format!("\nSnapshot: {}", status.label()));
                    }
                    text
                });
            }
        });

    egui::CentralPanel::default().show_inside(ui, |ui| {
        if app.team.is_none() {
            ui.centered_and_justified(|ui| {
                ui.weak("Select an agent, or create a new one.");
            });
            return;
        }
        draw_editor(app, ui, project);
    });
}

fn draw_editor(app: &mut App, ui: &mut egui::Ui, project: ak_core::ids::ProjectId) {
    let profiles = app.profiles.clone();
    let default_model = app.default_model.clone();
    let actors = app.board.actors.clone();
    let board = app.board.clone();
    let runner_description = app.runner_description.clone();
    let agent_row = app
        .team
        .as_ref()
        .and_then(|e| board.agent_by_handle(&e.handle).cloned());
    let live = agent_row
        .as_ref()
        .and_then(|a| app.live_runs.get(&a.actor_id).cloned());
    let run_events = app.run_events.clone();
    let selected_handle = app
        .team
        .as_ref()
        .map(|e| e.handle.clone())
        .unwrap_or_default();
    let others: Vec<String> = board
        .active_agents()
        .map(|a| a.handle.clone())
        .filter(|h| *h != selected_handle)
        .collect();
    let Some(editor) = app.team.as_mut() else {
        return;
    };
    let actions = &mut app.actions;

    if !editor.loaded {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak(format!("Loading @{}…", editor.handle));
        });
        return;
    }

    // Header with actions.
    ui.horizontal(|ui| {
        if editor.is_new {
            ui.heading("New agent");
        } else {
            ui.heading(format!("@{}", editor.handle));
        }
        if let Some(a) = &agent_row {
            ui.colored_label(state_color(a.state), a.state.label());
        }
        if editor.cost_today > 0.0 {
            ui.weak(format!("${:.2} today", editor.cost_today));
        }
        if editor.dirty {
            ui.weak("unsaved");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let can_save = editor.dirty && editor.retired_since.is_none();
            if ui
                .add_enabled(can_save, egui::Button::new("Save"))
                .clicked()
            {
                match editor.to_definition() {
                    Ok(definition) => {
                        actions.push(Action::Request(Request::SaveAgent {
                            project,
                            definition,
                        }));
                        editor.dirty = false;
                    }
                    Err(msg) => actions.push(Action::Toast(msg, true)),
                }
            }
            if !editor.is_new && editor.retired_since.is_none() {
                if editor.confirm_retire {
                    if ui
                        .button(
                            egui::RichText::new("Confirm retire")
                                .color(egui::Color32::from_rgb(220, 80, 80)),
                        )
                        .clicked()
                    {
                        actions.push(Action::Request(Request::RetireAgent {
                            project,
                            handle: editor.handle.clone(),
                        }));
                        editor.confirm_retire = false;
                    }
                    if ui.button("Cancel").clicked() {
                        editor.confirm_retire = false;
                    }
                } else if ui
                    .button("Retire…")
                    .on_hover_text(
                        "Deletes the definition file. Tickets and history keep the agent.",
                    )
                    .clicked()
                {
                    editor.confirm_retire = true;
                }
            }
            if ui.button("Close").clicked() {
                actions.push(Action::CloseAgent);
            }
        });
    });

    // Run controls.
    if let Some(a) = &agent_row {
        if editor.retired_since.is_none() {
            ui.horizontal(|ui| {
                let handle = editor.handle.clone();
                if a.state.is_active() {
                    if ui
                        .button("Stop run")
                        .on_hover_text("Kills the process. The agent can be woken again.")
                        .clicked()
                    {
                        actions.push(Action::Request(Request::StopAgent {
                            project,
                            handle: handle.clone(),
                        }));
                    }
                } else if ui
                    .button("Wake now")
                    .on_hover_text("Starts a run even with nothing pending.")
                    .clicked()
                {
                    actions.push(Action::Request(Request::WakeAgent {
                        project,
                        handle: handle.clone(),
                    }));
                }
                if a.state == AgentState::Halted || a.state == AgentState::Cooldown {
                    if ui
                        .button("Resume")
                        .on_hover_text("Clears the halt or cooldown so the agent can be woken.")
                        .clicked()
                    {
                        actions.push(Action::Request(Request::ResumeAgent {
                            project,
                            handle: handle.clone(),
                        }));
                    }
                } else if ui
                    .button("Halt")
                    .on_hover_text(
                        "Kills any run and refuses to wake the agent until you resume it.",
                    )
                    .clicked()
                {
                    actions.push(Action::Request(Request::HaltAgent {
                        project,
                        handle: handle.clone(),
                    }));
                }
                if let Some(until) = a.cooldown_until {
                    if a.state == AgentState::Cooldown {
                        ui.weak(format!("cooling down until {}", format_time(&until)));
                    }
                }
            });
            ui.add_space(4.0);
        }
    }

    if let Some(since) = editor.retired_since {
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(70, 50, 30))
            .corner_radius(4.0)
            .inner_margin(8.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("Retired").strong());
                ui.label(format!(
                    "The definition file disappeared on {}. History is kept. Recreate the agent to bring it back.",
                    format_time(&since)
                ));
                if ui.button("Recreate from template").clicked() {
                    let def = AgentDefinition::template(&editor.handle, &editor.description);
                    actions.push(Action::Request(Request::SaveAgent {
                        project,
                        definition: def,
                    }));
                }
            });
        ui.add_space(6.0);
    }

    let editable = editor.retired_since.is_none();

    egui::ScrollArea::vertical()
        .id_salt("agent_editor_scroll")
        .show(ui, |ui| {
            draw_runs(ui, editor, &live, &run_events, actions, project);
            draw_refine_panel(ui, editor, actions, project, &runner_description);
            draw_integrate_panel(ui, editor, actions, project, &runner_description);

            ui.add_enabled_ui(editable, |ui| {
                egui::Grid::new("agent_fields")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Handle");
                        if editor.is_new {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut editor.handle)
                                    .hint_text("backend, dba, qa…")
                                    .desired_width(240.0),
                            );
                            if r.changed() {
                                editor.dirty = true;
                            }
                        } else {
                            ui.monospace(&editor.handle);
                        }
                        ui.end_row();

                        ui.label("Description");
                        let r = ui.add(
                            egui::TextEdit::multiline(&mut editor.description)
                                .desired_rows(3)
                                .desired_width(f32::INFINITY)
                                .hint_text("What this agent owns and when to involve it. Other agents see this."),
                        );
                        if r.changed() {
                            editor.dirty = true;
                        }
                        ui.end_row();

                        ui.label("Model");
                        let default_label = default_model
                            .clone()
                            .map(|m| format!("project default ({m})"))
                            .unwrap_or_else(|| "project default (Claude Code default)".into());
                        if crate::widgets::model_picker(
                            ui,
                            "agent_model",
                            &mut editor.model,
                            &default_label,
                        ) {
                            editor.dirty = true;
                        }
                        ui.end_row();

                        ui.label("Permissions");
                        egui::ComboBox::from_id_salt("agent_profile")
                            .selected_text(&editor.permission_profile)
                            .show_ui(ui, |ui| {
                                for p in &profiles {
                                    if ui
                                        .selectable_label(&editor.permission_profile == p, p)
                                        .clicked()
                                    {
                                        editor.permission_profile = p.clone();
                                        editor.dirty = true;
                                    }
                                }
                            });
                        ui.end_row();

                        ui.label("Budgets");
                        ui.horizontal(|ui| {
                            for (label, field) in [
                                ("turns/run", &mut editor.max_turns),
                                ("USD/run", &mut editor.max_usd_run),
                                ("USD/day", &mut editor.max_usd_day),
                                ("min/run", &mut editor.wall_clock),
                            ] {
                                ui.weak(label);
                                let r = ui.add(
                                    egui::TextEdit::singleline(field)
                                        .hint_text("default")
                                        .desired_width(60.0),
                                );
                                if r.changed() {
                                    editor.dirty = true;
                                }
                            }
                        });
                        ui.end_row();
                    });

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Definition").strong());
                    ui.weak("markdown, Claude Code subagent format");
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let busy = !editor.integrate.running.is_empty();
                        let can_integrate = !editor.is_new
                            && !editor.dirty
                            && editor.retired_since.is_none()
                            && !busy
                            && !others.is_empty();
                        let why_not = if editor.dirty {
                            "Save first."
                        } else if busy {
                            "Integrate is running."
                        } else {
                            "Needs a saved agent and at least one teammate."
                        };
                        if ui
                            .add_enabled(can_integrate, egui::Button::new("Integrate"))
                            .on_hover_text(
                                "Proposes an edit to each teammate's definition about this agent, and a Team section for this agent. One Claude Code call per teammate; each proposal is reviewed on its own.",
                            )
                            .on_disabled_hover_text(why_not)
                            .clicked()
                        {
                            let mut list: Vec<(String, bool)> =
                                others.iter().map(|h| (h.clone(), true)).collect();
                            list.push((editor.handle.clone(), true));
                            editor.integrate.picker = Some(list);
                        }
                        let can_refine = matches!(
                            editor.refine,
                            RefineStatus::Idle | RefineStatus::Failed(_)
                        );
                        if ui
                            .add_enabled(can_refine, egui::Button::new("Refine"))
                            .on_hover_text(format!(
                                "Rewrites this draft into the house skeleton and style with Claude Code. Saves the draft first.\n{runner_description}"
                            ))
                            .clicked()
                        {
                            match editor.to_definition() {
                                Ok(draft) => {
                                    if editor.is_new {
                                        editor.handle = draft.handle().to_string();
                                        editor.is_new = false;
                                    }
                                    editor.dirty = false;
                                    editor.refine = RefineStatus::Running {
                                        since: Instant::now(),
                                    };
                                    actions.push(Action::Request(Request::Refine {
                                        project,
                                        draft,
                                    }));
                                }
                                Err(msg) => actions.push(Action::Toast(msg, true)),
                            }
                        }
                    });
                });
                let r = crate::mention_edit::multiline(
                    ui,
                    "agent_body",
                    &mut editor.body,
                    &actors,
                    "Identity, what it owns, what it reviews, what it never does, what done means.",
                    18,
                );
                if r.changed() {
                    editor.dirty = true;
                }
            });

            ui.add_space(8.0);
            egui::CollapsingHeader::new("Memory")
                .id_salt("agent_memory")
                .show(ui, |ui| {
                    ui.weak("Written by the agent at the end of each run. Read-only here.");
                    if editor.memory.trim().is_empty() {
                        ui.weak("Empty.");
                    } else {
                        ui.add(egui::Label::new(&editor.memory).wrap());
                    }
                });

            let open = editor.inbox.len();
            egui::CollapsingHeader::new(format!("Inbox ({open})"))
                .id_salt("agent_inbox")
                .default_open(open > 0)
                .show(ui, |ui| {
                    if editor.inbox.is_empty() {
                        ui.weak("Nothing pending.");
                    }
                    for item in &editor.inbox {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                egui::RichText::new(kind_label(item.notification.kind)).strong(),
                            );
                            if item.notification.wake {
                                ui.weak("wake");
                            }
                            ui.weak(format_time(&item.notification.created_at));
                            match &item.event {
                                Some(e) => {
                                    ui.label(describe_event(e, &board));
                                }
                                None => {
                                    ui.weak("(event no longer available)");
                                }
                            }
                        });
                    }
                });
        });
}

/// The live run, if any, and the run history with transcripts on demand.
fn draw_runs(
    ui: &mut egui::Ui,
    editor: &AgentEditor,
    live: &Option<crate::model::LiveRun>,
    run_events: &Option<(ak_core::ids::RunId, Vec<ak_core::run::RunEvent>)>,
    actions: &mut Vec<Action>,
    project: ak_core::ids::ProjectId,
) {
    if let Some(live) = live {
        let running = live.run.is_active();
        let title = if running {
            format!(
                "Run in progress, started {}",
                format_time(&live.run.started_at)
            )
        } else {
            format!(
                "Last run: {}{}",
                live.run.exit_reason.map(|r| r.label()).unwrap_or("unknown"),
                live.run
                    .cost_usd
                    .map(|c| format!(", ${c:.3}"))
                    .unwrap_or_default()
            )
        };
        egui::CollapsingHeader::new(title)
            .id_salt("live_run")
            .default_open(true)
            .show(ui, |ui| {
                if running {
                    ui.ctx()
                        .request_repaint_after(std::time::Duration::from_millis(500));
                }
                if let Some(err) = &live.run.error {
                    ui.colored_label(egui::Color32::from_rgb(240, 120, 120), err);
                }
                egui::ScrollArea::vertical()
                    .id_salt("live_events")
                    .max_height(260.0)
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
                        for e in &live.events {
                            let color = match e.kind {
                                RunEventKind::ToolCall => egui::Color32::from_rgb(120, 180, 255),
                                RunEventKind::ToolResult => ui.visuals().weak_text_color(),
                                RunEventKind::AssistantText => ui.visuals().text_color(),
                                RunEventKind::Result | RunEventKind::SystemInit => {
                                    egui::Color32::from_rgb(120, 220, 130)
                                }
                                RunEventKind::Stderr => egui::Color32::from_rgb(240, 120, 120),
                                RunEventKind::Supervisor | RunEventKind::Other => {
                                    egui::Color32::from_rgb(230, 190, 80)
                                }
                            };
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(format!(
                                        "{} {:<14} {}",
                                        e.at.with_timezone(&chrono::Local).format("%H:%M:%S"),
                                        e.kind.as_str(),
                                        e.summary()
                                    ))
                                    .color(color),
                                )
                                .wrap(),
                            );
                        }
                    });
            });
        ui.add_space(6.0);
    }

    if !editor.runs.is_empty() {
        egui::CollapsingHeader::new(format!("Run history ({})", editor.runs.len()))
            .id_salt("run_history")
            .show(ui, |ui| {
                for r in &editor.runs {
                    ui.horizontal(|ui| {
                        ui.weak(format_time(&r.started_at));
                        let label = r.exit_reason.map(|x| x.label()).unwrap_or("running");
                        ui.label(label);
                        ui.weak(format!(
                            "{} turns{}",
                            r.num_turns,
                            r.cost_usd.map(|c| format!(", ${c:.3}")).unwrap_or_default()
                        ));
                        if let Some(err) = &r.error {
                            ui.colored_label(
                                egui::Color32::from_rgb(240, 120, 120),
                                err.chars().take(80).collect::<String>(),
                            );
                        }
                        if ui.small_button("Transcript").clicked() {
                            actions.push(Action::Request(Request::LoadRunEvents {
                                project,
                                run_id: r.id,
                            }));
                        }
                    });
                    if let Some((id, events)) = run_events {
                        if *id == r.id {
                            egui::Frame::new()
                                .fill(ui.visuals().faint_bg_color)
                                .inner_margin(6.0)
                                .show(ui, |ui| {
                                    ui.set_width(ui.available_width());
                                    egui::ScrollArea::vertical()
                                        .id_salt(("transcript", r.id))
                                        .max_height(300.0)
                                        .show(ui, |ui| {
                                            ui.style_mut().override_text_style =
                                                Some(egui::TextStyle::Monospace);
                                            for e in events {
                                                ui.add(
                                                    egui::Label::new(format!(
                                                        "{} {:<14} {}",
                                                        e.at.with_timezone(&chrono::Local)
                                                            .format("%H:%M:%S"),
                                                        e.kind.as_str(),
                                                        e.summary()
                                                    ))
                                                    .wrap(),
                                                );
                                            }
                                        });
                                });
                        }
                    }
                }
            });
        ui.add_space(6.0);
    }
}

/// The Refine status strip and, when a proposal is waiting, its review.
fn draw_refine_panel(
    ui: &mut egui::Ui,
    editor: &mut AgentEditor,
    actions: &mut Vec<Action>,
    project: ak_core::ids::ProjectId,
    runner_description: &str,
) {
    match &editor.refine {
        RefineStatus::Idle => {}
        RefineStatus::Running { since } => {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(format!(
                    "Refining with Claude Code… {}s",
                    since.elapsed().as_secs()
                ));
                ui.weak(runner_description);
            });
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_millis(500));
            ui.add_space(6.0);
        }
        RefineStatus::Failed(msg) => {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(egui::Color32::from_rgb(220, 80, 80), "Refine failed:");
                ui.label(msg);
            });
            ui.add_space(6.0);
        }
        RefineStatus::Proposed(_) => {}
    }

    let mut decision: Option<Decision> = None;
    if let RefineStatus::Proposed(view) = &editor.refine {
        let pending = &view.pending;
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(30, 45, 60))
            .corner_radius(4.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("Refine proposal").strong());
                    if let Some(cost) = pending.call.cost_usd {
                        ui.weak(format!("${cost:.3}"));
                    }
                    if let Some(model) = &pending.call.model {
                        ui.weak(model);
                    }
                    ui.weak(format_time(&pending.call.started_at));
                });
                for w in &pending.call.warnings {
                    ui.colored_label(egui::Color32::from_rgb(230, 190, 80), format!("Note: {w}"));
                }
                ui.add_space(4.0);
                crate::diff_view::draw(ui, "refine_diff", &pending.original, &view.proposed_text);
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    if ui
                        .button("Accept and save")
                        .on_hover_text("Writes the proposed definition to the file.")
                        .clicked()
                    {
                        decision = Some(Decision::AcceptSave);
                    }
                    if ui
                        .button("Edit first")
                        .on_hover_text("Loads the proposal into the editor without saving.")
                        .clicked()
                    {
                        decision = Some(Decision::AcceptEdit);
                    }
                    if ui.button("Discard").clicked() {
                        decision = Some(Decision::Reject);
                    }
                });
            });
        ui.add_space(8.0);
    }

    if let Some(d) = decision {
        let RefineStatus::Proposed(view) =
            std::mem::replace(&mut editor.refine, RefineStatus::Idle)
        else {
            return;
        };
        let pending = view.pending;
        match d {
            Decision::AcceptSave => {
                editor.load_fields(&pending.proposed);
                editor.dirty = false;
                actions.push(Action::Request(Request::AcceptProposal {
                    project,
                    call_id: pending.call.id,
                    definition: pending.proposed,
                    save: true,
                }));
            }
            Decision::AcceptEdit => {
                editor.load_fields(&pending.proposed);
                editor.dirty = true;
                actions.push(Action::Request(Request::AcceptProposal {
                    project,
                    call_id: pending.call.id,
                    definition: pending.proposed,
                    save: false,
                }));
            }
            Decision::Reject => {
                actions.push(Action::Request(Request::RejectProposal {
                    project,
                    call_id: pending.call.id,
                    handle: editor.handle.clone(),
                }));
            }
        }
    }
}

enum Decision {
    AcceptSave,
    AcceptEdit,
    Reject,
}

/// The target picker, the running strip, failures, and one review per
/// proposal.
fn draw_integrate_panel(
    ui: &mut egui::Ui,
    editor: &mut AgentEditor,
    actions: &mut Vec<Action>,
    project: ak_core::ids::ProjectId,
    runner_description: &str,
) {
    let red = egui::Color32::from_rgb(220, 80, 80);
    let own = editor.handle.clone();

    // Picker.
    let mut send: Option<Vec<String>> = None;
    let mut close = false;
    if let Some(list) = editor.integrate.picker.as_mut() {
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(30, 45, 60))
            .corner_radius(4.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(egui::RichText::new("Integrate with").strong());
                ui.weak("Each chosen teammate gets a proposed edit about this agent. Choosing this agent itself rewrites its Team section from the roster.");
                for (handle, on) in list.iter_mut() {
                    let label = if *handle == own {
                        format!("@{handle} (its own Team section)")
                    } else {
                        format!("@{handle}")
                    };
                    ui.checkbox(on, label);
                }
                ui.horizontal(|ui| {
                    let chosen: Vec<String> = list
                        .iter()
                        .filter(|(_, on)| *on)
                        .map(|(h, _)| h.clone())
                        .collect();
                    if ui
                        .add_enabled(
                            !chosen.is_empty(),
                            egui::Button::new(format!("Integrate {} agent(s)", chosen.len())),
                        )
                        .on_hover_text(runner_description)
                        .clicked()
                    {
                        send = Some(chosen);
                    }
                    if ui.button("Cancel").clicked() {
                        close = true;
                    }
                });
            });
        ui.add_space(8.0);
    }
    if let Some(targets) = send {
        editor.integrate.start(targets.clone());
        actions.push(Action::Request(Request::Integrate {
            project,
            handle: own.clone(),
            targets,
        }));
    }
    if close {
        editor.integrate.picker = None;
    }

    // Progress and failures.
    if !editor.integrate.running.is_empty() {
        ui.horizontal(|ui| {
            ui.spinner();
            let secs = editor
                .integrate
                .since
                .map(|s| s.elapsed().as_secs())
                .unwrap_or(0);
            let names: Vec<String> = editor
                .integrate
                .running
                .iter()
                .map(|h| format!("@{h}"))
                .collect();
            ui.label(format!("Integrating with {}… {secs}s", names.join(", ")));
        });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
        ui.add_space(6.0);
    }
    for (target, error) in &editor.integrate.failures {
        ui.horizontal_wrapped(|ui| {
            ui.colored_label(red, format!("Integrate @{target} failed:"));
            ui.label(error);
        });
    }
    if !editor.integrate.failures.is_empty() {
        ui.add_space(6.0);
    }

    // Proposals.
    if editor.integrate.proposals.is_empty() {
        return;
    }
    let count = editor.integrate.proposals.len();
    let mut decisions: Vec<(usize, Decision)> = Vec::new();
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(format!("Integrate proposals ({count})")).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("Discard all").clicked() {
                decisions.extend((0..count).map(|i| (i, Decision::Reject)));
            }
            if ui.small_button("Accept all and save").clicked() {
                decisions.extend((0..count).map(|i| (i, Decision::AcceptSave)));
            }
        });
    });
    for (i, item) in editor.integrate.proposals.iter().enumerate() {
        let pending = &item.view.pending;
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(30, 45, 60))
            .corner_radius(4.0)
            .inner_margin(10.0)
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let what = if item.target == own {
                        format!("@{}: its own Team section", item.target)
                    } else {
                        format!("@{}", item.target)
                    };
                    ui.label(egui::RichText::new(what).strong());
                    if let Some(cost) = pending.call.cost_usd {
                        ui.weak(format!("${cost:.3}"));
                    }
                    if let Some(model) = &pending.call.model {
                        ui.weak(model);
                    }
                    ui.weak(format_time(&pending.call.started_at));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Discard").clicked() {
                            decisions.push((i, Decision::Reject));
                        }
                        if ui
                            .button("Accept and save")
                            .on_hover_text("Writes the proposed definition to that agent's file.")
                            .clicked()
                        {
                            decisions.push((i, Decision::AcceptSave));
                        }
                    });
                });
                for w in &pending.call.warnings {
                    ui.colored_label(egui::Color32::from_rgb(230, 190, 80), format!("Note: {w}"));
                }
                ui.add_space(4.0);
                crate::diff_view::draw(
                    ui,
                    &format!("integrate_diff_{}", item.target),
                    &pending.original,
                    &item.view.proposed_text,
                );
            });
        ui.add_space(6.0);
    }

    // Apply from the highest index down so removals keep the rest valid.
    decisions.sort_by_key(|d| std::cmp::Reverse(d.0));
    decisions.dedup_by(|a, b| a.0 == b.0);
    for (i, d) in decisions {
        if i >= editor.integrate.proposals.len() {
            continue;
        }
        let item = editor.integrate.proposals.remove(i);
        let pending = item.view.pending;
        match d {
            Decision::AcceptSave | Decision::AcceptEdit => {
                actions.push(Action::Request(Request::AcceptProposal {
                    project,
                    call_id: pending.call.id,
                    definition: pending.proposed,
                    save: true,
                }));
            }
            Decision::Reject => {
                actions.push(Action::Request(Request::RejectProposal {
                    project,
                    call_id: pending.call.id,
                    handle: own.clone(),
                }));
            }
        }
    }
}
