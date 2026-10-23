//! The cross-project view: every project's running agents, approvals
//! waiting, unread inbox, cost today and guardrail trips, in one place.

use ak_core::event::EventKind;

use crate::app::{Action, App, View};
use crate::model::format_time;
use crate::theme::{self, Tone};
use crate::widgets;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let green = theme::tone(ui.style(), Tone::Green);

    ui.horizontal(|ui| {
        ui.heading("Overview");
        ui.weak("every project, refreshed every few seconds");
    });
    ui.add_space(8.0);

    let running: usize = app.overview.iter().map(|p| p.running.len()).sum();
    let approvals: usize = app.overview.iter().map(|p| p.approvals).sum();
    let unread: usize = app.overview.iter().map(|p| p.unread).sum();
    let trips: usize = app.overview.iter().map(|p| p.trips.len()).sum();
    let cost: f64 = app.overview.iter().map(|p| p.cost_today).sum();
    let budget = app
        .settings
        .as_ref()
        .map(|s| s.global_daily_budget_usd)
        .unwrap_or(0.0);
    ui.horizontal_wrapped(|ui| {
        stat(ui, "running", running.to_string());
        stat(ui, "approvals waiting", approvals.to_string());
        stat(ui, "unread", unread.to_string());
        stat(ui, "guardrail trips today", trips.to_string());
        stat(ui, "cost today", format!("${cost:.2} of ${budget:.0}"));
    });
    ui.add_space(8.0);
    ui.separator();

    if app.overview.is_empty() {
        ui.weak("No projects yet. Add one from the sidebar.");
        return;
    }
    let projects = app.overview.clone();
    egui::ScrollArea::vertical()
        .id_salt("overview_scroll")
        .show(ui, |ui| {
            for p in &projects {
                let frame = theme::card(ui.style());
                theme::glass(ui, frame, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(egui::RichText::new(&p.name).strong().size(16.0));
                        if app.current == Some(p.id) {
                            ui.weak("open");
                        }
                        if p.paused {
                            widgets::chip(ui, Tone::Red, "agents paused");
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.small_button("Open board").clicked() {
                                app.actions.push(Action::OpenProject(p.id));
                                app.actions.push(Action::SwitchView(View::Board));
                            }
                            if p.approvals > 0
                                && ui
                                    .small_button(format!("{} approval(s)", p.approvals))
                                    .clicked()
                            {
                                app.actions.push(Action::OpenProject(p.id));
                                app.actions.push(Action::SwitchView(View::Inbox));
                            }
                        });
                    });
                    ui.horizontal_wrapped(|ui| {
                        ui.weak(format!("{} agent(s)", p.agents));
                        if p.halted > 0 {
                            widgets::chip(ui, Tone::Red, format!("{} halted", p.halted));
                        }
                        ui.weak(format!("{} unread", p.unread));
                        ui.weak(format!("${:.2} today", p.cost_today));
                    });
                    if p.running.is_empty() {
                        ui.weak("Nothing running.");
                    } else {
                        for (handle, since) in &p.running {
                            ui.horizontal(|ui| {
                                ui.colored_label(green, "●");
                                ui.label(format!("@{handle}"));
                                ui.weak(format!("running since {}", format_time(since)));
                            });
                        }
                    }
                    if !p.trips.is_empty() {
                        ui.add_space(8.0);
                        widgets::caption(ui, "Guardrail trips today");
                        for e in &p.trips {
                            if let EventKind::GuardrailTripped {
                                rule,
                                action,
                                detail,
                                ..
                            } = &e.kind
                            {
                                ui.horizontal_wrapped(|ui| {
                                    ui.weak(format_time(&e.occurred_at));
                                    widgets::chip(ui, Tone::Yellow, rule.label());
                                    ui.label(detail);
                                    ui.weak(action.label());
                                });
                            }
                        }
                    }
                });
                ui.add_space(8.0);
            }
        });
}

fn stat(ui: &mut egui::Ui, label: &str, value: String) {
    let frame = theme::card(ui.style());
    theme::glass(ui, frame, |ui| {
        ui.vertical(|ui| {
            ui.label(egui::RichText::new(value).strong().size(20.0));
            ui.weak(label);
        });
    });
}
