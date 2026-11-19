//! 按概览分辨率从低到高写入瓦片，所有 IFD 和索引都在数据块之前。
mod source;
mod writer;

use super::{Progress, check_cancelled, tiff_output::TILE};
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
    check_cancelled(cancelled)?;
    ensure!(
        input
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tif") || ext.eq_ignore_ascii_case("tiff")),
        "输入必须是 .tif 或 .tiff 文件"
    );
    let input = input
        .canonicalize()
        .with_context(|| format!("无法打开 {}", input.display()))?;
    if output.canonicalize().is_ok_and(|path| path == input) {
        anyhow::bail!("COG 输出不能指向源文件");
    }
    ensure!(
        !output.try_exists()?,
        "输出文件已存在：{}",
        output.display()
    );
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    progress(Progress {
        stage: "读取 GeoTIFF".into(),
        fraction: None,
    });
    let mut source =
        Source::open(&input).with_context(|| format!("无法转换 {}", input.display()))?;
    let mut temp = tempfile::NamedTempFile::new_in(parent)
        .with_context(|| format!("无法在 {} 创建临时文件", parent.display()))?;
    writer::write(
        &mut BufWriter::new(temp.as_file_mut()),
        &mut source,
        cancelled,
        progress,
        pool,
    )?;
    check_cancelled(cancelled)?;
    before_publish()?;
    check_cancelled(cancelled)?;
    temp.persist_noclobber(output).map_err(|error| {
        if error.error.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow::anyhow!("输出文件已存在：{}", output.display())
        } else {
            anyhow::Error::new(error).context(format!("无法发布 COG 至 {}", output.display()))
        }
    })?;
    Ok(output.to_owned())
}

#[cfg(test)]
mod tests;
