//! The human's inbox: every notification addressed to the human, newest
//! first, with the questions the team is waiting on pinned above the rest.
//! Approvals live in the tray along the bottom of the window.

use ak_core::NotificationState;

use crate::app::{Action, App};
use crate::backend::Request;
use crate::model::{describe_event, format_time, kind_label, InboxItem};
use crate::theme::{self, Tone};
use crate::widgets;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else { return };

    let items = app.inbox.clone();
    let (waiting, rest): (Vec<InboxItem>, Vec<InboxItem>) =
        items.into_iter().partition(|i| i.needs_you);
    // A pinned question can also have reached the human as a plain mention
    // or comment; it shows once, pinned.
    let rest: Vec<InboxItem> = rest
        .into_iter()
        .filter(|i| {
            !waiting.iter().any(|w| {
                w.excerpt.is_some()
                    && w.excerpt == i.excerpt
                    && w.event.as_ref().map(|e| e.actor_id) == i.event.as_ref().map(|e| e.actor_id)
            })
        })
        .collect();
    ui.horizontal(|ui| {
        ui.heading("Inbox");
        ui.weak(format!(
            "{} open",
            app.inbox
                .iter()
                .filter(|i| i.notification.state.is_open())
                .count()
        ));
        if !waiting.is_empty() {
            widgets::chip(ui, Tone::Orange, format!("{} need you", waiting.len()));
        }
    });
    ui.add_space(8.0);

    if waiting.is_empty() && rest.is_empty() {
        ui.weak("Nothing here. You will be notified when someone else mentions you, assigns you, replies to you, or moves a ticket you watch.");
        ui.weak("Your own actions never notify you.");
        return;
    }

    let scroll = egui::ScrollArea::vertical().id_salt("inbox_scroll");
    scroll.show(ui, |ui| {
        if !waiting.is_empty() {
            widgets::caption(ui, "Needs you");
            ui.weak(
                "Questions the team is waiting on. Answering where they were asked clears them.",
            );
            ui.add_space(6.0);
            for item in &waiting {
                let frame = widgets::breathing_card(ui, Tone::Orange);
                card(app, ui, project, item, frame);
                ui.add_space(8.0);
            }
            ui.add_space(6.0);
            widgets::caption(ui, "Everything else");
            ui.add_space(6.0);
        }
        for item in &rest {
            // A read item sinks into the inset fill, so open items stand out.
            let frame = if item.notification.state.is_open() {
                theme::card(ui.style())
            } else {
                theme::card(ui.style()).fill(theme::palette(ui.style()).inset)
            };
            card(app, ui, project, item, frame);
            ui.add_space(8.0);
        }
    });
}

/// Answers a question where it was asked: a comment on its ticket, or a
/// post in its thread. Answering there clears it from Needs you.
pub fn reply_box(
    app: &mut App,
    ui: &mut egui::Ui,
    project: ak_core::ids::ProjectId,
    id: egui::Id,
    ticket: Option<ak_core::ids::TicketId>,
    thread: Option<ak_core::ids::ThreadId>,
) {
    if ticket.is_none() && thread.is_none() {
        return;
    }
    let mut text: String = ui.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    ui.add_space(4.0);
    let actors = app.board.actors.clone();
    let out = crate::mention_edit::compose(
        ui,
        id.with("edit"),
        &mut text,
        &actors,
        &[],
        "Answer here. Enter sends, Shift+Enter for a new line.",
        2,
        true,
    );
    let mut send = out.submitted;
    ui.horizontal(|ui| {
        send |= widgets::filled_button_if(ui, !text.trim().is_empty(), "Answer", Tone::Accent)
            .clicked();
        ui.weak(if ticket.is_some() {
            "posted as a comment on the ticket"
        } else {
            "posted in the thread"
        });
    });
    if send && !text.trim().is_empty() {
        let body = std::mem::take(&mut text).trim().to_string();
        match (ticket, thread) {
            (Some(ticket), _) => app.actions.push(Action::Request(Request::Comment {
                project,
                ticket,
                body,
                parent: None,
            })),
            (None, Some(thread)) => app.actions.push(Action::Request(Request::PostMessage {
                project,
                thread,
                body,
            })),
            (None, None) => {}
        }
        app.toast("Answered", false);
    }
    ui.data_mut(|d| d.insert_temp(id, text));
}

fn card(
    app: &mut App,
    ui: &mut egui::Ui,
    project: ak_core::ids::ProjectId,
    item: &InboxItem,
    frame: egui::Frame,
) {
    let n = &item.notification;
    let open = n.state.is_open();
    theme::glass(ui, frame, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let label = if item.needs_you {
                "Question for you"
            } else {
                kind_label(n.kind)
            };
            let kind = egui::RichText::new(label).strong();
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
                    let done = if item.needs_you {
                        "Answered"
                    } else {
                        "Handled"
                    };
                    if ui.small_button(done).clicked() {
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
        if let Some(text) = &item.excerpt {
            ui.add_space(2.0);
            if item.needs_you {
                app.markdown.show(ui, text);
                if let Some(e) = &item.event {
                    reply_box(
                        app,
                        ui,
                        project,
                        egui::Id::new(("answer", n.id)),
                        e.kind.ticket_id(),
                        e.kind.thread_id(),
                    );
                }
            } else {
                let short: String = text
                    .replace("**", "")
                    .lines()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join("\n");
                ui.add(
                    egui::Label::new(egui::RichText::new(short).weak())
                        .wrap()
                        .truncate(),
                );
            }
        }
    });
}
