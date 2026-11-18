use anyhow::{Context, Result, anyhow, ensure};
use eframe::egui::{self, Color32, Context as EguiContext, RichText, Ui};
use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
};
use tiles_toolbox::{
    mosaic,
    pyramid::{self, Resampling},
    stacta,
    tiles::{Format, Index, Scheme},
};
use webgis_core::model_clip as clip;
use webgis_toolbox::dom;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tool {
    Raster,
    Tiles,
    Clip,
}
impl Tool {
    fn name(self) -> &'static str {
        match self {
            Self::Raster => "OSGB 转 DOM / DSM",
            Self::Tiles => "瓦片后处理",
            Self::Clip => "OSGB 模型裁剪",
        }
    }
}

pub struct App {
    selected: Tool,
    raster: RasterForm,
    tiles: TileForm,
    clip: ClipForm,
    worker: Option<Worker>,
    outcome: Option<Result<Outcome>>,
    font_loaded: bool,
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

#[derive(Clone)]
struct Stage {
    name: String,
    fraction: Option<f32>,
    publishing: bool,
}

struct TaskContext {
    egui: EguiContext,
    cancelled: Arc<AtomicBool>,
    stage: Arc<Mutex<Stage>>,
}

impl TaskContext {
    fn update(&self, name: impl Into<String>, fraction: Option<f32>) {
        *self.stage.lock().unwrap() = Stage {
            name: name.into(),
            fraction,
            publishing: false,
        };
        self.egui.request_repaint();
    }

    fn publish(&self) -> Result<()> {
        let mut stage = self.stage.lock().unwrap();
        ensure!(!self.cancelled.load(Ordering::Relaxed), "处理已取消");
        *stage = Stage {
            name: "发布结果".into(),
            fraction: None,
            publishing: true,
        };
        self.egui.request_repaint();
        Ok(())
    }
}

struct Worker {
    cancelled: Arc<AtomicBool>,
    cancellable: bool,
    stage: Arc<Mutex<Stage>>,
    receiver: mpsc::Receiver<Result<Outcome>>,
}

impl Worker {
    fn start(
        egui: &EguiContext,
        cancellable: bool,
        work: impl FnOnce(&TaskContext) -> Result<Outcome> + Send + 'static,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let stage = Arc::new(Mutex::new(Stage {
            name: "准备任务".into(),
            fraction: None,
            publishing: false,
        }));
        let task = TaskContext {
            egui: egui.clone(),
            cancelled: cancelled.clone(),
            stage: stage.clone(),
        };
        std::thread::Builder::new()
            .name("webgis-task".into())
            .spawn(move || {
                let result = work(&task);
                let result = if result.is_err() && task.cancelled.load(Ordering::Relaxed) {
                    Ok(Outcome::Cancelled)
                } else {
                    result
                };
                let _ = sender.send(result);
                task.egui.request_repaint();
            })?;
        Ok(Self {
            cancelled,
            cancellable,
            stage,
            receiver,
        })
    }
}

impl App {
    pub fn new(ctx: &EguiContext) -> Self {
        Self {
            selected: Tool::Raster,
            raster: RasterForm {
                resolution: "0.05".into(),
                ..Default::default()
            },
            tiles: TileForm::default(),
            clip: ClipForm::default(),
            worker: None,
            outcome: None,
            font_loaded: install_font(ctx),
        }
    }

    fn poll(&mut self) {
        if let Some(worker) = &self.worker {
            let result = match worker.receiver.try_recv() {
                Ok(result) => result,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Err(anyhow!("后台处理线程异常退出")),
            };
            if let Ok(Outcome::Layers { path, layers }) = &result
                && path == Path::new(self.clip.boundary.trim())
            {
                self.clip.layers.clone_from(layers);
                self.clip.layer = (layers.len() == 1).then(|| layers[0].clone());
            }
            self.outcome = Some(result);
            self.worker = None;
        }
    }

    fn start(&mut self, ctx: &EguiContext, action: Action) -> Result<()> {
        self.outcome = None;
        self.worker = Some(match (self.selected, action) {
            (Tool::Raster, Action::Preview | Action::Run) => {
                let request = self.raster.request()?;
                Worker::start(ctx, true, move |task| {
                    let update = |event: dom::Progress| task.update(event.stage, event.fraction);
                    if action == Action::Preview {
                        Ok(Outcome::RasterPreview(dom::preflight(
                            &request,
                            &task.cancelled,
                            update,
                        )?))
                    } else {
                        Ok(Outcome::Artifacts {
                            paths: dom::run(&request, &task.cancelled, update, || task.publish())?,
                            details: None,
                        })
                    }
                })?
            }
            (Tool::Clip, Action::Layers) => {
                let path = required_path("裁剪范围", &self.clip.boundary)?;
                Worker::start(ctx, false, move |task| {
                    task.update("扫描 ZIP 图层", None);
                    let layers = clip::zip_layers(&path).map_err(|e| anyhow!(e))?;
                    Ok(Outcome::Layers { path, layers })
                })?
            }
            (Tool::Clip, Action::Preview | Action::Run) => {
                let request = self.clip.request()?;
                Worker::start(ctx, true, move |task| {
                    task.update("预检模型、裁剪范围与输出", None);
                    if action == Action::Preview {
                        return clip::preflight_cancelable(&request, &task.cancelled)
                            .map(Outcome::ClipPreview)
                            .map_err(|e| anyhow!(e));
                    }
                    let result = clip::run(
                        &request,
                        &task.cancelled,
                        |p| {
                            task.update(
                                format!("裁剪 {}", p.file.display()),
                                Some(p.completed as f32 / p.discovered as f32),
                            );
                        },
                        || task.publish().map_err(|err| err.to_string()),
                    )
                    .map_err(|e| anyhow!(e))?;
                    let details = format!(
                        "已写出 {} 个 OSGB 文件，共 {} 个三角形。",
                        result.files, result.triangles
                    );
                    Ok(Outcome::Artifacts {
                        paths: std::iter::once(result.output)
                            .chain(result.entries)
                            .collect(),
                        details: Some(details),
                    })
                })?
            }
            (Tool::Tiles, Action::Run) => {
                let form = self.tiles.clone();
                Worker::start(ctx, false, move |task| form.run(task))?
            }
            _ => unreachable!(),
        });
        Ok(())
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &EguiContext, _: &mut eframe::Frame) {
        self.poll();
        let busy = self.worker.is_some();
        if busy {
            ctx.request_repaint_after(std::time::Duration::from_millis(200));
        }
        if busy && ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        egui::SidePanel::left("tools")
            .exact_width(212.)
            .resizable(false)
            .show(ctx, |ui| {
                ui.add_space(12.);
                ui.heading("WebGIS 工具箱");
                ui.add_space(16.);
                ui.add_enabled_ui(!busy, |ui| {
                    for tool in [Tool::Raster, Tool::Tiles, Tool::Clip] {
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
                });
            });
        let mut action = None;
        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(12.);
                ui.heading(self.selected.name());
                if !self.font_loaded {
                    ui.label("Install a Chinese font or set WEBGIS_CJK_FONT to a font path.");
                }
                ui.add_space(14.);
                ui.add_enabled_ui(!busy, |ui| {
                    match self.selected {
                        Tool::Raster => self.raster.ui(ui),
                        Tool::Tiles => self.tiles.ui(ui),
                        Tool::Clip => {
                            if self.clip.ui(ui) {
                                action = Some(Action::Layers);
                            }
                        }
                    }
                    ui.add_space(10.);
                    ui.horizontal(|ui| {
                        if self.selected != Tool::Tiles && ui.button("预检").clicked() {
                            action = Some(Action::Preview);
                        }
                        if ui.button(RichText::new("开始处理").strong()).clicked() {
                            action = Some(Action::Run);
                        }
                    });
                });
                if let Some(worker) = &self.worker {
                    ui.add_space(10.);
                    ui.separator();
                    let stage = worker.stage.lock().unwrap().clone();
                    ui.label(stage.name);
                    if let Some(value) = stage.fraction {
                        ui.add(egui::ProgressBar::new(value).show_percentage());
                    } else {
                        ui.spinner();
                    }
                    if worker.cancellable && !stage.publishing {
                        let pending = worker.cancelled.load(Ordering::Relaxed);
                        if ui
                            .add_enabled(
                                !pending,
                                egui::Button::new(if pending {
                                    "正在取消"
                                } else {
                                    "取消任务"
                                }),
                            )
                            .clicked()
                        {
                            let stage = worker.stage.lock().unwrap();
                            if !stage.publishing {
                                worker.cancelled.store(true, Ordering::Relaxed);
                            }
                        }
                    }
                }
                if let Some(outcome) = &self.outcome {
                    ui.add_space(12.);
                    ui.separator();
                    show_outcome(ui, outcome);
                }
            });
        });
        if let Some(action) = action
            && let Err(err) = self.start(ctx, action)
        {
            self.outcome = Some(Err(err));
        }
    }
}

fn install_font(ctx: &EguiContext) -> bool {
    let candidates = [
        "/usr/share/fonts/adobe-source-han-sans/SourceHanSansCN-Regular.otf",
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    ];
    let configured = std::env::var_os("WEBGIS_CJK_FONT").map(PathBuf::from);
    for path in configured.into_iter().chain(candidates.map(PathBuf::from)) {
        if let Ok(bytes) = std::fs::read(&path) {
            let mut fonts = egui::FontDefinitions::default();
            let mut font = egui::FontData::from_owned(bytes);
            // The Noto CJK collection orders JP, KR, SC, TC, HK.
            if path
                .file_name()
                .is_some_and(|p| p == "NotoSansCJK-Regular.ttc")
            {
                font.index = 2;
            }
            fonts.font_data.insert("Chinese".into(), Arc::new(font));
            for family in fonts.families.values_mut() {
                family.push("Chinese".into());
            }
            ctx.set_fonts(fonts);
            return true;
        }
    }
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
            if ui.small_button("复制路径").clicked() {
                ui.ctx().copy_text(value);
            }
        });
    }
}

fn show_outcome(ui: &mut Ui, outcome: &Result<Outcome>) {
    match outcome {
        Ok(Outcome::Cancelled) => {
            ui.label("任务已取消");
        }
        Err(error) => {
            ui.label(
                RichText::new("处理失败")
                    .color(Color32::from_rgb(220, 82, 75))
                    .strong(),
            );
            ui.add(egui::Label::new(format!("{error:#}")).selectable(true));
        }
        Ok(Outcome::RasterPreview(summary)) => {
            ui.strong("DOM / DSM 预检");
            ui.label(format!(
                "坐标模式　{}　　坐标系　{}",
                mode_label(summary.mode),
                summary.crs.as_deref().unwrap_or("本地")
            ));
            if let Some(path) = &summary.metadata {
                ui.label(format!("元数据　{}", path.display()));
            }
            let [x, y, z, east, north, high] = summary.bounds;
            ui.label(format!(
                "范围　X {x} 至 {east}　Y {y} 至 {north}　Z {z} 至 {high}"
            ));
            ui.label(format!("原点　{:?}", summary.origin));
            let [w, h] = summary.size;
            ui.label(format!(
                "栅格　{w} × {h}　　像元尺寸　{}",
                summary.resolution
            ));
            ui.label(format!(
                "选定模型文件及 LOD（{} 个）　{} 个三角形",
                summary.selected.len(),
                summary.triangles
            ));
            paths(ui, &summary.selected);
        }
        Ok(Outcome::ClipPreview(summary)) => {
            ui.strong("裁剪预检");
            ui.label(format!(
                "模型坐标系　{}　　原点　{:?}",
                summary.reference.srs, summary.reference.origin
            ));
            ui.label(format!("输出目录　{}", summary.output.display()));
            if summary.output_non_empty {
                ui.label("输出目录非空。运行前需要确认，同名产物仍受保护。");
            }
            ui.label("模型入口");
            paths(ui, &summary.entries);
        }
        Ok(Outcome::Layers { layers, .. }) => {
            ui.label(format!("找到 {} 个 SHP 图层。", layers.len()));
        }
        Ok(Outcome::Artifacts {
            paths: files,
            details,
        }) => {
            ui.strong("处理完成");
            if let Some(details) = details {
                ui.label(details);
            }
            paths(ui, files);
        }
    }
}

fn field(ui: &mut Ui, label: &str, value: &mut String, hint: &str) -> bool {
    ui.label(label);
    let changed = ui
        .add(
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .desired_width(f32::INFINITY),
        )
        .changed();
    ui.add_space(4.);
    changed
}

fn required_path(label: &str, value: &str) -> Result<PathBuf> {
    optional_path(value).with_context(|| format!("请填写{label}"))
}

fn optional_text(value: &str) -> Option<String> {
    (!value.trim().is_empty()).then(|| value.trim().to_owned())
}

fn optional_path(value: &str) -> Option<PathBuf> {
    optional_text(value).map(PathBuf::from)
}

fn mode_label(mode: dom::Mode) -> &'static str {
    match mode {
        dom::Mode::Relative => "相对原点",
        dom::Mode::Absolute => "绝对坐标",
        dom::Mode::Local => "本地坐标（无 CRS）",
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
        field(ui, "模型输入", &mut self.input, "OSGB 文件或模型目录");
        if field(ui, "输出目录", &mut self.output, "/路径/栅格成果") {
            self.overwrite = false;
        }
        field(
            ui,
            "元数据",
            &mut self.metadata,
            "留空自动发现 metadata.xml",
        );
        ui.horizontal_wrapped(|ui| {
            ui.label("坐标模式");
            for mode in [dom::Mode::Relative, dom::Mode::Absolute, dom::Mode::Local] {
                ui.radio_value(&mut self.mode, mode, mode_label(mode));
            }
        });
        if self.mode != dom::Mode::Local {
            field(
                ui,
                "坐标系",
                &mut self.crs,
                "留空使用元数据，或输入 EPSG / WKT",
            );
        }
        if self.mode != dom::Mode::Absolute {
            field(
                ui,
                "XYZ 原点",
                &mut self.origin,
                "留空使用元数据，或输入 X, Y, Z",
            );
        }
        field(ui, "像元尺寸（坐标单位）", &mut self.resolution, "0.05");
        ui.checkbox(&mut self.overwrite, "确认覆盖输出目录中的同名栅格产物");
    }

    fn request(&self) -> Result<dom::Request> {
        Ok(dom::Request {
            input: required_path("模型输入", &self.input)?,
            output: required_path("输出目录", &self.output)?,
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
            resolution: self.resolution.trim().parse().context("无效的像元尺寸")?,
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
        field(
            ui,
            "模型输入",
            &mut self.model,
            "OSGB 文件或包含 metadata.xml 的目录",
        );
        ui.checkbox(&mut self.manual_entry, "手选目录的 OSGB 入口");
        if self.manual_entry {
            field(ui, "入口文件", &mut self.entry, "目录内相对路径或绝对路径");
        }
        ui.checkbox(&mut self.manual_reference, "设置单文件的模型坐标系与原点");
        if self.manual_reference {
            field(
                ui,
                "模型坐标系",
                &mut self.crs,
                "EPSG / WKT / ENU:经度,纬度",
            );
            field(ui, "模型 XYZ 原点", &mut self.origin, "X, Y, Z");
        }
        if field(
            ui,
            "裁剪范围",
            &mut self.boundary,
            "SHP、ZIP、KML 或 GeoJSON",
        ) {
            self.layers.clear();
            self.layer = None;
        }
        let mut load = false;
        if Path::new(self.boundary.trim())
            .extension()
            .is_some_and(|s| s.eq_ignore_ascii_case("zip"))
        {
            load = ui.button("读取 ZIP 图层").clicked();
            if !self.layers.is_empty() {
                egui::ComboBox::from_id_salt("clip-layer")
                    .selected_text(self.layer.as_deref().unwrap_or("请选择图层"))
                    .show_ui(ui, |ui| {
                        for layer in &self.layers {
                            ui.selectable_value(&mut self.layer, Some(layer.clone()), layer);
                        }
                    });
            }
        }
        field(
            ui,
            "裁剪范围坐标系",
            &mut self.boundary_crs,
            "留空使用数据自带坐标系",
        );
        if field(ui, "输出目录", &mut self.output, "绝对路径，父目录须存在") {
            self.allow_nonempty = false;
        }
        ui.checkbox(
            &mut self.allow_nonempty,
            "确认向非空目录发布裁剪结果（保护同名文件）",
        );
        load
    }

    fn request(&self) -> Result<clip::Request> {
        Ok(clip::Request {
            model: required_path("模型输入", &self.model)?,
            entry: if self.manual_entry {
                Some(required_path("入口文件", &self.entry)?)
            } else {
                None
            },
            reference: if self.manual_reference {
                Some(clip::Reference {
                    srs: optional_text(&self.crs).context("请填写模型坐标系")?,
                    origin: dom::parse_origin(&self.origin)?,
                })
            } else {
                None
            },
            boundary: required_path("裁剪范围", &self.boundary)?,
            layer: self.layer.clone(),
            boundary_crs: optional_text(&self.boundary_crs),
            output: required_path("输出目录", &self.output)?,
            allow_non_empty_output: self.allow_nonempty,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TileOp {
    Mosaic,
    Pyramid,
    Stacta,
}

#[derive(Clone)]
struct TileForm {
    operation: TileOp,
    input: String,
    scheme: Scheme,
    zoom: u8,
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
            operation: TileOp::Mosaic,
            input: String::new(),
            scheme: Scheme::Xyz,
            zoom: 0,
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
    fn ui(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            for (op, label) in [
                (TileOp::Mosaic, "拼接 GeoTIFF"),
                (TileOp::Pyramid, "生成金字塔"),
                (TileOp::Stacta, "生成 STACTA"),
            ] {
                ui.radio_value(&mut self.operation, op, label);
            }
        });
        field(ui, "瓦片根目录", &mut self.input, "包含 z/x/y 图像的目录");
        ui.horizontal(|ui| {
            ui.label("瓦片行号");
            ui.radio_value(&mut self.scheme, Scheme::Xyz, "XYZ");
            ui.radio_value(&mut self.scheme, Scheme::Tms, "TMS");
        });
        if self.operation != TileOp::Stacta {
            ui.horizontal(|ui| {
                ui.label("源级别");
                ui.add(egui::DragValue::new(&mut self.zoom).range(0..=30));
            });
        }
        match self.operation {
            TileOp::Mosaic => {
                field(ui, "GeoTIFF 输出文件", &mut self.output, "/路径/mosaic.tif");
            }
            TileOp::Pyramid => {
                field(ui, "目标级别", &mut self.targets, "例如 0-2,4");
                ui.horizontal_wrapped(|ui| {
                    ui.label("重采样");
                    for (value, label) in [
                        (None, "自动"),
                        (Some(Resampling::Nearest), "最近邻"),
                        (Some(Resampling::Bilinear), "双线性"),
                        (Some(Resampling::Average), "平均"),
                    ] {
                        ui.radio_value(&mut self.resampling, value, label);
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("格式");
                    for (format, name) in [
                        (Format::Png, "PNG"),
                        (Format::Jpeg, "JPEG"),
                        (Format::Webp, "WebP"),
                    ] {
                        ui.radio_value(&mut self.format, format, name);
                    }
                });
                if self.format == Format::Jpeg {
                    field(ui, "JPEG 背景", &mut self.background, "#RRGGBB");
                }
                field(ui, "金字塔输出目录", &mut self.output, "新目录");
            }
            TileOp::Stacta => {
                field(
                    ui,
                    "时间（RFC 3339）",
                    &mut self.datetime,
                    "例如 2026-01-01T00:00:00Z",
                );
                field(ui, "STAC ID", &mut self.id, "留空使用瓦片根目录名称");
                if self.scheme == Scheme::Tms {
                    field(ui, "XYZ 输出目录", &mut self.output, "新目录");
                } else {
                    ui.label("在源目录生成 stacta.json。");
                }
            }
        }
    }

    fn run(self, task: &TaskContext) -> Result<Outcome> {
        let input = required_path("瓦片根目录", &self.input)?;
        task.update("扫描瓦片", None);
        let output = match self.operation {
            TileOp::Mosaic => {
                let output = required_path("GeoTIFF 输出文件", &self.output)?;
                let index = Index::scan(&input, self.scheme)?;
                task.update("拼接 GeoTIFF", None);
                mosaic::run(index.level(self.zoom)?, self.zoom, &output)?;
                output
            }
            TileOp::Pyramid => {
                let output = required_path("金字塔输出目录", &self.output)?;
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
                task.update("生成切片金字塔", None);
                pyramid::run(index.level(self.zoom)?, &options, &output)?;
                output
            }
            TileOp::Stacta => {
                let export = if self.scheme == Scheme::Tms {
                    Some(required_path("XYZ 输出目录", &self.output)?)
                } else {
                    None
                };
                let datetime = chrono::DateTime::parse_from_rfc3339(self.datetime.trim())
                    .context("时间须符合 RFC 3339")?;
                task.update("校验瓦片并生成 STACTA", None);
                stacta::run(
                    &input,
                    self.scheme,
                    export.as_deref(),
                    datetime,
                    optional_text(&self.id),
                )?;
                export.unwrap_or(input).join("stacta.json")
            }
        };
        Ok(Outcome::Artifacts {
            paths: vec![output.canonicalize()?],
            details: None,
        })
    }
}
