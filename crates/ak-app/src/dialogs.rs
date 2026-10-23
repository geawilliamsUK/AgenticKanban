//! Modal dialogs: new ticket, add project.

use std::path::PathBuf;

use ak_core::board::{ColumnKind, Priority};
use ak_core::commands::NewTicket;
use ak_core::ids::{ActorId, ColumnId};

use crate::app::{Action, App};
use crate::backend::Request;
use crate::model::BoardModel;
use crate::theme::{self, Tone};
use crate::widgets;

pub struct NewTicketDialog {
    pub title: String,
    pub description: String,
    pub column_id: Option<ColumnId>,
    pub assignee_id: Option<ActorId>,
    pub priority: Priority,
    pub labels: String,
}

impl NewTicketDialog {
    pub fn new(board: &BoardModel) -> Self {
        let column_id = board
            .columns
            .iter()
            .find(|c| c.kind == ColumnKind::Todo)
            .or_else(|| board.columns.first())
            .map(|c| c.id);
        Self {
            title: String::new(),
            description: String::new(),
            column_id,
            assignee_id: None,
            priority: Priority::Medium,
            labels: String::new(),
        }
    }
}

pub struct AddProjectDialog {
    pub path: PathBuf,
    pub name: String,
}

impl AddProjectDialog {
    pub fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        Self { path, name }
    }
}

pub fn draw(app: &mut App, ctx: &egui::Context) {
    draw_new_ticket(app, ctx);
    draw_add_project(app, ctx);
}

fn draw_new_ticket(app: &mut App, ctx: &egui::Context) {
    let Some(project) = app.current else { return };
    let Some(dialog) = app.new_ticket.as_mut() else {
        return;
    };
    let board = &app.board;
    let actions = &mut app.actions;

    let mut open = true;
    let mut close = false;
    egui::Window::new("New ticket")
        .collapsible(false)
        .resizable(true)
        .default_width(520.0)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            ui.add(
                egui::TextEdit::singleline(&mut dialog.title)
                    .id_salt("new_ticket_title")
                    .desired_width(f32::INFINITY)
                    .hint_text("Title"),
            );
            crate::mention_edit::multiline(
                ui,
                "new_description",
                &mut dialog.description,
                &board.actors,
                "Description (markdown, type @ to mention a teammate)",
                8,
            );
            ui.add_space(8.0);
            theme::glass(ui, theme::card(ui.style()), |ui| {
                ui.set_width(ui.available_width());
                egui::Grid::new("new_ticket_fields")
                    .num_columns(2)
                    .spacing([12.0, 6.0])
                    .show(ui, |ui| {
                        ui.label("Column");
                        let col_name = dialog
                            .column_id
                            .and_then(|id| board.column(id))
                            .map(|c| c.name.clone())
                            .unwrap_or_else(|| "Choose".into());
                        egui::ComboBox::from_id_salt("new_column")
                            .selected_text(col_name)
                            .show_ui(ui, |ui| {
                                for c in &board.columns {
                                    ui.selectable_value(&mut dialog.column_id, Some(c.id), &c.name);
                                }
                            });
                        ui.end_row();

                        ui.label("Assignee");
                        let who = dialog
                            .assignee_id
                            .map(|a| board.handle(a))
                            .unwrap_or_else(|| "Unassigned".into());
                        egui::ComboBox::from_id_salt("new_assignee")
                            .selected_text(who)
                            .show_ui(ui, |ui| {
                                ui.selectable_value(&mut dialog.assignee_id, None, "Unassigned");
                                for a in board.assignable() {
                                    ui.selectable_value(
                                        &mut dialog.assignee_id,
                                        Some(a.id),
                                        format!("@{}", a.handle),
                                    );
                                }
                            });
                        ui.end_row();

                        ui.label("Priority");
                        egui::ComboBox::from_id_salt("new_priority")
                            .selected_text(dialog.priority.label())
                            .show_ui(ui, |ui| {
                                for p in Priority::ALL {
                                    ui.selectable_value(&mut dialog.priority, p, p.label());
                                }
                            });
                        ui.end_row();

                        ui.label("Labels");
                        ui.add(
                            egui::TextEdit::singleline(&mut dialog.labels)
                                .id_salt("new_ticket_labels")
                                .hint_text("comma, separated"),
                        );
                        ui.end_row();
                    });
            });

            ui.add_space(8.0);
            ui.horizontal(|ui| {
                let can_create = !dialog.title.trim().is_empty() && dialog.column_id.is_some();
                if widgets::filled_button_if(ui, can_create, "Create", Tone::Accent).clicked() {
                    let labels: Vec<String> = dialog
                        .labels
                        .split(',')
                        .map(|s| s.trim().to_string())
                        .filter(|s| !s.is_empty())
                        .collect();
                    actions.push(Action::Request(Request::CreateTicket {
                        project,
                        input: NewTicket {
                            title: dialog.title.clone(),
                            description: dialog.description.clone(),
                            column_id: dialog.column_id.unwrap(),
                            assignee_id: dialog.assignee_id,
                            priority: dialog.priority,
                            labels,
                            parent_id: None,
                        },
                    }));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    if !open || close {
        app.new_ticket = None;
    }
}

fn draw_add_project(app: &mut App, ctx: &egui::Context) {
    let Some(dialog) = app.add_project.as_mut() else {
        return;
    };
    let actions = &mut app.actions;

    let mut open = true;
    let mut close = false;
    egui::Window::new("Add project")
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .open(&mut open)
        .show(ctx, |ui| {
            theme::glass(ui, theme::card(ui.style()), |ui| {
                ui.label("Repository");
                ui.monospace(dialog.path.display().to_string());
                ui.add_space(8.0);
                ui.label("Name");
                ui.add(
                    egui::TextEdit::singleline(&mut dialog.name)
                        .id_salt("add_project_name")
                        .desired_width(320.0),
                );
            });
            ui.add_space(8.0);
            ui.weak("The app keeps this project's board and agents in its own home directory. The repository is not modified.");
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if widgets::filled_button_if(
                    ui,
                    !dialog.name.trim().is_empty(),
                    "Add",
                    Tone::Accent,
                )
                .clicked()
                {
                    actions.push(Action::Request(Request::AddProject {
                        path: dialog.path.clone(),
                        name: Some(dialog.name.trim().to_string()),
                    }));
                    close = true;
                }
                if ui.button("Cancel").clicked() {
                    close = true;
                }
            });
        });
    if !open || close {
        app.add_project = None;
    }
}
