use super::{Outcome, processing_pool, task::TaskContext};
use anyhow::{Context, Result, anyhow, bail, ensure};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{
    fs,
    io::{self, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};
use webgis_core::{
    i18n::{self, Language},
    message, model_clip as clip, osgb_memory as memory,
};
use webgis_toolbox::dom::{self, Phase, TaskProgress};

pub const ARGUMENT: &str = "--internal-osgb-worker";
const MAX_FRAME: usize = 16 * memory::MIB;
const POLL: Duration = Duration::from_millis(250);
const MEMORY_EXIT: i32 = 73;
const RSS_EXIT: i32 = 74;

#[derive(Serialize, Deserialize)]
enum Job {
    RasterPreview(dom::Request),
    Raster(dom::Request, PathBuf, PathBuf),
    ClipPreview(clip::Request),
    Clip(clip::Request, PathBuf),
}

#[derive(Serialize, Deserialize)]
struct Request {
    language: String,
    threads: usize,
    budget: usize,
    job: Job,
}

#[derive(Serialize, Deserialize)]
enum Product {
    RasterPreview(dom::Summary),
    Raster(Vec<PathBuf>),
    ClipPreview(clip::Preflight),
    Clip(clip::Report),
}

#[derive(Serialize, Deserialize)]
enum Event {
    Progress(TaskProgress),
    Log(String),
    Finished(Result<Product, String>),
}

#[derive(Default)]
struct Frame(Vec<u8>);

impl Write for Frame {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME - self.0.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("OSGB IPC message exceeds {MAX_FRAME} bytes"),
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn write_frame(mut writer: impl Write, value: &impl Serialize) -> Result<()> {
    let mut frame = Frame::default();
    serde_json::to_writer(&mut frame, value)?;
    writer.write_all(&(frame.0.len() as u32).to_le_bytes())?;
    writer.write_all(&frame.0)?;
    writer.flush()?;
    Ok(())
}

fn read_frame<T: DeserializeOwned>(mut reader: impl Read) -> Result<Option<T>> {
    let mut len = [0; 4];
    if reader.read(&mut len[..1])? == 0 {
        return Ok(None);
    }
    reader.read_exact(&mut len[1..])?;
    let len = u32::from_le_bytes(len) as usize;
    ensure!(
        len <= MAX_FRAME,
        "OSGB IPC message exceeds {MAX_FRAME} bytes"
    );
    let mut bytes = vec![0; len];
    reader.read_exact(&mut bytes)?;
    Ok(Some(serde_json::from_slice(&bytes)?))
}

struct WorkerProcess(Child);

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn supervise(job: Job, threads: usize, task: &TaskContext) -> Result<Product> {
    let budget = memory::available_system().map_or(512 * memory::MIB, |available| {
        (available - available / 4).min(available.saturating_sub(memory::HEADROOM))
    });
    let request = Request {
        job,
        threads,
        budget,
        language: i18n::language().code().into(),
    };
    let mut command = Command::new(std::env::current_exe()?);
    if let Some(stage) = request.job.stage() {
        command.env("SQLITE_TMPDIR", stage);
    }
    command.arg(ARGUMENT);
    supervise_process(command, request, task)
}

fn supervise_process(
    mut command: Command,
    request: Request,
    task: &TaskContext,
) -> Result<Product> {
    let mut child = WorkerProcess(
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .context(message!(could_not_start_the_background_worker))?,
    );
    let mut input = child.0.stdin.take().unwrap();
    let mut output = BufReader::new(child.0.stdout.take().unwrap());
    let (send, receive) = mpsc::sync_channel(32);
    let (cancel_worker, cancel_request) = mpsc::sync_channel(1);
    let report_write_error = send.clone();
    let writer = std::thread::Builder::new()
        .name("osgb-request".into())
        .spawn(move || {
            if let Err(error) = write_frame(&mut input, &request) {
                let _ = report_write_error.send(Err(error));
                return;
            }
            drop(report_write_error);
            if cancel_request.recv().is_ok() {
                let _ = input.write_all(&[1]);
            }
        })?;
    let reader = std::thread::Builder::new()
        .name("osgb-ipc".into())
        .spawn(move || {
            loop {
                match read_frame::<Event>(&mut output) {
                    Ok(Some(event)) => {
                        if send.send(Ok(event)).is_err() {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(error) => {
                        let _ = send.send(Err(error));
                        break;
                    }
                }
            }
        })?;
    let mut result = None;
    let mut protocol_error = None;
    let mut cancel_at = None;
    let mut closed = false;
    let mut status = None;
    while !closed || status.is_none() {
        if task.cancelled.load(Ordering::Relaxed) {
            let start = *cancel_at.get_or_insert_with(|| {
                let _ = cancel_worker.try_send(());
                Instant::now()
            });
            if start.elapsed() >= Duration::from_secs(5) && status.is_none() {
                let _ = child.0.kill();
            }
        }
        if closed {
            std::thread::sleep(Duration::from_millis(25));
        } else {
            match receive.recv_timeout(Duration::from_millis(25)) {
                Ok(Ok(Event::Progress(progress))) => task.update(progress),
                Ok(Ok(Event::Log(log))) => task.log(&log),
                Ok(Ok(Event::Finished(value))) => result = Some(value),
                Ok(Err(error)) => {
                    protocol_error = Some(error);
                    if status.is_none() {
                        let _ = child.0.kill();
                    }
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => closed = true,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        if status.is_none() {
            status = child.0.try_wait()?;
        }
    }
    drop(cancel_worker);
    let _ = writer.join();
    let _ = reader.join();
    let status = status.unwrap();
    if task.cancelled.load(Ordering::Relaxed) {
        bail!(message!(processing_canceled));
    }
    if status.code() == Some(MEMORY_EXIT) {
        bail!(message!(osgb_memory_pressure));
    }
    if status.code() == Some(RSS_EXIT) {
        bail!(message!(osgb_rss_limit));
    }
    if let Some(error) = protocol_error {
        return Err(error).context(message!(the_background_worker_exited_unexpectedly));
    }
    ensure!(
        status.success(),
        "{} ({status})",
        message!(the_background_worker_exited_unexpectedly)
    );
    result
        .context(message!(the_background_worker_exited_unexpectedly))?
        .map_err(|error| anyhow!(error))
}

pub(super) fn raster(
    request: dom::Request,
    preview: bool,
    threads: usize,
    task: &TaskContext,
) -> Result<Outcome> {
    if preview {
        let Product::RasterPreview(summary) =
            supervise(Job::RasterPreview(request), threads, task)?
        else {
            bail!(message!(the_background_worker_exited_unexpectedly));
        };
        return Ok(Outcome::RasterPreview(summary));
    }
    let stage = dom::StagedRaster::new(&request).map_err(disk_error)?;
    let overwrite = request.overwrite;
    let Product::Raster(paths) = supervise(
        Job::Raster(request, stage.output.clone(), stage.path().to_owned()),
        threads,
        task,
    )?
    else {
        bail!(message!(the_background_worker_exited_unexpectedly));
    };
    task.publish_with_progress(Some(0.95))?;
    stage.publish(overwrite).map_err(disk_error)?;
    Ok(Outcome::Artifacts {
        paths,
        details: None,
    })
}

pub(super) fn clip(
    request: clip::Request,
    preview: bool,
    threads: usize,
    task: &TaskContext,
) -> Result<Outcome> {
    task.update(TaskProgress::new(
        Phase::CheckClip,
        message!(check_model_boundary_and_output),
        None,
    ));
    if preview {
        let Product::ClipPreview(summary) = supervise(Job::ClipPreview(request), threads, task)?
        else {
            bail!(message!(the_background_worker_exited_unexpectedly));
        };
        return Ok(Outcome::ClipPreview(summary));
    }
    let stage = clip::StagedClip::new(&request).map_err(|error| anyhow!(error))?;
    let allow_non_empty = request.allow_non_empty_output;
    let Product::Clip(report) =
        supervise(Job::Clip(request, stage.path().to_owned()), threads, task)?
    else {
        bail!(message!(the_background_worker_exited_unexpectedly));
    };
    task.publish()?;
    stage
        .publish(allow_non_empty)
        .map_err(|error| anyhow!(error))?;
    Ok(Outcome::Artifacts {
        paths: std::iter::once(report.output)
            .chain(report.entries)
            .collect(),
        details: Some(message!(
            wrote_osgb_files_with_triangles,
            report.files,
            report.triangles
        )),
    })
}

fn rss(label: &str) -> usize {
    fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| memory::proc_kib(&status, label))
        .unwrap_or(0)
}

fn send(event: Event) -> Result<()> {
    write_frame(io::stdout().lock(), &event)
}

fn watch_memory(cancelled: &AtomicBool, budget: usize, baseline: usize) {
    let mut low_samples = 0;
    let mut pressure_since = None;
    loop {
        std::thread::sleep(POLL);
        if cancelled.load(Ordering::Relaxed) {
            return;
        }
        // Code and shared-library pages are faulted in lazily. Reserve the
        // task budget for anonymous allocations and monitor system headroom
        // separately for file-backed pages.
        if rss("RssAnon:").saturating_sub(baseline) > budget {
            let _ = send(Event::Log(message!(
                osgb_worker_usage,
                rss("VmHWM:"),
                budget = budget
            )));
            std::process::exit(RSS_EXIT);
        }
        if memory::available_system().is_some_and(|available| available < memory::HEADROOM) {
            low_samples += 1;
            if low_samples >= 2 {
                let start = pressure_since.get_or_insert_with(|| {
                    memory::set_pressure(true);
                    let _ = send(Event::Log(message!(
                        osgb_pressure_serial,
                        rss("VmHWM:"),
                        budget = budget
                    )));
                    Instant::now()
                });
                if start.elapsed() >= Duration::from_secs(2) {
                    std::process::exit(MEMORY_EXIT);
                }
            }
        } else {
            low_samples = 0;
            pressure_since = None;
            memory::set_pressure(false);
        }
    }
}

impl Job {
    fn stage(&self) -> Option<&std::path::Path> {
        match self {
            Self::Raster(_, _, stage) | Self::Clip(_, stage) => Some(stage),
            _ => None,
        }
    }
}

fn staging_bytes(stage: Option<&std::path::Path>) -> u64 {
    stage
        .and_then(|stage| fs::read_dir(stage).ok())
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.metadata().ok())
        .filter(|metadata| metadata.is_file())
        .fold(0, |total, metadata| total.saturating_add(metadata.len()))
}

fn calculate(job: Job, threads: usize, cancelled: &AtomicBool, budget: usize) -> Result<Product> {
    let pool = processing_pool(threads)?;
    let stage = job.stage().map(std::path::Path::to_owned);
    let mut last = None;
    let mut last_update = Instant::now();
    let started = last_update;
    let mut progress = |event: TaskProgress| {
        if last != Some(event.phase) || event.concurrency.is_some() || last_update.elapsed() >= POLL
        {
            if last != Some(event.phase) {
                let _ = send(Event::Log(message!(
                    osgb_worker_metrics,
                    rss("VmHWM:"),
                    budget = budget,
                    ms = started.elapsed().as_millis(),
                    staged = staging_bytes(stage.as_deref())
                )));
            }
            if let Some(width) = event.concurrency {
                let _ = send(Event::Log(message!(osgb_worker_parallelism, width)));
            }
            last = Some(event.phase);
            last_update = Instant::now();
            let _ = send(Event::Progress(event));
        }
    };
    let mut last_width = 0;
    match job {
        Job::RasterPreview(request) => Ok(Product::RasterPreview(dom::preflight_with_progress(
            &request, cancelled, progress,
        )?)),
        Job::Raster(request, output, stage) => Ok(Product::Raster(dom::compute_staged(
            &request,
            &output,
            &stage,
            cancelled,
            progress,
            pool.as_ref(),
        )?)),
        Job::ClipPreview(request) => clip::preflight_cancelable(&request, cancelled)
            .map(Product::ClipPreview)
            .map_err(|error| anyhow!(error)),
        Job::Clip(request, stage) => clip::compute_staged(
            &request,
            &stage,
            cancelled,
            |p| {
                let mut event = TaskProgress::new(
                    Phase::ClipModel,
                    message!(
                        clipping_processed_models_found,
                        p.file.display(),
                        p.completed,
                        p.discovered
                    ),
                    None,
                );
                event.concurrency =
                    (p.concurrency > 0 && last_width != p.concurrency).then_some(p.concurrency);
                if p.concurrency > 0 {
                    last_width = p.concurrency;
                }
                progress(event);
            },
            pool.as_ref(),
        )
        .map(Product::Clip)
        .map_err(|error| anyhow!(error)),
    }
}

pub(crate) fn run() -> Result<()> {
    let mut input = BufReader::new(io::stdin());
    let request: Request = read_frame(&mut input)?.context("Missing OSGB worker request")?;
    let language = Language::from_code(&request.language);
    memory::configure(request.budget);
    let cancelled = Arc::new(AtomicBool::new(false));
    let signal = Arc::clone(&cancelled);
    std::thread::spawn(move || {
        let mut byte = [0];
        if input.read_exact(&mut byte).is_err() {
            std::process::exit(1);
        }
        signal.store(true, Ordering::Relaxed);
    });
    let signal = Arc::clone(&cancelled);
    let baseline = rss("RssAnon:");
    std::thread::spawn(move || {
        i18n::with_language(language, || watch_memory(&signal, request.budget, baseline))
    });
    i18n::with_language(language, || {
        let result = calculate(request.job, request.threads, &cancelled, request.budget)
            .map_err(|error| format!("{:#}", disk_error(error)));
        send(Event::Log(message!(
            osgb_worker_usage,
            rss("VmHWM:"),
            budget = request.budget
        )))?;
        send(Event::Finished(result))
    })
}

fn disk_error(error: anyhow::Error) -> anyhow::Error {
    if error.chain().any(|cause| {
        cause
            .downcast_ref::<io::Error>()
            .is_some_and(|io| io.kind() == io::ErrorKind::StorageFull)
            || cause
                .downcast_ref::<rusqlite::Error>()
                .is_some_and(|sql| sql.sqlite_error_code() == Some(rusqlite::ErrorCode::DiskFull))
    }) {
        error.context(message!(osgb_disk_full))
    } else {
        error
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::app::Worker;

    #[test]
    fn bounded_protocol_rejects_oversized_and_truncated_messages() {
        let oversized = (MAX_FRAME as u32 + 1).to_le_bytes();
        assert!(read_frame::<Event>(&oversized[..]).is_err());
        assert!(read_frame::<Event>(&[1, 0, 0][..]).is_err());
        assert!(read_frame::<Event>(&[4, 0, 0, 0, b'{'][..]).is_err());
        assert!(read_frame::<Event>(&[][..]).unwrap().is_none());
    }

    #[test]
    fn worker_failures_and_cancel_preserve_outputs_and_release_staging() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let output = directory.path().to_owned();
        let existing = output.join(dom::PRODUCTS[0]);
        fs::write(&existing, b"preexisting output")?;
        let expected = [
            (
                "kill -KILL $$",
                false,
                message!(the_background_worker_exited_unexpectedly),
            ),
            ("exit 73", false, message!(osgb_memory_pressure)),
            ("exec sleep 30", true, ""),
            ("head -c 1 >/dev/null", true, ""),
        ];
        for (action, cancel, expected) in expected {
            let mut raster = dom::Request {
                input: output.join("source.osgb"),
                output: output.clone(),
                metadata: None,
                crs: None,
                origin: None,
                mode: dom::Mode::Local,
                resolution: 1.,
                overwrite: true,
            };
            let block_write = action.starts_with("exec sleep");
            if block_write {
                // Larger than the pipe buffer. Cancellation must also work
                // when the subprocess stops before reading the request.
                raster.crs = Some("x".repeat(1024 * 1024));
            }
            let worker = Worker::start(
                &eframe::egui::Context::default(),
                "OSGB test",
                true,
                move |task| {
                    let stage = dom::StagedRaster::new(&raster)?;
                    fs::write(
                        stage.path().join(dom::PRODUCTS[0]),
                        b"partial worker output",
                    )?;
                    let request = Request {
                        job: Job::Raster(raster, stage.output.clone(), stage.path().to_owned()),
                        language: i18n::language().code().to_owned(),
                        threads: 2,
                        budget: 32 * memory::MIB,
                    };
                    let frame_bytes = serde_json::to_vec(&request)?.len() + 4;
                    let mut command = Command::new("sh");
                    let script = if block_write {
                        action.to_owned()
                    } else {
                        format!("head -c {frame_bytes} >/dev/null; {action}")
                    };
                    command.args(["-c", &script]);
                    if cancel {
                        task.cancelled.store(true, Ordering::Relaxed);
                    }
                    supervise_process(command, request, task)?;
                    bail!("Injected worker must never publish")
                },
            )?;
            let result = worker.receiver.recv_timeout(Duration::from_secs(8))?;
            if cancel {
                assert!(matches!(result, Ok(Outcome::Cancelled)));
            } else {
                let Err(error) = result else {
                    panic!("Worker unexpectedly succeeded")
                };
                assert!(error.to_string().contains(expected), "{error:#}");
            }
            assert_eq!(fs::read_dir(&output)?.count(), 1);
            assert_eq!(fs::read(&existing)?, b"preexisting output");
        }
        Ok(())
    }
}
