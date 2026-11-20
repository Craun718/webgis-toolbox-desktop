mod app;
mod console;

use eframe::egui;

fn main() -> eframe::Result {
    console::record(console::Level::Info, "WebGIS 工具箱", "启动软件");
    let result = eframe::run_native(
        "WebGIS 工具箱",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([960., 720.])
                .with_min_inner_size([660., 500.]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx)))),
    );
    match &result {
        Ok(()) => console::record(console::Level::Info, "WebGIS 工具箱", "退出软件"),
        Err(error) => {
            let mut message = format!("启动失败：{error}");
            let mut cause = std::error::Error::source(error);
            while let Some(error) = cause {
                message.push_str(&format!("：{error}"));
                cause = error.source();
            }
            console::record(console::Level::Error, "WebGIS 工具箱", &message)
        }
    };
    result
}
