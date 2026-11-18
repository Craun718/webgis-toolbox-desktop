mod app;

use eframe::egui;

fn main() -> eframe::Result {
    eframe::run_native(
        "WebGIS 工具箱",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([960., 720.])
                .with_min_inner_size([660., 500.]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx)))),
    )
}
