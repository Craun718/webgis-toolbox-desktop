use super::*;
use crate::dom::{
    geotiff::{Affine, Crs, write_geotags},
    tiff_output::{BigTiff, LevelWriter, RasterKind, RasterSpec},
};
use std::{fs, path::Path};
use tiff::{
    encoder::{Compression, TiffEncoder},
    tags::{Predictor, Tag},
};

fn make_rgb(path: &Path, crs: Option<&str>, transform: [f64; 6], empty: bool) -> Result<()> {
    let mut file = BigTiff::new_big(fs::File::create(path)?)?;
    let mut level = LevelWriter::new(&mut file, RasterKind::Dom)?;
    for row in 0..2 {
        for column in 0..2 {
            let mut alpha = vec![if empty || column == 1 { 0 } else { 255 }; TILE * TILE];
            if !empty && column == 0 {
                for values in alpha.chunks_exact_mut(TILE) {
                    values[TILE - 1] = 128;
                }
            }
            let planes = [
                vec![180; TILE * TILE],
                vec![if row == 0 { 210 } else { 30 }; TILE * TILE],
                vec![20; TILE * TILE],
                alpha,
            ];
            level.color([TILE; 2], &planes)?;
        }
    }
    level
        .finish(
            RasterSpec {
                size: [512; 2],
                crs: crs.map(Crs::parse).transpose()?,
                transform: Affine(transform),
            },
            false,
        )?
        .unwrap()
        .finish(&mut file)
}

fn world_transform() -> Affine {
    let step = 2.0 * HALF_WORLD / 512.0;
    Affine([-HALF_WORLD, step, 0., HALF_WORLD, 0., -step])
}

fn make_alpha(path: &Path, gray: bool, datetime: Option<&str>) -> Result<()> {
    use tiff::{
        encoder::colortype::{ColorType, RGBA8},
        tags::{PhotometricInterpretation, SampleFormat},
    };
    struct GrayA8;
    impl ColorType for GrayA8 {
        type Inner = u8;
        const TIFF_VALUE: PhotometricInterpretation = PhotometricInterpretation::BlackIsZero;
        const BITS_PER_SAMPLE: &'static [u16] = &[8, 8];
        const SAMPLE_FORMAT: &'static [SampleFormat] = &[SampleFormat::Uint; 2];

        fn horizontal_predict(row: &[u8], result: &mut Vec<u8>) {
            result.extend_from_slice(&row[..2]);
            result.extend(row[2..].iter().zip(row).map(|(a, b)| a.wrapping_sub(*b)));
        }
    }
    let mut file = TiffEncoder::new(fs::File::create(path)?)?
        .with_compression(Compression::Lzw)
        .with_predictor(Predictor::Horizontal);
    let mut values = Vec::new();
    for _ in 0..512 {
        for x in 0..512 {
            if gray {
                values.push(160);
            } else {
                values.extend_from_slice(&[180, 70, 20]);
            }
            values.push(match x {
                0..255 => 255,
                255 => 128,
                _ => 0,
            });
        }
    }
    let crs = Some(Crs::from_code(3857)?);
    if gray {
        let mut image = file.new_image::<GrayA8>(512, 512)?;
        image.rows_per_strip(64)?;
        write_geotags(image.encoder(), crs, world_transform(), false)?;
        image.encoder().write_tag(Tag::ExtraSamples, 2_u16)?;
        if let Some(datetime) = datetime {
            image.encoder().write_tag(Tag::DateTime, datetime)?;
        }
        image.write_data(&values)?;
    } else {
        let mut image = file.new_image::<RGBA8>(512, 512)?;
        image.rows_per_strip(64)?;
        write_geotags(image.encoder(), crs, world_transform(), false)?;
        image.encoder().write_tag(Tag::ExtraSamples, 2_u16)?;
        image.write_data(&values)?;
    }
    Ok(())
}

#[test]
fn reads_tiff_datetime_and_handles_missing_or_invalid_values() -> Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("metadata.tif");
    make_alpha(&input, true, Some("2024:05:06 12:34:56"))?;
    let MetadataTime::Found(datetime) = metadata_datetime(&input)? else {
        panic!("expected TIFF datetime");
    };
    assert_eq!(
        datetime.format("%Y:%m:%d %H:%M:%S").to_string(),
        "2024:05:06 12:34:56"
    );
    make_alpha(&input, true, Some("invalid"))?;
    assert!(matches!(
        metadata_datetime(&input)?,
        MetadataTime::Invalid(_)
    ));
    make_alpha(&input, true, None)?;
    assert_eq!(metadata_datetime(&input)?, MetadataTime::Missing);
    Ok(())
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
    assert_eq!(
        index["asset_templates"]["tiles"]["href"],
        "./{TileMatrix}/{TileCol}/{TileRow}.png"
    );
    Ok(())
}

fn check_alpha_layout(gray: bool) -> Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("alpha.tif");
    make_alpha(&input, gray, None)?;
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
    let index: serde_json::Value = serde_json::from_slice(&fs::read(output.join("stacta.json"))?)?;
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
    let root = tempfile::tempdir()?;
    let input = root.path().join("nodata.tif");
    let mut file = TiffEncoder::new(fs::File::create(&input)?)?;
    let mut image = file.new_image::<tiff::encoder::colortype::Gray8>(512, 512)?;
    write_geotags(
        image.encoder(),
        Some(Crs::from_code(3857)?),
        world_transform(),
        false,
    )?;
    image.encoder().write_tag(Tag::GdalNodata, "0")?;
    let mut pixels = vec![0; 512 * 512];
    for row in pixels.chunks_exact_mut(512) {
        row[..256].fill(160);
    }
    image.write_data(&pixels)?;
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

fn tile_request(input: PathBuf, output: PathBuf, zoom: u8) -> Request {
    Request {
        input,
        output,
        min_zoom: zoom,
        max_zoom: zoom,
        datetime: DateTime::parse_from_rfc3339("2026-01-01T00:00:00Z").unwrap(),
        id: None,
    }
}

fn locate([east, north]: [f64; 2], zoom: u8) -> (u32, u32, u32, u32) {
    let scale = TILE as f64 / tile_span(zoom);
    let x = ((east + HALF_WORLD) * scale).floor() as u32;
    let y = ((HALF_WORLD - north) * scale).floor() as u32;
    (
        x / TILE as u32,
        y / TILE as u32,
        x % TILE as u32,
        y % TILE as u32,
    )
}

#[test]
fn reprojects_known_geographic_cgcs2000_and_utm_points() -> Result<()> {
    let root = tempfile::tempdir()?;
    // 投影坐标取中央经线与北纬 24° 的交点，独立于运行时的反投影。
    for (code, center, lon, step) in [
        (4490, [114., 24.], 114_f64, 0.001),
        (4547, [500_000., 2_655_288.653_822_5], 114., 100.),
        (32649, [500_000., 2_654_226.538_441_13], 111., 100.),
    ] {
        let input = root.path().join(format!("{code}.tif"));
        let output = root.path().join(code.to_string());
        let mut file = TiffEncoder::new(fs::File::create(&input)?)?;
        let mut image = file.new_image::<tiff::encoder::colortype::Gray8>(32, 32)?;
        let [x, y] = center;
        let transform = Affine([x - 16. * step, step, 0., y + 16. * step, 0., -step]);
        write_geotags(
            image.encoder(),
            Some(Crs::from_code(code)?),
            transform,
            false,
        )?;
        let mut values = vec![0; 32 * 32];
        for row in values.chunks_exact_mut(32).skip(12).take(8) {
            row[12..20].fill(180);
        }
        image.write_data(&values)?;
        let request = tile_request(input, output.clone(), 12);
        run(&request, &AtomicBool::new(false), |_, _| {}, || Ok(()))?;
        let (col, row, px, py) = locate(
            [
                HALF_WORLD * lon / 180.,
                6_378_137. * 24_f64.to_radians().tan().asinh(),
            ],
            12,
        );
        let tile = image::open(output.join(relative_path(12, col, row, "png")))?.into_rgba8();
        assert_eq!(
            tile.get_pixel(px, py).0,
            [180, 180, 180, 255],
            "EPSG:{code}"
        );
    }
    Ok(())
}

#[test]
fn rotated_pixel_is_point_and_area_sampling_preserve_known_pixels() -> Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("rotated.tif");
    let output = root.path().join("tiles");
    let step = tile_span(8) / TILE as f64;
    let point = [(128. + 0.5) * step, (128. + 0.5) * step];
    let mut file = TiffEncoder::new(fs::File::create(&input)?)?;
    let mut image = file.new_image::<tiff::encoder::colortype::Gray8>(32, 32)?;
    let transform = Affine([
        point[0] - 8. * (step / 2. + step / 4.),
        step / 2.,
        step / 4.,
        point[1] - 8. * (step / 4. - step / 2.),
        step / 4.,
        -step / 2.,
    ]);
    write_geotags(
        image.encoder(),
        Some(Crs::from_code(3857)?),
        transform,
        true,
    )?;
    image.encoder().write_tag(Tag::GdalNodata, "0")?;
    let mut values = vec![0; 32 * 32];
    for row in values.chunks_exact_mut(32).skip(7).take(3) {
        row[7..10].fill(200);
    }
    image.write_data(&values)?;
    let request = tile_request(input, output.clone(), 8);
    run(&request, &AtomicBool::new(false), |_, _| {}, || Ok(()))?;
    let (x, y, px, py) = locate(point, 8);
    let tiles = image::open(output.join(relative_path(8, x, y, "png")))?.into_rgba8();
    assert_eq!(tiles.get_pixel(px, py).0, [200, 200, 200, 255]);
    let edge = tiles.get_pixel(px + 1, py).0;
    assert_eq!(edge[..3], [200; 3]);
    assert!(edge[3] > 0 && edge[3] < 255);
    assert_eq!(tiles.get_pixel(px + 10, py).0, [0; 4]);
    Ok(())
}

fn make_geokey_tiff(path: &Path, entries: &[[u16; 4]], doubles: &[f64]) -> Result<()> {
    let mut file = TiffEncoder::new(fs::File::create(path)?)?;
    let mut image = file.new_image::<tiff::encoder::colortype::Gray8>(32, 32)?;
    write_geotags(
        image.encoder(),
        None,
        Affine([113.9, 0.006, 0., 24.1, 0., -0.006]),
        false,
    )?;
    let mut directory = vec![1_u16, 1, 0, entries.len() as u16];
    directory.extend(entries.as_flattened());
    image
        .encoder()
        .write_tag(Tag::GeoKeyDirectoryTag, directory.as_slice())?;
    image
        .encoder()
        .write_tag(Tag::GeoDoubleParamsTag, doubles)?;
    let pixels: Vec<_> = (0..32 * 32).map(|index| (index % 251) as u8).collect();
    image.write_data(&pixels)?;
    Ok(())
}

const CUSTOM_KEYS: [[u16; 4]; 7] = [
    [1024, 0, 1, 2],
    [2048, 0, 1, 32767],
    [2050, 0, 1, 32767],
    [2054, 0, 1, 9102],
    [2057, 34736, 1, 0],
    [2059, 34736, 1, 1],
    [2061, 34736, 1, 2],
];
const CUSTOM_DOUBLES: [f64; 3] = [6_378_137., 298.257_222_101, 0.];

#[test]
fn tiles_fall_back_to_geographic_geokeys_with_one_warning_per_job() -> Result<()> {
    let root = tempfile::tempdir()?;
    let pool = rayon::ThreadPoolBuilder::new().num_threads(3).build()?;
    let cancelled = AtomicBool::new(false);
    let mut reference = None;
    for (code, reason) in [
        (Some(4490), None),
        (Some(32767), Some("32767")),
        (Some(0), Some("缺少可识别的 EPSG")),
        (None, Some("缺少可识别的 EPSG")),
        (Some(65000), Some("65000")),
    ] {
        let name = format!("{code:?}");
        let input = root.path().join(format!("{name}.tif"));
        let output = root.path().join(name);
        let mut keys = CUSTOM_KEYS.to_vec();
        if let Some(code) = code {
            keys[1][3] = code;
        } else {
            keys.remove(1);
        }
        if code == Some(65000) {
            keys.pop();
            keys.push([2051, 0, 1, 8901]);
            keys.sort_unstable();
        }
        let mut doubles = CUSTOM_DOUBLES;
        if reason.is_none() {
            doubles[0] = f64::NAN; // A valid EPSG takes priority over fallback parameters.
        }
        make_geokey_tiff(&input, &keys, &doubles)?;
        let mut request = tile_request(input.clone(), output.clone(), 12);
        request.id = Some("geokeys".into());
        let mut warnings = Vec::new();
        assert_eq!(
            run_with_logging(
                &request,
                &cancelled,
                |_| {},
                |warning| warnings.push(warning.to_owned()),
                || Ok(()),
                Some(&pool),
            )?,
            vec![12]
        );
        if let Some(reason) = reason {
            assert_eq!(warnings.len(), 1);
            assert!(warnings[0].contains(input.to_str().unwrap()));
            assert!(warnings[0].contains(reason), "{}", warnings[0]);
            assert!(warnings[0].contains("未应用到 WGS84 的基准转换"));
        } else {
            assert!(warnings.is_empty());
        }
        let mut files = std::collections::BTreeMap::new();
        for entry in walkdir::WalkDir::new(&output) {
            let entry = entry?;
            if entry.file_type().is_file() {
                files.insert(
                    entry.path().strip_prefix(&output)?.to_owned(),
                    fs::read(entry.path())?,
                );
            }
        }
        assert!(files.contains_key(Path::new("stacta.json")));
        assert!(
            files
                .keys()
                .filter(|path| path.extension().is_some_and(|ext| ext == "png"))
                .count()
                > 3
        );
        if let Some(reference) = &reference {
            assert_eq!(reference, &files);
        } else {
            reference = Some(files);
        }
    }
    Ok(())
}

#[test]
fn rejects_incomplete_or_conflicting_fallback_geokeys() -> Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("invalid.tif");
    let output = root.path().join("tiles");
    let mut without_flattening = CUSTOM_KEYS.to_vec();
    without_flattening.remove(5);
    for (keys, doubles, reason, fallback) in [
        (without_flattening, CUSTOM_DOUBLES, "2059", true),
        (
            CUSTOM_KEYS.to_vec(),
            [f64::INFINITY, CUSTOM_DOUBLES[1], 0.],
            "2057",
            true,
        ),
        (
            [CUSTOM_KEYS.as_slice(), &[[2051, 0, 1, 8901]]].concat(),
            [CUSTOM_DOUBLES[0], CUSTOM_DOUBLES[1], 1.],
            "格林尼治",
            true,
        ),
        (
            [CUSTOM_KEYS.as_slice(), &[[2058, 34736, 1, 0]]].concat(),
            CUSTOM_DOUBLES,
            "2058",
            true,
        ),
        (
            [CUSTOM_KEYS.as_slice(), &[[2062, 34736, 3, 0]]].concat(),
            CUSTOM_DOUBLES,
            "基准转换",
            true,
        ),
        (
            replaced_geokey(2, [2050, 0, 1, 6269]),
            CUSTOM_DOUBLES,
            "基准",
            true,
        ),
        (
            replaced_geokey(3, [2054, 0, 1, 9101]),
            CUSTOM_DOUBLES,
            "角度单位",
            true,
        ),
        (
            replaced_geokey(0, [1024, 0, 1, 1]),
            CUSTOM_DOUBLES,
            "二维地理坐标系",
            true,
        ),
        (
            replaced_geokey(1, [2048, 0, 1, 3857]),
            CUSTOM_DOUBLES,
            "与 GeoKey 坐标类型不符",
            false,
        ),
        (
            replaced_geokey(1, [2048, 0, 1, 4267]),
            CUSTOM_DOUBLES,
            "外部坐标转换格网",
            false,
        ),
    ] {
        make_geokey_tiff(&input, &keys, &doubles)?;
        let request = tile_request(input.clone(), output.clone(), 12);
        let error = run_with_logging(
            &request,
            &AtomicBool::new(false),
            |_| {},
            |_| panic!("unexpected warning"),
            || Ok(()),
            None,
        )
        .unwrap_err();
        let error = format!("{error:#}");
        assert!(error.contains(reason), "{error}");
        assert_eq!(error.contains("无法回退"), fallback, "{error}");
        assert!(!output.exists());
    }
    Ok(())
}

fn replaced_geokey(index: usize, replacement: [u16; 4]) -> Vec<[u16; 4]> {
    let mut keys = CUSTOM_KEYS.to_vec();
    keys[index] = replacement;
    keys
}

#[test]
fn decodes_jpeg_ycbcr_and_sub_ifd_mask() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("jpeg.tif");
    let mut tiff = TiffEncoder::new(fs::File::create(&path)?)?;
    let mut mask = tiff.extra_directory()?;
    let bits = mask.write_data(&[0x80_u8; 32][..])? as u32;
    for (tag, value) in [
        (Tag::ImageWidth, 16),
        (Tag::ImageLength, 16),
        (Tag::NewSubfileType, 4),
        (Tag::RowsPerStrip, 16),
        (Tag::StripByteCounts, 32),
        (Tag::StripOffsets, bits),
    ] {
        mask.write_tag(tag, value)?;
    }
    mask.write_tag(Tag::PhotometricInterpretation, 4_u16)?;
    let pointer = mask.finish_with_offsets()?.offset;
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 95).encode(
        &[200, 100, 25].repeat(16 * 16),
        16,
        16,
        image::ExtendedColorType::Rgb8,
    )?;
    let mut dir = tiff.image_directory()?;
    let offset = dir.write_data(jpeg.as_slice())? as u32;
    for (tag, value) in [
        (Tag::ImageWidth, 16),
        (Tag::ImageLength, 16),
        (Tag::RowsPerStrip, 16),
        (Tag::StripByteCounts, jpeg.len() as u32),
        (Tag::StripOffsets, offset),
    ] {
        dir.write_tag(tag, value)?;
    }
    dir.write_tag(Tag::SubIfd, pointer)?;
    dir.write_tag(Tag::BitsPerSample, &[8_u16; 3][..])?;
    dir.write_tag(Tag::SamplesPerPixel, 3_u16)?;
    dir.write_tag(Tag::PhotometricInterpretation, 6_u16)?;
    dir.write_tag(Tag::Compression, 7_u16)?;
    write_geotags(
        &mut dir,
        Some(Crs::from_code(3857)?),
        world_transform(),
        false,
    )?;
    dir.finish()?;
    let mut source = Source::open(&path)?;
    let sample = source.pixel(0, 7)?;
    assert_eq!(sample[3], 255.);
    for (sample, expected) in sample.into_iter().zip([200., 100., 25.]) {
        assert!((sample - expected).abs() < 4.);
    }
    assert_eq!(source.pixel(1, 7)?, [0.; 4]);
    Ok(())
}

#[test]
fn reads_large_bigtiff_through_independent_blocks() -> Result<()> {
    let root = tempfile::tempdir()?;
    let path = root.path().join("large.tif");
    let mut file = BigTiff::new_big(fs::File::create(&path)?)?;
    let mut dir = file.image_directory()?;
    let offsets = [
        dir.write_data(&[17_u8; TILE * TILE][..])?,
        dir.write_data(&[39_u8; TILE * TILE][..])?,
    ];
    // 等价于 4 GiB 解压数据，目录可复用相同的未压缩块。
    let size = 65_536_u32;
    let count = (size as usize / TILE).pow(2);
    let positions: Vec<_> = (0..count).map(|i| offsets[i % 2]).collect();
    dir.write_tag(Tag::ImageWidth, size)?;
    dir.write_tag(Tag::ImageLength, size)?;
    dir.write_tag(Tag::PhotometricInterpretation, 1_u16)?;
    dir.write_tag(Tag::BitsPerSample, 8_u16)?;
    dir.write_tag(Tag::TileWidth, TILE as u32)?;
    dir.write_tag(Tag::TileLength, TILE as u32)?;
    dir.write_tag(Tag::TileOffsets, positions.as_slice())?;
    dir.write_tag(
        Tag::TileByteCounts,
        vec![(TILE * TILE) as u64; count].as_slice(),
    )?;
    // 只有左上角的数据块落在 WebMercatorQuad 覆盖范围内。
    let span = tile_span(8);
    let step = span / TILE as f64;
    write_geotags(
        &mut dir,
        Some(Crs::from_code(3857)?),
        Affine([HALF_WORLD - span, step, 0., -HALF_WORLD + span, 0., -step]),
        false,
    )?;
    dir.finish()?;
    let mut source = Source::open(&path)?;
    assert_eq!(source.size, [size; 2]);
    assert_eq!(source.pixel(255, 1067)?, [17., 17., 17., 255.]);
    assert_eq!(source.pixel(256, 1067)?, [39., 39., 39., 255.]);
    assert_eq!(source.pixel(65_535, 65_535)?, [39., 39., 39., 255.]);
    let output = root.path().join("large-tiles");
    let request = tile_request(path, output.clone(), 8);
    run(&request, &AtomicBool::new(false), |_, _| {}, || Ok(()))?;
    let tile = image::open(output.join("8/255/255.png"))?.into_rgba8();
    assert_eq!(tile.get_pixel(128, 128).0, [17, 17, 17, 255]);
    Ok(())
}

#[test]
fn rejects_missing_georeferencing_and_unknown_or_grid_based_crs() -> Result<()> {
    let root = tempfile::tempdir()?;
    for (code, reason) in [
        (3857, "缺少地理定位"),
        (65000, "没有内置"),
        (4267, "外部坐标转换格网"),
    ] {
        let input = root.path().join(format!("{code}.tif"));
        let output = root.path().join(code.to_string());
        let mut tiff = TiffEncoder::new(fs::File::create(&input)?)?;
        let mut image = tiff.new_image::<tiff::encoder::colortype::Gray8>(16, 16)?;
        let model = if code == 4267 { 2 } else { 1 };
        let key = if model == 2 { 2048 } else { 3072 };
        image.encoder().write_tag(
            Tag::GeoKeyDirectoryTag,
            &[1_u16, 1, 0, 2, 1024, 0, 1, model, key, 0, 1, code][..],
        )?;
        image.write_data(&[128; 256])?;
        let request = tile_request(input.clone(), output.clone(), 0);
        let error = run(&request, &AtomicBool::new(false), |_, _| {}, || Ok(())).unwrap_err();
        let error = format!("{error:#}");
        assert!(
            error.contains(input.to_str().unwrap()) && error.contains(reason),
            "{error}"
        );
        assert!(!output.exists());
    }
    Ok(())
}

#[test]
fn rejects_missing_crs_all_transparent_and_preserves_conflicts_and_cancellation() -> Result<()> {
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
    assert!(format!("{error:#}").contains("缺少坐标系"));
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

#[test]
fn parallel_sampling_preserves_tiles_and_index_with_overlapping_candidate_ranges() -> Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("wrap.tif");
    make_rgb(
        &input,
        Some("EPSG:4326"),
        [175., 32. / 512., 0., 12., 0., -24. / 512.],
        false,
    )?;
    let source = Source::open(&input)?;
    let bounds = Grid::new(&source)?.bounds(source.size)?;
    assert_eq!(bounds.len(), 2);
    assert!(
        bounds
            .iter()
            .all(|&bound| candidate_tiles(bound, 0).min_x == 0)
    );
    assert_eq!(candidate_ranges(&bounds, 0).len(), 1);
    assert_eq!(candidate_ranges(&bounds, 3).len(), 2);

    let pool = rayon::ThreadPoolBuilder::new().num_threads(3).build()?;
    let mut request = tile_request(input, root.path().join("serial"), 0);
    request.max_zoom = 3;
    request.id = Some("test".into());
    let cancelled = AtomicBool::new(false);
    let mut progress = Vec::new();
    let serial = run(
        &request,
        &cancelled,
        |stage, fraction| progress.push((stage, fraction)),
        || Ok(()),
    )?;
    let fractions: Vec<_> = progress
        .iter()
        .map(|(stage, fraction)| fraction.expect(stage))
        .collect();
    assert_eq!(fractions.first(), Some(&0.0));
    assert!(fractions.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(fractions.iter().all(|&fraction| fraction < 1.0));
    assert_eq!(
        progress.last(),
        Some(&("生成 STACTA 索引".into(), Some(0.90)))
    );
    let serial_output = request.output;
    request.output = root.path().join("parallel");
    let parallel = run_with_pool(&request, &cancelled, |_, _| {}, || Ok(()), Some(&pool))?;
    assert_eq!(parallel, serial);
    assert!(parallel.contains(&0) && parallel.contains(&3));
    let files = |root: &Path| -> Result<std::collections::BTreeMap<PathBuf, Vec<u8>>> {
        walkdir::WalkDir::new(root)
            .into_iter()
            .filter_map(|entry| match entry {
                Ok(entry) if entry.file_type().is_file() => Some(Ok(entry.into_path())),
                Err(err) => Some(Err(err)),
                _ => None,
            })
            .map(|path| {
                let path = path?;
                Ok((path.strip_prefix(root)?.to_owned(), fs::read(path)?))
            })
            .collect()
    };
    assert_eq!(files(&serial_output)?, files(&request.output)?);

    request.output = root.path().join("cancelled");
    let result = run_with_pool(
        &request,
        &cancelled,
        |stage, fraction| {
            if stage == "生成地图切片" && fraction == Some(0.0) {
                cancelled.store(true, std::sync::atomic::Ordering::Relaxed);
            }
        },
        || panic!("取消后仍尝试发布"),
        Some(&pool),
    );
    assert!(format!("{:#}", result.unwrap_err()).contains("已取消"));
    assert!(!request.output.exists());
    Ok(())
}
