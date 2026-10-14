//! The Team view: the agents on this project and the editor for one of them,
//! including Refine proposals and their review.

use std::collections::BTreeMap;
use std::time::Instant;

use ak_core::agent::{AgentDefinition, AgentState, Budgets, Frontmatter};
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
    pub retired_since: Option<Timestamp>,
    pub dirty: bool,
    pub confirm_retire: bool,
    pub refine: RefineStatus,
}

impl AgentEditor {
    pub fn new_agent(profile: &str) -> Self {
        let def = AgentDefinition::template("", "");
        let mut editor =
            Self::from_definition("", Some(def), String::new(), Vec::new(), None, None);
        editor.is_new = true;
        editor.loaded = true;
        editor.permission_profile = profile.to_string();
        editor.description.clear();
        editor
    }

    pub fn pending(handle: &str) -> Self {
        let mut editor = Self::from_definition(handle, None, String::new(), Vec::new(), None, None);
        editor.loaded = false;
        editor
    }

    pub fn from_definition(
        handle: &str,
        definition: Option<AgentDefinition>,
        memory: String,
        inbox: Vec<InboxItem>,
        retired_since: Option<Timestamp>,
        pending: Option<PendingProposal>,
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
            retired_since,
            dirty: false,
            confirm_retire: false,
            refine: match pending {
                Some(p) => RefineStatus::Proposed(ProposalView::new(p)),
                None => RefineStatus::Idle,
            },
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
                });
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
                let rich = if a.is_retired() {
                    egui::RichText::new(text).weak().strikethrough()
                } else {
                    egui::RichText::new(text)
                };
                let resp = ui.selectable_label(is_selected, rich);
                if resp.clicked() {
                    app.actions.push(Action::OpenAgent(Some(a.handle.clone())));
                }
                resp.on_hover_text(if a.is_retired() {
                    "Retired: the definition file was removed".to_string()
                } else {
                    format!("{}\nState: {}", a.description, a.state.label())
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
            draw_refine_panel(ui, editor, actions, project, &runner_description);

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
                        ui.add_enabled(false, egui::Button::new("Integrate"))
                            .on_disabled_hover_text(
                                "Coming next: updates the rest of the team about this agent.",
                            );
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

            if let Some(a) = board.agent_by_handle(&editor.handle) {
                ui.add_space(6.0);
                ui.weak(format!(
                    "State: {}{}",
                    a.state.label(),
                    if a.state == AgentState::Idle {
                        " (agents cannot run until the supervisor lands)"
                    } else {
                        ""
                    }
                ));
            }
        });
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
