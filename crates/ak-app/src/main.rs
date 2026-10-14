//! AgenticKanban desktop application.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod backend;
mod board_view;
mod chats_view;
mod dialogs;
mod diff_view;
mod inbox_view;
mod markdown;
mod mention_edit;
mod model;
mod settings_view;
mod snapshot_view;
mod team_view;
mod ticket_view;
mod widgets;

fn main() -> eframe::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        )
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("AgenticKanban")
            .with_inner_size([1400.0, 860.0])
            .with_min_inner_size([900.0, 600.0]),
        ..Default::default()
    };
    eframe::run_native(
        "AgenticKanban",
        options,
        Box::new(|cc| Ok(Box::new(app::App::new(cc)))),
    )
}
