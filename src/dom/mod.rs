mod geotiff;
pub mod map_tiles;
mod raster;
mod tiff_mask;
mod tiff_output;
mod tiff_source;
mod tile_sampling;

use anyhow::{Context, Result, anyhow, bail, ensure};
use geotiff::{Affine, Crs};
use raster::{Mesh, TextureCache};
use regex::Regex;
use serde::Serialize;
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use tiff_output::{BigTiff, LevelWriter, NODATA, RasterKind, RasterSpec, TILE};
use walkdir::WalkDir;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Relative,
    Absolute,
    Local,
}

#[derive(Clone)]
pub struct Request {
    pub input: PathBuf,
    pub output: PathBuf,
    pub metadata: Option<PathBuf>,
    pub crs: Option<String>,
    pub origin: Option<[f64; 3]>,
    pub mode: Mode,
    pub resolution: f64,
    pub overwrite: bool,
}

#[derive(Serialize)]
pub struct Summary {
    pub selected: Vec<PathBuf>,
    pub metadata: Option<PathBuf>,
    pub crs: Option<String>,
    pub origin: [f64; 3],
    pub mode: Mode,
    pub bounds: [f64; 6],
    pub size: [i32; 2],
    pub resolution: f64,
    pub triangles: usize,
}

#[derive(Serialize)]
struct RunReport<'a> {
    schema_version: u32,
    #[serde(flatten)]
    summary: &'a Summary,
    products: &'a [PathBuf],
    dsm_valid_pixels: u64,
    dom_valid_pixels: u64,
}

pub struct Progress {
    pub stage: String,
    pub fraction: Option<f32>,
}

pub const PRODUCTS: [&str; 3] = ["dsm.tif", "dom.tif", "run-report.json"];

pub fn parse_origin(value: &str) -> Result<[f64; 3]> {
    let number = Regex::new(r"[-+]?(?:\d+(?:\.\d*)?|\.\d+)(?:[eE][-+]?\d+)?")?;
    let values = number
        .find_iter(value)
        .map(|m| m.as_str().parse::<f64>())
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let origin: [f64; 3] = values
        .try_into()
        .map_err(|_| anyhow!("原点须包含三个坐标"))?;
    ensure!(origin.iter().all(|v| v.is_finite()), "原点须为有限数值");
    Ok(origin)
}

pub fn is_metadata_file(path: &Path) -> bool {
    path.file_name()
        .is_some_and(|name| name.eq_ignore_ascii_case("metadata.xml"))
}

fn is_osgb(path: &Path) -> bool {
    path.extension()
        .is_some_and(|extension| extension.eq_ignore_ascii_case("osgb"))
}

fn scan_osgb(directory: &Path, cancelled: &AtomicBool) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for entry in WalkDir::new(directory) {
        check_cancelled(cancelled)?;
        let entry = entry?;
        if entry.path().is_file() && is_osgb(entry.path()) {
            files.push(entry.into_path());
        }
    }
    ensure!(!files.is_empty(), "metadata.xml 所在目录内没有 OSGB 文件");
    let pattern = Regex::new(r"(?i)^(.+)_L(\d+)_(\d+)$")?;
    let mut groups = BTreeMap::<String, (u32, Vec<PathBuf>)>::new();
    for path in &files {
        check_cancelled(cancelled)?;
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        if let Some(m) = pattern.captures(stem) {
            let level = m[2]
                .parse::<u32>()
                .with_context(|| path.display().to_string())?;
            let (max, parts) = groups.entry(m[1].to_lowercase()).or_default();
            if level > *max {
                *max = level;
                parts.clear();
            }
            if level == *max {
                parts.push(path.clone());
            }
        }
    }
    if !groups.is_empty() {
        files = groups.into_values().flat_map(|(_, parts)| parts).collect();
    }
    files.sort_by_cached_key(|path| (path.to_string_lossy().to_lowercase(), path.clone()));
    Ok(files)
}

fn metadata_values(path: &Path) -> Result<(Option<String>, Option<[f64; 3]>)> {
    use quick_xml::{Reader, events::Event};
    let xml = fs::read(path)?;
    let mut reader = Reader::from_reader(xml.as_slice());
    let mut crs = None;
    let mut origin = None;
    loop {
        match reader.read_event()? {
            Event::Start(tag) => {
                let name = tag.local_name();
                let name = name.as_ref();
                if crs.is_none() && name.eq_ignore_ascii_case(b"SRS") {
                    let text = xml_value(&mut reader, tag.name())?;
                    if !text.is_empty() {
                        crs = Some(text);
                    }
                } else if origin.is_none()
                    && (name.eq_ignore_ascii_case(b"SRSOrigin")
                        || name.eq_ignore_ascii_case(b"SRS_Origin"))
                {
                    let text = xml_value(&mut reader, tag.name())?;
                    if !text.is_empty() {
                        origin = Some(parse_origin(&text)?);
                    }
                }
            }
            Event::DocType(_) => bail!("metadata.xml 不支持 DTD"),
            Event::Eof => break,
            _ => {}
        }
    }
    Ok((crs, origin))
}

fn xml_value(
    reader: &mut quick_xml::Reader<&[u8]>,
    name: quick_xml::name::QName<'_>,
) -> Result<String> {
    use quick_xml::events::Event;
    let mut result = String::new();
    loop {
        match reader.read_event()? {
            Event::Text(text) => result.push_str(&text.decode()?),
            Event::CData(text) => result.push_str(&text.decode()?),
            Event::GeneralRef(entity) => {
                let reference = format!("&{};", entity.decode()?);
                result.push_str(&quick_xml::escape::unescape(&reference)?);
            }
            Event::Comment(_) => {}
            Event::End(tag) if tag.name() == name => return Ok(result.trim().to_owned()),
            _ => bail!("metadata.xml 的坐标系与原点字段必须为文本"),
        }
    }
}

fn prepare(
    request: &Request,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(Progress),
) -> Result<(Summary, Mesh, Option<Crs>)> {
    ensure!(
        request.resolution.is_finite() && request.resolution > 0.,
        "分辨率必须为有限正数"
    );
    ensure!(
        request.mode != Mode::Local || request.crs.is_none(),
        "本地模式不能指定坐标系"
    );
    ensure!(
        request.mode != Mode::Absolute || request.origin.is_none(),
        "绝对坐标模式不能指定原点偏移"
    );
    progress(Progress {
        stage: "扫描 OSGB 与元数据".into(),
        fraction: None,
    });
    let metadata_input = is_metadata_file(&request.input);
    ensure!(
        request.input.is_file() && (metadata_input || is_osgb(&request.input)),
        "请选择根 OSGB 文件或 metadata.xml"
    );
    let input = request
        .input
        .canonicalize()
        .with_context(|| request.input.display().to_string())?;
    let root = input.parent().context("缺少输入父目录")?;
    let (selected, metadata) = if metadata_input {
        (scan_osgb(root, cancelled)?, Some(input.clone()))
    } else {
        let metadata = request.metadata.clone().or_else(|| {
            root.ancestors()
                .map(|directory| directory.join("metadata.xml"))
                .find(|path| path.is_file())
        });
        (vec![input.clone()], metadata)
    };
    let (metadata_crs, metadata_origin) = metadata
        .as_deref()
        .map(|path| metadata_values(path).with_context(|| path.display().to_string()))
        .transpose()?
        .unwrap_or_default();
    let origin = match request.mode {
        Mode::Absolute => [0.; 3],
        Mode::Local => request.origin.or(metadata_origin).unwrap_or([0.; 3]),
        Mode::Relative => request
            .origin
            .or(metadata_origin)
            .context("缺少 SRSOrigin，请指定原点或改用绝对坐标／本地模式")?,
    };
    ensure!(origin.iter().all(|v| v.is_finite()), "原点须为有限数值");
    let crs = match request.mode {
        Mode::Local => None,
        _ => Some(request.crs.clone().or(metadata_crs).with_context(|| {
            let source = metadata.as_ref().unwrap_or(&input);
            format!("{} 缺少 SRS，请指定坐标系或选择本地模式", source.display())
        })?),
    };
    let epsg = crs
        .as_deref()
        .map(Crs::parse)
        .transpose()
        .with_context(|| {
            let source = if request.crs.is_some() {
                &input
            } else {
                metadata.as_ref().unwrap_or(&input)
            };
            format!("{} 的坐标系无效", source.display())
        })?;
    let mesh = Mesh::load(root, &selected, origin, cancelled, progress)?;
    let bounds = mesh.bounds;
    let [min_x, min_y, _, max_x, max_y, _] = bounds;
    ensure!(max_x > min_x && max_y > min_y, "模型没有有效的 XY 范围");
    ensure!(
        min_x + request.resolution > min_x && max_y - request.resolution < max_y,
        "像元尺寸低于坐标数值精度"
    );
    let size =
        [max_x - min_x, max_y - min_y].map(|span| (span / request.resolution).ceil().max(1.));
    ensure!(
        size.iter().all(|v| *v <= f64::from(i32::MAX)),
        "栅格尺寸超出支持范围"
    );
    let summary = Summary {
        selected,
        metadata,
        crs,
        origin,
        mode: request.mode,
        bounds,
        size: size.map(|v| v as i32),
        resolution: request.resolution,
        triangles: mesh.triangles.len(),
    };
    Ok((summary, mesh, epsg))
}

pub fn preflight(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> Result<Summary> {
    let (summary, _, _) = prepare(request, cancelled, &mut progress)?;
    check_cancelled(cancelled)?;
    Ok(summary)
}

pub fn run(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<Vec<PathBuf>> {
    let (summary, mesh, crs) = prepare(request, cancelled, &mut progress)?;
    check_cancelled(cancelled)?;
    fs::create_dir_all(&request.output)?;
    let output = request.output.canonicalize()?;
    let lock = fs::File::open(&output)?;
    use rustix::fs::{FlockOperation, flock};
    flock(&lock, FlockOperation::NonBlockingLockExclusive)
        .context("无法锁定输出目录，可能有另一个任务正在写入")?;
    check_conflicts(&output, request.overwrite)?;
    let staging = tempfile::Builder::new()
        .prefix(".osgb-raster-")
        .tempdir_in(&output)?;
    let stage = staging.path();
    let [width, height] = summary.size;
    let [west, _, _, _, north, _] = summary.bounds;
    let res = summary.resolution;
    let spec = RasterSpec {
        size: summary.size.map(|v| v as usize),
        transform: Affine([west, res, 0., north, 0., -res]),
        crs,
    };
    let dsm_path = stage.join(PRODUCTS[0]);
    let dom_path = stage.join(PRODUCTS[1]);
    let mut dsm_file = BigTiff::new_big(fs::File::create(&dsm_path)?)?;
    let mut dom_file = BigTiff::new_big(fs::File::create(&dom_path)?)?;
    let mut dsm = LevelWriter::new(&mut dsm_file, RasterKind::Dsm)?;
    let mut dom = LevelWriter::new(&mut dom_file, RasterKind::Dom)?;
    let index = raster::index(&mesh.triangles);
    let mut textures = TextureCache::new(&mesh.images);
    let (w, h) = (width as usize, height as usize);
    let count = w.div_ceil(TILE) as u64 * h.div_ceil(TILE) as u64;
    let mut done = 0_u64;
    let mut dsm_valid_pixels = 0_u64;
    let mut dom_valid_pixels = 0_u64;
    for y in (0..h).step_by(TILE) {
        for x in (0..w).step_by(TILE) {
            check_cancelled(cancelled)?;
            let rect = [x, y, TILE.min(w - x), TILE.min(h - y)];
            let block = raster::render(
                &mesh.triangles,
                &index,
                &mut textures,
                &summary,
                rect,
                cancelled,
            )?;
            let (z, color) = block.into_planes();
            dsm_valid_pixels += z.iter().filter(|&&v| v != NODATA).count() as u64;
            dom_valid_pixels += color[3].iter().filter(|&&v| v != 0).count() as u64;
            let size = [rect[2], rect[3]];
            dsm.height(size, &z)?;
            dom.color(size, &color)?;
            done += 1;
            progress(Progress {
                stage: format!("栅格化 {done}/{count} 块"),
                fraction: Some(done as f32 / count as f32),
            });
        }
    }
    ensure!(
        dsm_valid_pixels > 0,
        "没有像元中心落在模型表面，请减小像元尺寸"
    );
    ensure!(
        dom_valid_pixels > 0,
        "最高表面没有可见纹理像元，无法生成 DOM"
    );
    dsm.finish(spec, false)?;
    if let Some(mask) = dom.finish(spec, false)? {
        mask.finish(&mut dom_file)?;
    }
    let levels: Vec<_> = [2, 4, 8, 16]
        .into_iter()
        .filter(|&n| n <= w.min(h))
        .collect();
    progress(Progress {
        stage: "构建内部概览".into(),
        fraction: None,
    });
    tiff_output::overviews(
        &mut dsm_file,
        &dsm_path,
        spec,
        RasterKind::Dsm,
        &levels,
        cancelled,
    )
    .with_context(|| format!("构建 {} 的内部概览", PRODUCTS[0]))?;
    tiff_output::overviews(
        &mut dom_file,
        &dom_path,
        spec,
        RasterKind::Dom,
        &levels,
        cancelled,
    )
    .with_context(|| format!("构建 {} 的内部概览", PRODUCTS[1]))?;
    drop((dsm_file, dom_file));
    progress(Progress {
        stage: "校验写出结果".into(),
        fraction: None,
    });
    for (path, kind, valid) in [
        (&dsm_path, RasterKind::Dsm, dsm_valid_pixels),
        (&dom_path, RasterKind::Dom, dom_valid_pixels),
    ] {
        tiff_output::verify(path, spec, kind, &levels, valid, cancelled)
            .with_context(|| format!("校验 {}", path.display()))?;
    }
    let products: Vec<_> = PRODUCTS.iter().map(|name| output.join(name)).collect();
    let report = RunReport {
        schema_version: 1,
        summary: &summary,
        products: &products,
        dsm_valid_pixels,
        dom_valid_pixels,
    };
    fs::write(stage.join(PRODUCTS[2]), serde_json::to_vec_pretty(&report)?)?;
    ensure!(
        fs::read_dir(stage)?.count() == PRODUCTS.len(),
        "输出包含意外的 GeoTIFF 旁车文件"
    );
    check_cancelled(cancelled)?;
    before_publish()?;
    publish(stage, &output, request.overwrite)?;
    Ok(products)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    ensure!(!cancelled.load(Ordering::Relaxed), "处理已取消");
    Ok(())
}

fn check_conflicts(output: &Path, overwrite: bool) -> Result<()> {
    for name in PRODUCTS {
        let path = output.join(name);
        match path.symlink_metadata() {
            Ok(meta) => {
                ensure!(meta.is_file(), "产物位置不是常规文件：{}", path.display());
                ensure!(overwrite, "产物已存在，请确认覆盖：{}", path.display());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| path.display().to_string()),
        }
        if name.ends_with(".tif") {
            for suffix in [".msk", ".ovr", ".aux.xml", ".msk.ovr", ".aux", ".tfw", "w"] {
                let sidecar = output.join(format!("{name}{suffix}"));
                ensure!(
                    matches!(sidecar.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                    "产物位置存在旁车文件：{}",
                    sidecar.display()
                );
            }
            for extension in ["tfw", "wld", "tifw", "aux"] {
                let sidecar = path.with_extension(extension);
                ensure!(
                    matches!(sidecar.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                    "产物位置存在旁车文件：{}",
                    sidecar.display()
                );
            }
        }
    }
    Ok(())
}

fn rename_new(src: &Path, dst: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE)
        .with_context(|| format!("{} → {}", src.display(), dst.display()))
}

fn publish(stage: &Path, output: &Path, overwrite: bool) -> Result<()> {
    check_conflicts(output, overwrite)?;
    let backup = tempfile::Builder::new()
        .prefix(".osgb-raster-backup-")
        .tempdir_in(output)?;
    let mut old = Vec::new();
    let mut new = Vec::new();
    let published = (|| {
        for name in PRODUCTS {
            let path = output.join(name);
            if overwrite && path.symlink_metadata().is_ok() {
                rename_new(&path, &backup.path().join(name))?;
                old.push(name);
            }
            rename_new(&stage.join(name), &path)?;
            new.push(name);
        }
        Ok::<_, anyhow::Error>(())
    })();
    if let Err(error) = published {
        let mut failures = Vec::new();
        for name in new.into_iter().rev() {
            if let Err(err) = rename_new(&output.join(name), &stage.join(name)) {
                failures.push(format!("{err:#}"));
            }
        }
        for name in old.into_iter().rev() {
            if let Err(err) = rename_new(&backup.path().join(name), &output.join(name)) {
                failures.push(format!("{err:#}"));
            }
        }
        if !failures.is_empty() {
            let preserved = backup.keep();
            bail!(
                "发布失败：{error:#}。回滚失败：{}。备份保留于 {}",
                failures.join("；"),
                preserved.display()
            );
        }
        return Err(error).context("发布失败，已回滚");
    }
    Ok(())
}

#[cfg(test)]
mod tests;
