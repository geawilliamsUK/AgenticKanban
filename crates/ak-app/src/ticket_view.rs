//! The ticket panel: fields, freeze controls, comment tree, activity.

use std::collections::HashMap;

use ak_core::board::{Comment, Priority};
use ak_core::commands::TicketPatch;
use ak_core::ids::CommentId;

use crate::app::{Action, App};
use crate::backend::Request;
use crate::model::{describe_event, format_time, priority_color};

#[derive(Default)]
pub struct TicketPanelState {
    pub editing: Option<EditDraft>,
    pub compose: String,
    pub reply_to: Option<CommentId>,
    pub freeze_reason: String,
}

pub struct EditDraft {
    pub title: String,
    pub description: String,
    pub priority: Priority,
    pub labels: String,
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else { return };
    let Some(detail) = app.detail.clone() else {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak("Loading ticket…");
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("✕").clicked() {
                    app.actions.push(Action::SelectTicket(None));
                }
            });
        });
        return;
    };
    let t = &detail.ticket;

    // Header
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(&t.key).monospace().strong());
        if t.frozen {
            ui.colored_label(egui::Color32::from_rgb(90, 160, 230), "FROZEN");
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("✕").on_hover_text("Close").clicked() {
                app.actions.push(Action::SelectTicket(None));
            }
            if app.ticket_panel.editing.is_none() && ui.small_button("Edit").clicked() {
                app.ticket_panel.editing = Some(EditDraft {
                    title: t.title.clone(),
                    description: t.description.clone(),
                    priority: t.priority,
                    labels: t.labels.join(", "),
                });
            }
        });
    });

    egui::ScrollArea::vertical()
        .id_salt("ticket_scroll")
        .show(ui, |ui| {
            // Title and description, or the edit form.
            let mut finish_editing = false;
            if let Some(draft) = app.ticket_panel.editing.as_mut() {
                ui.add(
                    egui::TextEdit::singleline(&mut draft.title)
                        .desired_width(f32::INFINITY)
                        .hint_text("Title"),
                );
                crate::mention_edit::multiline(
                    ui,
                    "edit_description",
                    &mut draft.description,
                    &app.board.actors,
                    "Description (markdown, type @ to mention a teammate)",
                    8,
                );
                ui.horizontal(|ui| {
                    ui.label("Priority");
                    egui::ComboBox::from_id_salt("edit_priority")
                        .selected_text(draft.priority.label())
                        .show_ui(ui, |ui| {
                            for p in Priority::ALL {
                                ui.selectable_value(&mut draft.priority, p, p.label());
                            }
                        });
                });
                ui.horizontal(|ui| {
                    ui.label("Labels");
                    ui.add(
                        egui::TextEdit::singleline(&mut draft.labels).hint_text("comma, separated"),
                    );
                });
                ui.horizontal(|ui| {
                    if ui.button("Save").clicked() {
                        let labels: Vec<String> = draft
                            .labels
                            .split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect();
                        app.actions.push(Action::Request(Request::UpdateTicket {
                            project,
                            id: t.id,
                            patch: TicketPatch {
                                title: Some(draft.title.clone()),
                                description: Some(draft.description.clone()),
                                priority: Some(draft.priority),
                                labels: Some(labels),
                            },
                            version: t.version,
                        }));
                        finish_editing = true;
                    }
                    if ui.button("Cancel").clicked() {
                        finish_editing = true;
                    }
                });
            } else {
                ui.heading(&t.title);
                ui.add_space(4.0);
                app.markdown.show(ui, &t.description);
            }
            if finish_editing {
                app.ticket_panel.editing = None;
            }

            ui.add_space(8.0);
            ui.separator();

            // Column, assignee, priority, labels, reporter.
            egui::Grid::new("ticket_fields")
                .num_columns(2)
                .spacing([12.0, 6.0])
                .show(ui, |ui| {
                    ui.label("Column");
                    let current_col = app
                        .board
                        .column(t.column_id)
                        .map(|c| c.name.clone())
                        .unwrap_or_default();
                    egui::ComboBox::from_id_salt("move_column")
                        .selected_text(current_col)
                        .show_ui(ui, |ui| {
                            for c in &app.board.columns {
                                if ui.selectable_label(c.id == t.column_id, &c.name).clicked()
                                    && c.id != t.column_id
                                {
                                    app.actions.push(Action::Request(Request::MoveTicket {
                                        project,
                                        id: t.id,
                                        to: c.id,
                                        version: t.version,
                                    }));
                                }
                            }
                        });
                    ui.end_row();

                    ui.label("Assignee");
                    let current_assignee = t
                        .assignee_id
                        .map(|a| app.board.handle(a))
                        .unwrap_or_else(|| "Unassigned".into());
                    egui::ComboBox::from_id_salt("assignee")
                        .selected_text(current_assignee)
                        .show_ui(ui, |ui| {
                            if ui
                                .selectable_label(t.assignee_id.is_none(), "Unassigned")
                                .clicked()
                                && t.assignee_id.is_some()
                            {
                                app.actions.push(Action::Request(Request::AssignTicket {
                                    project,
                                    id: t.id,
                                    to: None,
                                    version: t.version,
                                }));
                            }
                            for a in app.board.assignable() {
                                let is = t.assignee_id == Some(a.id);
                                if ui.selectable_label(is, format!("@{}", a.handle)).clicked()
                                    && !is
                                {
                                    app.actions.push(Action::Request(Request::AssignTicket {
                                        project,
                                        id: t.id,
                                        to: Some(a.id),
                                        version: t.version,
                                    }));
                                }
                            }
                        });
                    ui.end_row();

                    ui.label("Priority");
                    ui.horizontal(|ui| {
                        let (rect, _) =
                            ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
                        ui.painter()
                            .circle_filled(rect.center(), 4.0, priority_color(t.priority));
                        ui.label(t.priority.label());
                    });
                    ui.end_row();

                    if !t.labels.is_empty() {
                        ui.label("Labels");
                        ui.horizontal_wrapped(|ui| {
                            for l in &t.labels {
                                ui.small(format!("#{l}"));
                            }
                        });
                        ui.end_row();
                    }

                    ui.label("Reporter");
                    ui.label(app.board.handle(t.reporter_id));
                    ui.end_row();

                    ui.label("Watchers");
                    ui.horizontal_wrapped(|ui| {
                        for w in &detail.watchers {
                            ui.small(app.board.handle(*w));
                        }
                    });
                    ui.end_row();

                    ui.label("Updated");
                    ui.label(format!("{}  (v{})", format_time(&t.updated_at), t.version));
                    ui.end_row();
                });

            ui.add_space(8.0);

            // Freeze controls.
            if t.frozen {
                egui::Frame::new()
                    .fill(egui::Color32::from_rgb(35, 60, 90))
                    .corner_radius(4.0)
                    .inner_margin(8.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(egui::RichText::new("Frozen").strong());
                        ui.label(t.freeze_reason.clone().unwrap_or_default());
                        ui.weak("Agents cannot change this ticket until you unfreeze it.");
                        if ui.button("Unfreeze").clicked() {
                            app.actions
                                .push(Action::Request(Request::Unfreeze { project, id: t.id }));
                        }
                    });
            } else {
                egui::CollapsingHeader::new("Freeze")
                    .id_salt("freeze_header")
                    .show(ui, |ui| {
                        ui.add(
                            egui::TextEdit::singleline(&mut app.ticket_panel.freeze_reason)
                                .hint_text("Reason")
                                .desired_width(f32::INFINITY),
                        );
                        if ui.button("Freeze ticket").clicked()
                            && !app.ticket_panel.freeze_reason.trim().is_empty()
                        {
                            app.actions.push(Action::Request(Request::Freeze {
                                project,
                                id: t.id,
                                reason: app.ticket_panel.freeze_reason.trim().to_string(),
                            }));
                            app.ticket_panel.freeze_reason.clear();
                        }
                    });
            }

            ui.add_space(8.0);
            ui.separator();

            // Comments as a tree.
            ui.label(egui::RichText::new(format!("Comments ({})", detail.comments.len())).strong());
            let mut children: HashMap<Option<CommentId>, Vec<&Comment>> = HashMap::new();
            for c in &detail.comments {
                children.entry(c.parent_comment_id).or_default().push(c);
            }
            let mut reply_clicked: Option<CommentId> = None;
            draw_comment_tree(ui, app, &children, None, 0, &mut reply_clicked);
            if let Some(id) = reply_clicked {
                app.ticket_panel.reply_to = Some(id);
            }
            if detail.comments.is_empty() {
                ui.weak("No comments yet.");
            }

            ui.add_space(6.0);
            if let Some(reply) = app.ticket_panel.reply_to {
                let author = detail
                    .comments
                    .iter()
                    .find(|c| c.id == reply)
                    .map(|c| app.board.handle(c.author_id))
                    .unwrap_or_default();
                ui.horizontal(|ui| {
                    ui.weak(format!("Replying to {author}"));
                    if ui.small_button("cancel").clicked() {
                        app.ticket_panel.reply_to = None;
                    }
                });
            }
            crate::mention_edit::multiline(
                ui,
                "compose",
                &mut app.ticket_panel.compose,
                &app.board.actors,
                "Write a comment. Type @ to mention a teammate.",
                3,
            );
            let send_label = if app.ticket_panel.reply_to.is_some() {
                "Reply"
            } else {
                "Comment"
            };
            if ui.button(send_label).clicked() && !app.ticket_panel.compose.trim().is_empty() {
                app.actions.push(Action::Request(Request::Comment {
                    project,
                    ticket: t.id,
                    body: app.ticket_panel.compose.trim().to_string(),
                    parent: app.ticket_panel.reply_to,
                }));
                app.ticket_panel.compose.clear();
                app.ticket_panel.reply_to = None;
            }

            ui.add_space(8.0);
            ui.separator();

            // Activity timeline.
            egui::CollapsingHeader::new(format!("Activity ({})", detail.events.len()))
                .id_salt("activity_header")
                .show(ui, |ui| {
                    for e in detail.events.iter().rev() {
                        ui.horizontal_wrapped(|ui| {
                            ui.weak(format_time(&e.occurred_at));
                            ui.label(describe_event(e, &app.board));
                        });
                    }
                });
        });
}

fn draw_comment_tree(
    ui: &mut egui::Ui,
    app: &App,
    children: &HashMap<Option<CommentId>, Vec<&Comment>>,
    parent: Option<CommentId>,
    depth: usize,
    reply_clicked: &mut Option<CommentId>,
) {
    let Some(list) = children.get(&parent) else {
        return;
    };
    for c in list {
        // The indent row is horizontal, so the comment itself must open a
        // vertical scope. A frame placed directly in the row inherits the
        // horizontal layout, which laid the body out sideways and made the
        // panel grow every frame.
        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 18.0);
            ui.vertical(|ui| {
                egui::Frame::new()
                    .fill(ui.visuals().faint_bg_color)
                    .corner_radius(4.0)
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(app.board.handle(c.author_id)).strong());
                            ui.weak(format_time(&c.created_at));
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if depth < 6 && ui.small_button("Reply").clicked() {
                                        *reply_clicked = Some(c.id);
                                    }
                                },
                            );
                        });
                        let body = crate::mention_edit::highlighted(ui, &c.body, &app.board.actors);
                        ui.add(egui::Label::new(body).wrap());
                    });
            });
        });
        ui.add_space(3.0);
        draw_comment_tree(ui, app, children, Some(c.id), depth + 1, reply_clicked);
    }
}
