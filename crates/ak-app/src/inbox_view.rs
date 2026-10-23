//! The human's inbox: approvals that block an agent, then every notification
//! addressed to the human, newest first.

use ak_core::NotificationState;

use crate::app::{Action, App};
use crate::backend::Request;
use crate::model::{describe_event, format_time, kind_label};
use crate::theme::{self, Tone};
use crate::widgets;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else { return };

    let approvals = app.approvals.clone();
    if !approvals.is_empty() {
        ui.horizontal(|ui| {
            ui.heading("Approvals");
            widgets::chip(ui, Tone::Yellow, format!("{} waiting", approvals.len()));
        });
        ui.weak("An agent wants to do something its permission profile does not cover. The agent is paused until you answer.");
        ui.add_space(8.0);
        for a in &approvals {
            let base = theme::tinted(ui.style(), Tone::Orange);
            theme::glass(ui, base, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(app.board.handle(a.agent_id)).strong());
                    ui.weak(format_time(&a.created_at));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::tinted_button(ui, "Deny", Tone::Red).clicked() {
                            app.actions.push(Action::Request(Request::DecideApproval {
                                project,
                                id: a.id,
                                approve: false,
                            }));
                        }
                        if widgets::tinted_button(ui, "Approve", Tone::Green).clicked() {
                            app.actions.push(Action::Request(Request::DecideApproval {
                                project,
                                id: a.id,
                                approve: true,
                            }));
                        }
                    });
                });
                ui.add(egui::Label::new(egui::RichText::new(&a.summary).monospace()).wrap());
                egui::CollapsingHeader::new("Full input")
                    .id_salt(("approval_input", a.id))
                    .show(ui, |ui| {
                        let pretty = serde_json::to_string_pretty(&a.input).unwrap_or_default();
                        ui.add(
                            egui::Label::new(egui::RichText::new(pretty).monospace().small())
                                .wrap(),
                        );
                    });
            });
            ui.add_space(8.0);
        }
        ui.add_space(8.0);
        ui.separator();
    }

    ui.horizontal(|ui| {
        ui.heading("Inbox");
        ui.weak(format!(
            "{} open",
            app.inbox
                .iter()
                .filter(|i| i.notification.state.is_open())
                .count()
        ));
    });
    ui.add_space(8.0);

    let items = app.inbox.clone();
    if items.is_empty() {
        ui.weak("Nothing here. You will be notified when someone else mentions you, assigns you, replies to you, or moves a ticket you watch.");
        ui.weak("Your own actions never notify you.");
        return;
    }

    let scroll = egui::ScrollArea::vertical().id_salt("inbox_scroll");
    scroll.show(ui, |ui| {
        for item in &items {
            let n = &item.notification;
            let open = n.state.is_open();
            // A read item sinks into the inset fill, so open items stand out.
            let base = if open {
                theme::card(ui.style())
            } else {
                theme::card(ui.style()).fill(theme::palette(ui.style()).inset)
            };
            theme::glass(ui, base, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    let kind = egui::RichText::new(kind_label(n.kind)).strong();
                    ui.label(if open { kind } else { kind.weak() });
                    ui.weak(format_time(&n.created_at));
                    if n.state == NotificationState::Held {
                        widgets::chip(ui, Tone::Accent, "held");
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if open {
                            if ui.small_button("Dismiss").clicked() {
                                app.actions
                                    .push(Action::Request(Request::SetNotificationState {
                                        project,
                                        id: n.id,
                                        state: NotificationState::Dismissed,
                                    }));
                            }
                            if ui.small_button("Handled").clicked() {
                                app.actions
                                    .push(Action::Request(Request::SetNotificationState {
                                        project,
                                        id: n.id,
                                        state: NotificationState::Handled,
                                    }));
                            }
                        }
                        if let Some(e) = &item.event {
                            if let Some(tid) = e.kind.ticket_id() {
                                if ui.small_button("Open ticket").clicked() {
                                    app.actions.push(Action::SelectTicket(Some(tid)));
                                    app.actions
                                        .push(Action::SwitchView(crate::app::View::Board));
                                }
                            }
                            if let Some(tid) = e.kind.thread_id() {
                                if ui.small_button("Open thread").clicked() {
                                    app.actions.push(Action::SelectThread(Some(tid)));
                                }
                            }
                        }
                    });
                });
                match &item.event {
                    Some(e) => {
                        ui.label(describe_event(e, &app.board));
                    }
                    None => {
                        ui.weak("(event no longer available)");
                    }
                }
            });
            ui.add_space(8.0);
        }
    });
}
