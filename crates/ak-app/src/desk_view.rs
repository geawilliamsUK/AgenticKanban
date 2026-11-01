//! The Desk: the human's own Claude Code conversation about the project,
//! for planning and steering the team. Sessions down the side; the
//! transcript, streamed as it is written, in the middle; a compose box that
//! sends on Enter and completes slash commands.
//!
//! `/standup`, `/clear` and `/model` are the app's: a standup that ends
//! with @lead briefed, a new session, and the session's model. Every other command goes to Claude Code as typed, which is how
//! `/compact`, `/context`, `/usage`, skills and custom commands work.

use std::collections::HashMap;

use ak_core::desk::{DeskEntry, DeskEntryKind, DeskSession};
use ak_core::ids::{DeskSessionId, ProjectId};
use ak_supervisor::{DeskNotice, DeskStatus};

use crate::app::{Action, App};
use crate::backend::Request;
use crate::model::format_time;
use crate::theme::{self, Tone};
use crate::{usage_view, widgets};

/// Commands the app answers itself, with what they do.
const OWN_COMMANDS: [(&str, &str); 3] = [
    (
        "standup",
        "catch up, then brief @lead: /standup ship auth first",
    ),
    ("clear", "start a new session"),
    ("model", "set this session's model: /model opus"),
];
/// Claude Code commands worth offering even before a process reports its list.
const KNOWN_COMMANDS: [&str; 3] = ["compact", "context", "usage"];

#[derive(Default)]
pub struct DeskState {
    /// The project these sessions belong to; a different one reloads.
    pub project: Option<ProjectId>,
    pub sessions: Vec<DeskSession>,
    pub selected: Option<DeskSessionId>,
    pub entries: Vec<DeskEntry>,
    /// The reply being written, until its entry lands.
    pub streaming: String,
    pub status: HashMap<DeskSessionId, DeskStatus>,
    /// The slash commands a process last reported, kept for completion.
    pub commands: Vec<String>,
    pub compose: String,
    show_archived: bool,
    renaming: Option<(DeskSessionId, String)>,
    /// A message typed before any session existed, sent once one opens.
    pub pending_send: Option<String>,
    /// The earlier message being edited: sending replaces it and what follows.
    editing: Option<i64>,
    /// A session whose transcript changed under the view and must reload.
    reload: Option<DeskSessionId>,
    /// Team updates that arrived while the Desk was not on screen.
    pub unseen_updates: usize,
}

impl DeskState {
    pub fn on_sessions(&mut self, sessions: Vec<DeskSession>) {
        self.sessions = sessions;
    }

    pub fn on_opened(
        &mut self,
        session: DeskSessionId,
        entries: Vec<DeskEntry>,
        status: DeskStatus,
    ) {
        self.selected = Some(session);
        self.entries = entries;
        self.streaming.clear();
        self.status.entry(session).or_insert(status);
    }

    pub fn on_notice(&mut self, notice: DeskNotice) {
        match notice {
            DeskNotice::Entry { project, entry } => {
                if entry.kind == DeskEntryKind::TeamUpdate
                    && self.project.is_none_or(|p| p == project)
                {
                    self.unseen_updates += 1;
                }
                if Some(project) != self.project || Some(entry.session_id) != self.selected {
                    return;
                }
                if self.entries.last().is_some_and(|e| e.seq >= entry.seq) {
                    return;
                }
                if matches!(
                    entry.kind,
                    DeskEntryKind::Assistant | DeskEntryKind::TurnEnd
                ) {
                    self.streaming.clear();
                }
                self.entries.push(entry);
            }
            DeskNotice::Delta {
                project,
                session,
                text,
            } => {
                if Some(project) == self.project && Some(session) == self.selected {
                    self.streaming.push_str(&text);
                }
            }
            DeskNotice::Status {
                project,
                session,
                status,
            } => {
                if Some(project) != self.project {
                    return;
                }
                if !status.slash_commands.is_empty() {
                    self.commands = status.slash_commands.clone();
                }
                if !status.busy && Some(session) == self.selected {
                    self.streaming.clear();
                }
                self.status.insert(session, status);
            }
            DeskNotice::Reset { project, session } => {
                if Some(project) == self.project && Some(session) == self.selected {
                    self.reload = Some(session);
                }
            }
            DeskNotice::Session { project, session } => {
                if Some(project) != self.project {
                    return;
                }
                match self.sessions.iter_mut().find(|s| s.id == session.id) {
                    Some(s) => *s = session,
                    None => self.sessions.push(session),
                }
                self.sessions
                    .sort_by_key(|s| std::cmp::Reverse(s.last_active_at));
            }
        }
    }

    fn session(&self, id: DeskSessionId) -> Option<&DeskSession> {
        self.sessions.iter().find(|s| s.id == id)
    }
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else {
        return;
    };
    if app.desk.project != Some(project) {
        app.desk = DeskState {
            project: Some(project),
            ..Default::default()
        };
        app.send(Request::LoadDesk { project });
    }
    app.desk.unseen_updates = 0;
    if let Some(session) = app.desk.reload.take() {
        app.desk.entries.clear();
        app.desk.streaming.clear();
        app.send(Request::OpenDeskSession { project, session });
    }
    // Open the most recent session the first time the list arrives.
    if app.desk.selected.is_none() {
        if let Some(first) = app.desk.sessions.iter().find(|s| !s.archived) {
            let session = first.id;
            app.desk.selected = Some(session);
            app.send(Request::OpenDeskSession { project, session });
        }
    }

    egui::SidePanel::left("desk_sessions")
        .resizable(true)
        .default_width(230.0)
        .frame(egui::Frame::NONE.inner_margin(egui::Margin {
            right: 12,
            ..Default::default()
        }))
        .show_inside(ui, |ui| draw_sessions(app, ui, project));

    egui::Frame::NONE
        .inner_margin(egui::Margin {
            left: 16,
            ..Default::default()
        })
        .show(ui, |ui| draw_session(app, ui, project));
}

/// The open session: header, transcript and compose box.
fn draw_session(app: &mut App, ui: &mut egui::Ui, project: ProjectId) {
    let Some(session_id) = app.desk.selected else {
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() / 3.0);
            ui.heading("Desk");
            ui.label("Your own Claude Code session for this project: plan, prioritise, and steer the team.");
            ui.add_space(8.0);
            if widgets::filled_button(ui, "Start a session", Tone::Accent).clicked() {
                app.send(Request::NewDeskSession {
                    project,
                    model: None,
                });
            }
        });
        return;
    };
    let session = app.desk.session(session_id).cloned();
    let status = app
        .desk
        .status
        .get(&session_id)
        .cloned()
        .unwrap_or_default();

    draw_header(app, ui, project, session.as_ref(), &status);
    ui.separator();

    let compose_height = 150.0;
    egui::ScrollArea::vertical()
        .id_salt(("desk_transcript", session_id))
        .max_height((ui.available_height() - compose_height).max(80.0))
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .show(ui, |ui| {
            draw_transcript(app, ui, session.as_ref(), &status);
        });

    ui.separator();
    draw_compose(app, ui, project, session_id, session.as_ref(), &status);
}

fn draw_sessions(app: &mut App, ui: &mut egui::Ui, project: ProjectId) {
    ui.horizontal(|ui| {
        widgets::caption(ui, "Sessions");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("New").clicked() {
                let model = app
                    .desk
                    .selected
                    .and_then(|s| app.desk.session(s))
                    .and_then(|s| s.model.clone());
                app.send(Request::NewDeskSession { project, model });
            }
        });
    });
    ui.add_space(4.0);
    let archived = app.desk.sessions.iter().filter(|s| s.archived).count();
    egui::ScrollArea::vertical()
        .id_salt("desk_session_list")
        .max_height(ui.available_height() - 30.0)
        .show(ui, |ui| {
            let sessions = app.desk.sessions.clone();
            if sessions.is_empty() {
                ui.weak("No sessions yet.");
            }
            let show_archived = app.desk.show_archived;
            for s in sessions.iter().filter(|s| show_archived || !s.archived) {
                let selected = app.desk.selected == Some(s.id);
                let live = app.desk.status.get(&s.id).is_some_and(|st| st.busy);
                let label = if live {
                    format!("● {}", s.title)
                } else {
                    s.title.clone()
                };
                let resp = widgets::nav_item(ui, label, selected).on_hover_text(format!(
                    "Last active {}, ${:.2}{}",
                    format_time(&s.last_active_at),
                    s.cost_usd,
                    if s.archived { ", archived" } else { "" }
                ));
                if resp.clicked() && !selected {
                    app.desk.selected = Some(s.id);
                    app.desk.entries.clear();
                    app.desk.streaming.clear();
                    app.send(Request::OpenDeskSession {
                        project,
                        session: s.id,
                    });
                }
                resp.context_menu(|ui| {
                    if ui.button("Rename…").clicked() {
                        app.desk.renaming = Some((s.id, s.title.clone()));
                        ui.close();
                    }
                    let (label, to) = if s.archived {
                        ("Unarchive", false)
                    } else {
                        ("Archive", true)
                    };
                    if ui.button(label).clicked() {
                        app.send(Request::UpdateDeskSession {
                            project,
                            session: s.id,
                            title: None,
                            model: None,
                            archived: Some(to),
                        });
                        ui.close();
                    }
                });
            }
        });
    if archived > 0 {
        ui.checkbox(
            &mut app.desk.show_archived,
            format!("Show {archived} archived"),
        );
    }
}

fn draw_header(
    app: &mut App,
    ui: &mut egui::Ui,
    project: ProjectId,
    session: Option<&DeskSession>,
    status: &DeskStatus,
) {
    let Some(session) = session else {
        return;
    };
    ui.horizontal(|ui| {
        match app.desk.renaming.as_mut() {
            Some((id, text)) if *id == session.id => {
                let r = ui.add(
                    egui::TextEdit::singleline(text)
                        .id_salt("desk_rename")
                        .desired_width(320.0),
                );
                let done = r.lost_focus() || ui.input(|i| i.key_pressed(egui::Key::Enter));
                r.request_focus();
                if done {
                    let title = text.clone();
                    app.desk.renaming = None;
                    app.send(Request::UpdateDeskSession {
                        project,
                        session: session.id,
                        title: Some(title),
                        model: None,
                        archived: None,
                    });
                }
            }
            _ => {
                // Leave room for the state chip and the model picker.
                let room = (ui.available_width() - 380.0).max(120.0);
                let r = ui
                    .scope(|ui| {
                        ui.set_max_width(room);
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(&session.title).size(18.0).strong(),
                            )
                            .truncate()
                            .sense(egui::Sense::click()),
                        )
                    })
                    .inner
                    .on_hover_text(format!(
                        "{}
Double-click to rename",
                        session.title
                    ));
                if r.double_clicked() {
                    app.desk.renaming = Some((session.id, session.title.clone()));
                }
            }
        }
        let (tone, label) = if status.busy {
            match status.activity.as_deref() {
                Some("compacting") => (Tone::Orange, "Compacting"),
                _ => (Tone::Yellow, "Working"),
            }
        } else if status.alive {
            (Tone::Green, "Ready")
        } else {
            (Tone::Teal, "Resumes on send")
        };
        widgets::chip(ui, tone, label);

        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut model = session.model.clone().unwrap_or_default();
            let shown = if model.is_empty() {
                "Default model".to_string()
            } else {
                model.clone()
            };
            let mut changed = false;
            egui::ComboBox::from_id_salt(("desk_model", session.id))
                .selected_text(shown)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(model.is_empty(), "Default model")
                        .on_hover_text("Claude Code's own default from your settings")
                        .clicked()
                    {
                        model.clear();
                        changed = true;
                    }
                    for alias in widgets::MODEL_ALIASES {
                        if ui.selectable_label(model == alias, alias).clicked() {
                            model = alias.to_string();
                            changed = true;
                        }
                    }
                });
            if changed {
                app.send(Request::UpdateDeskSession {
                    project,
                    session: session.id,
                    title: None,
                    model: Some((!model.is_empty()).then_some(model)),
                    archived: None,
                });
            }
        });
    });

    ui.horizontal(|ui| {
        if let (Some(used), Some(window)) = (status.context_tokens, status.context_window) {
            let fraction = used as f64 / window.max(1) as f64;
            ui.weak("Context");
            ui.scope(|ui| {
                ui.set_width(140.0);
                let tone = if fraction >= 0.8 {
                    Tone::Red
                } else if fraction >= 0.6 {
                    Tone::Orange
                } else {
                    Tone::Accent
                };
                usage_view::bar(ui, fraction, tone);
            })
            .response
            .on_hover_text("Claude Code compacts the conversation by itself near the limit; /compact does it now.");
            ui.weak(format!(
                "{} / {} ({:.0}%)",
                tokens(used),
                tokens(window),
                fraction * 100.0
            ));
            ui.add_space(12.0);
        }
        if session.cost_usd > 0.0 {
            let model = session.model.clone().or_else(|| status.model.clone());
            let shares = usage_view::shares(
                &app.calibration
                    .estimate_total([(model.as_deref(), session.cost_usd)]),
            )
            .map(|s| format!(" {s}"))
            .unwrap_or_default();
            ui.weak(format!("{}{shares} this session", dollars(session.cost_usd)))
                .on_hover_text("API-equivalent cost, and its estimated share of your subscription's windows.");
        }
    });
}

fn draw_transcript(
    app: &mut App,
    ui: &mut egui::Ui,
    session: Option<&DeskSession>,
    status: &DeskStatus,
) {
    let entries = app.desk.entries.clone();
    if entries.is_empty() && app.desk.streaming.is_empty() {
        ui.add_space(12.0);
        ui.weak("Ask anything about the project, or tell the Desk what the team should do next. It acts on the board as you.");
    }
    let results: HashMap<String, &DeskEntry> = entries
        .iter()
        .filter(|e| e.kind == DeskEntryKind::ToolResult)
        .map(|e| (e.text("tool_use_id").to_string(), e))
        .collect();
    let model = session
        .and_then(|s| s.model.clone())
        .or_else(|| status.model.clone());

    for e in &entries {
        match e.kind {
            DeskEntryKind::User => {
                ui.add_space(6.0);
                let frame = theme::card(ui.style())
                    .fill(theme::palette(ui.style()).tone_fill(Tone::Accent));
                theme::glass(ui, frame, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new("You").strong());
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let editing = app.desk.editing == Some(e.seq);
                            if editing {
                                widgets::chip(ui, Tone::Orange, "editing");
                            } else if ui
                                .add_enabled(!status.busy, egui::Button::new("Edit").small())
                                .on_hover_text("Change this message and continue from it. Your later messages and the replies to them are replaced.")
                                .on_disabled_hover_text("Stop the reply in progress to edit.")
                                .clicked()
                            {
                                app.desk.editing = Some(e.seq);
                                app.desk.compose = e.text("text").to_string();
                            }
                        });
                    });
                    ui.add(egui::Label::new(e.text("text")).wrap());
                });
            }
            DeskEntryKind::TeamUpdate => draw_team_update(app, ui, e),
            DeskEntryKind::Assistant => {
                ui.add_space(4.0);
                if e.payload["local_command"].as_bool() == Some(true) {
                    let frame = theme::card(ui.style());
                    theme::glass(ui, frame, |ui| {
                        ui.set_width(ui.available_width());
                        app.markdown.show(ui, e.text("text"));
                    });
                } else {
                    app.markdown.show(ui, e.text("text"));
                }
            }
            DeskEntryKind::ToolCall => {
                let id = e.text("id").to_string();
                let name = e.text("name").to_string();
                let input = e.payload.get("input").cloned().unwrap_or_default();
                let summary = ak_core::permissions::summarise("", &input)
                    .trim_start_matches(": ")
                    .to_string();
                let short = name.strip_prefix("mcp__board__").unwrap_or(&name);
                let result = results.get(&id);
                // A coloured dot, because the UI font has no tick or cross.
                let tone = match result {
                    None => Tone::Yellow,
                    Some(r) if r.payload["is_error"].as_bool() == Some(true) => Tone::Red,
                    Some(_) => Tone::Green,
                };
                let mut title: String = format!("{short} {summary}");
                if title.chars().count() > 110 {
                    title = title.chars().take(107).collect::<String>() + "...";
                }
                let mut job = egui::text::LayoutJob::default();
                let mono = egui::TextStyle::Monospace.resolve(ui.style());
                job.append(
                    "● ",
                    0.0,
                    egui::TextFormat::simple(mono.clone(), theme::tone(ui.style(), tone)),
                );
                job.append(
                    &title,
                    0.0,
                    egui::TextFormat::simple(mono, ui.visuals().weak_text_color()),
                );
                egui::CollapsingHeader::new(job)
                    .id_salt(("desk_tool", e.seq))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(
                                    serde_json::to_string_pretty(&input).unwrap_or_default(),
                                )
                                .monospace(),
                            )
                            .wrap(),
                        );
                        if let Some(r) = result {
                            ui.separator();
                            let color = if r.payload["is_error"].as_bool() == Some(true) {
                                theme::tone(ui.style(), Tone::Red)
                            } else {
                                ui.visuals().weak_text_color()
                            };
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(r.text("content"))
                                        .monospace()
                                        .color(color),
                                )
                                .wrap(),
                            );
                        }
                    });
            }
            DeskEntryKind::ToolResult => {}
            DeskEntryKind::TurnEnd => {
                let p = &e.payload;
                if p["interrupted"].as_bool() == Some(true) {
                    ui.weak("Stopped.");
                } else if let Some(err) = p["error"].as_str() {
                    ui.colored_label(theme::tone(ui.style(), Tone::Red), err);
                } else if let Some(cost) = p["cost_usd"].as_f64().filter(|c| *c > 0.0) {
                    let shares = usage_view::shares(
                        &app.calibration.estimate_total([(model.as_deref(), cost)]),
                    )
                    .map(|s| format!(" · {s}"))
                    .unwrap_or_default();
                    ui.weak(egui::RichText::new(format!("${cost:.3}{shares}")).small());
                }
                ui.add_space(4.0);
            }
            DeskEntryKind::Compacted => {
                let p = &e.payload;
                let n = |k: &str| p[k].as_u64().map(tokens).unwrap_or_else(|| "?".into());
                ui.add_space(6.0);
                ui.vertical_centered(|ui| {
                    ui.weak(format!(
                        "── Conversation compacted: {} → {} tokens ──",
                        n("pre_tokens"),
                        n("post_tokens")
                    ));
                });
                ui.add_space(6.0);
            }
            DeskEntryKind::Notice => {
                ui.weak(egui::RichText::new(e.text("message")).italics());
            }
        }
    }

    if !app.desk.streaming.is_empty() {
        ui.add_space(4.0);
        let text = app.desk.streaming.clone();
        app.markdown.show(ui, &text);
    }
    if status.busy {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(100));
        ui.horizontal(|ui| {
            ui.spinner();
            ui.weak(match status.activity.as_deref() {
                Some("compacting") => "Compacting the conversation…",
                _ if !status.alive => "Starting Claude Code…",
                _ => "Thinking…",
            });
        });
    }
}

/// What the team did, as the app posted it: questions breathe, the rest is
/// a quiet list. Each line opens the ticket or thread it came from.
fn draw_team_update(app: &mut App, ui: &mut egui::Ui, e: &DeskEntry) {
    let items = e.payload["items"].as_array().cloned().unwrap_or_default();
    let idle = e.payload["idle"].as_bool() == Some(true);
    let questions = items
        .iter()
        .filter(|i| i["needs_you"].as_bool() == Some(true))
        .count();
    ui.add_space(6.0);
    let frame = if questions > 0 {
        widgets::breathing_card(ui, Tone::Orange)
    } else {
        theme::tinted(ui.style(), Tone::Teal)
    };
    theme::glass(ui, frame, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let title = match (idle, questions) {
                (true, 0) => "The team is idle".to_string(),
                (true, n) => format!("The team is idle, waiting on you for {n} question(s)"),
                (false, 0) => "Team update".to_string(),
                (false, n) => format!("Team update: {n} question(s) for you"),
            };
            ui.label(egui::RichText::new(title).strong());
            ui.weak(format_time(&e.at));
        });
        for i in &items {
            let s = |k: &str| i[k].as_str().unwrap_or("").to_string();
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                if i["needs_you"].as_bool() == Some(true) {
                    widgets::chip(ui, Tone::Orange, "needs you");
                }
                ui.label(egui::RichText::new(format!("@{}", s("who"))).strong());
                ui.weak(s("subject"));
                if let Some(t) = i["ticket_id"].as_str().and_then(|t| t.parse().ok()) {
                    if ui.small_button("Open ticket").clicked() {
                        app.actions.push(Action::SelectTicket(Some(t)));
                        app.actions
                            .push(Action::SwitchView(crate::app::View::Board));
                    }
                }
                if let Some(t) = i["thread_id"].as_str().and_then(|t| t.parse().ok()) {
                    if ui.small_button("Open thread").clicked() {
                        app.actions.push(Action::SelectThread(Some(t)));
                    }
                }
            });
            app.markdown.show(ui, &s("text"));
            if i["needs_you"].as_bool() == Some(true) {
                if let Some(project) = app.current {
                    crate::inbox_view::reply_box(
                        app,
                        ui,
                        project,
                        egui::Id::new(("desk_answer", e.seq, &s("text"))),
                        i["ticket_id"].as_str().and_then(|t| t.parse().ok()),
                        i["thread_id"].as_str().and_then(|t| t.parse().ok()),
                    );
                }
            }
        }
    });
}

fn draw_compose(
    app: &mut App,
    ui: &mut egui::Ui,
    project: ProjectId,
    session_id: DeskSessionId,
    session: Option<&DeskSession>,
    status: &DeskStatus,
) {
    if app.desk.editing.is_some() {
        let frame = theme::tinted(ui.style(), Tone::Orange);
        theme::glass(ui, frame, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(
                    "Editing an earlier message. Sending replaces it and everything after it.",
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Cancel").clicked() {
                        app.desk.editing = None;
                        app.desk.compose.clear();
                    }
                });
            });
        });
        ui.add_space(4.0);
    }
    let commands = slash_commands(&app.desk.commands);
    let actors = app.board.actors.clone();
    let out = crate::mention_edit::compose(
        ui,
        ("desk_compose", session_id),
        &mut app.desk.compose,
        &actors,
        &commands,
        "Message the Desk. Enter sends, Shift+Enter for a new line, / for commands, @ to mention.",
        3,
        true,
    );
    let (resp, enter) = (out.response, out.submitted);

    ui.horizontal(|ui| {
        let can_send = !app.desk.compose.trim().is_empty();
        let clicked = widgets::filled_button_if(ui, can_send, "Send", Tone::Accent).clicked();
        if can_send && (clicked || enter) {
            let text = std::mem::take(&mut app.desk.compose).trim().to_string();
            submit(app, project, session_id, session, text);
            resp.request_focus();
        }
        if status.busy && widgets::tinted_button(ui, "Stop", Tone::Red).clicked() {
            app.send(Request::InterruptDesk {
                session: session_id,
            });
        }
        if status.busy {
            ui.weak("Messages sent now wait for the reply in progress.");
        }
    });
}

/// Sends a message, or answers the app's own commands.
fn submit(
    app: &mut App,
    project: ProjectId,
    session_id: DeskSessionId,
    session: Option<&DeskSession>,
    text: String,
) {
    if let Some(seq) = app.desk.editing.take() {
        let (text, shown) = match text.strip_prefix("/standup") {
            Some(notes) => (
                ak_core::desk::standup_prompt(notes, chrono::Local::now().date_naive()),
                Some(text.clone()),
            ),
            None => (text, None),
        };
        app.send(Request::EditDeskMessage {
            project,
            session: session_id,
            seq,
            text,
            shown,
        });
        return;
    }
    let mut words = text.split_whitespace();
    match words.next() {
        Some("/clear") => {
            app.actions.push(Action::Request(Request::NewDeskSession {
                project,
                model: session.and_then(|s| s.model.clone()),
            }));
        }
        Some("/model") => match words.next() {
            None => {
                let current = session
                    .and_then(|s| s.model.clone())
                    .unwrap_or_else(|| "the default".into());
                app.toast(
                    format!("This session uses {current}. /model opus, sonnet, haiku, fable, a full id, or default."),
                    false,
                );
            }
            Some(m) => {
                let model = (m != "default").then(|| m.to_string());
                app.toast(
                    format!(
                        "Model set to {}; it applies from the next message.",
                        model.as_deref().unwrap_or("the default")
                    ),
                    false,
                );
                app.send(Request::UpdateDeskSession {
                    project,
                    session: session_id,
                    title: None,
                    model: Some(model),
                    archived: None,
                });
            }
        },
        Some("/standup") => {
            let notes = text.trim_start_matches("/standup").trim();
            let today = chrono::Local::now().date_naive();
            if session.is_some_and(|s| s.title == DeskSession::UNTITLED) {
                app.send(Request::UpdateDeskSession {
                    project,
                    session: session_id,
                    title: Some(format!("Standup {}", today.format("%Y-%m-%d"))),
                    model: None,
                    archived: None,
                });
            }
            app.send(Request::SendToDesk {
                project,
                session: session_id,
                text: ak_core::desk::standup_prompt(notes, today),
                shown: Some(text),
            });
        }
        _ => app.send(Request::SendToDesk {
            project,
            session: session_id,
            text,
            shown: None,
        }),
    }
}

/// Every command the compose box completes: the app's own first, then
/// Claude Code's, from what the session reported plus a few it always has.
fn slash_commands(reported: &[String]) -> Vec<crate::mention_edit::Command> {
    let mut out: Vec<crate::mention_edit::Command> = OWN_COMMANDS
        .iter()
        .map(|(n, h)| crate::mention_edit::Command {
            name: n.to_string(),
            hint: h.to_string(),
        })
        .collect();
    let mut rest: Vec<String> = KNOWN_COMMANDS.iter().map(|c| c.to_string()).collect();
    rest.extend(reported.iter().cloned());
    rest.sort();
    rest.dedup();
    out.extend(
        rest.into_iter()
            .filter(|c| !OWN_COMMANDS.iter().any(|(n, _)| n == c))
            .map(|name| crate::mention_edit::Command {
                hint: if name.contains(':') {
                    "skill".into()
                } else {
                    "Claude Code".into()
                },
                name,
            }),
    );
    out
}

fn tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// Small costs keep a third decimal, so a cheap session is not "$0.00".
fn dollars(usd: f64) -> String {
    if usd < 1.0 {
        format!("${usd:.3}")
    } else {
        format!("${usd:.2}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_commands_put_the_apps_first_without_duplicates() {
        let reported = vec![
            "review".to_string(),
            "compact".to_string(),
            "engineering:standup".into(),
        ];
        let all = slash_commands(&reported);
        let names: Vec<&str> = all.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(&names[..3], &["standup", "clear", "model"]);
        assert!(names.contains(&"review"));
        assert_eq!(names.iter().filter(|n| **n == "compact").count(), 1);
        let skill = all
            .iter()
            .find(|c| c.name == "engineering:standup")
            .unwrap();
        assert_eq!(skill.hint, "skill");
    }

    #[test]
    fn token_counts_read_naturally() {
        assert_eq!(tokens(950), "950");
        assert_eq!(tokens(42_608), "42.6k");
        assert_eq!(tokens(1_000_000), "1.0M");
        assert_eq!(dollars(0.0042), "$0.004");
        assert_eq!(dollars(12.5), "$12.50");
    }
}
