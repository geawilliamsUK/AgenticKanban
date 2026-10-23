//! Chats: the chat list, the threads in the selected chat, and the selected
//! thread with its messages and a compose box. The human sees every chat and
//! may post in any of them.

use ak_core::chat::ChatKind;
use ak_core::ids::ActorId;

use crate::app::{Action, App};
use crate::backend::Request;
use crate::model::format_time;
use crate::theme::{self, Tone};
use crate::widgets;

/// Transient state for the Chats view: drafts and toggles.
#[derive(Default)]
pub struct ChatsState {
    pub search: String,
    pub show_archived: bool,
    pub compose: String,
    pub new_direct: bool,
    /// Group name and member checkboxes while the form is open.
    pub new_group: Option<(String, Vec<(ActorId, bool)>)>,
    /// Title and first message while the form is open.
    pub new_thread: Option<(String, String)>,
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else { return };
    let human = app.human.as_ref().map(|h| h.id);
    let actors = app.board.actors.clone();

    egui::SidePanel::left("chat_list")
        .resizable(true)
        .default_width(240.0)
        .show_inside(ui, |ui| {
            ui.heading("Chats");
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .small_button("New group…")
                        .on_hover_text("A named chat with several agents. Posts there wake nobody; @mentions do.")
                        .clicked()
                    {
                        let members = app
                            .board
                            .active_agents()
                            .map(|a| (a.actor_id, true))
                            .collect();
                        app.chats_ui.new_group = Some((String::new(), members));
                        app.chats_ui.new_direct = false;
                    }
                    if ui
                        .small_button("New direct…")
                        .on_hover_text("A private chat with one agent. Your messages there wake it.")
                        .clicked()
                    {
                        app.chats_ui.new_direct = !app.chats_ui.new_direct;
                        app.chats_ui.new_group = None;
                    }
                });
            });

            if app.chats_ui.new_direct {
                ui.weak("Message an agent directly:");
                let agents: Vec<(ActorId, String)> = app
                    .board
                    .active_agents()
                    .map(|a| (a.actor_id, a.handle.clone()))
                    .collect();
                if agents.is_empty() {
                    ui.weak("No agents yet.");
                }
                for (id, handle) in agents {
                    if ui.button(format!("@{handle}")).clicked() {
                        app.actions.push(Action::Request(Request::CreateDirectChat {
                            project,
                            with: id,
                        }));
                        app.chats_ui.new_direct = false;
                    }
                }
                ui.separator();
            }

            let mut submit_group: Option<(String, Vec<ActorId>)> = None;
            let mut cancel_group = false;
            if let Some((name, members)) = app.chats_ui.new_group.as_mut() {
                ui.add(
                    egui::TextEdit::singleline(name)
                        .id_salt("new_group_name")
                        .hint_text("Group name")
                        .desired_width(f32::INFINITY),
                );
                for (id, on) in members.iter_mut() {
                    ui.checkbox(on, app.board.handle(*id));
                }
                ui.horizontal(|ui| {
                    let chosen: Vec<ActorId> = members
                        .iter()
                        .filter(|(_, on)| *on)
                        .map(|(id, _)| *id)
                        .collect();
                    let ok = !name.trim().is_empty() && !chosen.is_empty();
                    if widgets::filled_button_if(ui, ok, "Create", Tone::Accent).clicked() {
                        submit_group = Some((name.clone(), chosen));
                    }
                    if ui.button("Cancel").clicked() {
                        cancel_group = true;
                    }
                });
                ui.separator();
            }
            if let Some((name, members)) = submit_group {
                app.actions.push(Action::Request(Request::CreateGroupChat {
                    project,
                    name,
                    members,
                }));
                app.chats_ui.new_group = None;
            }
            if cancel_group {
                app.chats_ui.new_group = None;
            }

            let search = ui.add(
                egui::TextEdit::singleline(&mut app.chats_ui.search)
                    .id_salt("thread_search")
                    .hint_text("Search threads…")
                    .desired_width(f32::INFINITY),
            );
            if search.changed() {
                if app.chats_ui.search.trim().is_empty() {
                    app.thread_hits.clear();
                } else {
                    app.actions.push(Action::Request(Request::SearchThreads {
                        project,
                        query: app.chats_ui.search.clone(),
                    }));
                }
            }
            if !app.chats_ui.search.trim().is_empty() {
                ui.weak(format!("{} result(s)", app.thread_hits.len()));
                let hits = app.thread_hits.clone();
                for h in &hits {
                    let selected = app.selected_thread == Some(h.thread.id);
                    let label = format!("{}\n{}", h.thread.title, h.snippet);
                    if ui.selectable_label(selected, label).clicked() {
                        app.actions.push(Action::SelectThread(Some(h.thread.id)));
                    }
                }
                ui.separator();
            }

            if app.chats.is_empty() {
                ui.weak("No chats yet. Start a direct chat with an agent, or a group.");
            }
            let chats = app.chats.clone();
            egui::ScrollArea::vertical()
                .id_salt("chat_list_scroll")
                .show(ui, |ui| {
                    for c in &chats {
                        let selected = app.selected_chat == Some(c.chat.id);
                        let prefix = match c.chat.kind {
                            ChatKind::Direct => "",
                            ChatKind::Group => "# ",
                        };
                        let label = format!("{prefix}{}", c.label(&app.board, human));
                        if ui.selectable_label(selected, label).clicked() {
                            app.actions.push(Action::SelectChat(Some(c.chat.id)));
                        }
                    }
                });
        });

    if let Some(chat_id) = app.selected_chat {
        let chat = app.chats.iter().find(|c| c.chat.id == chat_id).cloned();
        egui::SidePanel::left("thread_list")
            .resizable(true)
            .default_width(280.0)
            .show_inside(ui, |ui| {
                let title = chat
                    .as_ref()
                    .map(|c| c.label(&app.board, human))
                    .unwrap_or_default();
                ui.heading(title);
                ui.horizontal(|ui| {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("New thread").clicked() {
                            app.chats_ui.new_thread = Some((String::new(), String::new()));
                        }
                        if ui
                            .checkbox(&mut app.chats_ui.show_archived, "archived")
                            .changed()
                        {
                            app.actions.push(Action::Request(Request::LoadThreads {
                                project,
                                chat: chat_id,
                                include_archived: app.chats_ui.show_archived,
                            }));
                        }
                    });
                });
                if let Some(c) = &chat {
                    if c.chat.kind == ChatKind::Group {
                        let members: Vec<String> =
                            c.members.iter().map(|m| app.board.handle(*m)).collect();
                        ui.weak(format!("Members: {}", members.join(", ")));
                    }
                }

                let mut submit: Option<(String, String)> = None;
                let mut cancel = false;
                if let Some((title, body)) = app.chats_ui.new_thread.as_mut() {
                    ui.add(
                        egui::TextEdit::singleline(title)
                            .id_salt("new_thread_title")
                            .hint_text("Thread title")
                            .desired_width(f32::INFINITY),
                    );
                    crate::mention_edit::multiline(
                        ui,
                        "new_thread_body",
                        body,
                        &actors,
                        "First message. Type @ to mention a teammate.",
                        4,
                    );
                    ui.horizontal(|ui| {
                        let ok = !title.trim().is_empty() && !body.trim().is_empty();
                        if widgets::filled_button_if(ui, ok, "Start thread", Tone::Accent).clicked()
                        {
                            submit = Some((title.clone(), body.clone()));
                        }
                        if ui.button("Cancel").clicked() {
                            cancel = true;
                        }
                    });
                    ui.separator();
                }
                if let Some((title, body)) = submit {
                    app.actions.push(Action::Request(Request::CreateThread {
                        project,
                        chat: chat_id,
                        title,
                        body,
                    }));
                    app.chats_ui.new_thread = None;
                }
                if cancel {
                    app.chats_ui.new_thread = None;
                }

                let threads = app.threads.clone();
                if threads.is_empty() {
                    ui.weak("No threads yet. Every message lives in a thread with a title.");
                }
                egui::ScrollArea::vertical()
                    .id_salt("thread_list_scroll")
                    .show(ui, |ui| {
                        for t in &threads {
                            let mut text = t.title.clone();
                            if t.paused {
                                text.push_str("  (paused)");
                            }
                            let rich = if t.archived {
                                egui::RichText::new(text).weak()
                            } else {
                                egui::RichText::new(text)
                            };
                            let resp = ui.selectable_label(app.selected_thread == Some(t.id), rich);
                            if resp.clicked() {
                                app.actions.push(Action::SelectThread(Some(t.id)));
                            }
                            resp.on_hover_text(format!(
                                "last activity {}{}",
                                format_time(&t.last_activity_at),
                                if t.archived { ", archived" } else { "" }
                            ));
                        }
                    });
            });
    }

    egui::CentralPanel::default().show_inside(ui, |ui| {
        let Some(detail) = app.thread_detail.clone() else {
            ui.centered_and_justified(|ui| {
                ui.weak(if app.selected_chat.is_some() {
                    "Select a thread, or start one."
                } else {
                    "Select a chat."
                });
            });
            return;
        };
        ui.heading(&detail.thread.title);
        ui.horizontal(|ui| {
            if detail.thread.paused {
                widgets::chip(ui, Tone::Red, "paused");
            }
            if detail.thread.archived {
                ui.weak("archived");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let (label, next, hint) = if detail.thread.paused {
                    (
                        "Unpause",
                        false,
                        "Lets agents post again and releases what was held while paused.",
                    )
                } else {
                    (
                        "Pause",
                        true,
                        "Agents cannot post until you unpause. You still can.",
                    )
                };
                if ui.button(label).on_hover_text(hint).clicked() {
                    app.actions.push(Action::Request(Request::SetThreadPaused {
                        project,
                        thread: detail.thread.id,
                        paused: next,
                    }));
                }
            });
        });
        let participants: Vec<String> = detail
            .participants
            .iter()
            .map(|p| app.board.handle(*p))
            .collect();
        ui.weak(format!("Participants: {}", participants.join(", ")));
        ui.separator();

        let compose_height = 120.0;
        egui::ScrollArea::vertical()
            .id_salt(("thread_messages", detail.thread.id))
            .max_height((ui.available_height() - compose_height).max(80.0))
            .stick_to_bottom(true)
            .show(ui, |ui| {
                for m in &detail.messages {
                    let mine = human == Some(m.author_id);
                    let frame = theme::card(ui.style());
                    let frame = if mine {
                        frame.fill(theme::palette(ui.style()).tone_fill(Tone::Accent))
                    } else {
                        frame
                    };
                    theme::glass(ui, frame, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(app.board.handle(m.author_id)).strong());
                            ui.weak(format_time(&m.created_at));
                        });
                        app.markdown.show(ui, &m.body);
                    });
                    ui.add_space(8.0);
                }
            });

        ui.separator();
        let resp = crate::mention_edit::multiline(
            ui,
            ("compose", detail.thread.id),
            &mut app.chats_ui.compose,
            &actors,
            "Write a message. Type @ to mention a teammate. Ctrl+Enter sends.",
            3,
        );
        let shortcut = resp.has_focus()
            && ui.input(|i| i.modifiers.command && i.key_pressed(egui::Key::Enter));
        ui.horizontal(|ui| {
            let can_send = !app.chats_ui.compose.trim().is_empty();
            let clicked = widgets::filled_button_if(ui, can_send, "Send", Tone::Accent).clicked();
            if can_send && (clicked || shortcut) {
                let body = std::mem::take(&mut app.chats_ui.compose);
                app.actions.push(Action::Request(Request::PostMessage {
                    project,
                    thread: detail.thread.id,
                    body,
                }));
            }
            if detail.thread.paused {
                ui.weak("Paused: only you can post.");
            } else if detail.thread.archived {
                ui.weak("Archived: posting reopens the thread.");
            }
        });
    });
}
