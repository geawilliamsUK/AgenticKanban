//! The subscription usage meter in the sidebar, and the estimated shares
//! of each window shown beside dollar figures elsewhere.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use ak_core::run::AgentRun;
use ak_core::usage::{Calibration, UsageHolds, Window};
use chrono::{Local, Utc};

use crate::app::App;
use crate::backend::Request;
use crate::theme::{self, Tone};
use crate::widgets;

/// How long the meter shows a refresh as in flight before giving up on it.
const REFRESH_SHOWN_FOR: Duration = Duration::from_secs(45);

pub fn sidebar(app: &mut App, ui: &mut egui::Ui) {
    let refreshing = app
        .usage_refresh_requested
        .is_some_and(|t| t.elapsed() < REFRESH_SHOWN_FOR);
    ui.horizontal(|ui| {
        widgets::caption(ui, "Usage");
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if refreshing {
                ui.add(egui::Spinner::new().size(12.0));
                ui.ctx().request_repaint_after(Duration::from_millis(250));
            } else if ui
                .small_button("Refresh")
                .on_hover_text("Ask Claude Code for the current figures. This calls no model.")
                .clicked()
            {
                app.usage_refresh_requested = Some(Instant::now());
                app.send(Request::RefreshUsage);
            }
        });
    });
    ui.add_space(2.0);

    let Some(snapshot) = app.usage.clone() else {
        ui.weak("No report yet. Figures arrive with the first run or refresh.");
        return;
    };
    let now = Utc::now();
    let holds = app
        .settings
        .as_ref()
        .map(|s| s.usage_holds)
        .unwrap_or_default();
    if let Some(until) = snapshot.rejected_until.filter(|t| *t > now) {
        widgets::chip(
            ui,
            Tone::Red,
            format!("Limit reached until {}", local_time(until)),
        );
    }
    for window in Window::ALL {
        let Some(w) = snapshot.windows.get(&window) else {
            continue;
        };
        let used = w.current(now);
        let hold = hold_pct(&holds, window);
        let tone = if hold > 0 && used * 100.0 >= hold as f64 || used >= 0.9 {
            Tone::Red
        } else if used >= 0.7 {
            Tone::Orange
        } else {
            Tone::Green
        };
        let resp = ui
            .vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(window.label());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(egui::RichText::new(format!("{:.0}%", used * 100.0)).strong());
                        if let Some(t) = w.resets_at.filter(|t| *t > now) {
                            ui.weak(local_time(t));
                        }
                    });
                });
                bar(ui, used, tone);
            })
            .response;
        let hold_line = if hold > 0 {
            format!("New runs hold at {hold}%.")
        } else {
            "No hold is set for this window.".to_string()
        };
        let resets = w
            .resets_at
            .filter(|t| *t > now)
            .map(|t| format!("Resets {}. ", local_time(t)))
            .unwrap_or_default();
        resp.on_hover_text(format!(
            "{resets}{hold_line} Thresholds are in Settings.\n{}",
            rate_line(&app.calibration, window)
        ));
        ui.add_space(4.0);
    }
    let age = (now - snapshot.observed_at).num_minutes();
    ui.weak(if age < 1 {
        "updated just now".to_string()
    } else if age < 120 {
        format!("updated {age} min ago")
    } else {
        format!("updated {}", local_time(snapshot.observed_at))
    });
}

fn hold_pct(holds: &UsageHolds, window: Window) -> u32 {
    match window {
        Window::FiveHour => holds.five_hour_pct,
        Window::Week => holds.week_pct,
        Window::WeekFable => holds.fable_week_pct,
    }
}

/// What the calibration knows about a window, for hover text.
fn rate_line(c: &Calibration, window: Window) -> String {
    match c.overall.get(&window) {
        Some(r) if r.is_usable() => format!(
            "Per-run estimates use {} solo run(s): about {} of this window per $1 of API-equivalent cost.",
            r.runs,
            pct(r.per_usd)
        ),
        Some(r) => format!(
            "Per-run estimates need more solo runs ({} so far, {} of rise seen).",
            r.runs,
            pct(r.rise)
        ),
        None => "Per-run estimates appear once a few runs have finished on their own.".into(),
    }
}

pub fn bar(ui: &mut egui::Ui, fraction: f64, tone: Tone) {
    let (rect, _) =
        ui.allocate_exact_size(egui::vec2(ui.available_width(), 6.0), egui::Sense::hover());
    let palette = theme::palette(ui.style());
    ui.painter().rect_filled(rect, 3.0, palette.tone_fill(tone));
    let mut filled = rect;
    filled.set_width(rect.width() * fraction.clamp(0.0, 1.0) as f32);
    if filled.width() > 0.0 {
        ui.painter().rect_filled(filled, 3.0, palette.tone(tone));
    }
}

/// A reset time in local time: the clock today or tomorrow, else the day too.
fn local_time(t: ak_core::Timestamp) -> String {
    let local = t.with_timezone(&Local);
    if t - Utc::now() < chrono::Duration::hours(20) {
        local.format("%H:%M").to_string()
    } else {
        local.format("%a %H:%M").to_string()
    }
}

/// A share of a window: one decimal below a percent, whole percent above.
pub fn pct(fraction: f64) -> String {
    let p = fraction * 100.0;
    if p > 0.0 && p < 1.0 {
        format!("{p:.1}%")
    } else {
        format!("{p:.0}%")
    }
}

/// "≈2% of 5-hour, 0.4% of week" for estimated shares, or nothing when no
/// rate is known yet. The Fable week shows only when something used it.
pub fn shares(est: &BTreeMap<Window, f64>) -> Option<String> {
    let parts: Vec<String> = Window::ALL
        .into_iter()
        .filter_map(|w| {
            let v = *est.get(&w)?;
            if w == Window::WeekFable && v <= 0.0 {
                return None;
            }
            Some(format!("{} of {}", pct(v), w.label().to_lowercase()))
        })
        .collect();
    (!parts.is_empty()).then(|| format!("≈{}", parts.join(", ")))
}

/// The estimated shares of the given runs.
pub fn run_shares<'a>(
    c: &Calibration,
    runs: impl IntoIterator<Item = &'a AgentRun>,
) -> Option<String> {
    let est = c.estimate_total(runs.into_iter().filter_map(|r| {
        Some((
            r.usage.as_ref().and_then(|u| u.model.as_deref()),
            r.cost_usd?,
        ))
    }));
    shares(&est)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_read_naturally() {
        assert_eq!(pct(0.004), "0.4%");
        assert_eq!(pct(0.0), "0%");
        assert_eq!(pct(0.234), "23%");
        let est = BTreeMap::from([
            (Window::FiveHour, 0.02),
            (Window::Week, 0.004),
            (Window::WeekFable, 0.0),
        ]);
        assert_eq!(shares(&est).as_deref(), Some("≈2% of 5-hour, 0.4% of week"));
        assert_eq!(shares(&BTreeMap::new()), None);
    }
}
