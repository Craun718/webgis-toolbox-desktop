use std::{
    collections::{BTreeMap, BTreeSet, HashMap, VecDeque},
    fs::{self, File},
    io::{BufWriter, Write},
    path::Path,
};
use webgis_core::task_pool::{self, ThreadPool};

use anyhow::{Context, Result, ensure};
use clap::ValueEnum;
use image::{
    ExtendedColorType, ImageEncoder, Rgba, RgbaImage,
    codecs::{jpeg::JpegEncoder, png::PngEncoder, webp::WebPEncoder},
};

use crate::tiles::{
    Coord, Format, Level, MAX_ZOOM, Scheme, make_parent, publish_directory, relative_path,
};

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Resampling {
    Nearest,
    Bilinear,
    Average,
}

pub struct Options {
    pub scheme: Scheme,
    pub source_zoom: u8,
    pub zooms: BTreeSet<u8>,
    pub resampling: Option<Resampling>,
    pub format: Format,
    pub background: [u8; 3],
}

pub fn parse_background(value: &str) -> Result<[u8; 3]> {
    let hex = value.strip_prefix('#').unwrap_or_default();
    ensure!(
        hex.len() == 6 && hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
        "背景颜色必须是 #RRGGBB"
    );
    Ok([
        u8::from_str_radix(&hex[..2], 16)?,
        u8::from_str_radix(&hex[2..4], 16)?,
        u8::from_str_radix(&hex[4..], 16)?,
    ])
}

pub fn parse_zooms(value: &str) -> Result<BTreeSet<u8>> {
    fn parse(value: &str) -> Result<u8> {
        let zoom: u8 = value
            .trim()
            .parse()
            .with_context(|| format!("无效级别 {value:?}"))?;
        ensure!(zoom <= MAX_ZOOM, "级别必须在 0–{MAX_ZOOM} 范围内");
        Ok(zoom)
    }

    let mut zooms = BTreeSet::new();
    for item in value.split(',') {
        if let Some((start, end)) = item.split_once('-') {
            let (start, end) = (parse(start)?, parse(end)?);
            ensure!(start <= end, "级别范围反向：{item}");
            zooms.extend(start..=end);
        } else {
            zooms.insert(parse(item)?);
        }
    }
    Ok(zooms)
}

pub fn run(level: &Level, options: &Options, output: &Path) -> Result<()> {
    run_with_pool(level, options, output, None)
}

pub fn run_with_pool(
    level: &Level,
    options: &Options,
    output: &Path,
    pool: Option<&ThreadPool>,
) -> Result<()> {
    run_with_publish(level, options, output, pool, || Ok(()))
}

pub fn run_with_publish(
    level: &Level,
    options: &Options,
    output: &Path,
    pool: Option<&ThreadPool>,
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let path = |root: &Path, zoom: u8, (x, y)| {
        root.join(relative_path(
            zoom,
            x,
            options.scheme.row(zoom, y),
            options.format.extension(),
        ))
    };
    publish_directory(output, |root| {
        let mut size = None;
        for (&coord, tile) in level {
            let decoded = tile.decode(size)?;
            size = Some(decoded.image.width());
            let out = path(root, options.source_zoom, coord);
            let expected_bands = if options.format == Format::Jpeg { 3 } else { 4 };
            if tile.format == options.format && decoded.bands == Some(expected_bands) {
                make_parent(&out)?;
                fs::copy(&tile.path, &out).with_context(|| {
                    format!("复制 {} 到 {}", tile.path.display(), out.display())
                })?;
            } else {
                encode(&out, &decoded.image.into_rgba8(), options)?;
            }
        }

        let workers = task_pool::width(pool);
        let mut sources: Vec<_> = (0..workers)
            .map(|_| Source::new(level, size.unwrap(), options.source_zoom, workers))
            .collect();
        for &zoom in &options.zooms {
            let delta = i32::from(zoom) - i32::from(options.source_zoom);
            let factor = 1_u32 << delta.unsigned_abs();
            let render =
                |(coord, scale, method, children): (Coord, f64, Resampling, Vec<Coord>),
                 source: &mut Source<'_>|
                 -> Result<()> {
                    let image = if method == Resampling::Average && delta < 0 {
                        area_average(source, coord, factor, &children)?
                    } else {
                        sample_tile(source, coord, scale, method)?
                    };
                    if image.pixels().any(|pixel| pixel[3] != 0) {
                        encode(&path(root, zoom, coord), &image, options)?;
                    }
                    Ok(())
                };
            if delta < 0 {
                let mut groups = BTreeMap::<Coord, Vec<Coord>>::new();
                for &(x, y) in level.keys() {
                    groups
                        .entry((x / factor, y / factor))
                        .or_default()
                        .push((x, y));
                }
                let jobs = groups.into_iter().map(|(parent, children)| {
                    (
                        parent,
                        f64::from(factor),
                        options.resampling.unwrap_or(Resampling::Average),
                        children,
                    )
                });
                for batch in task_pool::batches(jobs, pool) {
                    task_pool::map_with(pool, batch, &mut sources, render)?;
                }
            } else if delta > 0 {
                let method = options.resampling.unwrap_or(Resampling::Bilinear);
                let scale = 1.0 / f64::from(factor);
                let jobs = level.keys().flat_map(|&(x, y)| {
                    (y * factor..(y + 1) * factor).flat_map(move |row| {
                        (x * factor..(x + 1) * factor)
                            .map(move |col| ((col, row), scale, method, Vec::new()))
                    })
                });
                for batch in task_pool::batches(jobs, pool) {
                    task_pool::map_with(pool, batch, &mut sources, render)?;
                }
            }
        }
        before_publish()
    })
}

fn encode(path: &Path, image: &RgbaImage, options: &Options) -> Result<()> {
    make_parent(path)?;
    let mut writer =
        BufWriter::new(File::create(path).with_context(|| format!("创建 {}", path.display()))?);
    let (w, h) = image.dimensions();
    match options.format {
        Format::Png => {
            PngEncoder::new(&mut writer).write_image(image.as_raw(), w, h, ExtendedColorType::Rgba8)
        }
        Format::Webp => WebPEncoder::new_lossless(&mut writer).write_image(
            image.as_raw(),
            w,
            h,
            ExtendedColorType::Rgba8,
        ),
        Format::Jpeg => {
            let mut rgb = Vec::with_capacity(w as usize * h as usize * 3);
            for pixel in image.pixels() {
                let alpha = u32::from(pixel[3]);
                for (&channel, background) in pixel.0[..3].iter().zip(options.background) {
                    rgb.push(
                        ((u32::from(channel) * alpha + u32::from(background) * (255 - alpha) + 127)
                            / 255) as u8,
                    );
                }
            }
            JpegEncoder::new_with_quality(&mut writer, 90).write_image(
                &rgb,
                w,
                h,
                ExtendedColorType::Rgb8,
            )
        }
    }
    .with_context(|| format!("编码 {}", path.display()))?;
    writer
        .flush()
        .with_context(|| format!("写入 {}", path.display()))
}

struct Source<'a> {
    level: &'a Level,
    size: u32,
    world_pixels: i64,
    capacity: usize,
    cache: HashMap<Coord, RgbaImage>,
    order: VecDeque<Coord>,
}

impl<'a> Source<'a> {
    fn new(level: &'a Level, size: u32, zoom: u8, workers: usize) -> Self {
        Self {
            level,
            size,
            world_pixels: i64::from(size) << zoom,
            capacity: (256 * 1024 * 1024 / workers / (size as usize * size as usize * 4))
                .clamp(1, 64),
            cache: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    fn tile(&mut self, coord: Coord) -> Result<Option<&RgbaImage>> {
        if !self.cache.contains_key(&coord) {
            let Some(tile) = self.level.get(&coord) else {
                return Ok(None);
            };
            if self.order.len() == self.capacity {
                self.cache.remove(&self.order.pop_front().unwrap());
            }
            self.cache.insert(coord, tile.rgba(self.size)?);
            self.order.push_back(coord);
        }
        Ok(self.cache.get(&coord))
    }

    fn pixel(&mut self, x: i64, y: i64) -> Result<Rgba<u8>> {
        // 世界边界处延伸边缘像元，范围内的缺块保持透明。
        let x = x.clamp(0, self.world_pixels - 1) as u64;
        let y = y.clamp(0, self.world_pixels - 1) as u64;
        let size = u64::from(self.size);
        let coord = ((x / size) as u32, (y / size) as u32);
        Ok(self.tile(coord)?.map_or(Rgba([0; 4]), |image| {
            *image.get_pixel((x % size) as u32, (y % size) as u32)
        }))
    }
}

#[derive(Clone, Copy, Default)]
struct Premultiplied {
    rgb: [f64; 3],
    alpha: f64,
}

impl Premultiplied {
    fn add(&mut self, pixel: Rgba<u8>, weight: f64) {
        let alpha = f64::from(pixel[3]) * weight;
        for (sum, channel) in self.rgb.iter_mut().zip(pixel.0) {
            *sum += f64::from(channel) * alpha;
        }
        self.alpha += alpha;
    }

    fn pixel(self, area: f64) -> Rgba<u8> {
        let alpha = (self.alpha / area).round() as u8;
        if alpha == 0 {
            return Rgba([0; 4]);
        }
        let [r, g, b] = self.rgb.map(|sum| (sum / self.alpha).round() as u8);
        Rgba([r, g, b, alpha])
    }
}

fn area_average(
    source: &mut Source<'_>,
    (col, row): Coord,
    factor: u32,
    children: &[Coord],
) -> Result<RgbaImage> {
    let size = source.size;
    let mut sums = vec![Premultiplied::default(); (size * size) as usize];
    for &(x, y) in children {
        let base_x = u64::from(x - col * factor) * u64::from(size);
        let base_y = u64::from(y - row * factor) * u64::from(size);
        let factor = u64::from(factor);
        let tile = source.tile((x, y))?.unwrap();
        for (px, py, pixel) in tile.enumerate_pixels() {
            let out_x = (base_x + u64::from(px)) / factor;
            let out_y = (base_y + u64::from(py)) / factor;
            sums[(out_y * u64::from(size) + out_x) as usize].add(*pixel, 1.0);
        }
    }
    let area = f64::from(factor).powi(2);
    let mut image = RgbaImage::new(size, size);
    for (pixel, sum) in image.pixels_mut().zip(sums) {
        *pixel = sum.pixel(area);
    }
    Ok(image)
}

fn sample_tile(
    source: &mut Source<'_>,
    (col, row): Coord,
    scale: f64,
    method: Resampling,
) -> Result<RgbaImage> {
    let size = f64::from(source.size);
    let mut image = RgbaImage::new(source.size, source.size);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        let sx = (f64::from(col) * size + f64::from(x) + 0.5) * scale;
        let sy = (f64::from(row) * size + f64::from(y) + 0.5) * scale;
        *pixel = if method == Resampling::Bilinear {
            let (sx, sy) = (sx - 0.5, sy - 0.5);
            let (ix, iy) = (sx.floor(), sy.floor());
            let (fx, fy) = (sx - ix, sy - iy);
            let mut sum = Premultiplied::default();
            for (dy, wy) in [(0, 1.0 - fy), (1, fy)] {
                for (dx, wx) in [(0, 1.0 - fx), (1, fx)] {
                    if wx * wy > 0.0 {
                        sum.add(source.pixel(ix as i64 + dx, iy as i64 + dy)?, wx * wy);
                    }
                }
            }
            sum.pixel(1.0)
        } else {
            // 2 的幂次上采样时，目标像元完整落在一个源像元内。
            source.pixel(sx.floor() as i64, sy.floor() as i64)?
        };
    }
    Ok(image)
}
