//! Small reusable controls.

/// The aliases Claude Code resolves to the current model of each tier.
pub const MODEL_ALIASES: [&str; 3] = ["opus", "sonnet", "haiku"];

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
                .hint_text("or a full model id")
                .desired_width(200.0),
        );
        if r.changed() {
            changed = true;
        }
    });
    changed
}
