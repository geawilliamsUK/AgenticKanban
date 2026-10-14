//! Text editing with `@mention` completion, and mention highlighting for
//! rendered bodies.

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};

use ak_core::{Actor, ActorKind};

const MAX_SUGGESTIONS: usize = 6;

/// A multiline text edit that offers a completion popup while the text before
/// the cursor is an `@` followed by the start of a handle. Clicking a name
/// inserts it and moves the cursor past it.
pub fn multiline(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    text: &mut String,
    actors: &[Actor],
    hint: &str,
    rows: usize,
) -> egui::Response {
    let output = egui::TextEdit::multiline(text)
        .id_salt(id_salt)
        .desired_width(f32::INFINITY)
        .desired_rows(rows)
        .hint_text(hint)
        .show(ui);
    let response = output.response.clone();
    if !response.has_focus() {
        return response;
    }
    let Some(cursor) = output.cursor_range.map(|r| r.primary.index) else {
        return response;
    };

    let chars: Vec<char> = text.chars().collect();
    let Some((at, query)) = mention_query(&chars, cursor) else {
        return response;
    };
    let matches: Vec<&Actor> = actors
        .iter()
        .filter(|a| a.kind != ActorKind::System && a.handle.starts_with(&query))
        .take(MAX_SUGGESTIONS)
        .collect();
    if matches.is_empty() {
        return response;
    }

    let mut chosen: Option<String> = None;
    egui::Area::new(response.id.with("mention_popup"))
        .order(egui::Order::Foreground)
        .fixed_pos(response.rect.left_bottom() + egui::vec2(0.0, 2.0))
        .show(ui.ctx(), |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.set_min_width(220.0);
                ui.weak("Mention");
                for a in &matches {
                    let label = format!("@{}   {}", a.handle, a.display_name);
                    if ui.selectable_label(false, label).clicked() {
                        chosen = Some(a.handle.clone());
                    }
                }
            });
        });

    if let Some(handle) = chosen {
        let insert: Vec<char> = format!("@{handle} ").chars().collect();
        let mut next: Vec<char> = chars[..at].to_vec();
        next.extend_from_slice(&insert);
        next.extend_from_slice(&chars[cursor.min(chars.len())..]);
        *text = next.into_iter().collect();

        let new_cursor = at + insert.len();
        let mut state =
            egui::text_edit::TextEditState::load(ui.ctx(), response.id).unwrap_or_default();
        state
            .cursor
            .set_char_range(Some(CCursorRange::one(CCursor::new(new_cursor))));
        state.store(ui.ctx(), response.id);
        response.request_focus();
    }
    response
}

/// If the text before `cursor` ends with `@partial`, returns the index of the
/// `@` and the partial handle, lowercased.
fn mention_query(chars: &[char], cursor: usize) -> Option<(usize, String)> {
    let cursor = cursor.min(chars.len());
    let mut start = cursor;
    while start > 0 && is_handle_char(chars[start - 1]) {
        start -= 1;
    }
    if start == 0 || chars[start - 1] != '@' {
        return None;
    }
    let at = start - 1;
    if at > 0 && chars[at - 1].is_alphanumeric() {
        return None;
    }
    let query: String = chars[start..cursor]
        .iter()
        .collect::<String>()
        .to_lowercase();
    Some((at, query))
}

fn is_handle_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// Lays out `text` with every `@handle` that names a known actor in an accent
/// colour, so authors see that a mention landed.
pub fn highlighted(ui: &egui::Ui, text: &str, actors: &[Actor]) -> LayoutJob {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let normal = TextFormat {
        font_id: font_id.clone(),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    let accent = TextFormat {
        font_id,
        color: egui::Color32::from_rgb(120, 180, 255),
        ..Default::default()
    };

    let mut job = LayoutJob::default();
    let chars: Vec<char> = text.chars().collect();
    let mut plain_start = 0;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '@' && (i == 0 || !chars[i - 1].is_alphanumeric()) {
            let mut end = i + 1;
            while end < chars.len() && is_handle_char(chars[end]) {
                end += 1;
            }
            while end > i + 1 && matches!(chars[end - 1], '-' | '_') {
                end -= 1;
            }
            let handle: String = chars[i + 1..end].iter().collect::<String>().to_lowercase();
            if end > i + 1 && actors.iter().any(|a| a.handle == handle) {
                let plain: String = chars[plain_start..i].iter().collect();
                job.append(&plain, 0.0, normal.clone());
                let mention: String = chars[i..end].iter().collect();
                job.append(&mention, 0.0, accent.clone());
                plain_start = end;
                i = end;
                continue;
            }
        }
        i += 1;
    }
    let tail: String = chars[plain_start..].iter().collect();
    job.append(&tail, 0.0, normal);
    job
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, cursor: usize) -> Option<(usize, String)> {
        let chars: Vec<char> = text.chars().collect();
        mention_query(&chars, cursor)
    }

    #[test]
    fn detects_partial_handle_before_cursor() {
        assert_eq!(q("hi @ba", 6), Some((3, "ba".into())));
        assert_eq!(q("@", 1), Some((0, "".into())));
        assert_eq!(q("hi @Back", 8), Some((3, "back".into())));
    }

    #[test]
    fn ignores_emails_and_text_without_at() {
        assert_eq!(q("mail me@ex", 10), None);
        assert_eq!(q("plain text", 10), None);
        assert_eq!(q("@dba done", 9), None);
    }
}
