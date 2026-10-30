//! The team analyst view: the digest of a window, the button that runs the
//! analyst over it, and every analysis with its recommendations to apply or
//! dismiss one by one.

use std::time::Instant;

use ak_core::ids::{CallId, ProjectId};
use ak_core::utility::UtilityState;
use ak_runtime::analyst::{Confidence, Decision, Digest, Target};

use crate::app::{Action, App};
use crate::backend::{AnalysisRecord, Request};
use crate::model::format_time;
use crate::theme::{self, Tone};
use crate::widgets;

const WINDOWS: [(u32, &str); 3] = [
    (24, "Last 24 hours"),
    (24 * 7, "Last 7 days"),
    (24 * 30, "Last 30 days"),
];

pub struct AnalystState {
    /// The window the digest covers, in hours.
    pub hours: u32,
    /// What the human wants the analyst to focus on, sent with the call.
    pub notes: String,
    /// The digest and its rendering as the analyst reads it.
    pub digest: Option<(Digest, String)>,
    pub loading: bool,
    pub running: Option<Instant>,
    pub failure: Option<String>,
    /// Newest first.
    pub analyses: Vec<AnalysisRecord>,
    pub show_text: bool,
}

impl Default for AnalystState {
    fn default() -> Self {
        Self {
            hours: 24 * 7,
            notes: String::new(),
            digest: None,
            loading: false,
            running: None,
            failure: None,
            analyses: Vec::new(),
            show_text: false,
        }
    }
}

impl AnalystState {
    /// Forgets a project's data; the window and notes stay.
    pub fn reset(&mut self) {
        self.digest = None;
        self.loading = false;
        self.running = None;
        self.failure = None;
        self.analyses.clear();
    }

    /// The requests that load the digest and the analyses of the window.
    pub fn load_requests(&mut self, project: ProjectId) -> [Request; 2] {
        self.loading = true;
        [
            Request::LoadDigest {
                project,
                hours: self.hours,
            },
            Request::LoadAnalyses { project },
        ]
    }
}

fn window_label(hours: u32) -> String {
    WINDOWS
        .iter()
        .find(|(h, _)| *h == hours)
        .map(|(_, l)| l.to_string())
        .unwrap_or_else(|| format!("Last {hours} hours"))
}

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else { return };

    ui.horizontal(|ui| {
        ui.heading("Team analyst");
        ui.weak("reads what the team did and proposes changes to definitions, guardrails and profiles. Nothing is applied until you say so.");
    });
    ui.add_space(8.0);

    // Controls.
    let mut reload = false;
    let runner_description = app.runner_description.clone();
    ui.horizontal(|ui| {
        ui.label("Window");
        egui::ComboBox::from_id_salt("analyst_window")
            .selected_text(window_label(app.analyst.hours))
            .show_ui(ui, |ui| {
                for (h, label) in WINDOWS {
                    if ui
                        .selectable_value(&mut app.analyst.hours, h, label)
                        .changed()
                    {
                        reload = true;
                    }
                }
            });
        if ui.small_button("Refresh").clicked() {
            reload = true;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let busy = app.analyst.running.is_some();
            if widgets::filled_button_if(ui, !busy, "Run analysis", Tone::Accent)
                .on_hover_text(format!(
                    "{runner_description}\nOne tool-less call that reads the digest and every definition. Its cost is recorded with the other utility calls and counts against the daily budget."
                ))
                .clicked()
            {
                app.analyst.running = Some(Instant::now());
                app.analyst.failure = None;
                app.actions.push(Action::Request(Request::Analyse {
                    project,
                    hours: app.analyst.hours,
                    notes: app.analyst.notes.clone(),
                }));
            }
        });
    });
    ui.horizontal(|ui| {
        ui.label("Notes");
        ui.add(
            egui::TextEdit::singleline(&mut app.analyst.notes)
                .hint_text("Optional: what to focus on, or what you already know")
                .desired_width(f32::INFINITY),
        );
    });
    if reload {
        for r in app.analyst.load_requests(project) {
            app.actions.push(Action::Request(r));
        }
    }

    if let Some(since) = app.analyst.running {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(format!("Analysing… {}s", since.elapsed().as_secs()));
        });
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(500));
    }
    if let Some(error) = app.analyst.failure.clone() {
        ui.add_space(6.0);
        let notice = theme::tinted(ui.style(), Tone::Red);
        theme::glass(ui, notice, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(theme::tone(ui.style(), Tone::Red), "Analysis failed:");
                ui.label(error);
            });
        });
    }
    ui.add_space(8.0);
    ui.separator();

    let mut decisions: Vec<(CallId, usize, bool)> = Vec::new();
    egui::ScrollArea::vertical()
        .id_salt("analyst_scroll")
        .show(ui, |ui| {
            let App {
                analyst, actions, ..
            } = app;
            draw_digest(ui, analyst);
            ui.add_space(12.0);
            draw_analyses(ui, analyst, &mut decisions);
            let _ = actions;
        });
    for (call_id, index, apply) in decisions {
        app.actions
            .push(Action::Request(Request::DecideRecommendation {
                project,
                call_id,
                index,
                apply,
            }));
    }
}

fn draw_digest(ui: &mut egui::Ui, state: &mut AnalystState) {
    widgets::caption(ui, "What the analyst sees");
    ui.add_space(4.0);
    let Some((d, text)) = state.digest.as_ref() else {
        if state.loading {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak("Reading the window…");
            });
        } else {
            ui.weak("No digest yet.");
        }
        return;
    };

    let trips = d.trips.len();
    let skips: u32 = d.agents.iter().map(|a| a.skips.values().sum::<u32>()).sum();
    let idle: u32 = d.agents.iter().map(|a| a.idle_runs).sum();
    ui.horizontal_wrapped(|ui| {
        stat(ui, "runs", d.total_runs().to_string());
        stat(ui, "idle runs", idle.to_string());
        stat(ui, "cost", format!("${:.2}", d.total_cost()));
        stat(ui, "guardrail trips", trips.to_string());
        stat(ui, "runs not started", skips.to_string());
        stat(ui, "frozen tickets", d.frozen_tickets.len().to_string());
    });
    ui.add_space(8.0);

    if d.agents.is_empty() {
        ui.weak("No agents on this project.");
    } else {
        let card = theme::card(ui.style());
        theme::glass(ui, card, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::horizontal()
                .id_salt("analyst_table_scroll")
                .show(ui, |ui| {
                    egui::Grid::new("analyst_table")
                        .striped(true)
                        .spacing([14.0, 6.0])
                        .show(ui, |ui| {
                            for h in [
                                "agent",
                                "runs",
                                "completed",
                                "idle",
                                "turns",
                                "cost",
                                "tool calls (board)",
                                "tool errors",
                                "commits",
                                "merges",
                                "moves",
                                "comments",
                                "messages",
                                "approvals (denied)",
                                "trips",
                                "not started",
                            ] {
                                ui.label(egui::RichText::new(h).small().strong());
                            }
                            ui.end_row();
                            for a in &d.agents {
                                let completed = a.exits.get("Completed").copied().unwrap_or(0);
                                let skips: u32 = a.skips.values().sum();
                                ui.label(egui::RichText::new(format!("@{}", a.handle)).strong())
                                    .on_hover_text(format!(
                                        "{} · {} · {}",
                                        a.model.as_deref().unwrap_or("default model"),
                                        a.profile,
                                        a.state
                                    ));
                                ui.label(a.runs.to_string());
                                ui.label(completed.to_string());
                                ui.label(a.idle_runs.to_string());
                                ui.label(a.turns.to_string());
                                ui.label(format!("${:.2}", a.cost_usd));
                                ui.label(format!("{} ({})", a.tool_calls, a.board_calls));
                                ui.label(a.tool_errors.to_string());
                                ui.label(a.commits.to_string());
                                ui.label(a.merges.to_string());
                                ui.label(a.ticket_moves.to_string());
                                ui.label(a.comments.to_string());
                                ui.label(a.messages.to_string());
                                ui.label(format!("{} ({})", a.approvals, a.approvals_denied));
                                ui.label(a.trips.to_string());
                                ui.label(skips.to_string());
                                ui.end_row();
                            }
                        });
                });
        });
    }
    ui.add_space(6.0);
    ui.checkbox(&mut state.show_text, "Show the digest as text")
        .on_hover_text("The markdown the analyst reads, with the per-agent detail, trips, skips, thresholds and profiles.");
    if state.show_text {
        let frame = theme::card(ui.style());
        theme::glass(ui, frame, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical()
                .id_salt("analyst_text")
                .max_height(360.0)
                .show(ui, |ui| {
                    ui.add(
                        egui::Label::new(egui::RichText::new(text).monospace().small())
                            .wrap()
                            .selectable(true),
                    );
                });
        });
    }
}

fn draw_analyses(
    ui: &mut egui::Ui,
    state: &AnalystState,
    decisions: &mut Vec<(CallId, usize, bool)>,
) {
    widgets::caption(ui, &format!("Analyses ({})", state.analyses.len()));
    ui.add_space(4.0);
    if state.analyses.is_empty() {
        ui.weak("None yet. Run one from the button above, or set the project to run the analyst after every so many runs in Settings.");
        return;
    }
    for (n, rec) in state.analyses.iter().enumerate() {
        let call = &rec.call;
        let state_label = match call.state {
            UtilityState::Running => "running",
            UtilityState::Proposed => "awaiting your decisions",
            UtilityState::Accepted => "done, something applied",
            UtilityState::Rejected => "done, nothing applied",
            UtilityState::Failed => "failed",
        };
        let title = format!(
            "{}  ·  {}{}{}",
            format_time(&call.started_at),
            state_label,
            call.cost_usd
                .map(|c| format!("  ·  ${c:.3}"))
                .unwrap_or_default(),
            call.model
                .as_deref()
                .map(|m| format!("  ·  {m}"))
                .unwrap_or_default()
        );
        egui::CollapsingHeader::new(egui::RichText::new(title).strong())
            .id_salt(("analysis", call.id))
            .default_open(n == 0)
            .show(ui, |ui| {
                if let Some(error) = &call.error {
                    ui.colored_label(theme::tone(ui.style(), Tone::Red), error);
                }
                if !rec.analysis.summary.is_empty() {
                    ui.add(egui::Label::new(&rec.analysis.summary).wrap());
                }
                for w in &call.warnings {
                    ui.colored_label(theme::tone(ui.style(), Tone::Yellow), format!("Note: {w}"));
                }
                if rec.analysis.recommendations.is_empty() && call.error.is_none() {
                    ui.weak("No recommendations.");
                }
                ui.add_space(6.0);
                for (i, r) in rec.analysis.recommendations.iter().enumerate() {
                    let tone = match r.decision {
                        Some(Decision::Applied) => Tone::Green,
                        Some(Decision::Dismissed) => Tone::Orange,
                        None => Tone::Accent,
                    };
                    let card = theme::tinted(ui.style(), tone);
                    theme::glass(ui, card, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(egui::RichText::new(&r.title).strong());
                            let ctone = match r.confidence {
                                Confidence::High => Tone::Green,
                                Confidence::Medium => Tone::Yellow,
                                Confidence::Low => Tone::Orange,
                            };
                            widgets::chip(
                                ui,
                                ctone,
                                format!("{} confidence", r.confidence.label()),
                            );
                            widgets::chip(ui, Tone::Accent, r.target.label());
                        });
                        if !r.evidence.is_empty() {
                            ui.horizontal_wrapped(|ui| {
                                ui.weak("Evidence:");
                                ui.label(&r.evidence);
                            });
                        }
                        if !r.change.is_empty() {
                            ui.add(egui::Label::new(&r.change).wrap());
                        }
                        if !r.expected_effect.is_empty() {
                            ui.weak(format!("Expected: {}", r.expected_effect));
                        }
                        if let Target::AgentSection { section, text, .. } = &r.target {
                            egui::CollapsingHeader::new(format!(
                                "Proposed text for section {section}"
                            ))
                            .id_salt(("rec_text", call.id, i))
                            .show(ui, |ui| {
                                ui.add(
                                    egui::Label::new(egui::RichText::new(text).monospace().small())
                                        .wrap(),
                                );
                            });
                        }
                        ui.horizontal(|ui| match r.decision {
                            Some(Decision::Applied) => widgets::chip(ui, Tone::Green, "applied"),
                            Some(Decision::Dismissed) => {
                                widgets::chip(ui, Tone::Orange, "dismissed")
                            }
                            None => {
                                let (label, hint) = if r.target.is_human() {
                                    ("Mark done", "Records that you handled this yourself.")
                                } else {
                                    ("Apply", "Makes the change now and records it.")
                                };
                                if widgets::tinted_button(ui, label, Tone::Green)
                                    .on_hover_text(hint)
                                    .clicked()
                                {
                                    decisions.push((call.id, i, true));
                                }
                                if widgets::tinted_button(ui, "Dismiss", Tone::Red)
                                    .on_hover_text(
                                        "Records that you chose not to; the next analysis knows.",
                                    )
                                    .clicked()
                                {
                                    decisions.push((call.id, i, false));
                                }
                            }
                        });
                    });
                    ui.add_space(6.0);
                }
            });
        ui.add_space(4.0);
    }
}

fn stat(ui: &mut egui::Ui, label: &str, value: String) {
    let frame = theme::card(ui.style());
    theme::glass(ui, frame, |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(value).strong().size(18.0));
            ui.weak(label);
        });
    });
}
