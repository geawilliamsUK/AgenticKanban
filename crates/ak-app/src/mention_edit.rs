//! Text editing with completion: `@mention` handles everywhere, and slash
//! commands where a caller offers them. Known mentions and commands are
//! coloured as they are typed, and the same highlighting renders bodies.
//!
//! The suggestion list opens above the text box. Up and Down move through
//! it, Tab or Enter takes the highlighted entry, Escape closes it, and a
//! click takes the entry clicked. A click moves focus off the text box
//! before it lands, so the list stays drawn while the pointer is over it.

use egui::text::{CCursor, CCursorRange, LayoutJob, TextFormat};
use egui::{Key, Modifiers};

use ak_core::{Actor, ActorKind};

use crate::theme::{self, Tone};
use crate::widgets;

const MAX_SUGGESTIONS: usize = 8;

/// A slash command offered for completion: the name without the slash, and
/// what it does.
#[derive(Clone, Debug, PartialEq)]
pub struct Command {
    pub name: String,
    pub hint: String,
}

/// What the compose box did this frame.
pub struct ComposeOutput {
    pub response: egui::Response,
    /// Enter was pressed with no modifier and no list was open.
    pub submitted: bool,
}

#[derive(Clone, Debug, PartialEq)]
struct Suggestion {
    /// What replaces the partial text.
    insert: String,
    label: String,
    hint: String,
}

#[derive(Clone, Default)]
struct ListState {
    /// The partial text the list was built for, to keep the highlight while
    /// it does not change.
    query: String,
    /// The character range the chosen entry replaces.
    range: (usize, usize),
    items: Vec<Suggestion>,
    selected: usize,
    /// Where the list was drawn, so a click on it keeps it alive.
    rect: Option<egui::Rect>,
}

/// A multiline text edit with `@mention` completion.
pub fn multiline(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    text: &mut String,
    actors: &[Actor],
    hint: &str,
    rows: usize,
) -> egui::Response {
    compose(ui, id_salt, text, actors, &[], hint, rows, false).response
}

/// A multiline text edit with `@mention` and, when `commands` is not empty,
/// `/command` completion. With `enter_submits`, Enter reports a submit and
/// Shift+Enter starts a new line.
#[allow(clippy::too_many_arguments)]
pub fn compose(
    ui: &mut egui::Ui,
    id_salt: impl std::hash::Hash,
    text: &mut String,
    actors: &[Actor],
    commands: &[Command],
    hint: &str,
    rows: usize,
    enter_submits: bool,
) -> ComposeOutput {
    let id = ui.make_persistent_id(id_salt);
    let list_id = id.with("completion");
    let mut list: ListState = ui.data(|d| d.get_temp(list_id)).unwrap_or_default();
    let focused = ui.memory(|m| m.has_focus(id));

    // Keys first, before the text box can turn them into text.
    let mut accept: Option<usize> = None;
    let mut submitted = false;
    if focused && !list.items.is_empty() {
        ui.input_mut(|i| {
            let plain = i.modifiers.is_none();
            if plain && i.consume_key(Modifiers::NONE, Key::ArrowDown) {
                list.selected = (list.selected + 1) % list.items.len();
            }
            if plain && i.consume_key(Modifiers::NONE, Key::ArrowUp) {
                list.selected = (list.selected + list.items.len() - 1) % list.items.len();
            }
            if plain
                && (i.consume_key(Modifiers::NONE, Key::Tab)
                    || i.consume_key(Modifiers::NONE, Key::Enter))
            {
                accept = Some(list.selected);
            }
            if i.consume_key(Modifiers::NONE, Key::Escape) {
                list.items.clear();
            }
        });
    } else if focused && enter_submits {
        // `consume_key` ignores an extra Shift, so check for none first:
        // Shift+Enter must stay a new line.
        submitted =
            ui.input_mut(|i| i.modifiers.is_none() && i.consume_key(Modifiers::NONE, Key::Enter));
    }
    if let Some(n) = accept {
        apply(ui.ctx(), id, text, &list, n);
        list.items.clear();
    }

    let mut layouter = |ui: &egui::Ui, buf: &dyn egui::TextBuffer, wrap_width: f32| {
        let mut job = highlighted_with(ui, buf.as_str(), actors, commands);
        job.wrap.max_width = wrap_width;
        ui.fonts(|f| f.layout_job(job))
    };
    // egui moves focus on Tab before this code runs, using the lock the
    // text box set last frame; while the list is open the box keeps Tab, and
    // the code above turns it into a pick.
    let list_open = !list.items.is_empty();
    let output = egui::TextEdit::multiline(text)
        .id(id)
        .lock_focus(list_open)
        .desired_width(f32::INFINITY)
        .desired_rows(rows)
        .hint_text(hint)
        .layouter(&mut layouter)
        .show(ui);
    let response = output.response.clone();
    // egui also drops focus on Escape before any widget runs, unless the
    // focused widget claims it; while the list is open, Escape only closes
    // the list. Set after the text box, which sets its own filter.
    if list_open && response.has_focus() {
        ui.memory_mut(|m| {
            m.set_focus_lock_filter(
                id,
                egui::EventFilter {
                    tab: true,
                    horizontal_arrows: true,
                    vertical_arrows: true,
                    escape: true,
                },
            )
        });
    }

    if response.has_focus() {
        let cursor = output
            .cursor_range
            .map(|r| r.primary.index)
            .unwrap_or_else(|| text.chars().count());
        match suggestions(text, cursor, actors, commands) {
            Some((query, range, items)) => {
                if query != list.query || items != list.items {
                    list.selected = 0;
                }
                list.query = query;
                list.range = range;
                list.items = items;
            }
            None => list.items.clear(),
        }
    } else {
        // Keep the list while a click on it is under way.
        let over = list
            .rect
            .zip(ui.ctx().pointer_latest_pos())
            .is_some_and(|(r, p)| r.contains(p));
        if !over {
            list.items.clear();
        }
    }

    let mut chosen = None;
    if list.items.is_empty() {
        list.rect = None;
    } else {
        let shown = egui::Area::new(list_id)
            .order(egui::Order::Foreground)
            .pivot(egui::Align2::LEFT_BOTTOM)
            .fixed_pos(response.rect.left_top() - egui::vec2(0.0, 4.0))
            .show(ui.ctx(), |ui| {
                // Opaque, so the text behind does not show through the list.
                let fill = ui.visuals().window_fill.to_opaque();
                egui::Frame::popup(ui.style()).fill(fill).show(ui, |ui| {
                    ui.set_min_width(280.0);
                    let title = if list.items[0].insert.starts_with('/') {
                        "Commands"
                    } else {
                        "Mention"
                    };
                    ui.horizontal(|ui| {
                        widgets::caption(ui, title);
                        ui.weak("↑↓ Tab");
                    });
                    for (n, s) in list.items.iter().enumerate() {
                        let r = ui.horizontal(|ui| {
                            let r = ui.selectable_label(n == list.selected, &s.label);
                            if !s.hint.is_empty() {
                                ui.weak(&s.hint);
                            }
                            r
                        });
                        if r.inner.clicked() {
                            chosen = Some(n);
                        }
                    }
                });
            });
        list.rect = Some(shown.response.rect);
    }
    if let Some(n) = chosen {
        apply(ui.ctx(), id, text, &list, n);
        list.items.clear();
        list.rect = None;
        response.request_focus();
    }
    ui.data_mut(|d| d.insert_temp(list_id, list));

    ComposeOutput {
        response,
        submitted,
    }
}

/// Replaces the partial text with entry `n` and puts the cursor after it.
fn apply(ctx: &egui::Context, id: egui::Id, text: &mut String, list: &ListState, n: usize) {
    let Some(s) = list.items.get(n) else {
        return;
    };
    let chars: Vec<char> = text.chars().collect();
    let (start, end) = (list.range.0.min(chars.len()), list.range.1.min(chars.len()));
    let insert: Vec<char> = format!("{} ", s.insert).chars().collect();
    let mut next: Vec<char> = chars[..start].to_vec();
    next.extend_from_slice(&insert);
    // Skip a space the text already has after the replaced part.
    let rest = &chars[end..];
    let rest = rest.strip_prefix(&[' ']).unwrap_or(rest);
    next.extend_from_slice(rest);
    *text = next.into_iter().collect();
    let mut state = egui::text_edit::TextEditState::load(ctx, id).unwrap_or_default();
    state
        .cursor
        .set_char_range(Some(CCursorRange::one(CCursor::new(start + insert.len()))));
    state.store(ctx, id);
}

/// The completions for the text before `cursor`: the partial text, the
/// range it covers, and the entries.
fn suggestions(
    text: &str,
    cursor: usize,
    actors: &[Actor],
    commands: &[Command],
) -> Option<(String, (usize, usize), Vec<Suggestion>)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());
    if !commands.is_empty() {
        if let Some(partial) = command_query(&chars, cursor) {
            let end = chars
                .iter()
                .position(|c| c.is_whitespace())
                .unwrap_or(chars.len());
            let items: Vec<Suggestion> = commands
                .iter()
                .filter(|c| c.name.to_lowercase().starts_with(&partial))
                .take(MAX_SUGGESTIONS)
                .map(|c| Suggestion {
                    insert: format!("/{}", c.name),
                    label: format!("/{}", c.name),
                    hint: c.hint.clone(),
                })
                .collect();
            return (!items.is_empty()).then(|| (format!("/{partial}"), (0, end), items));
        }
    }
    let (at, query) = mention_query(&chars, cursor)?;
    let items: Vec<Suggestion> = actors
        .iter()
        .filter(|a| a.kind != ActorKind::System && a.handle.starts_with(&query))
        .take(MAX_SUGGESTIONS)
        .map(|a| Suggestion {
            insert: format!("@{}", a.handle),
            label: format!("@{}", a.handle),
            hint: a.display_name.clone(),
        })
        .collect();
    (!items.is_empty()).then(|| (format!("@{query}"), (at, cursor), items))
}

/// If the text is a bare `/partial` up to the cursor, the partial command,
/// lowercased.
fn command_query(chars: &[char], cursor: usize) -> Option<String> {
    if chars.first() != Some(&'/') {
        return None;
    }
    let head = &chars[1..cursor.max(1)];
    if head.iter().any(|c| c.is_whitespace()) {
        return None;
    }
    Some(head.iter().collect::<String>().to_lowercase())
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
    highlighted_with(ui, text, actors, &[])
}

/// As `highlighted`, and a leading `/command` that is one of `commands` in
/// another colour.
fn highlighted_with(
    ui: &egui::Ui,
    text: &str,
    actors: &[Actor],
    commands: &[Command],
) -> LayoutJob {
    let font_id = egui::TextStyle::Body.resolve(ui.style());
    let normal = TextFormat {
        font_id: font_id.clone(),
        color: ui.visuals().text_color(),
        ..Default::default()
    };
    let accent = TextFormat {
        font_id: font_id.clone(),
        color: theme::tone(ui.style(), Tone::Accent),
        ..Default::default()
    };
    let command = TextFormat {
        font_id,
        color: theme::tone(ui.style(), Tone::Teal),
        ..Default::default()
    };

    let mut job = LayoutJob::default();
    let chars: Vec<char> = text.chars().collect();
    let mut plain_start = 0;
    let mut i = 0;
    if chars.first() == Some(&'/') {
        let end = chars
            .iter()
            .position(|c| c.is_whitespace())
            .unwrap_or(chars.len());
        let name: String = chars[1..end].iter().collect::<String>().to_lowercase();
        if commands.iter().any(|c| c.name.to_lowercase() == name) {
            job.append(&chars[..end].iter().collect::<String>(), 0.0, command);
            plain_start = end;
            i = end;
        }
    }
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
    use ak_core::ids::ActorId;

    fn q(text: &str, cursor: usize) -> Option<(usize, String)> {
        let chars: Vec<char> = text.chars().collect();
        mention_query(&chars, cursor)
    }

    fn actor(handle: &str) -> Actor {
        Actor {
            id: ActorId::new(),
            kind: ActorKind::Agent,
            handle: handle.into(),
            display_name: handle.to_uppercase(),
        }
    }

    fn cmd(name: &str) -> Command {
        Command {
            name: name.into(),
            hint: String::new(),
        }
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

    #[test]
    fn suggests_mentions_and_commands_with_the_range_they_replace() {
        let actors = vec![actor("lead"), actor("legal"), actor("dev")];
        let commands = vec![cmd("standup"), cmd("model"), cmd("compact")];

        let (query, range, items) = suggestions("brief @le", 9, &actors, &commands).unwrap();
        assert_eq!(query, "@le");
        assert_eq!(range, (6, 9));
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].insert, "@lead");

        let (query, range, items) = suggestions("/st", 3, &actors, &commands).unwrap();
        assert_eq!(query, "/st");
        assert_eq!(range, (0, 3));
        assert_eq!(items[0].insert, "/standup");
        // Once the command has an argument, the list is done.
        assert!(suggestions("/standup ship", 13, &actors, &commands).is_none());
        // Without commands, a slash is plain text.
        assert!(suggestions("/st", 3, &actors, &[]).is_none());
        assert!(suggestions("hello", 5, &actors, &commands).is_none());
    }
}
