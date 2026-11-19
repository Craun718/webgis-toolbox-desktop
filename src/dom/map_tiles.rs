use super::{check_cancelled, tiff_output::TILE, tiff_source::Source, tile_sampling::Grid};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset};
use image::RgbaImage;
use std::{path::PathBuf, sync::atomic::AtomicBool};
use tiles_toolbox::{
    stacta,
    tiles::{Bounds, HALF_WORLD, publish_directory, relative_path, tile_span},
};

pub const MAX_ZOOM: u8 = 23;

pub struct Request {
    pub input: PathBuf,
    pub output: PathBuf,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub datetime: DateTime<FixedOffset>,
    pub id: Option<String>,
}

pub fn run(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(String, Option<f32>),
    publish: impl FnOnce() -> Result<()>,
) -> Result<Vec<u8>> {
    ensure!(
        request.min_zoom <= request.max_zoom,
        "最小级别不能大于最大级别"
    );
    ensure!(request.max_zoom <= MAX_ZOOM, "级别须在 0–{MAX_ZOOM} 之间");
    ensure!(
        request
            .input
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("tif") || ext.eq_ignore_ascii_case("tiff")),
        "请选择 .tif 或 .tiff 文件"
    );
    check_cancelled(cancelled)?;
    let mut source = Source::open(&request.input)
        .with_context(|| format!("读取 {} 失败", request.input.display()))?;
    let grid = Grid::new(&source)?;
    let bounds = grid
        .bounds(source.size)
        .with_context(|| format!("投影 {} 的覆盖范围失败", request.input.display()))?;
    let id = request
        .id
        .as_deref()
        .or_else(|| request.output.file_name().and_then(|name| name.to_str()))
        .filter(|value| !value.trim().is_empty())
        .context("无法从输出目录推导 STAC ID，请填写 ID")?;
    let zoom_ranges: Vec<_> = (request.min_zoom..=request.max_zoom)
        .map(|zoom| (zoom, candidate_ranges(&bounds, zoom)))
        .collect();
    let mut total = 0_u64;
    for (zoom, ranges) in &zoom_ranges {
        for &bounds in ranges {
            stacta::check_raster_size(*zoom, bounds, TILE as u32)?;
            total += u64::from(bounds.max_x - bounds.min_x + 1)
                * u64::from(bounds.max_y - bounds.min_y + 1);
        }
    }
    let mut levels = Vec::new();
    publish_directory(&request.output, |staging| {
        let mut visited = 0_u64;
        let mut rgba = RgbaImage::new(TILE as u32, TILE as u32);
        progress("生成地图切片".into(), Some(0.0));
        for (zoom, candidates) in zoom_ranges {
            let span = tile_span(zoom);
            let pixel = span / TILE as f64;
            let mut coverage: Option<Bounds> = None;
            for (x, y) in candidates.into_iter().flat_map(|bounds| {
                (bounds.min_y..=bounds.max_y)
                    .flat_map(move |y| (bounds.min_x..=bounds.max_x).map(move |x| (x, y)))
            }) {
                check_cancelled(cancelled)?;
                let left = -HALF_WORLD + f64::from(x) * span;
                let top = HALF_WORLD - f64::from(y) * span;
                if grid
                    .render(&mut source, &mut rgba, [left, top], pixel, cancelled)
                    .with_context(|| {
                        format!("采样 {} 的 {zoom}/{x}/{y} 失败", request.input.display())
                    })?
                {
                    let path = staging.join(relative_path(zoom, x, y, "png"));
                    tiles_toolbox::tiles::make_parent(&path)?;
                    rgba.save(&path)
                        .with_context(|| format!("写入瓦片 {}", path.display()))?;
                    coverage
                        .get_or_insert(Bounds {
                            min_x: x,
                            max_x: x,
                            min_y: y,
                            max_y: y,
                        })
                        .include(x, y);
                }
                visited += 1;
                if visited.is_multiple_of(32) || visited == total {
                    progress(
                        format!("生成 {zoom} 级地图切片"),
                        Some(visited as f32 / total as f32),
                    );
                }
            }
            if let Some(coverage) = coverage {
                levels.push((zoom, coverage));
            }
        }
        check_cancelled(cancelled)?;
        ensure!(
            !levels.is_empty(),
            "{} 没有有效瓦片",
            request.input.display()
        );
        progress("生成 STACTA 索引".into(), None);
        stacta::write_rgba_png(
            staging,
            id,
            request.datetime,
            TILE as u32,
            levels.iter().copied(),
        )?;
        check_cancelled(cancelled)?;
        publish()
    })?;
    Ok(levels.into_iter().map(|(zoom, _)| zoom).collect())
}

fn candidate_ranges(bounds: &[[f64; 4]], zoom: u8) -> Vec<Bounds> {
    let mut ranges: Vec<Bounds> = Vec::with_capacity(bounds.len());
    for &bound in bounds {
        let next = candidate_tiles(bound, zoom);
        if let Some(current) = ranges.last_mut()
            && next.min_x <= current.max_x + 1
        {
            current.max_x = current.max_x.max(next.max_x);
        } else {
            ranges.push(next);
        }
    }
    ranges
}

fn candidate_tiles([west, south, east, north]: [f64; 4], zoom: u8) -> Bounds {
    let span = tile_span(zoom);
    let last = f64::from((1_u32 << zoom) - 1);
    let first_tile = |offset: f64| (offset / span).floor().clamp(0.0, last) as u32;
    // 东、南边界为开区间，落在瓦片边线上时排除相邻瓦片。
    let last_tile = |offset: f64| ((offset / span).ceil() - 1.0).clamp(0.0, last) as u32;
    let min_x = first_tile(west + HALF_WORLD);
    let min_y = first_tile(HALF_WORLD - north);
    Bounds {
        min_x,
        max_x: last_tile(east + HALF_WORLD).max(min_x),
        min_y,
        max_y: last_tile(HALF_WORLD - south).max(min_y),
    }
}

#[cfg(test)]
mod tests;
