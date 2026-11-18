use std::{
    fs,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result, ensure};
use chrono::{DateTime, FixedOffset, SecondsFormat};
use serde_json::{Map, json};

use crate::tiles::{
    Bounds, Format, HALF_WORLD, Index, Scheme, make_parent, publish_directory, publish_file,
    relative_path, tile_span,
};

pub fn run(
    input: &Path,
    scheme: Scheme,
    output: Option<&Path>,
    datetime: DateTime<FixedOffset>,
    id: Option<String>,
) -> Result<()> {
    ensure!(
        (scheme == Scheme::Tms) == output.is_some(),
        "--output 仅用于 TMS 导出，且为必填项"
    );
    let id = match id {
        Some(id) => id,
        None => input
            .canonicalize()?
            .file_name()
            .and_then(|name| name.to_str())
            .context("无法从切片根目录推导 ID，请指定 --id")?
            .to_owned(),
    };
    ensure!(!id.trim().is_empty(), "--id 不能为空");
    let index = Index::scan(input, scheme)?;
    let mut size = None;
    let mut layout = None;
    for (&zoom, level) in &index.0 {
        for (&(x, y), tile) in level {
            if scheme == Scheme::Xyz {
                ensure!(
                    tile.path == input.join(relative_path(zoom, x, y, tile.extension())),
                    "相对路径模板需要十进制且无前导零的坐标：{}",
                    tile.path.display()
                );
            }
            let decoded = tile.decode(size)?;
            size = Some(decoded.image.width());
            let bands = decoded.bands.with_context(|| {
                format!(
                    "STACTA 需要 8 位显式灰度、RGB 或 Alpha 波段：{}",
                    tile.path.display()
                )
            })?;
            ensure!(
                bands == decoded.image.color().channel_count(),
                "STACTA 需要显式的 Alpha 波段：{}",
                tile.path.display()
            );
            let tile_layout = (tile.extension(), bands, tile.format);
            if let Some((expected, ref first_path)) = layout {
                ensure!(
                    expected == tile_layout,
                    "扩展名或波段布局不一致：{} 与 {}",
                    tile.path.display(),
                    first_path
                );
            } else {
                layout = Some((tile_layout, tile.path.display()));
            }
        }
    }

    let ((ext, bands, format), _) = layout.unwrap();
    let size = size.unwrap();
    let write_index = |root: &Path| {
        write_index(
            root,
            &id,
            datetime,
            size,
            (ext, bands, format),
            index
                .0
                .iter()
                .map(|(&zoom, level)| (zoom, Bounds::of(level))),
        )
    };
    if let Some(output) = output {
        publish_directory(output, |root| {
            for (&zoom, level) in &index.0 {
                for (&(x, y), tile) in level {
                    let path = root.join(relative_path(zoom, x, y, ext));
                    make_parent(&path)?;
                    fs::copy(&tile.path, &path)
                        .with_context(|| format!("复制 {}", tile.path.display()))?;
                }
            }
            write_index(root)
        })
    } else {
        write_index(input)
    }
}

/// 写入由切片工具直接生成的 RGBA PNG 索引。
/// 每一级的范围须按实际写入的 XYZ 瓦片统计。
pub fn write_rgba_png(
    root: &Path,
    id: &str,
    datetime: DateTime<FixedOffset>,
    size: u32,
    levels: impl IntoIterator<Item = (u8, Bounds)>,
) -> Result<()> {
    write_index(root, id, datetime, size, ("png", 4, Format::Png), levels)
}

/// 校验 GDAL STACTA 使用的原点偏移和栅格尺寸。
pub fn check_raster_size(zoom: u8, bounds: Bounds, tile_size: u32) -> Result<()> {
    ensure!(zoom <= crate::tiles::MAX_ZOOM, "瓦片级别超出范围：{zoom}");
    let matrix_width = 1_u32 << zoom;
    ensure!(
        bounds.min_x <= bounds.max_x
            && bounds.max_x < matrix_width
            && bounds.min_y <= bounds.max_y
            && bounds.max_y < matrix_width,
        "级别 {zoom} 的瓦片范围无效"
    );
    let fits_gdal = |tiles: u32| u64::from(tiles) * u64::from(tile_size) <= i32::MAX as u64;
    ensure!(
        fits_gdal(bounds.min_x)
            && fits_gdal(bounds.min_y)
            && fits_gdal(bounds.max_x - bounds.min_x + 1)
            && fits_gdal(bounds.max_y - bounds.min_y + 1),
        "级别 {zoom} 的像元原点偏移或栅格尺寸超出 GDAL STACTA 的 32 位限制"
    );
    Ok(())
}

fn write_index(
    root: &Path,
    id: &str,
    datetime: DateTime<FixedOffset>,
    size: u32,
    (ext, bands, format): (&str, u8, Format),
    levels: impl IntoIterator<Item = (u8, Bounds)>,
) -> Result<()> {
    ensure!(!id.trim().is_empty(), "--id 不能为空");
    ensure!(size > 0, "瓦片像元尺寸必须大于零");
    let mut matrices = Vec::new();
    let mut limits = Map::new();
    let mut bbox = [180.0_f64, 90.0_f64, -180.0_f64, -90.0_f64];

    for (zoom, bounds) in levels {
        check_raster_size(zoom, bounds, size)?;
        let matrix_width = 1_u32 << zoom;
        let span = tile_span(zoom);
        let resolution = span / f64::from(size);
        matrices.push(json!({
            "type": "TileMatrixType",
            "identifier": zoom.to_string(),
            "scaleDenominator": resolution / 0.00028,
            "topLeftCorner": [-HALF_WORLD, HALF_WORLD],
            "tileWidth": size, "tileHeight": size,
            "matrixWidth": matrix_width, "matrixHeight": matrix_width,
        }));
        limits.insert(
            zoom.to_string(),
            json!({
                "min_tile_col": bounds.min_x, "max_tile_col": bounds.max_x,
                "min_tile_row": bounds.min_y, "max_tile_row": bounds.max_y,
            }),
        );

        let longitude = |x: u32| f64::from(x) * 360.0 / f64::from(matrix_width) - 180.0;
        let latitude = |y: u32| {
            let mercator =
                std::f64::consts::PI * (1.0 - 2.0 * f64::from(y) / f64::from(matrix_width));
            mercator.sinh().atan().to_degrees()
        };
        bbox[0] = bbox[0].min(longitude(bounds.min_x));
        bbox[1] = bbox[1].min(latitude(bounds.max_y + 1));
        bbox[2] = bbox[2].max(longitude(bounds.max_x + 1));
        bbox[3] = bbox[3].max(latitude(bounds.min_y));
    }

    ensure!(!matrices.is_empty(), "没有可写入 STACTA 的瓦片级别");
    let names: &[&str] = match bands {
        1 => &["gray"],
        2 => &["gray", "alpha"],
        3 => &["red", "green", "blue"],
        4 => &["red", "green", "blue", "alpha"],
        _ => unreachable!(),
    };
    let raster_bands: Vec<_> = names
        .iter()
        .map(|name| json!({"data_type": "uint8", "name": name}))
        .collect();
    let eo_bands: Vec<_> = names
        .iter()
        .map(|&name| match name {
            "red" | "green" | "blue" => json!({"name": name, "common_name": name}),
            _ => json!({"name": name}),
        })
        .collect();
    let media = match format {
        Format::Png => "image/png",
        Format::Jpeg => "image/jpeg",
        Format::Webp => "image/webp",
    };

    let [west, south, east, north] = bbox;
    let document = json!({
        "stac_version": "1.0.0",
        "stac_extensions": [
            "https://stac-extensions.github.io/tiled-assets/v1.0.0/schema.json",
            "https://stac-extensions.github.io/eo/v1.1.0/schema.json",
            "https://stac-extensions.github.io/raster/v1.1.0/schema.json"
        ],
        "type": "Feature",
        "id": id,
        "bbox": bbox,
        "geometry": {
            "type": "Polygon",
            "coordinates": [[[west, south], [east, south], [east, north], [west, north], [west, south]]],
        },
        "properties": {
            "datetime": datetime.to_utc().to_rfc3339_opts(SecondsFormat::AutoSi, true),
            "tiles:tile_matrix_sets": {
                "WebMercatorQuad": {
                    "type": "TileMatrixSetType",
                    "identifier": "WebMercatorQuad",
                    "supportedCRS": "http://www.opengis.net/def/crs/EPSG/0/3857",
                    "tileMatrix": matrices,
                }
            },
            "tiles:tile_matrix_links": {
                "WebMercatorQuad": { "url": "#WebMercatorQuad", "limits": limits }
            },
        },
        "asset_templates": {
            "tiles": {
                "href": format!("./{{TileMatrix}}/{{TileCol}}/{{TileRow}}.{ext}"),
                "type": media,
                "roles": ["data"],
                "eo:bands": eo_bands,
                "raster:bands": raster_bands,
            },
        },
        "assets": {},
        "links": [],
    });

    publish_file(&root.join("stacta.json"), |file| {
        let mut writer = BufWriter::new(file);
        serde_json::to_writer_pretty(&mut writer, &document)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        Ok(())
    })
}
