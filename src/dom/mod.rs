use webgis_core::task_pool::{self, ThreadPool};
use webgis_core::{message, osgb_output::OutputLock};
pub mod cog;
mod geotiff;
pub mod map_tiles;
mod raster;
mod tiff_mask;
mod tiff_output;
mod tiff_source;
mod tile_crs;
mod tile_sampling;

use anyhow::{Context, Result, anyhow, bail, ensure};
use geotiff::{Affine, Crs};
#[cfg(test)]
use raster::TextureCache;
use raster::{Mesh, Storage};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    io::{BufWriter, ErrorKind, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use tiff_output::{BigTiff, LevelWriter, NODATA, RasterKind, RasterSpec, TILE};
use walkdir::WalkDir;
use webgis_core::osgb_memory as memory;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Relative,
    Absolute,
    Local,
}

#[derive(Clone, Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
pub struct Progress {
    pub stage: String,
    pub fraction: Option<f32>,
}

/// Stable processing stages used by the desktop task log.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Prepare,
    ScanModels,
    ReadModel,
    SpillGeometry,
    IndexTriangles,
    Rasterize,
    BuildOverviews,
    ValidateRaster,
    ReadGeoTiff,
    WriteCogLevel(usize),
    FlushCog,
    ScanLayers,
    CheckClip,
    ClipModel,
    PrepareTiles,
    ScanTiles,
    Mosaic,
    Pyramid,
    CreateStacta,
    GenerateTiles,
    GenerateTerrain,
    IndexTiles,
    Publish,
}

/// Progress with a stage identifier independent of its localized display text.
#[derive(Serialize, Deserialize)]
pub struct TaskProgress {
    pub phase: Phase,
    pub display: Progress,
    pub concurrency: Option<usize>,
}

impl TaskProgress {
    pub fn new(phase: Phase, stage: impl Into<String>, fraction: Option<f32>) -> Self {
        Self {
            phase,
            concurrency: None,
            display: Progress {
                stage: stage.into(),
                fraction,
            },
        }
    }

    pub fn with_concurrency(mut self, concurrency: usize) -> Self {
        self.concurrency = Some(concurrency);
        self
    }
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
        .map_err(|_| anyhow!(message!(the_origin_must_contain_three_coordinates)))?;
    ensure!(
        origin.iter().all(|v| v.is_finite()),
        message!(origin_coordinates_must_be_finite_numbers)
    );
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
    let mut roots = Vec::new();
    for entry in fs::read_dir(directory)? {
        check_cancelled(cancelled)?;
        let entry = entry?;
        if entry.file_name().eq_ignore_ascii_case("Block.osgb") && entry.path().is_file() {
            roots.push(entry.path());
        }
    }
    ensure!(
        roots.len() <= 1,
        message!(the_model_folder_has_multiple_block_osgb_entries)
    );
    if !roots.is_empty() {
        return Ok(roots);
    }

    // 无场景入口的数据仍按原有约定选择独立切片。
    let mut files = Vec::new();
    for entry in WalkDir::new(directory) {
        check_cancelled(cancelled)?;
        let entry = entry?;
        if entry.path().is_file() && is_osgb(entry.path()) {
            files.push(entry.into_path());
        }
    }
    ensure!(
        !files.is_empty(),
        message!(no_osgb_files_in_the_metadata_xml_folder)
    );
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
            Event::DocType(_) => bail!(message!(dtd_is_unsupported_in_metadata_xml)),
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
            _ => bail!(message!(crs_and_origin_fields_in_metadata_xml_must_be_text)),
        }
    }
}

fn prepare(
    request: &Request,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(TaskProgress),
    storage: Storage<'_>,
) -> Result<(Summary, Mesh, Option<Crs>)> {
    ensure!(
        request.resolution.is_finite() && request.resolution > 0.,
        message!(resolution_must_be_finite_and_positive)
    );
    ensure!(
        request.mode != Mode::Local || request.crs.is_none(),
        message!(local_mode_does_not_accept_a_crs)
    );
    ensure!(
        request.mode != Mode::Absolute || request.origin.is_none(),
        message!(absolute_coordinate_mode_does_not_accept_an_origin_offset)
    );
    progress(TaskProgress::new(
        Phase::ScanModels,
        message!(scan_osgb_and_metadata),
        None,
    ));
    let input_metadata = match fs::metadata(&request.input) {
        Err(error) if error.kind() == ErrorKind::NotFound => {
            bail!(message!(input_file_does_not_exist, request.input.display()));
        }
        result => result
            .with_context(|| message!(could_not_inspect_input_file, request.input.display()))?,
    };
    let metadata_input = is_metadata_file(&request.input);
    ensure!(
        input_metadata.is_file() && (metadata_input || is_osgb(&request.input)),
        message!(select_a_root_osgb_file_or_metadata_xml)
    );
    let input = request
        .input
        .canonicalize()
        .with_context(|| message!(could_not_open, request.input.display()))?;
    let root = input
        .parent()
        .context(message!(input_has_no_parent_folder))?;
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
        .map(|path| metadata_values(path).with_context(|| message!(could_not_read, path.display())))
        .transpose()?
        .unwrap_or_default();
    let origin = match request.mode {
        Mode::Absolute => [0.; 3],
        Mode::Local => request.origin.or(metadata_origin).unwrap_or([0.; 3]),
        Mode::Relative => request
            .origin
            .or(metadata_origin)
            .context(message!(missing_srs_origin))?,
    };
    ensure!(
        origin.iter().all(|v| v.is_finite()),
        message!(origin_coordinates_must_be_finite_numbers)
    );
    let crs = match request.mode {
        Mode::Local => None,
        _ => Some(request.crs.clone().or(metadata_crs).with_context(|| {
            let source = metadata.as_ref().unwrap_or(&input);
            message!(
                lacks_srs_specify_the_crs_or_select_local_mode,
                source.display()
            )
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
            message!(invalid_crs_in, source.display())
        })?;
    let (mesh, selected) = Mesh::load(root, selected, origin, cancelled, progress, storage)?;
    let bounds = mesh.bounds;
    let [min_x, min_y, _, max_x, max_y, _] = bounds;
    ensure!(
        max_x > min_x && max_y > min_y,
        message!(model_has_no_valid_xy_extent)
    );
    ensure!(
        min_x + request.resolution > min_x && max_y - request.resolution < max_y,
        message!(pixel_size_is_below_coordinate_precision)
    );
    let size =
        [max_x - min_x, max_y - min_y].map(|span| (span / request.resolution).ceil().max(1.));
    ensure!(
        size.iter().all(|v| *v <= f64::from(i32::MAX)),
        message!(raster_dimensions_exceed_the_supported_limits)
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
        triangles: mesh.count,
    };
    Ok((summary, mesh, epsg))
}

pub fn preflight(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> Result<Summary> {
    preflight_with_progress(request, cancelled, |event| progress(event.display))
}

pub fn preflight_with_progress(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(TaskProgress),
) -> Result<Summary> {
    let (summary, _, _) = prepare(request, cancelled, &mut progress, Storage::Preview)?;
    check_cancelled(cancelled)?;
    Ok(summary)
}

pub fn run(
    request: &Request,
    cancelled: &AtomicBool,
    progress: impl FnMut(Progress),
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<Vec<PathBuf>> {
    run_with_pool(request, cancelled, progress, before_publish, None)
}

pub fn run_with_pool(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
    before_publish: impl FnOnce() -> Result<()>,
    pool: Option<&ThreadPool>,
) -> Result<Vec<PathBuf>> {
    run_with_progress(
        request,
        cancelled,
        |event| progress(event.display),
        before_publish,
        pool,
    )
}

pub fn run_with_progress(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(TaskProgress),
    before_publish: impl FnOnce() -> Result<()>,
    pool: Option<&ThreadPool>,
) -> Result<Vec<PathBuf>> {
    let staging = StagedRaster::new(request)?;
    let products = compute_staged(
        request,
        &staging.output,
        staging.path(),
        cancelled,
        &mut progress,
        pool,
    )?;
    check_cancelled(cancelled)?;
    before_publish()?;
    staging.publish(request.overwrite)?;
    Ok(products)
}

pub struct StagedRaster {
    pub output: PathBuf,
    staging: tempfile::TempDir,
    _lock: OutputLock,
}

impl StagedRaster {
    pub fn new(request: &Request) -> Result<Self> {
        fs::create_dir_all(&request.output)
            .with_context(|| message!(create_folder, request.output.display()))?;
        let output = request
            .output
            .canonicalize()
            .with_context(|| message!(could_not_resolve_output_path, request.output.display()))?;
        let lock = OutputLock::acquire(&output).context(message!(output_folder_lock_failed))?;
        check_conflicts(&output, request.overwrite)?;
        let staging = tempfile::Builder::new()
            .prefix(".osgb-raster-")
            .tempdir_in(&output)
            .with_context(|| message!(create_temporary_folder_in, output.display()))?;
        Ok(Self {
            output,
            staging,
            _lock: lock,
        })
    }

    pub fn path(&self) -> &Path {
        self.staging.path()
    }

    pub fn publish(self, overwrite: bool) -> Result<()> {
        publish(self.path(), &self.output, overwrite)
    }
}

pub fn compute_staged(
    request: &Request,
    output: &Path,
    stage: &Path,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(TaskProgress),
    pool: Option<&ThreadPool>,
) -> Result<Vec<PathBuf>> {
    let storage = match std::env::var("WEBGIS_INTERNAL_OSGB_GEOMETRY").as_deref() {
        Ok("blocks") => Storage::Chunked(stage),
        _ => Storage::Auto(stage),
    };
    compute_with_storage(
        request,
        output,
        stage,
        cancelled,
        &mut progress,
        pool,
        storage,
    )
}

fn compute_with_storage(
    request: &Request,
    output: &Path,
    stage: &Path,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(TaskProgress),
    pool: Option<&ThreadPool>,
    storage: Storage<'_>,
) -> Result<Vec<PathBuf>> {
    const MODEL_COMPLETE: f32 = 0.20;
    const RASTER_COMPLETE: f32 = 0.80;
    let (summary, mut mesh, crs) = prepare(
        request,
        cancelled,
        &mut |mut event| {
            event.display.fraction = event
                .display
                .fraction
                .map(|fraction| fraction * MODEL_COMPLETE);
            progress(event);
        },
        storage,
    )?;
    check_cancelled(cancelled)?;
    let [width, height] = summary.size;
    let [west, _, _, _, north, _] = summary.bounds;
    let res = summary.resolution;
    let spec = RasterSpec {
        size: summary.size.map(|v| v as usize),
        transform: Affine([west, res, 0., north, 0., -res]),
        crs,
    };
    let (w, h) = (width as usize, height as usize);
    let count = w.div_ceil(TILE) as u64 * h.div_ceil(TILE) as u64;
    // Per-tile offsets, byte counts and coverage entries across all output
    // levels, including in-memory copies created by IFD writes and validation.
    let metadata_bytes = usize::try_from(count)
        .unwrap_or(usize::MAX)
        .saturating_mul(256);
    let output_bytes = metadata_bytes.saturating_add(8 * memory::MIB);
    let base_block = TILE * TILE * 24;
    let candidate_bytes = mesh.count.saturating_mul(2 * size_of::<usize>());
    if storage.automatic_stage().is_some()
        && !mesh.is_on_disk()
        && output_bytes
            .saturating_add(base_block)
            .saturating_add(candidate_bytes)
            .saturating_add(mesh.texture_peak)
            > memory::available_budget()
    {
        mesh.spill(stage, cancelled, &mut progress)?;
    }
    mesh.seal_disk();
    let _output = memory::reserve(output_bytes, message!(osgb_raster_buffers))?;
    let block_bytes = base_block.saturating_add(mesh.disk_query_bytes().unwrap_or(candidate_bytes));
    let dsm_path = stage.join(PRODUCTS[0]);
    let dom_path = stage.join(PRODUCTS[1]);
    let mut dsm_file = BigTiff::new_big(fs::File::create(&dsm_path)?)?;
    let mut dom_file = BigTiff::new_big(fs::File::create(&dom_path)?)?;
    let mut dsm = LevelWriter::new(&mut dsm_file, RasterKind::Dsm)?;
    let mut dom = LevelWriter::new(&mut dom_file, RasterKind::Dom)?;
    let index = if mesh.is_on_disk() {
        None
    } else {
        progress(TaskProgress::new(
            Phase::IndexTriangles,
            message!(build_triangle_index),
            Some(MODEL_COMPLETE),
        ));
        Some(raster::index(&mesh.triangles))
    };
    let textures = mesh.textures();
    progress(TaskProgress::new(
        Phase::Rasterize,
        message!(rasterize_0_count_blocks, count = count),
        Some(MODEL_COMPLETE),
    ));
    let raster_fraction =
        |done| MODEL_COMPLETE + (RASTER_COMPLETE - MODEL_COMPLETE) * (done as f32 / count as f32);
    let mut done = 0_u64;
    let mut dsm_valid_pixels = 0_u64;
    let mut dom_valid_pixels = 0_u64;
    let mut positions = (0..h)
        .step_by(TILE)
        .flat_map(|y| (0..w).step_by(TILE).map(move |x| (x, y)))
        .peekable();
    let mut last_width = None;
    while positions.peek().is_some() {
        if memory::pressure()
            || memory::available_budget() < block_bytes.saturating_add(mesh.texture_peak)
        {
            textures.trim_idle();
        }
        if memory::pressure() {
            mesh.reclaim_disk_cache();
        }
        tiff_output::wait_for_memory(cancelled, &[&dsm_path, &dom_path])?;
        let width = memory::width(task_pool::width(pool)).min(
            (memory::available_budget().saturating_sub(mesh.texture_peak) / block_bytes).max(1),
        );
        let batch: Vec<_> = positions.by_ref().take(width).collect();
        if last_width != Some(batch.len()) {
            last_width = Some(batch.len());
            progress(
                TaskProgress::new(
                    Phase::Rasterize,
                    message!(rasterize_done_count_blocks, done = done, count = count),
                    Some(raster_fraction(done)),
                )
                .with_concurrency(batch.len()),
            );
        }
        let _blocks = memory::reserve(
            block_bytes.saturating_mul(batch.len()),
            message!(osgb_raster_buffers),
        )?;
        let blocks = task_pool::map(pool, batch, |(x, y)| {
            check_cancelled(cancelled)?;
            let rect = [x, y, TILE.min(w - x), TILE.min(h - y)];
            let block = mesh.render(index.as_ref(), &textures, &summary, rect, cancelled)?;
            Ok::<_, anyhow::Error>(([rect[2], rect[3]], block))
        })?;
        for (size, block) in blocks {
            check_cancelled(cancelled)?;
            let (z, color) = block.into_planes();
            dsm_valid_pixels += z.iter().filter(|&&v| v != NODATA).count() as u64;
            dom_valid_pixels += color[3].iter().filter(|&&v| v != 0).count() as u64;
            dsm.height(size, &z)?;
            dom.color(size, &color)?;
            done += 1;
            progress(TaskProgress::new(
                Phase::Rasterize,
                message!(rasterize_done_count_blocks, done = done, count = count),
                Some(raster_fraction(done)),
            ));
        }
    }
    drop(textures);
    drop(index);
    mesh.finish()?;
    ensure!(
        dsm_valid_pixels > 0,
        message!(no_pixel_centers_fall_on_the_model_surface_reduce_the_pixel_size)
    );
    ensure!(
        dom_valid_pixels > 0,
        message!(the_topmost_surface_has_no_visible_texture_pixels_dom_cannot_be_generated)
    );
    dsm.finish(spec, false)?;
    if let Some(mask) = dom.finish(spec, false)? {
        mask.finish(&mut dom_file)?;
    }
    let levels: Vec<_> = [2, 4, 8, 16]
        .into_iter()
        .filter(|&n| n <= w.min(h))
        .collect();
    progress(TaskProgress::new(
        Phase::BuildOverviews,
        message!(build_internal_overviews),
        Some(RASTER_COMPLETE),
    ));
    tiff_output::overviews(
        &mut dsm_file,
        &dsm_path,
        spec,
        RasterKind::Dsm,
        &levels,
        cancelled,
    )
    .with_context(|| message!(build_internal_overviews_for, PRODUCTS[0]))?;
    tiff_output::overviews(
        &mut dom_file,
        &dom_path,
        spec,
        RasterKind::Dom,
        &levels,
        cancelled,
    )
    .with_context(|| message!(build_internal_overviews_for, PRODUCTS[1]))?;
    drop((dsm_file, dom_file));
    progress(TaskProgress::new(
        Phase::ValidateRaster,
        message!(validate_written_files),
        Some(0.90),
    ));
    for (path, kind, valid) in [
        (&dsm_path, RasterKind::Dsm, dsm_valid_pixels),
        (&dom_path, RasterKind::Dom, dom_valid_pixels),
    ] {
        tiff_output::verify(path, spec, kind, &levels, valid, cancelled)
            .with_context(|| message!(validate, path.display()))?;
    }
    let products: Vec<_> = PRODUCTS.iter().map(|name| output.join(name)).collect();
    let report = RunReport {
        schema_version: 1,
        summary: &summary,
        products: &products,
        dsm_valid_pixels,
        dom_valid_pixels,
    };
    let mut report_file = BufWriter::new(fs::File::create(stage.join(PRODUCTS[2]))?);
    serde_json::to_writer_pretty(&mut report_file, &report)?;
    report_file.flush()?;
    ensure!(
        fs::read_dir(stage)?.count() == PRODUCTS.len(),
        message!(output_contains_unexpected_geotiff_sidecar_files)
    );
    check_cancelled(cancelled)?;
    Ok(products)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<()> {
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        message!(processing_canceled)
    );
    Ok(())
}

fn check_conflicts(output: &Path, overwrite: bool) -> Result<()> {
    for name in PRODUCTS {
        let path = output.join(name);
        match path.symlink_metadata() {
            Ok(meta) => {
                ensure!(
                    meta.is_file(),
                    message!(output_path_is_not_a_regular_file, path.display())
                );
                ensure!(
                    overwrite,
                    message!(output_already_exists_confirm_overwrite, path.display())
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error).with_context(|| path.display().to_string()),
        }
        if name.ends_with(".tif") {
            for suffix in [".msk", ".ovr", ".aux.xml", ".msk.ovr", ".aux", ".tfw", "w"] {
                let sidecar = output.join(format!("{name}{suffix}"));
                ensure!(
                    matches!(sidecar.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                    message!(
                        a_sidecar_file_already_exists_at_the_output_location,
                        sidecar.display()
                    )
                );
            }
            for extension in ["tfw", "wld", "tifw", "aux"] {
                let sidecar = path.with_extension(extension);
                ensure!(
                    matches!(sidecar.symlink_metadata(), Err(e) if e.kind() == std::io::ErrorKind::NotFound),
                    message!(
                        a_sidecar_file_already_exists_at_the_output_location,
                        sidecar.display()
                    )
                );
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn rename_new(src: &Path, dst: &Path) -> Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, src, CWD, dst, RenameFlags::NOREPLACE)
        .with_context(|| format!("{} → {}", src.display(), dst.display()))
}

#[cfg(windows)]
fn rename_new(src: &Path, dst: &Path) -> Result<()> {
    use std::{io, os::windows::ffi::OsStrExt};
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let from: Vec<_> = src.as_os_str().encode_wide().chain([0]).collect();
    let to: Vec<_> = dst.as_os_str().encode_wide().chain([0]).collect();
    // The paths are derived from a canonicalized output path and fixed filenames.
    if unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), 0) } == 0 {
        return Err(io::Error::last_os_error())
            .with_context(|| format!("{} → {}", src.display(), dst.display()));
    }
    Ok(())
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
            bail!(message!(
                publishing_failed_error_rollback_failed_backup_retained_at,
                failures.join("；"),
                preserved.display(),
                error = error
            ));
        }
        return Err(error).context(message!(publishing_failed_changes_were_rolled_back));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
