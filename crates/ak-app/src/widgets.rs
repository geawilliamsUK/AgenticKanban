//! Small reusable controls.

use crate::theme::{self, Tone, CONTROL_RADIUS};

/// The aliases Claude Code resolves to the current model of each tier.
pub const MODEL_ALIASES: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

/// A model choice: a combo of the Claude Code aliases plus a free-text field
/// for a full model id. An empty value means the default named by
/// `default_label`. Returns true when the value changed.
pub fn model_picker(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    value: &mut String,
    default_label: &str,
) -> bool {
    let mut changed = false;
    // The text field keeps its own id, so a layout change above it cannot move
    // the caret or drop focus while someone is typing.
    let text_id = egui::Id::new(("model-text", &id_salt));
    ui.horizontal(|ui| {
        let shown = if value.trim().is_empty() {
            default_label.to_string()
        } else {
            value.clone()
        };
        egui::ComboBox::from_id_salt(id_salt)
            .selected_text(shown)
            .width(220.0)
            .show_ui(ui, |ui| {
                if ui
                    .selectable_label(value.trim().is_empty(), default_label)
                    .clicked()
                {
                    value.clear();
                    changed = true;
                }
                for alias in MODEL_ALIASES {
                    if ui.selectable_label(value == alias, alias).clicked() {
                        *value = alias.to_string();
                        changed = true;
                    }
                }
            });
        let r = ui.add(
            egui::TextEdit::singleline(value)
                .id(text_id)
                .hint_text("or a full model id")
                .desired_width(200.0),
        );
        if r.changed() {
            changed = true;
        }
    });
    changed
}

/// A small capsule label in a tone, for priorities, states and counts.
pub fn chip(ui: &mut egui::Ui, tone: Tone, text: impl Into<String>) {
    let p = theme::palette(ui.style());
    egui::Frame::new()
        .fill(p.tone_fill(tone))
        .corner_radius(255)
        .inner_margin(egui::Margin::symmetric(8, 2))
        .show(ui, |ui| {
            ui.label(egui::RichText::new(text).small().color(p.tone(tone)));
        });
}

/// A solid button in a tone: the primary action of a view or dialog.
pub fn filled_button(ui: &mut egui::Ui, text: impl Into<String>, tone: Tone) -> egui::Response {
    filled_button_if(ui, true, text, tone)
}

/// A solid button that can be disabled. It goes through `ui.add_enabled`, so
/// its id and its disabled look match a plain `add_enabled` call.
pub fn filled_button_if(
    ui: &mut egui::Ui,
    enabled: bool,
    text: impl Into<String>,
    tone: Tone,
) -> egui::Response {
    let p = theme::palette(ui.style());
    ui.add_enabled(
        enabled,
        egui::Button::new(egui::RichText::new(text).color(egui::Color32::WHITE)).fill(p.tone(tone)),
    )
}

/// A button washed with a tone, for a choice that is good or bad, such as
/// Approve or Deny.
pub fn tinted_button(ui: &mut egui::Ui, text: impl Into<String>, tone: Tone) -> egui::Response {
    let p = theme::palette(ui.style());
    ui.add(egui::Button::new(egui::RichText::new(text).color(p.tone(tone))).fill(p.tone_fill(tone)))
}

/// A small grey heading over a group of controls, set in capitals.
pub fn caption(ui: &mut egui::Ui, text: &str) {
    let p = theme::palette(ui.style());
    ui.label(
        egui::RichText::new(text.to_uppercase())
            .small()
            .color(p.text_secondary),
    );
}

/// A full-width row for a source list such as the sidebar. The selected row is
/// tinted with the accent, and the row takes the pointer's hover like a button.
pub fn nav_item(ui: &mut egui::Ui, text: impl Into<String>, selected: bool) -> egui::Response {
    let width = ui.available_width();
    ui.add(
        egui::Button::selectable(selected, (text.into(), egui::Atom::grow()))
            .min_size(egui::vec2(width, 30.0))
            .corner_radius(CONTROL_RADIUS),
    )
}

/// A slow breath from 0 to 1 and back, for things that wait on the human.
/// Keeps the UI repainting while it is shown.
pub fn breath(ui: &egui::Ui) -> f32 {
    const PERIOD: f64 = 2.4;
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_millis(33));
    let t = ui.input(|i| i.time) / PERIOD * std::f64::consts::TAU;
    (0.5 - 0.5 * t.cos()) as f32
}

/// A card frame whose rim and glow breathe in `tone`, for items that wait
/// on the human.
pub fn breathing_card(ui: &egui::Ui, tone: Tone) -> egui::Frame {
    let b = breath(ui);
    let palette = theme::palette(ui.style());
    let rim = palette.tone(tone).gamma_multiply(0.45 + 0.55 * b);
    theme::tinted(ui.style(), tone)
        .stroke(egui::Stroke::new(1.0 + 1.5 * b, rim))
        .shadow(egui::epaint::Shadow {
            offset: [0, 0],
            blur: (6.0 + 14.0 * b) as u8,
            spread: 0,
            color: palette.tone(tone).gamma_multiply(0.25 * b),
        })
}

/// A dot that breathes in `tone` at `center`, for a nav item with something
/// waiting.
pub fn breathing_dot(ui: &egui::Ui, center: egui::Pos2, tone: Tone) {
    let b = breath(ui);
    let color = theme::tone(ui.style(), tone);
    ui.painter()
        .circle_filled(center, 3.0 + 1.5 * b, color.gamma_multiply(0.6 + 0.4 * b));
    ui.painter().circle_stroke(
        center,
        4.5 + 2.5 * b,
        egui::Stroke::new(1.0_f32, color.gamma_multiply(0.5 * (1.0 - b))),
    );
}
