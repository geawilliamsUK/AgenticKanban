//! Settings: this computer's global settings and the open project's policy.

use std::path::PathBuf;

use ak_core::config::{GlobalSettings, ProjectPolicy, PushPolicy, ThemeMode};

use crate::app::{Action, App};
use crate::backend::Request;
use crate::theme::{self, Tone};
use crate::widgets::{self, model_picker};

pub struct SettingsEditor {
    pub global: GlobalSettings,
    /// Text forms of optional fields, so the controls can edit them.
    pub binary: String,
    pub authoring_model: String,
    pub global_dirty: bool,

    pub project: Option<ProjectPolicy>,
    pub project_authoring_model: String,
    pub project_default_model: String,
    pub project_integration_branch: String,
    pub project_dirty: bool,
}

impl SettingsEditor {
    pub fn new(settings: &GlobalSettings, policy: Option<&ProjectPolicy>) -> Self {
        let mut editor = Self {
            global: settings.clone(),
            binary: String::new(),
            authoring_model: String::new(),
            global_dirty: false,
            project: None,
            project_authoring_model: String::new(),
            project_integration_branch: String::new(),
            project_default_model: String::new(),
            project_dirty: false,
        };
        editor.load_global(settings);
        editor.load_project(policy);
        editor
    }

    fn load_global(&mut self, settings: &GlobalSettings) {
        self.global = settings.clone();
        self.binary = settings
            .claude_binary
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        self.authoring_model = settings.authoring_model.clone().unwrap_or_default();
        self.global_dirty = false;
    }

    fn load_project(&mut self, policy: Option<&ProjectPolicy>) {
        self.project = policy.cloned();
        self.project_authoring_model = policy
            .and_then(|p| p.authoring_model.clone())
            .unwrap_or_default();
        self.project_default_model = policy
            .and_then(|p| p.default_model.clone())
            .unwrap_or_default();
        self.project_integration_branch = policy
            .and_then(|p| p.integration_branch.clone())
            .unwrap_or_default();
        self.project_dirty = false;
    }

    /// Called when fresh values arrive. Unsaved edits are kept.
    pub fn refresh_global(&mut self, settings: &GlobalSettings) {
        if !self.global_dirty {
            self.load_global(settings);
        }
    }

    pub fn refresh_project(&mut self, policy: Option<&ProjectPolicy>) {
        if !self.project_dirty {
            self.load_project(policy);
        }
    }

    fn global_to_save(&self) -> GlobalSettings {
        let mut s = self.global.clone();
        s.human_handle = s.human_handle.trim().to_lowercase();
        s.human_display_name = s.human_display_name.trim().to_string();
        s.claude_binary = opt_path(&self.binary);
        s.authoring_model = opt_string(&self.authoring_model);
        s
    }

    fn project_to_save(&self) -> Option<ProjectPolicy> {
        let mut p = self.project.clone()?;
        p.name = p.name.trim().to_string();
        p.authoring_model = opt_string(&self.project_authoring_model);
        p.default_model = opt_string(&self.project_default_model);
        p.integration_branch = opt_string(&self.project_integration_branch);
        Some(p)
    }
}

fn opt_string(s: &str) -> Option<String> {
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn opt_path(s: &str) -> Option<PathBuf> {
    opt_string(s).map(PathBuf::from)
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(settings) = app.settings.clone() else {
        ui.weak("Loading settings…");
        return;
    };
    if app.settings_editor.is_none() {
        app.settings_editor = Some(SettingsEditor::new(&settings, app.policy.as_ref()));
    }
    let project = app.current;
    let project_name = app.project_name.clone();
    let probe = app.claude_probe.clone();
    let probing = app.probing;
    let bridge_line = app
        .bridge_addr
        .clone()
        .map(|a| format!("listening on {a}"))
        .unwrap_or_else(|| "not running: agents cannot start".into());
    let editor = app.settings_editor.as_mut().expect("just created");
    let actions = &mut app.actions;

    // First visit: ask about Claude Code once.
    if probe.is_none() && !probing {
        actions.push(Action::ProbeClaude);
    }

    ui.heading("Settings");
    ui.add_space(8.0);

    egui::ScrollArea::vertical()
        .id_salt("settings_scroll")
        .show(ui, |ui| {
            theme::glass(ui, theme::card(ui.style()), |ui| {
                ui.set_width(ui.available_width());
                widgets::caption(ui, "This computer");
                ui.weak("Stored in ~/.agentic-kanban/config.toml. Applies to every project.");
                ui.add_space(8.0);

                egui::Grid::new("global_settings")
                    .num_columns(2)
                    .spacing([16.0, 8.0])
                    .show(ui, |ui| {
                        ui.label("Your handle");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut editor.global.human_handle)
                                .id_salt("human_handle")
                                .desired_width(200.0),
                        );
                        if r.changed() {
                            editor.global_dirty = true;
                        }
                        ui.end_row();

                        ui.label("Display name");
                        let r = ui.add(
                            egui::TextEdit::singleline(&mut editor.global.human_display_name)
                                .id_salt("human_display_name")
                                .desired_width(200.0),
                        );
                        if r.changed() {
                            editor.global_dirty = true;
                        }
                        ui.end_row();

                        ui.label("Agent bridge");
                        ui.label(bridge_line.clone());
                        ui.end_row();

                        ui.label("Appearance");
                        ui.horizontal(|ui| {
                            for (m, label) in [
                                (ThemeMode::System, "Follow Windows"),
                                (ThemeMode::Light, "Light"),
                                (ThemeMode::Dark, "Dark"),
                            ] {
                                if ui
                                    .selectable_value(&mut editor.global.theme, m, label)
                                    .changed()
                                {
                                    editor.global_dirty = true;
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Claude Code binary");
                        ui.horizontal(|ui| {
                            let r = ui.add(
                                egui::TextEdit::singleline(&mut editor.binary)
                                    .id_salt("claude_binary")
                                    .hint_text("auto-detect")
                                    .desired_width(360.0),
                            );
                            if r.changed() {
                                editor.global_dirty = true;
                            }
                            if ui.button("Browse…").clicked() {
                                if let Some(path) = rfd::FileDialog::new()
                                    .set_title("Claude Code executable")
                                    .pick_file()
                                {
                                    editor.binary = path.display().to_string();
                                    editor.global_dirty = true;
                                }
                            }
                            if ui
                                .add_enabled(!probing, egui::Button::new("Check"))
                                .on_hover_text("Finds the binary and asks it whether it is logged in. Save first to check a new path.")
                                .clicked()
                            {
                                actions.push(Action::ProbeClaude);
                            }
                        });
                        ui.end_row();

                        ui.label("");
                        ui.vertical(|ui| match (&probe, probing) {
                            (_, true) => {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    ui.weak("Checking…");
                                });
                            }
                            (None, false) => {
                                ui.weak("Not checked yet.");
                            }
                            (Some(p), false) => {
                                match &p.binary {
                                    Some(b) => {
                                        ui.label(format!(
                                            "Found {}{}",
                                            b.display(),
                                            p.version
                                                .as_ref()
                                                .map(|v| format!("  ({v})"))
                                                .unwrap_or_default()
                                        ));
                                    }
                                    None => {
                                        ui.colored_label(
                                            theme::tone(ui.style(), Tone::Red),
                                            p.error.clone().unwrap_or_else(|| "Not found".into()),
                                        );
                                    }
                                }
                                match p.logged_in {
                                    Some(true) => {
                                        ui.colored_label(
                                            theme::tone(ui.style(), Tone::Green),
                                            format!(
                                                "Logged in ({})",
                                                p.auth_method.clone().unwrap_or_default()
                                            ),
                                        );
                                    }
                                    Some(false) => {
                                        ui.colored_label(
                                            theme::tone(ui.style(), Tone::Yellow),
                                            "Not logged in. Run `claude` in a terminal and use /login.",
                                        );
                                    }
                                    None => {
                                        if let Some(e) = &p.error {
                                            if p.binary.is_some() {
                                                ui.weak(e);
                                            }
                                        }
                                    }
                                }
                            }
                        });
                        ui.end_row();

                        ui.label("Refine model");
                        if model_picker(
                            ui,
                            "global_authoring_model",
                            &mut editor.authoring_model,
                            "Claude Code default",
                        ) {
                            editor.global_dirty = true;
                        }
                        ui.end_row();

                        ui.label("Concurrent agent runs");
                        if ui
                            .add(egui::DragValue::new(&mut editor.global.max_concurrent_runs).range(1..=16))
                            .changed()
                        {
                            editor.global_dirty = true;
                        }
                        ui.end_row();

                        ui.label("Concurrent Refine calls");
                        if ui
                            .add(
                                egui::DragValue::new(&mut editor.global.max_concurrent_utility_calls)
                                    .range(1..=8),
                            )
                            .changed()
                        {
                            editor.global_dirty = true;
                        }
                        ui.end_row();

                        ui.label("Daily budget (USD)");
                        if ui
                            .add(
                                egui::DragValue::new(&mut editor.global.global_daily_budget_usd)
                                    .range(0.0..=10_000.0)
                                    .speed(1.0)
                                    .prefix("$"),
                            )
                            .changed()
                        {
                            editor.global_dirty = true;
                        }
                        ui.end_row();

                        ui.label("Keep run files (days)");
                        if ui
                            .add(egui::DragValue::new(&mut editor.global.run_retention_days).range(1..=365))
                            .changed()
                        {
                            editor.global_dirty = true;
                        }
                        ui.end_row();
                    });
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if widgets::filled_button_if(
                    ui,
                    editor.global_dirty,
                    "Save",
                    Tone::Accent,
                )
                .clicked()
                {
                    let settings = editor.global_to_save();
                    match ak_core::Actor::validate_handle(&settings.human_handle) {
                        Ok(()) => {
                            actions.push(Action::Request(Request::SaveGlobalSettings(settings)));
                            editor.global_dirty = false;
                        }
                        Err(e) => actions.push(Action::Toast(e.to_string(), true)),
                    }
                }
                if editor.global_dirty && ui.button("Revert").clicked() {
                    let s = settings.clone();
                    editor.global_dirty = false;
                    editor.refresh_global(&s);
                }
                ui.weak("Model and binary changes apply to the next Refine. The handle applies everywhere.");
            });

            ui.add_space(12.0);

            match (project, editor.project.as_mut()) {
                (Some(project), Some(policy)) => {
                    theme::glass(ui, theme::card(ui.style()), |ui| {
                        ui.set_width(ui.available_width());
                        widgets::caption(ui, &format!("Project: {project_name}"));
                        ui.weak("Stored in the project's project.toml. Exported with the team.");
                        ui.add_space(8.0);

                        let global_authoring = opt_string(&editor.authoring_model)
                            .map(|m| format!("global ({m})"))
                            .unwrap_or_else(|| "global (Claude Code default)".into());

                        egui::Grid::new("project_settings")
                            .num_columns(2)
                            .spacing([16.0, 8.0])
                            .show(ui, |ui| {
                                ui.label("Name");
                                let r = ui.add(
                                    egui::TextEdit::singleline(&mut policy.name)
                                        .id_salt("project_name")
                                        .desired_width(300.0),
                                );
                                if r.changed() {
                                    editor.project_dirty = true;
                                }
                                ui.end_row();

                                ui.label("Ticket key prefix");
                                ui.horizontal(|ui| {
                                    ui.monospace(&policy.key_prefix);
                                    ui.weak("fixed after creation");
                                });
                                ui.end_row();

                                ui.label("Refine model");
                                if model_picker(
                                    ui,
                                    "project_authoring_model",
                                    &mut editor.project_authoring_model,
                                    &global_authoring,
                                ) {
                                    editor.project_dirty = true;
                                }
                                ui.end_row();

                                ui.label("Agent run model");
                                if model_picker(
                                    ui,
                                    "project_default_model",
                                    &mut editor.project_default_model,
                                    "Claude Code default",
                                ) {
                                    editor.project_dirty = true;
                                }
                                ui.end_row();

                                ui.label("Integration branch");
                                ui.horizontal(|ui| {
                                    if ui
                                        .add(
                                            egui::TextEdit::singleline(
                                                &mut editor.project_integration_branch,
                                            )
                                            .id_salt("integration_branch")
                                            .hint_text("default branch")
                                            .desired_width(160.0),
                                        )
                                        .changed()
                                    {
                                        editor.project_dirty = true;
                                    }
                                    ui.weak("ticket branches start here and the integrator merges here");
                                });
                                ui.end_row();

                                ui.label("Archive idle threads after");
                                ui.horizontal(|ui| {
                                    if ui
                                        .add(
                                            egui::DragValue::new(&mut policy.thread_archive_days)
                                                .range(1..=365),
                                        )
                                        .changed()
                                    {
                                        editor.project_dirty = true;
                                    }
                                    ui.weak("days without activity. Posting reopens a thread.");
                                });
                                ui.end_row();

                                ui.label("Push policy");
                                egui::ComboBox::from_id_salt("push_policy")
                                    .selected_text(match policy.push_policy {
                                        PushPolicy::Never => "Never push",
                                        PushPolicy::Allowed => "Agents may push",
                                    })
                                    .show_ui(ui, |ui| {
                                        if ui
                                            .selectable_value(
                                                &mut policy.push_policy,
                                                PushPolicy::Never,
                                                "Never push",
                                            )
                                            .changed()
                                            || ui
                                                .selectable_value(
                                                    &mut policy.push_policy,
                                                    PushPolicy::Allowed,
                                                    "Agents may push",
                                                )
                                                .changed()
                                        {
                                            editor.project_dirty = true;
                                        }
                                    });
                                ui.end_row();

                                ui.label("Project MCP servers");
                                if ui
                                    .checkbox(
                                        &mut policy.allow_project_mcp,
                                        "Let agent runs load the repository's .mcp.json",
                                    )
                                    .changed()
                                {
                                    editor.project_dirty = true;
                                }
                                ui.end_row();
                            });

                        egui::CollapsingHeader::new("Guardrails")
                            .id_salt("guardrails")
                            .show(ui, |ui| {
                                ui.weak("Each rule has a threshold and a window; the action is fixed. Zero disables a rule.");
                                let g = &mut policy.guardrails;
                                let mut changed = false;
                                egui::Grid::new("guardrail_fields")
                                    .num_columns(3)
                                    .spacing([12.0, 6.0])
                                    .show(ui, |ui| {
                                        ui.label("Ticket ping-pong");
                                        changed |= ui.add(egui::DragValue::new(&mut g.ping_pong_moves)).changed();
                                        ui.horizontal(|ui| {
                                            ui.weak("moves or reassignments in");
                                            changed |= ui.add(egui::DragValue::new(&mut g.ping_pong_window_minutes).suffix(" min")).changed();
                                            ui.weak("freezes the ticket");
                                        });
                                        ui.end_row();

                                        ui.label("Comment storm");
                                        changed |= ui.add(egui::DragValue::new(&mut g.comment_storm_comments)).changed();
                                        ui.horizontal(|ui| {
                                            ui.weak("agent comments in");
                                            changed |= ui.add(egui::DragValue::new(&mut g.comment_storm_window_minutes).suffix(" min")).changed();
                                            ui.weak("freezes the ticket");
                                        });
                                        ui.end_row();

                                        ui.label("Mention loop");
                                        changed |= ui.add(egui::DragValue::new(&mut g.mention_loop_mentions)).changed();
                                        ui.horizontal(|ui| {
                                            ui.weak("agent-to-agent mentions in");
                                            changed |= ui.add(egui::DragValue::new(&mut g.mention_loop_window_minutes).suffix(" min")).changed();
                                            ui.weak("with no progress freezes the ticket or pauses the thread");
                                        });
                                        ui.end_row();

                                        ui.label("Run churn");
                                        changed |= ui.add(egui::DragValue::new(&mut g.run_churn_runs)).changed();
                                        ui.horizontal(|ui| {
                                            ui.weak("runs in a row that handle nothing cool the agent down for");
                                            changed |= ui.add(egui::DragValue::new(&mut g.run_churn_cooldown_minutes).suffix(" min")).changed();
                                        });
                                        ui.end_row();

                                        ui.label("Repeated tool call");
                                        changed |= ui.add(egui::DragValue::new(&mut g.repeated_tool_calls)).changed();
                                        ui.weak("identical calls in a row stop the run");
                                        ui.end_row();

                                        ui.label("Turns per run");
                                        changed |= ui.add(egui::DragValue::new(&mut g.max_turns_per_run)).changed();
                                        ui.weak("unless the agent sets its own");
                                        ui.end_row();

                                        ui.label("Cost per run");
                                        changed |= ui.add(egui::DragValue::new(&mut g.max_usd_per_run).speed(0.1).prefix("$")).changed();
                                        ui.weak("unless the agent sets its own");
                                        ui.end_row();

                                        ui.label("Cost per agent per day");
                                        changed |= ui.add(egui::DragValue::new(&mut g.max_usd_per_agent_per_day).speed(0.5).prefix("$")).changed();
                                        ui.weak("no more runs for that agent today");
                                        ui.end_row();

                                        ui.label("Wall clock per run");
                                        changed |= ui.add(egui::DragValue::new(&mut g.max_wall_clock_minutes).suffix(" min")).changed();
                                        ui.weak("stops the run");
                                        ui.end_row();

                                        ui.label("Wake storm");
                                        changed |= ui.add(egui::DragValue::new(&mut g.wake_storm_per_hour)).changed();
                                        ui.weak("wakes per hour; further wakes wait and you are told");
                                        ui.end_row();
                                    });
                                if changed {
                                    editor.project_dirty = true;
                                }
                            });
                    });

                    // Below the card: saving reads the whole editor, which the card partly borrows.
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if widgets::filled_button_if(
                            ui,
                            editor.project_dirty,
                            "Save",
                            Tone::Accent,
                        )
                        .clicked()
                        {
                            if let Some(policy) = editor.project_to_save() {
                                if policy.name.is_empty() {
                                    actions.push(Action::Toast(
                                        "project name cannot be empty".into(),
                                        true,
                                    ));
                                } else {
                                    actions.push(Action::Request(Request::SaveProjectPolicy {
                                        project,
                                        policy,
                                    }));
                                    editor.project_dirty = false;
                                }
                            }
                        }
                        ui.weak("Per-agent models are set in each agent's definition.");
                    });
                }
                _ => {
                    ui.weak("Open a project to edit its settings.");
                }
            }
        });
}
