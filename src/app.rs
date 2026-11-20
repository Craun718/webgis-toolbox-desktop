pub(crate) use settings::startup_language;
use webgis_core::{
    i18n::{self, Language},
    message,
};
use webgis_toolbox::dom::{self, Phase, TaskProgress};
mod file_dialog;
mod settings;
mod task;

use anyhow::{Context, Result, anyhow};
use eframe::egui::{self, Color32, Context as EguiContext, RichText, Ui};
use settings::Settings;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, mpsc},
};
use task::{TaskContext, TaskWindow, Worker};
use tiles_toolbox::{
    mosaic,
    pyramid::{self, Resampling},
    stacta,
    tiles::{Format, Index, Scheme},
};
use webgis_core::{model_clip as clip, task_pool::ThreadPool};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Raster,
    Cog,
    Tiles(TileOp),
    Clip,
    Settings,
}
impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Cog => message!(create_cloud_optimized_geotiff),
            Self::Raster => message!(osgb_to_dom_dsm),
            Self::Tiles(operation) => operation.name(),
            Self::Clip => message!(clip_osgb_models),
            Self::Settings => message!(settings),
        }
    }
}

pub struct App {
    selected: Tool,
    raster: RasterForm,
    cog: CogForm,
    tiles: [TileForm; TileOp::ALL.len()],
    clip: ClipForm,
    worker: Option<Worker>,
    task_window: Option<TaskWindow>,
    task_sequence: u64,
    show_close_notice: bool,
    outcome: Option<(Language, Result<Outcome>)>,
    font_loaded: bool,
    settings: Settings,
}

enum Outcome {
    Cancelled,
    RasterPreview(dom::Summary),
    ClipPreview(clip::Preflight),
    Layers {
        path: PathBuf,
        layers: Vec<String>,
    },
    Artifacts {
        paths: Vec<PathBuf>,
        details: Option<String>,
    },
}

impl App {
    pub fn new(ctx: &EguiContext) -> Self {
        ctx.all_styles_mut(|style| style.spacing.button_padding += egui::vec2(4.0, 4.0));
        let settings = Settings::new(ctx);
        Self {
            selected: Tool::Raster,
            raster: RasterForm {
                resolution: "0.05".into(),
                ..Default::default()
            },
            cog: CogForm::default(),
            tiles: Default::default(),
            clip: ClipForm::default(),
            worker: None,
            task_window: None,
            task_sequence: 0,
            show_close_notice: false,
            outcome: None,
            font_loaded: install_font(ctx, settings.language()),
            settings,
        }
    }

    fn tool_link(&mut self, ui: &mut Ui, tool: Tool) {
        if ui
            .selectable_label(self.selected == tool, tool.name())
            .clicked()
            && self.selected != tool
        {
            self.selected = tool;
            self.outcome = None;
        }
        ui.add_space(4.);
    }

    fn poll(&mut self) {
        if let Some(worker) = &self.worker {
            let result = match worker.receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => {
                    let result = i18n::with_language(worker.language(), || {
                        Err(anyhow!(message!(the_background_worker_exited_unexpectedly)))
                    });
                    worker.finish_disconnected(&result);
                    result
                }
            };
            if let Ok(Outcome::Layers { path, layers }) = &result
                && path == Path::new(self.clip.boundary.trim())
            {
                self.clip.layers.clone_from(layers);
                self.clip.layer = (layers.len() == 1).then(|| layers[0].clone());
            }
            self.outcome = Some((worker.language(), result));
            self.worker = None;
        }
    }

    fn start(&mut self, ctx: &EguiContext, action: Action) -> Result<()> {
        self.outcome = None;
        let operation = match action {
            Action::Layers => message!(scan_zip_layers),
            Action::Preview => message!(preflight),
            Action::Run => message!(process),
        };
        let name = format!("{} · {operation}", self.selected.name());
        let threads = self.settings.threads();
        let worker = match (self.selected, action) {
            (Tool::Raster, Action::Preview | Action::Run) => {
                let request = self.raster.request()?;
                Worker::start(ctx, &name, true, move |task| {
                    let update = |event| task.update(event);
                    if action == Action::Preview {
                        Ok(Outcome::RasterPreview(dom::preflight_with_progress(
                            &request,
                            &task.cancelled,
                            update,
                        )?))
                    } else {
                        let pool = processing_pool(threads)?;
                        Ok(Outcome::Artifacts {
                            paths: dom::run_with_progress(
                                &request,
                                &task.cancelled,
                                update,
                                || task.publish_with_progress(Some(0.95)),
                                pool.as_ref(),
                            )?,
                            details: None,
                        })
                    }
                })?
            }
            (Tool::Cog, Action::Run) => {
                let input = required_path(message!(source_geotiff), &self.cog.input)?;
                let output = required_path(message!(output_cog), &self.cog.output)?;
                Worker::start(ctx, &name, true, move |task| {
                    let pool = processing_pool(threads)?;
                    let output = dom::cog::run_with_progress(
                        &input,
                        &output,
                        &task.cancelled,
                        |event| task.update(event),
                        || task.publish_with_progress(Some(0.95)),
                        pool.as_ref(),
                    )?;
                    Ok(Outcome::Artifacts {
                        paths: vec![output],
                        details: None,
                    })
                })?
            }
            (Tool::Clip, Action::Layers) => {
                let path = required_path(message!(clip_boundary), &self.clip.boundary)?;
                Worker::start(ctx, &name, false, move |task| {
                    task.update(TaskProgress::new(
                        Phase::ScanLayers,
                        message!(scan_zip_layers),
                        None,
                    ));
                    let layers = clip::zip_layers(&path).map_err(|e| anyhow!(e))?;
                    Ok(Outcome::Layers { path, layers })
                })?
            }
            (Tool::Clip, Action::Preview | Action::Run) => {
                let request = self.clip.request()?;
                Worker::start(ctx, &name, true, move |task| {
                    task.update(TaskProgress::new(
                        Phase::CheckClip,
                        message!(check_model_boundary_and_output),
                        None,
                    ));
                    if action == Action::Preview {
                        return clip::preflight_cancelable(&request, &task.cancelled)
                            .map(Outcome::ClipPreview)
                            .map_err(|e| anyhow!(e));
                    }
                    let pool = processing_pool(threads)?;
                    let result = clip::run_with_pool(
                        &request,
                        &task.cancelled,
                        |p| {
                            task.update(TaskProgress::new(
                                Phase::ClipModel,
                                message!(
                                    clipping_processed_models_found,
                                    p.file.display(),
                                    p.completed,
                                    p.discovered
                                ),
                                None,
                            ));
                        },
                        || task.publish().map_err(|err| err.to_string()),
                        pool.as_ref(),
                    )
                    .map_err(|e| anyhow!(e))?;
                    let details = message!(
                        wrote_osgb_files_with_triangles,
                        result.files,
                        result.triangles
                    );
                    Ok(Outcome::Artifacts {
                        paths: std::iter::once(result.output)
                            .chain(result.entries)
                            .collect(),
                        details: Some(details),
                    })
                })?
            }
            (Tool::Tiles(operation), Action::Run) => {
                let form = self.tiles[operation as usize].clone();
                Worker::start(ctx, &name, operation == TileOp::MapTiles, move |task| {
                    let pool = processing_pool(threads)?;
                    form.run(operation, task, pool.as_ref())
                })?
            }
            _ => unreachable!(),
        };
        self.task_sequence += 1;
        self.task_window = Some(worker.window(self.task_sequence));
        self.worker = Some(worker);
        Ok(())
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &EguiContext, _: &mut eframe::Frame) {
        let language = self.settings.language();
        i18n::with_language(language, || self.show(ctx));
        let saved = self.settings.language();
        if saved != language {
            if (saved == Language::Japanese) != (language == Language::Japanese) {
                self.font_loaded = install_font(ctx, saved);
            }
            i18n::with_language(saved, || {
                ctx.send_viewport_cmd(egui::ViewportCommand::Title(
                    message!(webgis_toolbox).into(),
                ));
            });
            ctx.request_repaint();
        }
    }
}

impl App {
    fn show(&mut self, ctx: &EguiContext) {
        self.poll();
        let busy = self.worker.is_some();
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        if busy && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.show_close_notice = true;
        }
        egui::SidePanel::left("tools")
            .exact_width(212.)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(12.);
                ui.heading(message!(webgis_toolbox));
                ui.add_space(16.);
                ui.add_enabled_ui(!busy, |ui| {
                    ui.label(RichText::new(message!(3d_models)).strong());
                    ui.indent("model_tools", |ui| {
                        self.tool_link(ui, Tool::Raster);
                        self.tool_link(ui, Tool::Clip);
                    });
                    ui.add_space(8.);
                    ui.label(RichText::new(message!(raster_tools)).strong());
                    ui.indent("raster_tools", |ui| {
                        self.tool_link(ui, Tool::Cog);
                    });
                    ui.add_space(8.);
                    ui.label(RichText::new(message!(tile_tools)).strong());
                    ui.indent("tile_tools", |ui| {
                        for operation in TileOp::ALL {
                            self.tool_link(ui, Tool::Tiles(operation));
                        }
                    });
                    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
                        self.tool_link(ui, Tool::Settings);
                    });
                });
            });
        let mut action = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(12.);
                ui.heading(self.selected.name());
                if !self.font_loaded {
                    ui.label(if i18n::language() == Language::Japanese {
                        message!(missing_japanese_font)
                    } else {
                        message!(missing_chinese_font)
                    });
                }
                ui.add_space(14.);
                if self.selected == Tool::Settings {
                    self.settings.ui(ui);
                    ui.add_space(18.);
                    ui.separator();
                    ui.add_space(10.);
                    ui.heading(message!(about));
                    ui.add_space(8.);
                    egui::Grid::new("about_project").show(ui, |ui| {
                        ui.label(message!(author));
                        ui.label(env!("CARGO_PKG_AUTHORS"));
                        ui.end_row();

                        ui.label(message!(license));
                        ui.label(env!("CARGO_PKG_LICENSE"));
                        ui.end_row();

                        let repository = env!("CARGO_PKG_REPOSITORY");
                        ui.label(message!(repository));
                        ui.hyperlink_to(repository, repository);
                        ui.end_row();
                    });
                    return;
                }
                ui.add_enabled_ui(!busy, |ui| {
                    match self.selected {
                        Tool::Raster => self.raster.ui(ui),
                        Tool::Cog => self.cog.ui(ui),
                        Tool::Tiles(operation) => self.tiles[operation as usize].ui(ui, operation),
                        Tool::Clip => {
                            if self.clip.ui(ui) {
                                action = Some(Action::Layers);
                            }
                        }
                        Tool::Settings => unreachable!(),
                    }
                    ui.add_space(10.);
                    ui.horizontal(|ui| {
                        if matches!(self.selected, Tool::Raster | Tool::Clip)
                            && ui.button(message!(preflight)).clicked()
                        {
                            action = Some(Action::Preview);
                        }
                        if ui.button(RichText::new(message!(run)).strong()).clicked() {
                            action = Some(Action::Run);
                        }
                    });
                });
                if let Some(worker) = &self.worker {
                    worker.show_cancel_button(ui);
                }
                if let Some((language, outcome)) = &self.outcome {
                    ui.add_space(12.);
                    ui.separator();
                    i18n::with_language(*language, || show_outcome(ui, outcome));
                }
            });
        });
        if let Some(action) = action
            && let Err(err) = self.start(ctx, action)
        {
            self.outcome = Some((i18n::language(), Err(err)));
        }
        if let Some(window) = &self.task_window
            && window.show(ctx, self.worker.is_some())
        {
            self.task_window = None;
            ctx.request_repaint();
        }
        if let Some(worker) = &self.worker {
            if self.show_close_notice {
                let response =
                    egui::Modal::new(egui::Id::new("task_close_notice")).show(ctx, |ui| {
                        ui.heading(message!(task_in_progress));
                        ui.label(worker.close_notice());
                        ui.button(message!(ok)).clicked()
                    });
                if response.inner {
                    self.show_close_notice = false;
                }
            }
        } else {
            self.show_close_notice = false;
        }
    }
}

fn processing_pool(threads: usize) -> Result<Option<ThreadPool>> {
    if threads == 1 {
        return Ok(None);
    }
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map(Some)
        .context(message!(could_not_create_the_task_thread_pool))
}

fn install_font(ctx: &EguiContext, language: Language) -> bool {
    let japanese = language == Language::Japanese;
    let family = if japanese {
        "Noto Sans CJK JP"
    } else {
        "Noto Sans CJK SC"
    };
    let system_match = std::process::Command::new("fc-match")
        .args(["-f", "%{family}\n%{file}\n%{index}", family])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let output = String::from_utf8(output.stdout).ok()?;
            let mut lines = output.lines();
            let matches = lines.next()?.split(',').any(|name| name == family);
            let path = PathBuf::from(lines.next()?);
            let index = lines.next()?.parse().ok()?;
            matches.then_some((path, index))
        });
    let noto_index = if japanese { 0 } else { 2 };
    let noto = [
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ]
    .map(|path| (PathBuf::from(path), noto_index));
    let chinese = [
        "/usr/share/fonts/adobe-source-han-sans/SourceHanSansCN-Regular.otf",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    ]
    .map(|path| (PathBuf::from(path), 0));
    let configured = std::env::var_os("WEBGIS_CJK_FONT")
        .map(PathBuf::from)
        .map(|path| {
            let index = if path
                .file_name()
                .is_some_and(|name| name == "NotoSansCJK-Regular.ttc")
            {
                noto_index
            } else {
                0
            };
            (path, index)
        });
    let mut fonts = egui::FontDefinitions::default();
    for (path, index) in configured
        .into_iter()
        .chain(system_match)
        .chain(noto)
        .chain(chinese.into_iter().filter(|_| !japanese))
    {
        if let Ok(bytes) = std::fs::read(&path) {
            let mut font = egui::FontData::from_owned(bytes);
            font.index = index;
            fonts.font_data.insert("CJK".into(), Arc::new(font));
            for family in fonts.families.values_mut() {
                family.push("CJK".into());
            }
            ctx.set_fonts(fonts);
            return true;
        }
    }
    ctx.set_fonts(fonts);
    false
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Action {
    Layers,
    Preview,
    Run,
}

fn paths(ui: &mut Ui, paths: &[PathBuf]) {
    for path in paths {
        ui.horizontal_wrapped(|ui| {
            let value = path.display().to_string();
            ui.label(&value);
            if ui.button(message!(copy_path)).clicked() {
                ui.ctx().copy_text(value);
            }
        });
    }
}

fn show_outcome(ui: &mut Ui, outcome: &Result<Outcome>) {
    match outcome {
        Ok(Outcome::Cancelled) => {
            ui.label(message!(task_canceled));
        }
        Err(error) => {
            ui.label(
                RichText::new(message!(processing_failed))
                    .color(Color32::from_rgb(220, 82, 75))
                    .strong(),
            );
            ui.add(egui::Label::new(format!("{error:#}")).selectable(true));
        }
        Ok(Outcome::RasterPreview(summary)) => {
            ui.strong(message!(dom_dsm_preflight));
            ui.label(message!(
                coordinate_mode_crs,
                mode_label(summary.mode),
                summary.crs.as_deref().unwrap_or(message!(local))
            ));
            if let Some(path) = &summary.metadata {
                ui.label(message!(metadata_value, path.display()));
            }
            let [x, y, z, east, north, high] = summary.bounds;
            ui.label(message!(
                coordinate_extent,
                x = x,
                east = east,
                y = y,
                north = north,
                z = z,
                high = high
            ));
            ui.label(message!(origin, summary.origin));
            let [w, h] = summary.size;
            ui.label(message!(
                raster_dimensions_and_pixel_size,
                summary.resolution,
                w = w,
                h = h
            ));
            ui.label(message!(
                selected_model_files_and_lods_triangles,
                summary.selected.len(),
                summary.triangles
            ));
            paths(ui, &summary.selected);
        }
        Ok(Outcome::ClipPreview(summary)) => {
            ui.strong(message!(clip_preflight));
            ui.label(message!(
                model_crs_origin,
                summary.reference.srs,
                summary.reference.origin
            ));
            ui.label(message!(output_folder_value, summary.output.display()));
            if summary.output_non_empty {
                ui.label(message!(clip_nonempty_output_notice));
            }
            ui.label(message!(model_entry));
            paths(ui, &summary.entries);
        }
        Ok(Outcome::Layers { layers, .. }) => {
            ui.label(message!(found_shp_layers, layers.len()));
        }
        Ok(Outcome::Artifacts {
            paths: files,
            details,
        }) => {
            ui.strong(message!(processing_complete));
            if let Some(details) = details {
                ui.label(details);
            }
            paths(ui, files);
        }
    }
}

fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) -> bool {
    ui.label(label);
    let changed = text_edit(ui, value, hint);
    ui.add_space(4.);
    changed
}

fn text_edit(ui: &mut Ui, value: &mut String, hint: &str) -> bool {
    let response = ui.add(
        egui::TextEdit::singleline(value)
            .hint_text(hint)
            .desired_width(f32::INFINITY),
    );
    let changed = response.changed();
    if !hint.is_empty() && value.is_empty() {
        response.on_hover_text(hint);
    }
    changed
}

#[derive(Clone, Copy)]
enum PathPicker<'a> {
    File(&'static str, &'static [&'static str]),
    FileOrFolder(&'static str, &'static [&'static str]),
    Folder,
    NewFolder,
    SaveFile(&'static str, &'static [&'static str], &'a str),
}

#[derive(Default)]
struct CogForm {
    input: String,
    output: String,
}

impl CogForm {
    fn ui(&mut self, ui: &mut Ui) {
        const EXTENSIONS: &[&str] = &["tif", "tiff"];
        let changed = path_field(
            ui,
            message!(source_geotiff),
            &mut self.input,
            "",
            PathPicker::File("GeoTIFF", EXTENSIONS),
        );
        let default = dom::cog::default_output(Path::new(self.input.trim()));
        if changed {
            self.output = if self.input.trim().is_empty() {
                String::new()
            } else {
                default.to_string_lossy().into_owned()
            };
        }
        let default_name = default
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("output_cog.tif");
        path_field(
            ui,
            message!(output_cog),
            &mut self.output,
            "",
            PathPicker::SaveFile("GeoTIFF", EXTENSIONS, default_name),
        );
    }
}

fn path_field(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    picker: PathPicker<'_>,
) -> bool {
    ui.label(label);
    let row_size = egui::vec2(ui.available_width(), ui.spacing().interact_size.y);
    let row = ui.allocate_ui_with_layout(
        row_size,
        egui::Layout::right_to_left(egui::Align::Center),
        |ui| {
            let mut picked = None;
            let current = Path::new(value.trim());
            let directory = current.ancestors().find(|path| path.is_dir());
            let dialog =
                |filter, selection| file_dialog::select(label, directory, filter, selection);
            if matches!(picker, PathPicker::Folder | PathPicker::FileOrFolder(..))
                && ui.button(message!(browse_folders)).clicked()
            {
                picked = dialog(None, file_dialog::Selection::Folder);
            }
            match picker {
                PathPicker::File(name, extensions) | PathPicker::FileOrFolder(name, extensions) => {
                    if ui.button(message!(browse_files)).clicked() {
                        picked = dialog(Some((name, extensions)), file_dialog::Selection::File);
                    }
                }
                PathPicker::SaveFile(name, extensions, default_name) => {
                    if ui.button(message!(save_as)).clicked() {
                        let file_name = current
                            .file_name()
                            .and_then(|name| name.to_str())
                            .filter(|_| !current.is_dir())
                            .unwrap_or(default_name);
                        picked = dialog(
                            Some((name, extensions)),
                            file_dialog::Selection::Save(Some(file_name)),
                        );
                    }
                }
                PathPicker::NewFolder => {
                    if ui.button(message!(new_folder)).clicked() {
                        let name = current
                            .file_name()
                            .and_then(|name| name.to_str())
                            .filter(|_| !current.is_dir());
                        // 导出任务会创建目录，因此这里只选定目标路径。
                        picked = dialog(None, file_dialog::Selection::Save(name));
                    }
                }
                PathPicker::Folder => {}
            }
            let mut changed = false;
            if let Some(path) = picked {
                let selected = path.to_string_lossy();
                if value != selected.as_ref() {
                    *value = selected.into_owned();
                    changed = true;
                }
            }
            let edited = text_edit(ui, value, hint);
            changed || edited
        },
    );
    ui.add_space(4.);
    row.inner
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mosaic_option_controls_internal_overviews() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let input = dir.path().join("tiles");
        for x in 0..2 {
            let column = input.join(format!("1/{x}"));
            std::fs::create_dir_all(&column)?;
            image::RgbaImage::from_pixel(256, 256, image::Rgba([x, 63, 127, 255]))
                .save(column.join("0.png"))?;
        }
        for enabled in [false, true] {
            let output = dir.path().join(format!("{enabled}.tif"));
            let form = TileForm {
                input: input.display().to_string(),
                output: output.display().to_string(),
                zoom: 1,
                build_overviews: enabled,
                ..Default::default()
            };
            let worker = Worker::start(&egui::Context::default(), "mosaic", false, move |task| {
                form.run(TileOp::Mosaic, task, None)
            })?;
            worker.receiver.recv()??;
            let mut decoder = tiff::decoder::Decoder::new(std::fs::File::open(output)?)?;
            assert_eq!(decoder.dimensions()?, (512, 256));
            assert_eq!(decoder.more_images(), enabled);
            if enabled {
                decoder.next_image()?;
                assert_eq!(decoder.dimensions()?, (256, 128));
                assert_eq!(decoder.get_tag_u32(tiff::tags::Tag::NewSubfileType)?, 1);
            }
        }
        Ok(())
    }

    #[test]
    fn path_field_uses_one_control_row() {
        egui::__run_test_ui(|ui| {
            ui.allocate_ui(egui::vec2(500., 400.), |ui| {
                let start_y = ui.next_widget_position().y;
                let max_height = ui.text_style_height(&egui::TextStyle::Body)
                    + ui.spacing().interact_size.y
                    + 3. * ui.spacing().item_spacing.y
                    + 4.;
                path_field(ui, "输入路径", &mut String::new(), "", PathPicker::Folder);
                let height = ui.next_widget_position().y - start_y;
                assert!(
                    height <= max_height,
                    "路径字段高度为 {height}，上限为 {max_height}"
                );
            });
        });
    }
}

fn required_path(label: &str, value: &str) -> Result<PathBuf> {
    optional_path(value).with_context(|| message!(enter_a_value_for_label, label = label))
}

fn optional_text(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_owned())
}

fn optional_path(value: &str) -> Option<PathBuf> {
    optional_text(value).map(PathBuf::from)
}

fn mode_label(mode: dom::Mode) -> &'static str {
    match mode {
        dom::Mode::Relative => message!(relative_to_origin),
        dom::Mode::Absolute => message!(absolute_coordinates),
        dom::Mode::Local => message!(local_coordinates_no_crs),
    }
}

#[derive(Default)]
struct RasterForm {
    input: String,
    output: String,
    metadata: String,
    crs: String,
    origin: String,
    resolution: String,
    mode: dom::Mode,
    overwrite: bool,
}

impl RasterForm {
    fn ui(&mut self, ui: &mut Ui) {
        path_field(
            ui,
            message!(input_model),
            &mut self.input,
            message!(root_osgb_file_or_metadata_xml),
            PathPicker::File("OSGB / metadata.xml", &["osgb", "xml"]),
        );
        if path_field(
            ui,
            message!(output_folder),
            &mut self.output,
            message!(path_raster_output),
            PathPicker::Folder,
        ) {
            self.overwrite = false;
        }
        if !dom::is_metadata_file(Path::new(self.input.trim())) {
            path_field(
                ui,
                message!(metadata),
                &mut self.metadata,
                message!(leave_blank_to_find_metadata_xml_automatically),
                PathPicker::File("XML", &["xml"]),
            );
        }
        ui.horizontal_wrapped(|ui| {
            ui.label(message!(coordinate_mode));
            for mode in [dom::Mode::Relative, dom::Mode::Absolute, dom::Mode::Local] {
                ui.radio_value(&mut self.mode, mode, mode_label(mode));
            }
        });
        if self.mode != dom::Mode::Local {
            field(
                ui,
                message!(crs),
                &mut self.crs,
                message!(use_metadata_or_enter_epsg_wkt),
            );
        }
        if self.mode != dom::Mode::Absolute {
            field(
                ui,
                message!(xyz_origin),
                &mut self.origin,
                message!(use_metadata_or_enter_x_y_z),
            );
        }
        field(
            ui,
            message!(pixel_size_crs_units),
            &mut self.resolution,
            "0.05",
        );
        ui.checkbox(
            &mut self.overwrite,
            message!(overwrite_existing_rasters_with_the_same_names),
        );
    }

    fn request(&self) -> Result<dom::Request> {
        Ok(dom::Request {
            input: required_path(message!(input_model), &self.input)?,
            output: required_path(message!(output_folder), &self.output)?,
            metadata: optional_path(&self.metadata),
            crs: (self.mode != dom::Mode::Local)
                .then(|| optional_text(&self.crs))
                .flatten(),
            origin: if self.mode == dom::Mode::Absolute {
                None
            } else {
                optional_text(&self.origin)
                    .map(|origin| dom::parse_origin(&origin))
                    .transpose()?
            },
            mode: self.mode,
            resolution: self
                .resolution
                .trim()
                .parse()
                .context(message!(invalid_pixel_size))?,
            overwrite: self.overwrite,
        })
    }
}

#[derive(Default)]
struct ClipForm {
    model: String,
    entry: String,
    crs: String,
    origin: String,
    boundary: String,
    boundary_crs: String,
    output: String,
    layers: Vec<String>,
    layer: Option<String>,
    manual_reference: bool,
    manual_entry: bool,
    allow_nonempty: bool,
}

impl ClipForm {
    fn ui(&mut self, ui: &mut Ui) -> bool {
        path_field(
            ui,
            message!(input_model),
            &mut self.model,
            message!(osgb_file_or_folder_containing_metadata_xml),
            PathPicker::FileOrFolder("OSGB", &["osgb"]),
        );
        ui.checkbox(
            &mut self.manual_entry,
            message!(choose_the_folder_s_osgb_entry_manually),
        );
        if self.manual_entry {
            path_field(
                ui,
                message!(entry_file),
                &mut self.entry,
                message!(relative_path_within_the_folder_or_absolute_path),
                PathPicker::File("OSGB", &["osgb"]),
            );
        }
        ui.checkbox(
            &mut self.manual_reference,
            message!(set_the_crs_and_origin_for_a_single_model_file),
        );
        if self.manual_reference {
            field(
                ui,
                message!(model_crs),
                &mut self.crs,
                message!(epsg_wkt_enu_longitude_latitude),
            );
            field(ui, message!(model_xyz_origin), &mut self.origin, "X, Y, Z");
        }
        if path_field(
            ui,
            message!(clip_boundary),
            &mut self.boundary,
            message!(shp_zip_kml_or_geojson),
            PathPicker::File(
                message!(clip_boundary),
                &["shp", "zip", "kml", "geojson", "json"],
            ),
        ) {
            self.layers.clear();
            self.layer = None;
        }
        let mut load = false;
        if Path::new(self.boundary.trim())
            .extension()
            .is_some_and(|s| s.eq_ignore_ascii_case("zip"))
        {
            load = ui.button(message!(load_zip_layers)).clicked();
            if !self.layers.is_empty() {
                egui::ComboBox::from_id_salt("clip-layer")
                    .selected_text(self.layer.as_deref().unwrap_or(message!(select_a_layer)))
                    .show_ui(ui, |ui| {
                        for layer in &self.layers {
                            ui.selectable_value(&mut self.layer, Some(layer.clone()), layer);
                        }
                    });
            }
        }
        field(
            ui,
            message!(boundary_crs),
            &mut self.boundary_crs,
            message!(leave_blank_to_use_the_embedded_crs),
        );
        if path_field(
            ui,
            message!(output_folder),
            &mut self.output,
            message!(absolute_path_with_an_existing_parent_folder),
            PathPicker::Folder,
        ) {
            self.allow_nonempty = false;
        }
        ui.checkbox(
            &mut self.allow_nonempty,
            message!(write_results_to_a_nonempty_folder_and_protect_same_name_files),
        );
        load
    }

    fn request(&self) -> Result<clip::Request> {
        Ok(clip::Request {
            model: required_path(message!(input_model), &self.model)?,
            entry: if self.manual_entry {
                Some(required_path(message!(entry_file), &self.entry)?)
            } else {
                None
            },
            reference: if self.manual_reference {
                Some(clip::Reference {
                    srs: optional_text(&self.crs).context(message!(enter_the_model_crs))?,
                    origin: dom::parse_origin(&self.origin)?,
                })
            } else {
                None
            },
            boundary: required_path(message!(clip_boundary), &self.boundary)?,
            layer: self.layer.clone(),
            boundary_crs: optional_text(&self.boundary_crs),
            output: required_path(message!(output_folder), &self.output)?,
            allow_non_empty_output: self.allow_nonempty,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TileOp {
    Mosaic,
    Pyramid,
    Stacta,
    MapTiles,
}

impl TileOp {
    const ALL: [Self; 4] = [Self::Mosaic, Self::Pyramid, Self::Stacta, Self::MapTiles];

    fn name(self) -> &'static str {
        match self {
            Self::Mosaic => message!(mosaic_to_geotiff),
            Self::Pyramid => message!(generate_tile_pyramid),
            Self::Stacta => message!(generate_stacta),
            Self::MapTiles => message!(map_tiles),
        }
    }
}

#[derive(Clone)]
struct TileForm {
    build_overviews: bool,
    input: String,
    scheme: Scheme,
    zoom: u8,
    min_zoom: u8,
    max_zoom: u8,
    targets: String,
    output: String,
    resampling: Option<Resampling>,
    format: Format,
    background: String,
    datetime: String,
    id: String,
}

impl Default for TileForm {
    fn default() -> Self {
        Self {
            build_overviews: false,
            input: String::new(),
            scheme: Scheme::Xyz,
            zoom: 0,
            min_zoom: 6,
            max_zoom: 18,
            targets: String::new(),
            output: String::new(),
            resampling: None,
            format: Format::Png,
            background: "#FFFFFF".into(),
            datetime: String::new(),
            id: String::new(),
        }
    }
}

impl TileForm {
    fn ui(&mut self, ui: &mut Ui, operation: TileOp) {
        if operation == TileOp::MapTiles {
            path_field(
                ui,
                "GeoTIFF",
                &mut self.input,
                message!(georeferenced_tif_tiff_with_crs),
                PathPicker::File("GeoTIFF", &["tif", "tiff"]),
            );
        } else {
            path_field(
                ui,
                message!(tile_root_folder),
                &mut self.input,
                message!(folder_containing_z_x_y_images),
                PathPicker::Folder,
            );
            ui.horizontal(|ui| {
                ui.label(message!(tile_row_scheme));
                ui.radio_value(&mut self.scheme, Scheme::Xyz, "XYZ");
                ui.radio_value(&mut self.scheme, Scheme::Tms, "TMS");
            });
        }
        if matches!(operation, TileOp::Mosaic | TileOp::Pyramid) {
            ui.horizontal(|ui| {
                ui.label(message!(source_zoom));
                ui.add(egui::DragValue::new(&mut self.zoom).range(0..=30));
            });
        }
        match operation {
            TileOp::Mosaic => {
                path_field(
                    ui,
                    message!(output_geotiff),
                    &mut self.output,
                    message!(path_mosaic_tif),
                    PathPicker::SaveFile("GeoTIFF", &["tif", "tiff"], "mosaic.tif"),
                );
                ui.checkbox(
                    &mut self.build_overviews,
                    message!(generate_internal_pyramid),
                );
            }
            TileOp::Pyramid => {
                field(
                    ui,
                    message!(target_zoom_levels),
                    &mut self.targets,
                    message!(e_g_0_2_4),
                );
                ui.horizontal_wrapped(|ui| {
                    ui.label(message!(resampling));
                    for (value, label) in [
                        (None, message!(auto)),
                        (Some(Resampling::Nearest), message!(nearest_neighbor)),
                        (Some(Resampling::Bilinear), message!(bilinear)),
                        (Some(Resampling::Average), message!(average)),
                    ] {
                        ui.radio_value(&mut self.resampling, value, label);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label(message!(format));
                    for (format, name) in [
                        (Format::Png, "PNG"),
                        (Format::Jpeg, "JPEG"),
                        (Format::Webp, "WebP"),
                    ] {
                        ui.radio_value(&mut self.format, format, name);
                    }
                });
                if self.format == Format::Jpeg {
                    field(
                        ui,
                        message!(jpeg_background),
                        &mut self.background,
                        "#RRGGBB",
                    );
                }
                path_field(
                    ui,
                    message!(tile_output_folder),
                    &mut self.output,
                    message!(new_folder_2),
                    PathPicker::NewFolder,
                );
            }
            TileOp::Stacta => {
                field(
                    ui,
                    message!(time_rfc_3339),
                    &mut self.datetime,
                    message!(e_g_2026_01_01t00_00_00z),
                );
                field(
                    ui,
                    "STAC ID",
                    &mut self.id,
                    message!(defaults_to_the_tile_root_folder_name),
                );
                if self.scheme == Scheme::Tms {
                    path_field(
                        ui,
                        message!(xyz_output_folder),
                        &mut self.output,
                        message!(new_folder_2),
                        PathPicker::NewFolder,
                    );
                } else {
                    ui.label(message!(creates_stacta_json_in_the_input_folder));
                }
            }
            TileOp::MapTiles => {
                ui.horizontal(|ui| {
                    ui.label(message!(zoom_levels));
                    ui.label(message!(min));
                    ui.add(
                        egui::DragValue::new(&mut self.min_zoom)
                            .range(0..=dom::map_tiles::MAX_ZOOM),
                    );
                    ui.label(message!(max));
                    ui.add(
                        egui::DragValue::new(&mut self.max_zoom)
                            .range(0..=dom::map_tiles::MAX_ZOOM),
                    );
                });
                field(
                    ui,
                    message!(time_rfc_3339),
                    &mut self.datetime,
                    message!(e_g_2026_01_01t00_00_00z),
                );
                field(
                    ui,
                    "STAC ID",
                    &mut self.id,
                    message!(defaults_to_the_output_folder_name),
                );
                path_field(
                    ui,
                    message!(output_folder),
                    &mut self.output,
                    message!(new_folder_2),
                    PathPicker::NewFolder,
                );
            }
        }
    }

    fn run(
        self,
        operation: TileOp,
        task: &TaskContext,
        pool: Option<&ThreadPool>,
    ) -> Result<Outcome> {
        let input = required_path(
            if operation == TileOp::MapTiles {
                "GeoTIFF"
            } else {
                message!(tile_root_folder)
            },
            &self.input,
        )?;
        task.update(if operation == TileOp::MapTiles {
            TaskProgress::new(Phase::PrepareTiles, message!(prepare_map_tiles), None)
        } else {
            TaskProgress::new(Phase::ScanTiles, message!(scan_tiles), None)
        });
        let output = match operation {
            TileOp::Mosaic => {
                let output = required_path(message!(output_geotiff), &self.output)?;
                let index = Index::scan(&input, self.scheme)?;
                task.update(TaskProgress::new(
                    Phase::Mosaic,
                    message!(mosaic_to_geotiff),
                    None,
                ));
                let level = index.level(self.zoom)?;
                if self.build_overviews {
                    anyhow::ensure!(
                        !output.try_exists()?,
                        message!(output_file_already_exists, output.display())
                    );
                    let parent = output
                        .parent()
                        .filter(|p| !p.as_os_str().is_empty())
                        .unwrap_or(Path::new("."));
                    let scratch = tempfile::tempdir_in(parent)?;
                    let base = scratch.path().join("mosaic.tif");
                    mosaic::run_with_pool(level, self.zoom, &base, pool)?;
                    dom::cog::run_with_progress(
                        &base,
                        &output,
                        &task.cancelled,
                        |event| task.update(event),
                        || task.publish_with_progress(Some(0.95)),
                        pool,
                    )?;
                } else {
                    mosaic::run_with_publish(level, self.zoom, &output, pool, || task.publish())?;
                }
                output
            }
            TileOp::Pyramid => {
                let output = required_path(message!(tile_output_folder), &self.output)?;
                let options = pyramid::Options {
                    scheme: self.scheme,
                    source_zoom: self.zoom,
                    zooms: pyramid::parse_zooms(self.targets.trim())?,
                    resampling: self.resampling,
                    format: self.format,
                    background: if self.format == Format::Jpeg {
                        pyramid::parse_background(self.background.trim())?
                    } else {
                        [255; 3]
                    },
                };
                let index = Index::scan(&input, self.scheme)?;
                task.update(TaskProgress::new(
                    Phase::Pyramid,
                    message!(generate_tile_pyramid),
                    None,
                ));
                pyramid::run_with_publish(
                    index.level(self.zoom)?,
                    &options,
                    &output,
                    pool,
                    || task.publish(),
                )?;
                output
            }
            TileOp::Stacta => {
                let export = if self.scheme == Scheme::Tms {
                    Some(required_path(message!(xyz_output_folder), &self.output)?)
                } else {
                    None
                };
                let datetime = chrono::DateTime::parse_from_rfc3339(self.datetime.trim())
                    .context(message!(time_must_follow_rfc_3339))?;
                task.update(TaskProgress::new(
                    Phase::CreateStacta,
                    message!(validate_tiles_and_generate_stacta),
                    None,
                ));
                stacta::run_with_publish(
                    &input,
                    self.scheme,
                    export.as_deref(),
                    datetime,
                    optional_text(&self.id),
                    pool,
                    || task.publish(),
                )?;
                export.unwrap_or(input).join("stacta.json")
            }
            TileOp::MapTiles => {
                let output = required_path(message!(output_folder), &self.output)?;
                let datetime = chrono::DateTime::parse_from_rfc3339(self.datetime.trim())
                    .context(message!(time_must_follow_rfc_3339))?;
                let request = dom::map_tiles::Request {
                    input,
                    output: output.clone(),
                    min_zoom: self.min_zoom,
                    max_zoom: self.max_zoom,
                    datetime,
                    id: optional_text(&self.id),
                };
                let levels = dom::map_tiles::run_with_progress(
                    &request,
                    &task.cancelled,
                    |event| task.update(event),
                    || task.publish_with_progress(Some(0.95)),
                    pool,
                )?;
                let missing: Vec<_> = (self.min_zoom..=self.max_zoom)
                    .filter(|z| !levels.contains(z))
                    .map(|z| z.to_string())
                    .collect();
                let details = if missing.is_empty() {
                    None
                } else {
                    Some(message!(
                        no_valid_tiles_at_these_zoom_levels,
                        missing.join("、")
                    ))
                };
                return Ok(Outcome::Artifacts {
                    paths: vec![output.canonicalize().with_context(|| {
                        message!(could_not_resolve_output_path, output.display())
                    })?],
                    details,
                });
            }
        };
        Ok(Outcome::Artifacts {
            paths: vec![
                output
                    .canonicalize()
                    .with_context(|| message!(could_not_resolve_output_path, output.display()))?,
            ],
            details: None,
        })
    }
}
