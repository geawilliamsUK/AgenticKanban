//! A line diff between two texts, for reviewing proposals.

use similar::{ChangeTag, TextDiff};

use crate::theme::{self, Tone};
use crate::widgets;

pub fn draw(ui: &mut egui::Ui, id_salt: &str, old: &str, new: &str) {
    let diff = TextDiff::from_lines(old, new);
    let (insertions, deletions) = diff
        .iter_all_changes()
        .fold((0, 0), |(i, d), c| match c.tag() {
            ChangeTag::Insert => (i + 1, d),
            ChangeTag::Delete => (i, d + 1),
            ChangeTag::Equal => (i, d),
        });
    ui.horizontal(|ui| {
        widgets::chip(ui, Tone::Green, format!("+{insertions}"));
        widgets::chip(ui, Tone::Red, format!("-{deletions}"));
        if insertions == 0 && deletions == 0 {
            ui.weak("No changes.");
        }
    });

    egui::ScrollArea::vertical()
        .id_salt(id_salt)
        .max_height(440.0)
        .show(ui, |ui| {
            ui.style_mut().override_text_style = Some(egui::TextStyle::Monospace);
            ui.spacing_mut().item_spacing.y = 0.0;
            for change in diff.iter_all_changes() {
                let line = change.value().trim_end_matches(['\r', '\n']);
                let (sign, tone) = match change.tag() {
                    ChangeTag::Insert => ("+", Some(Tone::Green)),
                    ChangeTag::Delete => ("-", Some(Tone::Red)),
                    ChangeTag::Equal => (" ", None),
                };
                let color = match tone {
                    Some(tone) => theme::tone(ui.style(), tone),
                    None => ui.visuals().weak_text_color(),
                };
                // A run of changed lines reads as one block, so each line is a flat wash.
                let frame = match tone {
                    Some(tone) => theme::tinted(ui.style(), tone),
                    None => egui::Frame::new(),
                };
                frame
                    .stroke(egui::Stroke::NONE)
                    .corner_radius(egui::CornerRadius::ZERO)
                    .inner_margin(egui::Margin::symmetric(4, 1))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(format!("{sign} {line}")).color(color),
                            )
                            .wrap(),
                        );
                    });
            }
        });
}
