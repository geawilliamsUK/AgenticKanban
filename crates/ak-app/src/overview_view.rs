//! The cross-project view: every project's running agents, approvals
//! waiting, unread inbox, cost today and guardrail trips, in one place.

use ak_core::event::EventKind;

use crate::app::{Action, App, View};
use crate::model::format_time;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let red = egui::Color32::from_rgb(240, 120, 120);
    let green = egui::Color32::from_rgb(120, 220, 130);
    let amber = egui::Color32::from_rgb(230, 190, 80);

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
    ui.add_space(4.0);
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
                egui::Frame::new()
                    .fill(ui.visuals().faint_bg_color)
                    .corner_radius(4.0)
                    .inner_margin(10.0)
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(&p.name).strong().size(16.0));
                            if app.current == Some(p.id) {
                                ui.weak("open");
                            }
                            if p.paused {
                                ui.colored_label(red, "agents paused");
                            }
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
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
                                },
                            );
                        });
                        ui.horizontal_wrapped(|ui| {
                            ui.weak(format!("{} agent(s)", p.agents));
                            if p.halted > 0 {
                                ui.colored_label(red, format!("{} halted", p.halted));
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
                            ui.add_space(4.0);
                            ui.label(egui::RichText::new("Guardrail trips today").strong());
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
                                        ui.colored_label(amber, rule.label());
                                        ui.label(detail);
                                        ui.weak(action.label());
                                    });
                                }
                            }
                        }
                    });
                ui.add_space(6.0);
            }
        });
}

fn stat(ui: &mut egui::Ui, label: &str, value: String) {
    ui.vertical(|ui| {
        ui.label(egui::RichText::new(value).strong().size(20.0));
        ui.weak(label);
    });
    ui.add_space(18.0);
}
