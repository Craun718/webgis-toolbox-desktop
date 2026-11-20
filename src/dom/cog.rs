//! 按概览分辨率从低到高写入瓦片，所有 IFD 和索引都在数据块之前。

use webgis_core::message;
mod source;
mod writer;

use super::{Phase, Progress, TaskProgress, check_cancelled, tiff_output::TILE};
use anyhow::{Context, Result, ensure};
use source::Source;
use std::{
    io::BufWriter,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use webgis_core::task_pool::ThreadPool;

pub fn default_output(source: &Path) -> PathBuf {
    let mut name = source.file_stem().unwrap_or_default().to_os_string();
    name.push("_cog.tif");
    source.with_file_name(name)
}

pub fn run(
    input: &Path,
    output: &Path,
    cancelled: &AtomicBool,
    progress: impl Fn(Progress),
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<PathBuf> {
    run_with_pool(input, output, cancelled, progress, before_publish, None)
}

pub fn run_with_pool(
    input: &Path,
    output: &Path,
    cancelled: &AtomicBool,
    progress: impl Fn(Progress),
    before_publish: impl FnOnce() -> Result<()>,
    pool: Option<&ThreadPool>,
) -> Result<PathBuf> {
    run_with_progress(
        input,
        output,
        cancelled,
        |event| progress(event.display),
        before_publish,
        pool,
    )
}

pub fn run_with_progress(
    input: &Path,
    output: &Path,
    cancelled: &AtomicBool,
    progress: impl Fn(TaskProgress),
    before_publish: impl FnOnce() -> Result<()>,
    pool: Option<&ThreadPool>,
) -> Result<PathBuf> {
    check_cancelled(cancelled)?;
    ensure!(
        input
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tif") || ext.eq_ignore_ascii_case("tiff")),
        message!(input_must_be_a_tif_or_tiff_file)
    );
    let input = input
        .canonicalize()
        .with_context(|| message!(could_not_open, input.display()))?;
    if output.canonicalize().is_ok_and(|path| path == input) {
        anyhow::bail!(message!(cog_output_cannot_point_to_the_source_file));
    }
    ensure!(
        !output.try_exists()?,
        message!(output_file_already_exists, output.display())
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    progress(TaskProgress::new(
        Phase::ReadGeoTiff,
        message!(read_geotiff),
        None,
    ));
    let mut source =
        Source::open(&input).with_context(|| message!(could_not_read, input.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| message!(could_not_create_a_temporary_file_in, parent.display()))?;
    writer::write(
        &mut BufWriter::new(temp.as_file_mut()),
        &mut source,
        cancelled,
        progress,
        pool,
    )
    .map_err(|error| {
        if error.is::<std::io::Error>() {
            error.context(message!(write_cog, output.display()))
        } else {
            error
        }
    })?;
    check_cancelled(cancelled)?;
    before_publish()?;
    check_cancelled(cancelled)?;
    temp.persist_noclobber(output).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::anyhow!(message!(output_file_already_exists, output.display()))
        } else {
            anyhow::Error::new(error).context(message!(could_not_publish_cog_to, output.display()))
        }
    })?;
    Ok(output.to_owned())
}

#[cfg(test)]
mod tests;
