//! The kanban board: columns with draggable cards.

use ak_core::board::Ticket;
use ak_core::ids::TicketId;

use crate::app::{Action, App};
use crate::backend::Request;
use crate::dialogs::NewTicketDialog;
use crate::model::{priority_color, BoardModel};
use crate::theme::{self, Tone};
use crate::widgets;

const COLUMN_WIDTH: f32 = 270.0;

pub fn draw(app: &mut App, ui: &mut egui::Ui) {
    let Some(project) = app.current else { return };

    ui.horizontal(|ui| {
        ui.heading(&app.project_name);
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::filled_button(ui, "New ticket", Tone::Accent).clicked() {
                app.new_ticket = Some(NewTicketDialog::new(&app.board));
            }
            ui.weak(format!("{} tickets", app.board.tickets.len()));
        });
    });
    ui.add_space(8.0);

    let board = &app.board;
    let selected = app.selected;
    let actions = &mut app.actions;

    egui::ScrollArea::horizontal()
        .id_salt("board_scroll")
        .show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for column in &board.columns {
                    let tickets: Vec<&Ticket> = board.tickets_in(column.id).collect();
                    ui.allocate_ui_with_layout(
                        egui::vec2(COLUMN_WIDTH, ui.available_height()),
                        egui::Layout::top_down(egui::Align::Min),
                        |ui| {
                            ui.set_width(COLUMN_WIDTH);
                            ui.horizontal(|ui| {
                                ui.label(egui::RichText::new(&column.name).strong());
                                widgets::chip(ui, Tone::Gray, format!("{}", tickets.len()));
                            });
                            ui.add_space(8.0);

                            // The drop zone sets its own fill and stroke for the drop highlight,
                            // so the card supplies only the shape and padding.
                            let frame = theme::card(ui.style());
                            let (_, dropped) = ui.dnd_drop_zone::<TicketId, ()>(frame, |ui| {
                                ui.set_min_height(ui.available_height() - 12.0);
                                ui.set_width(COLUMN_WIDTH - 26.0);
                                egui::ScrollArea::vertical()
                                    .id_salt(("col", column.id))
                                    .show(ui, |ui| {
                                        for t in &tickets {
                                            if card(ui, t, board, selected == Some(t.id)) {
                                                actions.push(Action::SelectTicket(Some(t.id)));
                                            }
                                            ui.add_space(8.0);
                                        }
                                        if tickets.is_empty() {
                                            ui.weak("Drop tickets here");
                                        }
                                    });
                            });
                            if let Some(id) = dropped {
                                if let Some(t) = board.ticket(*id) {
                                    if t.column_id != column.id {
                                        actions.push(Action::Request(Request::MoveTicket {
                                            project,
                                            id: t.id,
                                            to: column.id,
                                            version: t.version,
                                        }));
                                    }
                                }
                            }
                        },
                    );
                    ui.add_space(8.0);
                }
            });
        });
}

/// Draws one card and returns true when it was clicked rather than dragged.
///
/// The whole card is one click-and-drag widget. Labels inside it are made
/// non-selectable: egui labels normally sense the pointer for text selection,
/// which is what left only the card's padding ring responding to clicks.
/// A drag sets the drag-and-drop payload that the column drop zones read.
fn card(ui: &mut egui::Ui, t: &Ticket, board: &BoardModel, selected: bool) -> bool {
    let id = egui::Id::new(("card", t.id));
    let hovered = ui.ctx().read_response(id).is_some_and(|r| r.hovered());
    let dragging_this =
        egui::DragAndDrop::payload::<TicketId>(ui.ctx()).is_some_and(|p| *p == t.id);

    let stroke = if selected {
        egui::Stroke::new(1.5_f32, ui.visuals().selection.stroke.color)
    } else if hovered {
        ui.visuals().widgets.hovered.bg_stroke
    } else {
        ui.visuals().widgets.noninteractive.bg_stroke
    };
    // Tiles sit raised on the column, and brighten under the pointer.
    let palette = theme::palette(ui.style());
    let fill = if dragging_this {
        ui.visuals().widgets.active.bg_fill
    } else if hovered {
        palette.control_hover
    } else {
        palette.card_hover
    };

    let base = theme::card(ui.style())
        .fill(fill)
        .stroke(stroke)
        .corner_radius(theme::CONTROL_RADIUS);
    let frame = theme::glass(ui, base, |ui| {
        ui.style_mut().interaction.selectable_labels = false;
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(egui::RichText::new(&t.key).monospace().weak());
            let (rect, _) = ui.allocate_exact_size(egui::vec2(8.0, 8.0), egui::Sense::hover());
            ui.painter()
                .circle_filled(rect.center(), 4.0, priority_color(ui.style(), t.priority));
            if t.frozen {
                widgets::chip(ui, Tone::Accent, "FROZEN");
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if let Some(a) = t.assignee_id {
                    ui.weak(board.handle(a));
                }
            });
        });
        ui.add(egui::Label::new(&t.title).wrap());
        if !t.labels.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for l in &t.labels {
                    widgets::chip(ui, Tone::Gray, format!("#{l}"));
                }
            });
        }
    });

    let response = ui
        .interact(frame.response.rect, id, egui::Sense::click_and_drag())
        .on_hover_cursor(egui::CursorIcon::PointingHand);
    if response.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), t.id);
    }
    if dragging_this {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
        ghost(ui.ctx(), t);
    }
    response.clicked()
}

/// A small floating copy of the card that follows the pointer while dragging.
fn ghost(ctx: &egui::Context, t: &Ticket) {
    let Some(pos) = ctx.pointer_latest_pos() else {
        return;
    };
    egui::Area::new(egui::Id::new(("card_ghost", t.id)))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + egui::vec2(14.0, 14.0))
        .interactable(false)
        .show(ctx, |ui| {
            let sheet = theme::palette(ui.style()).sheet;
            let base = theme::card(ui.style())
                .fill(sheet)
                .corner_radius(theme::CONTROL_RADIUS);
            theme::glass(ui, base, |ui| {
                ui.set_max_width(COLUMN_WIDTH - 40.0);
                ui.label(egui::RichText::new(&t.key).monospace().weak());
                ui.label(&t.title);
            });
        });
}
