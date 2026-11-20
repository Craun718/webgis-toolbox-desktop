use webgis_core::{i18n::with_language, message};
mod app;
mod console;

use eframe::egui;

fn main() -> eframe::Result {
    if std::env::args_os().nth(1).as_deref() == Some(app::osgb_worker::ARGUMENT.as_ref()) {
        if let Err(error) = app::osgb_worker::run() {
            eprintln!("{error:#}");
            std::process::exit(1);
        }
        return Ok(());
    }
    with_language(app::startup_language(), run)
}

fn run() -> eframe::Result {
    console::record(
        console::Level::Info,
        message!(webgis_toolbox),
        message!(application_started),
    );
    let result = eframe::run_native(
        "webgis-toolbox",
        eframe::NativeOptions {
            renderer: eframe::Renderer::Glow,
            viewport: egui::ViewportBuilder::default()
                .with_title(message!(webgis_toolbox))
                .with_inner_size([960., 720.])
                .with_min_inner_size([660., 500.]),
            ..Default::default()
        },
        Box::new(|cc| Ok(Box::new(app::App::new(&cc.egui_ctx)))),
    );
    match &result {
        Ok(()) => with_language(app::startup_language(), || {
            console::record(
                console::Level::Info,
                message!(webgis_toolbox),
                message!(application_exited),
            )
        }),
        Err(error) => {
            let mut message = message!(application_failed_to_start_error, error = error);
            let mut cause = std::error::Error::source(error);
            while let Some(error) = cause {
                message.push_str(&message!(error_cause, error));
                cause = error.source();
            }
            console::record(console::Level::Error, message!(webgis_toolbox), &message)
        }
    };
    result
}
