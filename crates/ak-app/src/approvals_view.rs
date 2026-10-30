//! The approval tray: a panel along the bottom of the window that shows the
//! open project's pending approvals on every view, so the human never has to
//! go to the Inbox to unblock an agent.
//!
//! The oldest request sits at the bottom and newer ones stack above it. The
//! list opens scrolled to the bottom and is as tall as the oldest two cards,
//! so the first request stays where it appeared, the second stacks on top of
//! it, and a third arrives above them out of sight until you scroll up.

use std::collections::HashMap;

use ak_core::approval::Approval;
use ak_core::ids::{ApprovalId, ProjectId};

use crate::app::{Action, App};
use crate::backend::Request;
use crate::model::format_time;
use crate::theme::{self, Tone};
use crate::widgets;

/// How many cards the tray shows before it scrolls.
const VISIBLE: usize = 2;
/// Space between cards, and between the header row and the cards.
const GAP: f32 = 8.0;
/// A card's height before it has been drawn once.
const CARD_GUESS: f32 = 96.0;
/// The header row's height before it has been drawn once.
const HEADER_GUESS: f32 = 24.0;

#[derive(Default)]
pub struct ApprovalTray {
    /// Folded down to the header row, which keeps the count in view.
    pub collapsed: bool,
    /// Each card's drawn height from the last frame. A bottom panel takes its
    /// height from its content while a scroll area takes its height from the
    /// panel, so the tray is given an exact height worked out from these.
    heights: HashMap<ApprovalId, f32>,
    header_height: Option<f32>,
    /// The panel's height this frame, or zero when it is not shown, so
    /// overlays such as the toast can keep clear of it.
    shown_height: f32,
}

impl ApprovalTray {
    /// How much of the window's bottom the tray takes, frame included.
    pub fn shown_height(&self) -> f32 {
        self.shown_height
    }

    /// The height of the cards region: the oldest two cards and the gaps.
    fn cards_height(&self, approvals: &[Approval]) -> f32 {
        let shown = approvals.len().min(VISIBLE);
        approvals
            .iter()
            .take(VISIBLE)
            .map(|a| self.heights.get(&a.id).copied().unwrap_or(CARD_GUESS))
            .sum::<f32>()
            + GAP * shown.saturating_sub(1) as f32
    }

    /// The whole panel's height, frame included.
    fn panel_height(&self, frame: &egui::Frame, approvals: &[Approval]) -> f32 {
        let margins = frame.total_margin().sum().y;
        let header = self.header_height.unwrap_or(HEADER_GUESS);
        let cards = if self.collapsed {
            0.0
        } else {
            GAP + self.cards_height(approvals)
        };
        margins + header + cards
    }
}

/// Draws the tray as a bottom panel. Call it after the sidebar and before the
/// central panel so it spans the content area.
pub fn draw(app: &mut App, ctx: &egui::Context) {
    app.tray.shown_height = 0.0;
    let Some(project) = app.current else { return };
    if app.approvals.is_empty() {
        app.tray.heights.clear();
        return;
    }
    let frame = theme::panel(&ctx.style());
    let height = app.tray.panel_height(&frame, &app.approvals);
    app.tray.shown_height = height;
    egui::TopBottomPanel::bottom("approval_tray")
        .resizable(false)
        .show_separator_line(false)
        .exact_height(height)
        .frame(frame)
        .show(ctx, |ui| draw_inner(app, ui, project));
}

fn draw_inner(app: &mut App, ui: &mut egui::Ui, project: ProjectId) {
    // Oldest first, as the backend sends them.
    let approvals = app.approvals.clone();
    app.tray
        .heights
        .retain(|id, _| approvals.iter().any(|a| a.id == *id));

    let header = ui.horizontal(|ui| {
        let (glyph, hint) = if app.tray.collapsed {
            ("▸", "Show the requests")
        } else {
            ("▾", "Hide the requests; the count stays")
        };
        if ui.small_button(glyph).on_hover_text(hint).clicked() {
            app.tray.collapsed = !app.tray.collapsed;
        }
        ui.label(egui::RichText::new("Approvals").strong());
        widgets::chip(ui, Tone::Yellow, format!("{} waiting", approvals.len()));
        ui.weak("An agent wants to do something its profile does not cover. It is paused until you answer.");
    });
    let header_height = header.response.rect.height();
    if app.tray.header_height != Some(header_height) {
        app.tray.header_height = Some(header_height);
        ui.ctx().request_repaint();
    }
    if app.tray.collapsed {
        return;
    }
    ui.add_space(GAP);

    // The panel is sized so that this fills exactly the oldest two cards.
    egui::ScrollArea::vertical()
        .id_salt("approval_tray_scroll")
        .auto_shrink([false, false])
        .stick_to_bottom(true)
        .show(ui, |ui| {
            // Newest on top, oldest at the bottom.
            for (i, a) in approvals.iter().rev().enumerate() {
                if i > 0 {
                    ui.add_space(GAP);
                }
                let height = card(app, ui, project, a);
                if app.tray.heights.insert(a.id, height) != Some(height) {
                    // The panel's exact height catches up next frame.
                    ui.ctx().request_repaint();
                }
            }
        });
}

/// One request: who, when, what, Approve and Deny, and the full input on
/// demand. Returns the card's drawn height.
fn card(app: &mut App, ui: &mut egui::Ui, project: ProjectId, a: &Approval) -> f32 {
    let base = theme::tinted(ui.style(), Tone::Orange);
    let out = theme::glass(ui, base, |ui| {
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
                ui.add(egui::Label::new(egui::RichText::new(pretty).monospace().small()).wrap());
            });
    });
    out.response.rect.height()
}
