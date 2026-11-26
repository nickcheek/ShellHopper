mod app;
mod config;
mod presentation_mask;
#[cfg(target_os = "macos")]
mod menu_bar;
mod terminal;
mod theme;
#[cfg(target_os = "macos")]
mod updater;

use app::ShellHopperApp;

fn main() -> eframe::Result {
    let icon = eframe::icon_data::from_png_bytes(include_bytes!("../assets/ShellHopperIcon.png"))
        .expect("embedded ShellHopper icon should be a PNG");
    let options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([900.0, 600.0])
            .with_icon(icon)
            // Draw our own sidebar and tab strip up to the window's top edge.
            .with_fullsize_content_view(true)
            .with_titlebar_shown(false)
            .with_title_shown(false),
        ..Default::default()
    };
    eframe::run_native(
        "ShellHopper",
        options,
        Box::new(|cc| Ok(Box::new(ShellHopperApp::new(cc)))),
    )
}
