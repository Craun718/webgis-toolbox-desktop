use super::Outcome;
use crate::console;
use anyhow::{Context, Result, ensure};
use eframe::egui;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use webgis_core::{
    i18n::{self, Language},
    message,
};
use webgis_toolbox::dom::{Phase, TaskProgress};

const LOG_LIMIT: usize = 2_000;

struct Stage {
    phase: Phase,
    name: String,
    fraction: Option<f32>,
    publishing: bool,
}

struct TaskState {
    name: String,
    stage: Stage,
    last_progress_log: Option<Instant>,
    final_status: Option<&'static str>,
    logs: VecDeque<String>,
}

impl TaskState {
    fn new(name: &str) -> Self {
        let mut state = Self {
            name: name.into(),
            stage: Stage {
                phase: Phase::Prepare,
                name: message!(prepare_task).into(),
                fraction: None,
                publishing: false,
            },
            last_progress_log: None,
            final_status: None,
            logs: VecDeque::new(),
        };
        state.log(console::Level::Info, message!(task_started));
        state
    }

    fn log(&mut self, level: console::Level, message: &str) {
        if self.logs.len() == LOG_LIMIT {
            self.logs.pop_front();
        }
        self.logs
            .push_back(console::record(level, &self.name, message));
    }

    fn update(&mut self, event: TaskProgress, now: Instant) {
        let changed = self.stage.phase != event.phase;
        self.stage.phase = event.phase;
        self.stage.name = event.display.stage;
        let fraction = event.display.fraction;
        self.stage.fraction = fraction;
        if changed
            || self
                .last_progress_log
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
        {
            let message = match fraction {
                Some(fraction) => message!(progress_percentage, self.stage.name, fraction * 100.0),
                None => self.stage.name.clone(),
            };
            self.log(console::Level::Info, &message);
            self.last_progress_log = Some(now);
        }
    }

    fn request_cancel(&mut self, control: &TaskControl) {
        if control.cancellable
            && self.final_status.is_none()
            && !self.stage.publishing
            && !control.cancelled.swap(true, Ordering::Relaxed)
        {
            self.log(console::Level::Info, message!(cancel_requested));
        }
    }

    fn finish(&mut self, result: &Result<Outcome>) {
        let (status, level) = match result {
            Ok(Outcome::Cancelled) => (message!(task_canceled), console::Level::Info),
            Ok(_) => (message!(task_completed), console::Level::Info),
            Err(_) => (message!(task_failed), console::Level::Error),
        };
        self.final_status = Some(status);
        match result {
            Err(error) => self.log(
                level,
                &message!(status_error, status = status, error = error),
            ),
            _ => self.log(level, status),
        }
    }
}

struct TaskControl {
    language: Language,
    cancellable: bool,
    cancelled: Arc<AtomicBool>,
    state: Mutex<TaskState>,
}

pub(super) struct TaskContext {
    pub(super) cancelled: Arc<AtomicBool>,
    control: Arc<TaskControl>,
}

impl TaskContext {
    pub(super) fn update(&self, event: TaskProgress) {
        self.control
            .state
            .lock()
            .unwrap()
            .update(event, Instant::now());
    }

    pub(super) fn log(&self, message: &str) {
        self.control
            .state
            .lock()
            .unwrap()
            .log(console::Level::Info, message);
    }

    pub(super) fn publish(&self) -> Result<()> {
        self.publish_with_progress(None)
    }

    pub(super) fn publish_with_progress(&self, fraction: Option<f32>) -> Result<()> {
        let mut state = self.control.state.lock().unwrap();
        ensure!(
            !self.cancelled.load(Ordering::Relaxed),
            message!(processing_canceled)
        );
        state.stage = Stage {
            phase: Phase::Publish,
            name: message!(save_results).into(),
            fraction,
            publishing: true,
        };
        state.log(console::Level::Info, message!(save_results));
        Ok(())
    }
}

pub(super) struct Worker {
    control: Arc<TaskControl>,
    pub(super) receiver: mpsc::Receiver<Result<Outcome>>,
}

impl Worker {
    pub(super) fn start(
        egui: &egui::Context,
        name: &str,
        cancellable: bool,
        work: impl FnOnce(&TaskContext) -> Result<Outcome> + Send + 'static,
    ) -> Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let language = i18n::language();
        let control = Arc::new(TaskControl {
            language,
            cancellable,
            cancelled: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(TaskState::new(name)),
        });
        let egui = egui.clone();
        let task = TaskContext {
            cancelled: control.cancelled.clone(),
            control: control.clone(),
        };
        std::thread::Builder::new()
            .name("webgis-task".into())
            .spawn(move || {
                i18n::with_language(language, || {
                    let result = work(&task);
                    let mut state = task.control.state.lock().unwrap();
                    let result = if result.is_err() && task.cancelled.load(Ordering::Relaxed) {
                        Ok(Outcome::Cancelled)
                    } else {
                        result
                    };
                    state.finish(&result);
                    drop(state);
                    let _ = sender.send(result);
                    egui.request_repaint_of(egui::ViewportId::ROOT);
                })
            })
            .context(message!(could_not_start_the_background_worker))?;
        Ok(Self { control, receiver })
    }

    pub(super) fn window(&self, sequence: u64) -> TaskWindow {
        TaskWindow {
            control: self.control.clone(),
            id: egui::ViewportId::from_hash_of(("task-window", sequence)),
            closed: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(super) fn language(&self) -> Language {
        self.control.language
    }

    pub(super) fn finish_disconnected(&self, result: &Result<Outcome>) {
        i18n::with_language(self.language(), || {
            self.control.state.lock().unwrap().finish(result)
        });
    }

    pub(super) fn close_notice(&self) -> &'static str {
        let state = self.control.state.lock().unwrap();
        if self.control.cancelled.load(Ordering::Relaxed) {
            message!(wait_for_cancel_before_closing)
        } else if self.control.cancellable && !state.stage.publishing {
            message!(cancel_before_closing)
        } else {
            message!(wait_for_task_before_closing)
        }
    }

    pub(super) fn show_cancel_button(&self, ui: &mut egui::Ui) {
        if self.control.cancellable {
            let mut state = self.control.state.lock().unwrap();
            let enabled = state.final_status.is_none()
                && !state.stage.publishing
                && !self.control.cancelled.load(Ordering::Relaxed);
            if ui
                .add_enabled(enabled, egui::Button::new(message!(cancel_task)))
                .clicked()
            {
                state.request_cancel(&self.control);
            }
        }
    }
}

#[derive(Clone)]
pub(super) struct TaskWindow {
    control: Arc<TaskControl>,
    id: egui::ViewportId,
    closed: Arc<AtomicBool>,
}

impl TaskWindow {
    pub(super) fn show(&self, ctx: &egui::Context, busy: bool) -> bool {
        if self.closed.load(Ordering::Relaxed) {
            return true;
        }

        let title = self.control.state.lock().unwrap().name.clone();
        let window = self.clone();
        ctx.show_viewport_deferred(
            self.id,
            egui::ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size([720., 410.])
                .with_min_inner_size([360., 220.])
                .with_resizable(true),
            move |ctx, class| {
                if busy {
                    ctx.request_repaint_after(super::TASK_REFRESH_INTERVAL);
                }
                let embedded = class == egui::ViewportClass::Embedded;
                if !embedded && ctx.input(|input| input.viewport().close_requested()) {
                    if !busy {
                        window.close(ctx);
                        return;
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }

                let content = |ui: &mut egui::Ui| {
                    let close =
                        i18n::with_language(window.control.language, || window.content(ui, busy));
                    if close {
                        window.close(ctx);
                    }
                };
                if embedded {
                    egui::Window::new(&title)
                        .id(egui::Id::new(window.id))
                        .show(ctx, content);
                } else {
                    egui::CentralPanel::default().show(ctx, content);
                }
            },
        );
        self.closed.load(Ordering::Relaxed)
    }

    fn close(&self, ctx: &egui::Context) {
        self.closed.store(true, Ordering::Relaxed);
        ctx.request_repaint_of(egui::ViewportId::ROOT);
    }

    fn content(&self, ui: &mut egui::Ui, busy: bool) -> bool {
        let (stage, fraction, publishing, status, pending, log) = {
            let state = self.control.state.lock().unwrap();
            (
                state.stage.name.clone(),
                state.stage.fraction,
                state.stage.publishing,
                state.final_status,
                self.control.cancelled.load(Ordering::Relaxed),
                state
                    .logs
                    .iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
        };
        let mut close = false;
        ui.horizontal_wrapped(|ui| {
            ui.label(status.unwrap_or(if pending { message!(canceling) } else { &stage }));
            if status.is_none() && fraction.is_none() {
                ui.spinner();
            }
            if status.is_none() {
                if self.control.cancellable {
                    if ui
                        .add_enabled(
                            !publishing && !pending,
                            egui::Button::new(message!(cancel_task)),
                        )
                        .clicked()
                    {
                        self.control
                            .state
                            .lock()
                            .unwrap()
                            .request_cancel(&self.control);
                        ui.ctx().request_repaint_of(egui::ViewportId::ROOT);
                    }
                } else {
                    ui.weak(message!(this_task_cannot_be_canceled));
                }
            } else {
                close = ui
                    .add_enabled(!busy, egui::Button::new(message!(close)))
                    .clicked();
            }
            if ui.button(message!(copy_log)).clicked() {
                ui.ctx().copy_text(log.clone());
            }
        });
        if status.is_none()
            && let Some(value) = fraction
        {
            ui.add(egui::ProgressBar::new(value).show_percentage());
        }

        ui.separator();
        let mut read_only = log.as_str();
        let font_id = egui::TextStyle::Monospace.resolve(ui.style());
        let mut layouter = |ui: &egui::Ui, text: &dyn egui::TextBuffer, wrap_width| {
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = wrap_width;
            let normal = egui::TextFormat::simple(font_id.clone(), ui.visuals().text_color());
            let error = egui::TextFormat::simple(font_id.clone(), ui.visuals().error_fg_color);
            for line in text.as_str().split_inclusive('\n') {
                if let Some(body) = line.strip_prefix(console::Level::ERROR_LABEL) {
                    job.append(console::Level::ERROR_LABEL, 0.0, error.clone());
                    job.append(body, 0.0, normal.clone());
                } else {
                    job.append(line, 0.0, normal.clone());
                }
            }
            ui.fonts_mut(|fonts| fonts.layout_job(job))
        };
        egui::ScrollArea::both()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut read_only)
                        .code_editor()
                        .layouter(&mut layouter)
                        .desired_rows(1)
                        .desired_width(f32::INFINITY),
                );
            });
        close
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{anyhow, bail};

    #[test]
    fn task_window_registers_an_independent_native_repaint_callback() {
        let ctx = egui::Context::default();
        ctx.set_embed_viewports(false);
        let window = TaskWindow {
            control: Arc::new(TaskControl {
                language: i18n::language(),
                cancellable: false,
                cancelled: Arc::new(AtomicBool::new(false)),
                state: Mutex::new(TaskState::new("test task")),
            }),
            id: egui::ViewportId::from_hash_of("test task"),
            closed: Arc::new(AtomicBool::new(false)),
        };

        let output = ctx.run(egui::RawInput::default(), |ctx| {
            assert!(!window.show(ctx, true));
        });

        let viewport = &output.viewport_output[&window.id];
        assert!(viewport.class == egui::ViewportClass::Deferred);
        assert!(viewport.viewport_ui_cb.is_some());
    }

    #[test]
    fn progress_logs_on_phase_change_and_at_most_once_per_second_within_a_phase() {
        let mut state = TaskState::new("处理");
        let start = Instant::now();
        state.update(
            TaskProgress::new(Phase::ReadModel, "读取模型 a.osgb", Some(0.0)),
            start,
        );
        state.update(
            TaskProgress::new(Phase::ReadModel, "Different display text", Some(0.4)),
            start + Duration::from_millis(999),
        );
        assert_eq!(state.logs.len(), 2);
        assert_eq!(state.stage.name, "Different display text");
        assert_eq!(state.stage.fraction, Some(0.4));

        state.update(
            TaskProgress::new(Phase::ReadModel, "读取模型 c.osgb", Some(0.5)),
            start + Duration::from_secs(1),
        );
        state.update(
            TaskProgress::new(Phase::Rasterize, "栅格化 1/2 块", Some(0.5)),
            start + Duration::from_millis(1001),
        );
        state.update(
            TaskProgress::new(Phase::Rasterize, "栅格化 2/2 块", Some(1.0)),
            start + Duration::from_millis(1002),
        );
        assert_eq!(state.logs.len(), 4);
        assert!(state.logs[2].ends_with("读取模型 c.osgb（50%）"));
        assert!(state.logs[3].ends_with("栅格化 1/2 块（50%）"));
    }

    #[test]
    fn cancel_records_once_and_finishes_when_work_returns() {
        let (release, wait) = mpsc::channel();
        let worker = Worker::start(&egui::Context::default(), "预检", true, move |task| {
            wait.recv_timeout(Duration::from_secs(5))?;
            task.publish()?;
            unreachable!("收到取消请求后不能保存结果");
        })
        .unwrap();

        {
            let mut state = worker.control.state.lock().unwrap();
            state.request_cancel(&worker.control);
            state.request_cancel(&worker.control);
            assert_eq!(state.logs.len(), 2);
            assert!(state.logs[1].contains("已请求取消"));
            assert!(state.final_status.is_none());
        }
        release.send(()).unwrap();
        assert!(matches!(
            worker
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap(),
            Ok(Outcome::Cancelled)
        ));
        let state = worker.control.state.lock().unwrap();
        assert!(state.logs[2].ends_with("任务已取消"));
        assert_eq!(state.final_status, Some("任务已取消"));
    }

    #[test]
    fn publishing_blocks_cancel_and_records_success() {
        let (published, ready) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        let worker = Worker::start(&egui::Context::default(), "处理", true, move |task| {
            task.publish_with_progress(Some(0.95))?;
            published.send(())?;
            wait.recv_timeout(Duration::from_secs(5))?;
            Ok(Outcome::Artifacts {
                paths: Vec::new(),
                details: None,
            })
        })
        .unwrap();

        ready.recv_timeout(Duration::from_secs(5)).unwrap();
        {
            let mut state = worker.control.state.lock().unwrap();
            assert!(state.stage.publishing);
            assert_eq!(state.stage.fraction, Some(0.95));
            assert!(state.logs[1].ends_with("保存结果"));
            state.request_cancel(&worker.control);
            assert!(!worker.control.cancelled.load(Ordering::Relaxed));
            assert_eq!(state.logs.len(), 2);
        }
        release.send(()).unwrap();
        assert!(
            worker
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_ok()
        );
        let state = worker.control.state.lock().unwrap();
        assert!(state.logs[2].ends_with("任务已完成"));
    }

    #[test]
    fn task_language_reaches_serial_and_parallel_processing_and_the_log() {
        use std::path::Path;
        use webgis_core::task_pool::{self, ThreadPoolBuilder};
        use webgis_toolbox::dom::cog;

        for (language, title, expected) in [
            (
                Language::English,
                "Create COG",
                "Input must be a .tif or .tiff file",
            ),
            (
                Language::Japanese,
                "COG の作成",
                "入力は .tif または .tiff ファイルである必要があります",
            ),
        ] {
            let (release, wait) = mpsc::channel();
            let worker = i18n::with_language(language, || {
                Worker::start(&egui::Context::default(), title, true, move |task| {
                    wait.recv_timeout(Duration::from_secs(5))?;
                    let pool = ThreadPoolBuilder::new().num_threads(2).build()?;
                    let validate = |()| {
                        let error = cog::run(
                            Path::new("bad.jpg"),
                            Path::new("result.tif"),
                            &task.cancelled,
                            |_| {},
                            || Ok(()),
                        )
                        .unwrap_err();
                        assert_eq!(error.to_string(), expected);
                        Ok::<_, anyhow::Error>(())
                    };
                    task_pool::map(None, vec![()], validate)?;
                    task_pool::map(Some(&pool), vec![(), ()], validate)?;
                    task_pool::map_with(Some(&pool), vec![(), ()], &mut [(), ()], |item, _| {
                        validate(item)
                    })?;
                    task.update(TaskProgress::new(
                        Phase::ReadGeoTiff,
                        message!(read_geotiff),
                        Some(0.0),
                    ));
                    bail!(expected);
                })
            })
            .unwrap();

            assert_eq!(i18n::language(), Language::Chinese);
            release.send(()).unwrap();
            let result = worker
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap();
            assert_eq!(result.err().unwrap().to_string(), expected);
            let state = worker.control.state.lock().unwrap();
            assert_eq!(state.logs.len(), 3);
            assert!(
                state.logs[1].contains(i18n::with_language(language, || message!(read_geotiff)))
            );
            assert!(state.logs[2].contains(title));
            assert!(state.logs[2].ends_with(expected));
        }
    }

    #[test]
    fn failure_logs_full_chain_and_each_task_has_its_own_bounded_log() {
        let worker = Worker::start(&egui::Context::default(), "扫描", false, |_| {
            bail!(
                anyhow!("打开文件")
                    .context("扫描失败")
                    .context("无法读取输入")
            )
        })
        .unwrap();
        assert!(
            worker
                .receiver
                .recv_timeout(Duration::from_secs(5))
                .unwrap()
                .is_err()
        );
        let state = worker.control.state.lock().unwrap();
        assert!(state.logs[1].starts_with("[ERRO] "));
        assert!(state.logs[1].ends_with("扫描 | 任务失败：无法读取输入: 扫描失败: 打开文件"));

        let mut next = TaskState::new("处理");
        assert_eq!(next.logs.len(), 1);
        for index in 0..LOG_LIMIT {
            next.log(console::Level::Info, &index.to_string());
        }
        assert_eq!(next.logs.len(), LOG_LIMIT);
        assert!(next.logs[0].ends_with(" | 0"));
    }
}
