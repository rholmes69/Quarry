#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Quarry — a DB Browser for SQLite–style workbench written in Rust.

mod app;
mod csv_io;
mod db;
mod ui;

fn main() -> eframe::Result<()> {
    let initial = std::env::args_os().nth(1).map(std::path::PathBuf::from);
    let options = eframe::NativeOptions {
        renderer: eframe::Renderer::Glow,
        viewport: egui::ViewportBuilder::default()
            .with_title("Quarry — SQLite Browser")
            .with_inner_size([1280.0, 820.0])
            .with_min_inner_size([760.0, 480.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Quarry",
        options,
        Box::new(move |cc| Ok(Box::new(app::App::new(cc, initial)))),
    )
}
