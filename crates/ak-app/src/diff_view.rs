//! A line diff between two texts, for reviewing proposals.

use similar::{ChangeTag, TextDiff};

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
        ui.colored_label(
            egui::Color32::from_rgb(120, 220, 130),
            format!("+{insertions}"),
        );
        ui.colored_label(
            egui::Color32::from_rgb(240, 120, 120),
            format!("-{deletions}"),
        );
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
                let (sign, color, fill) = match change.tag() {
                    ChangeTag::Insert => (
                        "+",
                        egui::Color32::from_rgb(120, 220, 130),
                        egui::Color32::from_rgb(22, 58, 32),
                    ),
                    ChangeTag::Delete => (
                        "-",
                        egui::Color32::from_rgb(240, 120, 120),
                        egui::Color32::from_rgb(70, 26, 26),
                    ),
                    ChangeTag::Equal => (
                        " ",
                        ui.visuals().weak_text_color(),
                        egui::Color32::TRANSPARENT,
                    ),
                };
                egui::Frame::new()
                    .fill(fill)
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
