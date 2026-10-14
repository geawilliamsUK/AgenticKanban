//! Markdown rendering for descriptions and comments.

use egui_commonmark::{CommonMarkCache, CommonMarkViewer};

#[derive(Default)]
pub struct Markdown {
    cache: CommonMarkCache,
}

impl Markdown {
    pub fn show(&mut self, ui: &mut egui::Ui, text: &str) {
        if text.trim().is_empty() {
            ui.weak("No description.");
            return;
        }
        CommonMarkViewer::new().show(ui, &mut self.cache, text);
    }
}
