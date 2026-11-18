use super::{
    check_cancelled,
    gdal::{Gdal, TILE},
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset};
use image::RgbaImage;
use std::{ffi::CStr, path::PathBuf, sync::atomic::AtomicBool};
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
    let api = Gdal::load()?;
    let source = api.open(&request.input)?;
    let projection = unsafe { (api.GDALGetProjectionRef)(source.handle) };
    ensure!(
        !projection.is_null() && !unsafe { CStr::from_ptr(projection) }.to_bytes().is_empty(),
        "输入影像缺少坐标系"
    );
    let mut source_transform = [0.0; 6];
    api.check(
        unsafe { (api.GDALGetGeoTransform)(source.handle, source_transform.as_mut_ptr()) },
        "输入影像缺少地理定位信息",
    )?;
    let determinant =
        source_transform[1] * source_transform[5] - source_transform[2] * source_transform[4];
    ensure!(
        source_transform.iter().all(|value| value.is_finite())
            && determinant.is_finite()
            && determinant != 0.0,
        "输入影像的地理变换无效"
    );
    let count = unsafe { (api.GDALGetRasterCount)(source.handle) };
    ensure!(
        (1..=4).contains(&count),
        "仅支持灰度、灰度 Alpha、RGB 或 RGBA 波段布局"
    );
    for band in 1..=count {
        ensure!(
            unsafe { (api.GDALGetRasterDataType)(source.band(band)) } == 1,
            "仅支持 8 位 GeoTIFF 波段"
        );
    }
    let first_interp = unsafe { (api.GDALGetRasterColorInterpretation)(source.band(1)) };
    ensure!(
        (count <= 2 && first_interp == 1) || (count >= 3 && first_interp == 3),
        "不支持的波段颜色布局"
    );
    if count == 2 {
        ensure!(
            unsafe { (api.GDALGetRasterColorInterpretation)(source.band(2)) } == 6,
            "灰度第二波段必须是 Alpha"
        );
    }
    if count >= 3 {
        ensure!(
            unsafe { (api.GDALGetRasterColorInterpretation)(source.band(2)) } == 4
                && unsafe { (api.GDALGetRasterColorInterpretation)(source.band(3)) } == 5,
            "RGB 波段顺序必须为红、绿、蓝"
        );
    }
    if count == 4 {
        ensure!(
            unsafe { (api.GDALGetRasterColorInterpretation)(source.band(4)) } == 6,
            "RGBA 第四波段必须是 Alpha"
        );
    }
    let warped = api.warp_mercator(&source)?;
    let source_mask_flags = unsafe { (api.GDALGetMaskFlags)(source.band(1)) };
    const ALL_VALID: i32 = 1;
    const ALPHA: i32 = 4;
    // 显式 Alpha 已参与输出透明度计算，独立掩膜才需要额外重采样。
    let mask_source = (source_mask_flags & (ALL_VALID | ALPHA) == 0)
        .then(|| api.mask_vrt(&source))
        .transpose()?;
    let mask_warped = mask_source
        .as_ref()
        .map(|mask| api.warp_mercator(mask))
        .transpose()?;
    let mask_band = mask_warped.as_ref().map(|mask| mask.band(1));
    let bands: Vec<_> = (1..=count).map(|band| warped.band(band)).collect();
    let mut gt = [0.0; 6];
    api.check(
        unsafe { (api.GDALGetGeoTransform)(warped.handle, gt.as_mut_ptr()) },
        "读取重投影范围失败",
    )?;
    ensure!(
        gt[2].abs() < 1e-10 && gt[4].abs() < 1e-10 && gt[1] > 0.0 && gt[5] < 0.0,
        "重投影结果不是北向上的栅格"
    );
    let width = unsafe { (api.GDALGetRasterXSize)(warped.handle) } as f64;
    let height = unsafe { (api.GDALGetRasterYSize)(warped.handle) } as f64;
    let bounds = [gt[0], gt[3] + height * gt[5], gt[0] + width * gt[1], gt[3]];
    let bounds = [
        bounds[0].max(-HALF_WORLD),
        bounds[1].max(-HALF_WORLD),
        bounds[2].min(HALF_WORLD),
        bounds[3].min(HALF_WORLD),
    ];
    ensure!(
        bounds[0] < bounds[2] && bounds[1] < bounds[3],
        "影像不覆盖 WebMercatorQuad 范围"
    );
    let id = request
        .id
        .as_deref()
        .or_else(|| request.output.file_name().and_then(|name| name.to_str()))
        .filter(|value| !value.trim().is_empty())
        .context("无法从输出目录推导 STAC ID，请填写 ID")?;
    let grid = RasterGrid {
        gt,
        width,
        height,
        bounds,
    };
    let zoom_ranges: Vec<_> = (request.min_zoom..=request.max_zoom)
        .map(|zoom| (zoom, candidate_tiles(grid.bounds, zoom)))
        .collect();
    for &(zoom, bounds) in &zoom_ranges {
        stacta::check_raster_size(zoom, bounds, TILE as u32)?;
    }
    let total: u64 = zoom_ranges
        .iter()
        .map(|(_, bounds)| {
            u64::from(bounds.max_x - bounds.min_x + 1) * u64::from(bounds.max_y - bounds.min_y + 1)
        })
        .sum();
    let mut levels = Vec::new();
    publish_directory(&request.output, |staging| {
        let mut visited = 0_u64;
        let mut planes = vec![vec![0; TILE * TILE]; bands.len()];
        let mut mask = vec![0; TILE * TILE];
        let mut rgba = RgbaImage::new(TILE as u32, TILE as u32);
        progress("生成地图切片".into(), Some(0.0));
        for (zoom, candidates) in zoom_ranges {
            let span = tile_span(zoom);
            let resampling = if span / TILE as f64 > gt[1].max(-gt[5]) {
                5
            } else {
                1
            };
            let mut coverage: Option<Bounds> = None;
            for y in candidates.min_y..=candidates.max_y {
                for x in candidates.min_x..=candidates.max_x {
                    check_cancelled(cancelled)?;
                    if let Some(window) = grid.tile_window(x, y, span) {
                        let count = (window.size[0] * window.size[1]) as usize;
                        for (&band, plane) in bands.iter().zip(&mut planes) {
                            api.read_resampled(
                                band,
                                window.source,
                                window.size,
                                &mut plane[..count],
                                resampling,
                            )?;
                        }
                        if let Some(band) = mask_band {
                            api.read_resampled(
                                band,
                                window.source,
                                window.size,
                                &mut mask[..count],
                                resampling,
                            )?;
                        }
                        if render_tile(
                            &mut rgba,
                            &planes,
                            mask_band.map(|_| mask.as_slice()),
                            window,
                        ) {
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
                    }
                    visited += 1;
                    if visited.is_multiple_of(32) || visited == total {
                        progress(
                            format!("生成 {zoom} 级地图切片"),
                            Some(visited as f32 / total as f32),
                        );
                    }
                }
            }
            if let Some(coverage) = coverage {
                levels.push((zoom, coverage));
            }
        }
        check_cancelled(cancelled)?;
        ensure!(!levels.is_empty(), "输入影像没有有效瓦片");
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

struct RasterGrid {
    gt: [f64; 6],
    width: f64,
    height: f64,
    bounds: [f64; 4],
}

struct TileWindow {
    source: [i32; 4],
    offset: [usize; 2],
    size: [i32; 2],
}

impl RasterGrid {
    fn tile_window(&self, x: u32, y: u32, span: f64) -> Option<TileWindow> {
        let [left, dx, _, top, _, dy] = self.gt;
        let [west, south, east, north] = self.bounds;
        let tile_left = -HALF_WORLD + f64::from(x) * span;
        let tile_top = HALF_WORLD - f64::from(y) * span;
        let px0 = ((tile_left.max(west) - left) / dx).floor().max(0.0) as i32;
        let py0 = ((tile_top.min(north) - top) / dy).floor().max(0.0) as i32;
        let px1 = (((tile_left + span).min(east) - left) / dx)
            .ceil()
            .min(self.width) as i32;
        let py1 = (((tile_top - span).max(south) - top) / dy)
            .ceil()
            .min(self.height) as i32;
        let pixels_per_unit = TILE as f64 / span;
        let ox = ((left + f64::from(px0) * dx - tile_left) * pixels_per_unit)
            .round()
            .clamp(0.0, TILE as f64) as i32;
        let oy = ((tile_top - (top + f64::from(py0) * dy)) * pixels_per_unit)
            .round()
            .clamp(0.0, TILE as f64) as i32;
        let dw =
            ((f64::from(px1 - px0) * dx * pixels_per_unit).round() as i32).min(TILE as i32 - ox);
        let dh =
            ((f64::from(py1 - py0) * -dy * pixels_per_unit).round() as i32).min(TILE as i32 - oy);
        (px1 > px0 && py1 > py0 && dw > 0 && dh > 0).then_some(TileWindow {
            source: [px0, py0, px1 - px0, py1 - py0],
            offset: [ox as usize, oy as usize],
            size: [dw, dh],
        })
    }
}

fn render_tile(
    rgba: &mut RgbaImage,
    planes: &[Vec<u8>],
    mask: Option<&[u8]>,
    window: TileWindow,
) -> bool {
    let [ox, oy] = window.offset;
    let [width, height] = window.size.map(|size| size as usize);
    let (red, green, blue, alpha) = match planes {
        [gray] => (gray, gray, gray, None),
        [gray, alpha] => (gray, gray, gray, Some(alpha)),
        [red, green, blue] => (red, green, blue, None),
        [red, green, blue, alpha] => (red, green, blue, Some(alpha)),
        _ => unreachable!(),
    };
    let rgba: &mut [u8] = rgba.as_mut();
    rgba.fill(0);
    let mut visible = false;
    for row in 0..height {
        let output_start = ((oy + row) * TILE + ox) * 4;
        let pixels = &mut rgba[output_start..output_start + width * 4];
        for (col, pixel) in pixels.chunks_exact_mut(4).enumerate() {
            let index = row * width + col;
            let alpha = alpha.map_or(255, |values| values[index]);
            let mask = mask.map_or(255, |values| values[index]);
            let opacity = (u16::from(alpha) * u16::from(mask) / 255) as u8;
            visible |= opacity != 0;
            pixel.copy_from_slice(&[red[index], green[index], blue[index], opacity]);
        }
    }
    visible
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::gdal::{Gdal, TILE};
    use std::{ffi::CString, fs, path::Path, process::Command};

    fn make_rgb(path: &Path, crs: Option<&str>, transform: [f64; 6], empty: bool) -> Result<()> {
        let api = Gdal::load()?;
        let _config = api.configure();
        let srs = crs.map(|definition| api.srs(definition)).transpose()?;
        let ds = api.create(path, [512, 512], transform, srs.as_ref(), true)?;
        let mut planes = [
            vec![180; 512 * 512],
            vec![70; 512 * 512],
            vec![20; 512 * 512],
            vec![255; 512 * 512],
        ];
        for y in 0..512 {
            let green = if y < 256 { 210 } else { 30 };
            planes[1][y * 512..(y + 1) * 512].fill(green);
        }
        if empty {
            planes[3].fill(0);
        } else {
            for y in 0..512 {
                planes[3][y * 512 + 255] = 128;
                for x in 256..512 {
                    planes[3][y * 512 + x] = 0;
                }
            }
        }
        ds.write_dom([0, 0, 512, 512], &mut planes)?;
        ds.close()
    }

    fn check_tiling(crs: &str, transform: [f64; 6], prefix: &str) -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = root.path().join("source.tif");
        make_rgb(&input, Some(crs), transform, false)?;
        let output = root.path().join("tiles");
        let request = Request {
            input,
            output: output.clone(),
            min_zoom: 0,
            max_zoom: 0,
            datetime: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")?,
            id: Some(prefix.to_owned()),
        };
        let levels = run(&request, &AtomicBool::new(false), |_, _| {}, || Ok(()))?;
        assert_eq!(levels, [0]);
        let moved = root.path().join("relocated");
        fs::rename(&output, &moved)?;
        let tile = image::open(moved.join("0/0/0.png"))?.into_rgba8();
        assert_eq!(tile.dimensions(), (TILE as u32, TILE as u32));
        assert!(tile.get_pixel(24, 24)[3] > 0);
        assert_eq!(tile.get_pixel(232, 24)[3], 0);
        assert!(tile.get_pixel(24, 24)[1] > tile.get_pixel(24, 232)[1]);
        assert_eq!(&tile.get_pixel(24, 24).0[..3], &[180, 210, 20]);
        let index_path = moved.join("stacta.json");
        let index: serde_json::Value = serde_json::from_slice(&fs::read(&index_path)?)?;
        assert_eq!(index["id"], prefix);
        assert_eq!(
            index["properties"]["tiles:tile_matrix_links"]["WebMercatorQuad"]["limits"]["0"]["max_tile_col"],
            0
        );
        if Command::new("gdalinfo").arg("--version").output().is_ok() {
            let opened = Command::new("gdalinfo").arg(&index_path).output()?;
            ensure!(
                opened.status.success(),
                "GDAL 无法打开搬移后的 STACTA：{}",
                String::from_utf8_lossy(&opened.stderr)
            );
            let metadata = String::from_utf8_lossy(&opened.stdout);
            ensure!(
                metadata.contains("256"),
                "GDAL STACTA 信息不完整：{metadata}"
            );
        }
        Ok(())
    }

    fn make_alpha(path: &Path, gray: bool) -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let api = Gdal::load()?;
        let name = CString::new(path.as_os_str().as_bytes())?;
        let driver = unsafe { (api.GDALGetDriverByName)(c"GTiff".as_ptr()) };
        let count = if gray { 2 } else { 4 };
        let ds = unsafe {
            (api.GDALCreate)(driver, name.as_ptr(), 512, 512, count, 1, std::ptr::null())
        };
        ensure!(!ds.is_null(), "测试 GeoTIFF 创建失败");
        let mut transform = [
            -HALF_WORLD,
            2.0 * HALF_WORLD / 512.0,
            0.0,
            HALF_WORLD,
            0.0,
            -2.0 * HALF_WORLD / 512.0,
        ];
        api.check(
            unsafe { (api.GDALSetGeoTransform)(ds, transform.as_mut_ptr()) },
            "测试 GeoTIFF 仿射变换失败",
        )?;
        let srs = api.srs("EPSG:3857")?;
        api.check(
            unsafe { (api.GDALSetSpatialRef)(ds, srs.handle) },
            "测试 GeoTIFF 坐标系设置失败",
        )?;
        let interpretations: &[i32] = if gray { &[1, 6] } else { &[3, 4, 5, 6] };
        let values: &[u8] = if gray {
            &[160, 160]
        } else {
            &[180, 70, 20, 255]
        };
        let mut pixels = vec![0_u8; 512 * 512];
        for (band_index, (&interpretation, &value)) in
            interpretations.iter().zip(values).enumerate()
        {
            api.check(
                unsafe {
                    (api.GDALSetRasterColorInterpretation)(
                        (api.GDALGetRasterBand)(ds, band_index as i32 + 1),
                        interpretation,
                    )
                },
                "测试 GeoTIFF 波段颜色设置失败",
            )?;
            pixels.fill(value);
            if interpretation == 6 {
                for y in 0..512 {
                    pixels[y * 512 + 255] = 128;
                    pixels[y * 512 + 256..(y + 1) * 512].fill(0);
                }
            }
            api.check(
                unsafe {
                    (api.GDALRasterIO)(
                        (api.GDALGetRasterBand)(ds, band_index as i32 + 1),
                        1,
                        0,
                        0,
                        512,
                        512,
                        pixels.as_mut_ptr().cast(),
                        512,
                        512,
                        1,
                        0,
                        0,
                    )
                },
                "测试 GeoTIFF 数据写入失败",
            )?;
        }
        api.check(unsafe { (api.GDALClose)(ds) }, "测试 GeoTIFF 关闭失败")
    }

    fn check_alpha_layout(gray: bool) -> Result<()> {
        let root = tempfile::tempdir()?;
        let input = root.path().join("alpha.tif");
        make_alpha(&input, gray)?;
        let output = root.path().join("tiles");
        let request = Request {
            input,
            output: output.clone(),
            min_zoom: 0,
            max_zoom: 2,
            datetime: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")?,
            id: None,
        };
        run(&request, &AtomicBool::new(false), |_, _| {}, || Ok(()))?;
        let tile = image::open(output.join("0/0/0.png"))?.into_rgba8();
        assert!(tile.get_pixel(24, 128)[3] > 0);
        assert!(tile.get_pixel(127, 128)[3] > 0 && tile.get_pixel(127, 128)[3] < 255);
        assert_eq!(tile.get_pixel(232, 128)[3], 0);
        if gray {
            assert_eq!(&tile.get_pixel(24, 128).0[..3], &[160, 160, 160]);
        } else {
            assert_eq!(&tile.get_pixel(24, 128).0[..3], &[180, 70, 20]);
        }
        let native = image::open(output.join("1/0/0.png"))?.into_rgba8();
        assert_eq!(native.get_pixel(255, 128)[3], 128);
        let index: serde_json::Value =
            serde_json::from_slice(&fs::read(output.join("stacta.json"))?)?;
        let last_column = (0..4)
            .rfind(|column| output.join("2").join(column.to_string()).is_dir())
            .unwrap();
        assert!(last_column < 3);
        assert_eq!(
            index["properties"]["tiles:tile_matrix_links"]["WebMercatorQuad"]["limits"]["2"]["max_tile_col"],
            last_column
        );
        let upsampled = image::open(output.join("2/1/0.png"))?.into_rgba8();
        assert!(upsampled.get_pixel(255, 128)[3] > 0 && upsampled.get_pixel(255, 128)[3] < 255);
        Ok(())
    }

    fn check_nodata_mask() -> Result<()> {
        use std::os::unix::ffi::OsStrExt;
        let root = tempfile::tempdir()?;
        let input = root.path().join("nodata.tif");
        let name = CString::new(input.as_os_str().as_bytes())?;
        let api = Gdal::load()?;
        let driver = unsafe { (api.GDALGetDriverByName)(c"GTiff".as_ptr()) };
        let ds =
            unsafe { (api.GDALCreate)(driver, name.as_ptr(), 512, 512, 1, 1, std::ptr::null()) };
        ensure!(!ds.is_null(), "NoData 测试 GeoTIFF 创建失败");
        let mut transform = [
            -HALF_WORLD,
            2.0 * HALF_WORLD / 512.0,
            0.0,
            HALF_WORLD,
            0.0,
            -2.0 * HALF_WORLD / 512.0,
        ];
        api.check(
            unsafe { (api.GDALSetGeoTransform)(ds, transform.as_mut_ptr()) },
            "NoData 测试仿射变换失败",
        )?;
        let srs = api.srs("EPSG:3857")?;
        api.check(
            unsafe { (api.GDALSetSpatialRef)(ds, srs.handle) },
            "NoData 测试坐标系失败",
        )?;
        let band = unsafe { (api.GDALGetRasterBand)(ds, 1) };
        api.check(
            unsafe { (api.GDALSetRasterColorInterpretation)(band, 1) },
            "NoData 测试灰度波段设置失败",
        )?;
        api.check(
            unsafe { (api.GDALSetRasterNoDataValue)(band, 0.0) },
            "NoData 测试值设置失败",
        )?;
        let mut pixels = vec![160_u8; 512 * 512];
        for row in 0..512 {
            pixels[row * 512 + 256..(row + 1) * 512].fill(0);
        }
        api.check(
            unsafe {
                (api.GDALRasterIO)(
                    band,
                    1,
                    0,
                    0,
                    512,
                    512,
                    pixels.as_mut_ptr().cast(),
                    512,
                    512,
                    1,
                    0,
                    0,
                )
            },
            "NoData 测试影像写入失败",
        )?;
        api.check(unsafe { (api.GDALClose)(ds) }, "NoData 测试影像关闭失败")?;
        let output = root.path().join("tiles");
        let request = Request {
            input,
            output: output.clone(),
            min_zoom: 0,
            max_zoom: 0,
            datetime: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z")?,
            id: None,
        };
        run(&request, &AtomicBool::new(false), |_, _| {}, || Ok(()))?;
        let tile = image::open(output.join("0/0/0.png"))?.into_rgba8();
        assert!(tile.get_pixel(24, 128)[3] > 0);
        assert_eq!(tile.get_pixel(232, 128)[3], 0);
        assert_eq!(&tile.get_pixel(24, 128).0[..3], &[160, 160, 160]);
        Ok(())
    }

    #[test]
    fn xyz_png_and_stacta_preserve_3857_orientation_and_internal_mask() -> Result<()> {
        let pixel = 2.0 * HALF_WORLD / 512.0;
        check_tiling(
            "EPSG:3857",
            [-HALF_WORLD, pixel, 0.0, HALF_WORLD, 0.0, -pixel],
            "mercator",
        )
    }

    #[test]
    fn xyz_png_and_stacta_reproject_4326() -> Result<()> {
        check_tiling(
            "EPSG:4326",
            [-180.0, 360.0 / 512.0, 0.0, 85.0, 0.0, -170.0 / 512.0],
            "geographic",
        )
    }

    #[test]
    fn handles_explicit_rgba_alpha() -> Result<()> {
        check_alpha_layout(false)
    }

    #[test]
    fn handles_gray_alpha() -> Result<()> {
        check_alpha_layout(true)
    }

    #[test]
    fn nodata_pixels_become_transparent() -> Result<()> {
        check_nodata_mask()
    }

    #[test]
    fn rejects_missing_crs_all_transparent_and_preserves_conflicts_and_cancellation() -> Result<()>
    {
        let root = tempfile::tempdir()?;
        let transform = [
            -HALF_WORLD,
            2.0 * HALF_WORLD / 512.0,
            0.0,
            HALF_WORLD,
            0.0,
            -2.0 * HALF_WORLD / 512.0,
        ];
        let no_crs = root.path().join("no-crs.tif");
        make_rgb(&no_crs, None, transform, false)?;
        let output = root.path().join("no-crs-output");
        let request = |input: PathBuf, output: PathBuf| Request {
            input,
            output,
            min_zoom: 0,
            max_zoom: 0,
            datetime: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap(),
            id: None,
        };
        let error = run(
            &request(no_crs, output.clone()),
            &AtomicBool::new(false),
            |_, _| {},
            || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("缺少坐标系"));
        assert!(!output.exists());

        let source = root.path().join("transparent.tif");
        make_rgb(&source, Some("EPSG:3857"), transform, true)?;
        let transparent_output = root.path().join("transparent-output");
        let error = run(
            &request(source.clone(), transparent_output.clone()),
            &AtomicBool::new(false),
            |_, _| {},
            || Ok(()),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("没有有效瓦片"));
        assert!(!transparent_output.exists());

        let conflict = root.path().join("existing");
        fs::create_dir(&conflict)?;
        fs::write(conflict.join("keep.txt"), b"preserve")?;
        let error = run(
            &request(source.clone(), conflict.clone()),
            &AtomicBool::new(false),
            |_, _| {},
            || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("已存在"));
        assert_eq!(fs::read(conflict.join("keep.txt"))?, b"preserve");

        let valid_source = root.path().join("valid.tif");
        make_rgb(&valid_source, Some("EPSG:3857"), transform, false)?;
        let entries_before = fs::read_dir(root.path())?.count();
        let output = root.path().join("cancelled-during-index");
        let cancelled = AtomicBool::new(false);
        let error = run(
            &request(valid_source, output.clone()),
            &cancelled,
            |stage, _| {
                if stage == "生成 STACTA 索引" {
                    cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
                }
            },
            || panic!("取消后仍尝试发布"),
        )
        .unwrap_err();
        assert!(format!("{error:#}").contains("已取消"));
        assert!(!output.exists());
        assert_eq!(fs::read_dir(root.path())?.count(), entries_before);

        let cancelled_output = root.path().join("cancelled");
        let cancelled = AtomicBool::new(true);
        let error = run(
            &request(source, cancelled_output.clone()),
            &cancelled,
            |_, _| {},
            || Ok(()),
        )
        .unwrap_err();
        assert!(error.to_string().contains("已取消"));
        assert!(!cancelled_output.exists());
        Ok(())
    }
}
