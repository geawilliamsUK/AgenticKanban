//! Export to and Import from a repository's `.agentic/` snapshot, with the
//! per-agent diff review Integrate also uses.

use ak_runtime::snapshot::{ExportOptions, ImportReview, ImportSelection, ImportStatus};

use crate::app::{Action, App};
use crate::backend::Request;
use crate::theme::{self, Tone};
use crate::widgets;

#[derive(Default)]
pub struct ExportDialog {
    pub options: ExportOptions,
}

pub struct ImportDialog {
    pub review: ImportReview,
    pub selected: Vec<bool>,
    pub memory: Vec<bool>,
    pub policy: bool,
    pub expanded: Option<usize>,
}

impl ImportDialog {
    /// New and changed agents are on by default; identical ones and memory
    /// are off; the policy is on only when the snapshot is this project's.
    pub fn new(review: ImportReview) -> Self {
        let selected = review
            .candidates
            .iter()
            .map(|c| c.status != ImportStatus::Identical)
            .collect();
        let memory = vec![false; review.candidates.len()];
        let policy = review.policy.is_some() && review.same_project == Some(true);
        Self {
            review,
            selected,
            memory,
            policy,
            expanded: None,
        }
    }

    fn selection(&self) -> ImportSelection {
        let pick = |flags: &[bool]| -> Vec<String> {
            self.review
                .candidates
                .iter()
                .zip(flags)
                .filter(|(_, on)| **on)
                .map(|(c, _)| c.handle.clone())
                .collect()
        };
        ImportSelection {
            handles: pick(&self.selected),
            memory_for: pick(&self.memory),
            policy: self.policy,
        }
    }
}

pub fn draw(app: &mut App, ctx: &egui::Context) {
    draw_export(app, ctx);
    draw_import(app, ctx);
}

fn draw_export(app: &mut App, ctx: &egui::Context) {
    let Some(project) = app.current else { return };
    let Some(dialog) = app.export_dialog.as_mut() else {
        return;
    };
    let actions = &mut app.actions;
    let mut open = true;
    let mut done = false;
    egui::Window::new("Export agents to project")
        .collapsible(false)
        .resizable(false)
        .default_width(520.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.label(
                "Writes a snapshot of this project's agent definitions into .agentic/ in the \
                 repository, for sharing through git. Runs never read it. Commit it yourself, \
                 on whatever branch you are on.",
            );
            ui.add_space(8.0);
            theme::glass(ui, theme::card(ui.style()), |ui| {
                ui.set_width(ui.available_width());
                ui.checkbox(
                    &mut dialog.options.include_policy,
                    "Include the project policy (columns, guardrails, profiles, models)",
                );
                ui.checkbox(
                    &mut dialog.options.include_memory,
                    "Include agent memory (this machine's experience)",
                );
                ui.checkbox(&mut dialog.options.stage, "Stage the files with git add");
            });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if widgets::filled_button(ui, "Export", Tone::Accent).clicked() {
                    actions.push(Action::Request(Request::ExportAgents {
                        project,
                        options: dialog.options.clone(),
                    }));
                    done = true;
                }
                if ui.button("Cancel").clicked() {
                    done = true;
                }
            });
        });
    if !open || done {
        app.export_dialog = None;
    }
}

fn draw_import(app: &mut App, ctx: &egui::Context) {
    let Some(project) = app.current else { return };
    let Some(dialog) = app.import_dialog.as_mut() else {
        return;
    };
    let mut open = true;
    let mut close = false;
    let mut apply = false;
    egui::Window::new("Import agents from project")
        .collapsible(false)
        .resizable(true)
        .default_width(760.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.weak(format!("Snapshot in {}", dialog.review.source.display()));
            match dialog.review.same_project {
                Some(true) => {
                    ui.weak("The snapshot carries this project's shared id.");
                }
                Some(false) => {
                    theme::glass(ui, theme::tinted(ui.style(), Tone::Yellow), |ui| {
                        ui.label(
                            "The snapshot comes from a different project: its shared id differs. \
                             Importing its policy would replace this project's columns, guardrails, \
                             profiles and key prefix.",
                        );
                    });
                }
                None => {
                    ui.weak("The snapshot has no policy file.");
                }
            }
            for (handle, err) in &dialog.review.errors {
                ui.colored_label(theme::tone(ui.style(), Tone::Red), format!("{handle}: {err}"));
            }
            if dialog.review.candidates.is_empty() {
                ui.weak("No agents in the snapshot.");
            }
            ui.add_space(8.0);
            egui::ScrollArea::vertical()
                .id_salt("import_review")
                .max_height(480.0)
                .show(ui, |ui| {
                    for i in 0..dialog.review.candidates.len() {
                        let c = &dialog.review.candidates[i];
                        let expanded = dialog.expanded == Some(i);
                        let mut toggle = false;
                        theme::glass(ui, theme::card(ui.style()), |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut dialog.selected[i], "");
                                ui.label(egui::RichText::new(format!("@{}", c.handle)).strong());
                                let (label, tone) = match c.status {
                                    ImportStatus::New => ("new", Tone::Green),
                                    ImportStatus::Changed => ("changed", Tone::Yellow),
                                    ImportStatus::Identical => ("identical", Tone::Gray),
                                };
                                widgets::chip(ui, tone, label);
                                if c.incoming_memory.is_some() {
                                    let allowed = dialog.selected[i] || c.current.is_some();
                                    ui.add_enabled(allowed, egui::Checkbox::new(&mut dialog.memory[i], "memory too"))
                                        .on_hover_text("Replaces this machine's memory for the agent with the snapshot's.");
                                }
                                if c.status == ImportStatus::Changed
                                    && ui
                                        .small_button(if expanded { "Hide diff" } else { "Diff" })
                                        .clicked()
                                {
                                    toggle = true;
                                }
                            });
                            ui.weak(&c.incoming.frontmatter.description);
                            if expanded {
                                let current = c
                                    .current
                                    .as_ref()
                                    .map(|d| d.render())
                                    .unwrap_or_default();
                                crate::diff_view::draw(
                                    ui,
                                    &format!("import_diff_{}", c.handle),
                                    &current,
                                    &c.incoming.render(),
                                );
                            }
                        });
                        if toggle {
                            dialog.expanded = if expanded { None } else { Some(i) };
                        }
                        ui.add_space(8.0);
                    }
                });
            if dialog.review.policy.is_some() {
                ui.checkbox(&mut dialog.policy, "Import the project policy");
            }
            let n = dialog.selected.iter().filter(|s| **s).count();
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if widgets::filled_button_if(
                    ui,
                    n > 0 || dialog.policy,
                    format!("Import {n} agent(s)"),
                    Tone::Accent,
                )
                .clicked()
                {
                    apply = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    if apply {
        let selection = dialog.selection();
        app.actions.push(Action::Request(Request::ApplyImport {
            project,
            review: dialog.review.clone(),
            selection,
        }));
        close = true;
    }
    if !open || close {
        app.import_dialog = None;
    }
}
