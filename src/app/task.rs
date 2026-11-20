use super::Outcome;
use crate::console;
use anyhow::{Result, ensure};
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

const LOG_LIMIT: usize = 2_000;

struct Stage {
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
                name: "准备任务".into(),
                fraction: None,
                publishing: false,
            },
            last_progress_log: None,
            final_status: None,
            logs: VecDeque::new(),
        };
        state.log("INFO", "任务开始");
        state
    }

    fn log(&mut self, level: &str, message: &str) {
        if self.logs.len() == LOG_LIMIT {
            self.logs.pop_front();
        }
        self.logs
            .push_back(console::record(level, &self.name, message));
    }

    fn update(&mut self, name: String, fraction: Option<f32>, now: Instant) {
        let changed = phase(&self.stage.name) != phase(&name);
        self.stage.name = name;
        self.stage.fraction = fraction;
        if changed
            || self
                .last_progress_log
                .is_none_or(|last| now.duration_since(last) >= Duration::from_secs(1))
        {
            let message = match fraction {
                Some(fraction) => format!("{}（{:.0}%）", self.stage.name, fraction * 100.0),
                None => self.stage.name.clone(),
            };
            self.log("INFO", &message);
            self.last_progress_log = Some(now);
        }
    }

    fn request_cancel(&mut self, control: &TaskControl) {
        if control.cancellable
            && self.final_status.is_none()
            && !self.stage.publishing
            && !control.cancelled.swap(true, Ordering::Relaxed)
        {
            self.log("INFO", "已请求取消，等待后台任务退出");
        }
    }

    fn finish(&mut self, result: &Result<Outcome>) {
        let (status, level) = match result {
            Ok(Outcome::Cancelled) => ("任务已取消", "INFO"),
            Ok(_) => ("任务已完成", "INFO"),
            Err(_) => ("任务失败", "ERROR"),
        };
        self.final_status = Some(status);
        match result {
            Err(error) => self.log(level, &format!("{status}：{error:#}")),
            _ => self.log(level, status),
        }
    }
}

fn phase(name: &str) -> &str {
    for prefix in ["读取模型 ", "栅格化 ", "裁剪 "] {
        if name.starts_with(prefix) {
            return prefix;
        }
    }
    if name.starts_with("生成 ") && name.ends_with("级地图切片") {
        return "生成地图切片";
    }
    name
}

struct TaskControl {
    cancellable: bool,
    cancelled: Arc<AtomicBool>,
    state: Mutex<TaskState>,
}

pub(super) struct TaskContext {
    egui: egui::Context,
    pub(super) cancelled: Arc<AtomicBool>,
    control: Arc<TaskControl>,
}

impl TaskContext {
    pub(super) fn update(&self, name: impl Into<String>, fraction: Option<f32>) {
        let mut state = self.control.state.lock().unwrap();
        state.update(name.into(), fraction, Instant::now());
        drop(state);
        self.egui.request_repaint_of(egui::ViewportId::ROOT);
    }

    pub(super) fn publish(&self) -> Result<()> {
        self.publish_with_progress(None)
    }

    pub(super) fn publish_with_progress(&self, fraction: Option<f32>) -> Result<()> {
        let mut state = self.control.state.lock().unwrap();
        ensure!(!self.cancelled.load(Ordering::Relaxed), "处理已取消");
        state.stage = Stage {
            name: "发布结果".into(),
            fraction,
            publishing: true,
        };
        state.log("INFO", "发布结果");
        drop(state);
        self.egui.request_repaint_of(egui::ViewportId::ROOT);
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
        let control = Arc::new(TaskControl {
            cancellable,
            cancelled: Arc::new(AtomicBool::new(false)),
            state: Mutex::new(TaskState::new(name)),
        });
        let task = TaskContext {
            egui: egui.clone(),
            cancelled: control.cancelled.clone(),
            control: control.clone(),
        };
        std::thread::Builder::new()
            .name("webgis-task".into())
            .spawn(move || {
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
                task.egui.request_repaint_of(egui::ViewportId::ROOT);
            })?;
        Ok(Self { control, receiver })
    }

    pub(super) fn window(&self, sequence: u64) -> TaskWindow {
        TaskWindow {
            control: self.control.clone(),
            id: egui::ViewportId::from_hash_of(("task-window", sequence)),
        }
    }

    pub(super) fn finish_disconnected(&self, result: &Result<Outcome>) {
        self.control.state.lock().unwrap().finish(result);
    }

    pub(super) fn close_notice(&self) -> &'static str {
        let state = self.control.state.lock().unwrap();
        if self.control.cancelled.load(Ordering::Relaxed) {
            "请等待任务取消完成后再关闭窗口。"
        } else if self.control.cancellable && !state.stage.publishing {
            "请先点击主界面的『取消任务』，等待任务停止后再关闭窗口。"
        } else {
            "请等待当前任务结束后再关闭窗口。"
        }
    }

    pub(super) fn show_cancel_button(&self, ui: &mut egui::Ui) {
        if self.control.cancellable {
            let mut state = self.control.state.lock().unwrap();
            let enabled = state.final_status.is_none()
                && !state.stage.publishing
                && !self.control.cancelled.load(Ordering::Relaxed);
            if ui
                .add_enabled(enabled, egui::Button::new("取消任务"))
                .clicked()
            {
                state.request_cancel(&self.control);
            }
        }
    }
}

pub(super) struct TaskWindow {
    control: Arc<TaskControl>,
    id: egui::ViewportId,
}

impl TaskWindow {
    pub(super) fn show(&self, ctx: &egui::Context, busy: bool) -> bool {
        let title = self.control.state.lock().unwrap().name.clone();
        ctx.show_viewport_immediate(
            self.id,
            egui::ViewportBuilder::default()
                .with_title(title.clone())
                .with_inner_size([720., 410.])
                .with_min_inner_size([360., 220.])
                .with_resizable(true),
            |ctx, class| {
                let close_requested = ctx.input(|input| input.viewport().close_requested());
                if class != egui::ViewportClass::Embedded && close_requested {
                    if !busy {
                        return true;
                    }
                    ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                }

                let mut close = false;
                let content = |ui: &mut egui::Ui| {
                    close = self.content(ui, busy);
                };
                if class == egui::ViewportClass::Embedded {
                    egui::Window::new(&title).show(ctx, content);
                } else {
                    egui::CentralPanel::default().show(ctx, content);
                }
                close
            },
        )
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
            ui.label(status.unwrap_or(if pending { "正在取消" } else { &stage }));
            if status.is_none() && fraction.is_none() {
                ui.spinner();
            }
            if status.is_none() {
                if self.control.cancellable {
                    if ui
                        .add_enabled(!publishing && !pending, egui::Button::new("取消任务"))
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
                    ui.weak("当前任务不支持取消");
                }
            } else {
                close = ui.add_enabled(!busy, egui::Button::new("关闭")).clicked();
            }
            if ui.button("复制日志").clicked() {
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
        egui::ScrollArea::both()
            .stick_to_bottom(true)
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut read_only)
                        .code_editor()
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
    fn progress_logs_on_phase_change_and_at_most_once_per_second_within_a_phase() {
        let mut state = TaskState::new("处理");
        let start = Instant::now();
        state.update("读取模型 a.osgb".into(), Some(0.0), start);
        state.update(
            "读取模型 b.osgb".into(),
            Some(0.4),
            start + Duration::from_millis(999),
        );
        assert_eq!(state.logs.len(), 2);
        assert_eq!(state.stage.name, "读取模型 b.osgb");
        assert_eq!(state.stage.fraction, Some(0.4));

        state.update(
            "读取模型 c.osgb".into(),
            Some(0.5),
            start + Duration::from_secs(1),
        );
        state.update(
            "栅格化 1/2 块".into(),
            Some(0.5),
            start + Duration::from_millis(1001),
        );
        state.update(
            "栅格化 2/2 块".into(),
            Some(1.0),
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
            unreachable!("收到取消请求后不能发布结果");
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
            assert!(state.logs[1].ends_with("发布结果"));
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
        assert!(
            state.logs[1].contains("[ERROR] 扫描 | 任务失败：无法读取输入: 扫描失败: 打开文件")
        );

        let mut next = TaskState::new("处理");
        assert_eq!(next.logs.len(), 1);
        for index in 0..LOG_LIMIT {
            next.log("INFO", &index.to_string());
        }
        assert_eq!(next.logs.len(), LOG_LIMIT);
        assert!(next.logs[0].ends_with(" | 0"));
    }
}
