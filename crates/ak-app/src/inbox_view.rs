//! The human's inbox: every notification addressed to them, newest first.

use ak_core::NotificationState;

use crate::app::{Action, App};
use crate::backend::Request;
use crate::model::{describe_event, format_time, kind_label};

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else { return };

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
    ui.add_space(6.0);

    let items = app.inbox.clone();
    if items.is_empty() {
        ui.weak("Nothing here. You will be notified when someone else mentions you, assigns you, replies to you, or moves a ticket you watch.");
        ui.weak(
            "Your own actions never notify you, so until an agent joins the team this stays empty.",
        );
        return;
    }

    egui::ScrollArea::vertical().show(ui, |ui| {
        for item in &items {
            let n = &item.notification;
            let open = n.state.is_open();
            let fill = if open {
                ui.visuals().faint_bg_color
            } else {
                ui.visuals().window_fill
            };
            egui::Frame::new()
                .fill(fill)
                .corner_radius(4.0)
                .inner_margin(8.0)
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let kind = egui::RichText::new(kind_label(n.kind)).strong();
                        ui.label(if open { kind } else { kind.weak() });
                        ui.weak(format_time(&n.created_at));
                        if n.state == NotificationState::Held {
                            ui.colored_label(egui::Color32::from_rgb(90, 160, 230), "held");
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if open {
                                if ui.small_button("Dismiss").clicked() {
                                    app.actions.push(Action::Request(
                                        Request::SetNotificationState {
                                            project,
                                            id: n.id,
                                            state: NotificationState::Dismissed,
                                        },
                                    ));
                                }
                                if ui.small_button("Handled").clicked() {
                                    app.actions.push(Action::Request(
                                        Request::SetNotificationState {
                                            project,
                                            id: n.id,
                                            state: NotificationState::Handled,
                                        },
                                    ));
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
            ui.add_space(4.0);
        }
    });
}
