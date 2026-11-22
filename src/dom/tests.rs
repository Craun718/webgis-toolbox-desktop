use super::*;
use image::{ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};
use std::io::Cursor;

fn append_u32(bytes: &mut Vec<u8>, values: &[u32]) {
    for value in values {
        bytes.extend(value.to_le_bytes());
    }
}

fn string(bytes: &mut Vec<u8>, value: &str) {
    append_u32(bytes, &[value.len() as u32]);
    bytes.extend(value.as_bytes());
}

fn block(bytes: &mut Vec<u8>, value: &[u8]) {
    append_u32(bytes, &[4 + value.len() as u32]);
    bytes.extend(value);
}

fn object(class: &str, id: u32, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    string(&mut out, class);
    let mut value = Vec::new();
    append_u32(&mut value, &[id, 0, 2]);
    value.push(0);
    value.extend_from_slice(body);
    block(&mut out, &value);
    out
}

enum Texture<'a> {
    Inline(&'a [u8]),
    File(&'a str),
}

fn state(id: u32, source: Texture<'_>) -> Vec<u8> {
    let mut texture = vec![0; 2 + 5 + 48 + 3 + 13 + 2];
    texture.push(1);
    string(&mut texture, "osg::Image");
    append_u32(&mut texture, &[id + 2]);
    match source {
        Texture::Inline(png) => {
            string(&mut texture, "albedo.png");
            append_u32(&mut texture, &[0, 1, png.len() as u32]);
            texture.extend(png);
        }
        Texture::File(path) => {
            string(&mut texture, path);
            append_u32(&mut texture, &[0, 2]);
        }
    }
    append_u32(&mut texture, &[0, 2]);
    texture.extend([0; 9]);
    let mut attribute = object("osg::Texture2D", id + 1, &texture);
    append_u32(&mut attribute, &[1]);
    let mut attributes = Vec::new();
    append_u32(&mut attributes, &[1]);
    block(&mut attributes, &attribute);
    let mut state = vec![0, 0, 0, 1];
    append_u32(&mut state, &[1]);
    block(&mut state, &attributes);
    state.extend([0; 17]);
    state.extend([1, 0, 0]);
    object("osg::StateSet", id, &state)
}

fn geometry(id: u32, height: f32, source: Texture<'_>) -> Vec<u8> {
    geometry_with_faces(id, height, source, 1)
}

fn geometry_with_faces(id: u32, height: f32, source: Texture<'_>, faces: u32) -> Vec<u8> {
    let array = |values: &[f32]| {
        let mut out = 4u32.to_le_bytes().to_vec();
        out.extend([0, 0]);
        append_u32(&mut out, &[3]);
        for value in values {
            out.extend(value.to_le_bytes());
        }
        out
    };
    let mut body = vec![1];
    body.extend(state(id + 4, source));
    body.extend([0, 0, 0, 1, 1, 0, 0, 0, 0, 0]);
    append_u32(&mut body, &[1]);
    let mut indices = Vec::new();
    append_u32(&mut indices, &[0, 4, 3 * faces]);
    for _ in 0..faces {
        append_u32(&mut indices, &[0, 1, 2]);
    }
    body.extend(object("osg::DrawElementsUInt", id + 1, &indices));
    body.push(1);
    body.extend(object(
        "osg::Vec3Array",
        id + 2,
        &array(&[0., 0., height, 4., 0., height, 0., 4., height]),
    ));
    body.extend([0; 4]);
    append_u32(&mut body, &[1]);
    body.extend(object(
        "osg::Vec2Array",
        id + 3,
        &array(&[0., 0., 1., 0., 0., 1.]),
    ));
    append_u32(&mut body, &[0]);
    object("osg::Geometry", id, &body)
}

fn fixture(input: &Path) -> Vec<u8> {
    fixture_with_faces(input, 1)
}

fn fixture_with_faces(input: &Path, faces: u32) -> Vec<u8> {
    textured_fixture(input, faces, [1., 4.])
}

fn textured_fixture(input: &Path, faces: u32, heights: [f32; 2]) -> Vec<u8> {
    let mut png = Cursor::new(Vec::new());
    RgbaImage::from_fn(1, 2, |_, row| {
        if row == 0 {
            Rgba([240, 0, 0, 255])
        } else {
            Rgba([0, 0, 240, 0])
        }
    })
    .write_to(&mut png, ImageFormat::Png)
    .unwrap();
    RgbImage::from_pixel(1, 1, Rgb([0, 240, 0]))
        .save(input.join("lower.jpg"))
        .unwrap();
    let mut children = geometry_with_faces(10, heights[0], Texture::File("lower.jpg"), faces);
    children.extend(geometry_with_faces(
        20,
        heights[1],
        Texture::Inline(png.get_ref()),
        faces,
    ));
    let mut body = vec![0, 0, 0, 0, 0, 1];
    append_u32(&mut body, &[u32::MAX]);
    body.extend([0, 1]);
    append_u32(&mut body, &[2]);
    block(&mut body, &children);
    append_u32(&mut body, &[0]);
    for value in [
        1f64, 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 12., 18., 2., 1.,
    ] {
        body.extend(value.to_le_bytes());
    }
    let mut scene = vec![0xa1, 0x0e, 0x91, 0x6c, 0x45, 0x45, 0xfb, 0x1a];
    append_u32(&mut scene, &[1, 131, 4, 1]);
    scene.push(b'0');
    scene.extend(object("osg::MatrixTransform", 1, &body));
    scene
}

fn request(root: &Path) -> Request {
    let input = root.join("model");
    fs::create_dir(&input).unwrap();
    let scene = fixture(&input);
    for name in ["tile.osgb", "tile_L8_0.osgb", "tile_L9_0.osgb"] {
        fs::write(
            input.join(name),
            if name == "tile_L9_0.osgb" {
                &scene[..]
            } else {
                b"coarse"
            },
        )
        .unwrap();
    }
    fs::write(
        input.join("metadata.xml"),
        "<ModelMetadata><SRS>EPSG:3857</SRS><SRSOrigin>100,200,20</SRSOrigin></ModelMetadata>",
    )
    .unwrap();
    Request {
        input: input.join("metadata.xml"),
        output: root.join("output"),
        metadata: None,
        crs: None,
        origin: None,
        mode: Mode::Relative,
        resolution: 1.,
        overwrite: false,
    }
}

#[test]
fn accepts_root_osgb_and_rejects_other_model_inputs() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = request(temp.path());
    let metadata = request.input.clone();
    let root_osgb = metadata.with_file_name("root.osgb");
    fs::copy(metadata.with_file_name("tile_L9_0.osgb"), &root_osgb).unwrap();
    let flag = AtomicBool::new(false);

    request.input = root_osgb.clone();
    let (summary, _, _) = prepare(&request, &flag, &mut |_| {}, Storage::Memory).unwrap();
    assert_eq!(summary.selected, [root_osgb]);
    assert_eq!(summary.metadata.as_ref(), Some(&metadata));

    let other_xml = metadata.with_file_name("other.xml");
    fs::copy(&metadata, &other_xml).unwrap();
    for invalid in [metadata.parent().unwrap().to_owned(), other_xml] {
        request.input = invalid;
        let error = prepare(&request, &flag, &mut |_| {}, Storage::Memory)
            .err()
            .unwrap();
        assert_eq!(error.to_string(), "请选择根 OSGB 文件或 metadata.xml");
    }

    request.input = metadata.with_file_name("missing.osgb");
    let error = prepare(&request, &flag, &mut |_| {}, Storage::Memory)
        .err()
        .unwrap();
    assert_eq!(
        error.to_string(),
        format!("输入文件不存在：{}", request.input.display())
    );
}

#[test]
fn preflight_localizes_progress_and_validation_without_changing_model_data() {
    use webgis_core::i18n::{Language, with_language};

    let temp = tempfile::tempdir().unwrap();
    let mut request = request(temp.path());
    let cancelled = AtomicBool::new(false);
    with_language(Language::Japanese, || {
        let mut progress = Vec::new();
        let summary = preflight_with_progress(&request, &cancelled, |event| {
            progress.push(event);
        })
        .unwrap();
        assert_eq!(summary.crs.as_deref(), Some("EPSG:3857"));
        assert_eq!(summary.origin, [100., 200., 20.]);
        assert!(progress.iter().any(|event| {
            event.phase == Phase::ReadModel
                && event.display.stage.contains("読み込み中")
                && event.display.stage.contains("tile_L9_0.osgb")
        }));
    });

    request.input = request.input.with_file_name("missing.osgb");
    with_language(Language::English, || {
        let error = preflight(&request, &cancelled, |_| {}).err().unwrap();
        assert_eq!(
            error.to_string(),
            format!("Input file does not exist: {}", request.input.display())
        );
    });
}

#[test]
fn follows_mixed_lod_pages_and_discards_embedded_coarse_geometry() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = request(temp.path());
    let root = request.input.with_file_name("Block.osgb");
    let low = root.with_file_name("tile_L8_0.osgb");
    let high = root.with_file_name("tile_L9_0.osgb");
    let leaf = fs::read(&high).unwrap();
    fs::write(&low, &leaf).unwrap();

    let mut lod = vec![0, 0, 0, 0, 0, 1];
    append_u32(&mut lod, &[u32::MAX]);
    lod.push(0);
    append_u32(&mut lod, &[0]);
    lod.push(0);
    append_u32(&mut lod, &[0]);
    lod.extend([0, 0]);
    append_u32(&mut lod, &[0]);
    lod.extend([0, 1]);
    let mut names = Vec::new();
    for name in ["", "tile_L8_0.osgb", "tile_L9_0.osgb"] {
        string(&mut names, name);
    }
    append_u32(&mut lod, &[3]);
    block(&mut lod, &names);
    append_u32(&mut lod, &[3]);
    block(&mut lod, &[0; 24]);
    lod.push(1);
    append_u32(&mut lod, &[1]);
    block(&mut lod, &geometry(10, 100., Texture::File("coarse.jpg")));

    let mut scene = leaf[..25].to_vec();
    scene.extend(object("osg::PagedLOD", 1, &lod));
    fs::write(&root, scene).unwrap();

    for input in [request.input.clone(), root] {
        request.input = input;
        let summary = preflight(&request, &AtomicBool::new(false), |_| {}).unwrap();
        assert_eq!(summary.selected, [low.clone(), high.clone()]);
        assert_eq!(summary.triangles, 4);
        assert_eq!(summary.bounds, [112., 218., 23., 116., 222., 26.]);
        let mut progress = Vec::new();
        request.overwrite = true;
        run(
            &request,
            &AtomicBool::new(false),
            |event| progress.push(event),
            || Ok(()),
        )
        .unwrap();
        let reading: Vec<_> = progress
            .iter()
            .filter(|event| event.stage.starts_with("已读取 "))
            .map(|event| event.fraction.unwrap())
            .collect();
        assert_eq!(reading.len(), 3);
        assert_eq!(reading[0], 0.0);
        assert!(reading.windows(2).all(|pair| pair[0] < pair[1]));
        // 目录另有一个未被引用的 OSGB，估算按目录计数，实际读取仍沿场景树。
        assert_eq!(reading[1], 0.05);
        assert_eq!(reading[2], 0.10);
    }
    let stage = StagedRaster::new(&request).unwrap();
    for storage in [
        Storage::Disk(stage.path()),
        Storage::ChunkedDisk(stage.path()),
    ] {
        let paths = compute_with_storage(
            &request,
            &stage.output,
            stage.path(),
            &AtomicBool::new(false),
            |_| {},
            None,
            storage,
        )
        .unwrap();
        for path in paths {
            assert_eq!(
                fs::read(&path).unwrap(),
                fs::read(stage.path().join(path.file_name().unwrap())).unwrap(),
            );
        }
    }
}

#[test]
fn renders_highest_surface_texture_and_mask_consistently_across_blocks() {
    let temp = tempfile::tempdir().unwrap();
    let request = request(temp.path());
    let flag = AtomicBool::new(false);
    let (summary, mesh, _) =
        prepare(&request, &flag, &mut |_| {}, Storage::Chunked(temp.path())).unwrap();
    assert!(!mesh.is_on_disk());
    assert_eq!(
        summary.selected,
        [request.input.with_file_name("tile_L9_0.osgb")]
    );
    assert_eq!(summary.bounds, [112., 218., 23., 116., 222., 26.]);
    assert_eq!(summary.size, [4, 4]);
    let index = raster::index(&mesh.triangles);
    let textures = TextureCache::new(&mesh.images);
    let render = |rect| {
        raster::render(&mesh.triangles, &index, &textures, &summary, rect, &flag)
            .unwrap()
            .into_planes()
    };
    let (z, pixels) = render([0, 0, 4, 4]);
    assert_eq!(z[0], 26.);
    assert_eq!(z[3], NODATA);
    assert_eq!(
        [pixels[0][0], pixels[1][0], pixels[2][0], pixels[3][0]],
        [240, 0, 0, 255]
    );
    assert_eq!(z[12], 26.);
    assert_eq!(pixels[3][12], 0);
    assert_eq!(pixels[3][3], 0);
    for x in [0, 2] {
        let (block_z, block_pixels) = render([x, 0, 2, 4]);
        for row in 0..4 {
            assert_eq!(
                block_z[row * 2..row * 2 + 2],
                z[row * 4 + x..row * 4 + x + 2]
            );
            for band in 0..4 {
                assert_eq!(
                    block_pixels[band][row * 2..row * 2 + 2],
                    pixels[band][row * 4 + x..row * 4 + x + 2]
                );
            }
        }
    }
    flag.store(true, Ordering::Relaxed);
    assert!(prepare(&request, &flag, &mut |_| {}, Storage::Memory).is_err());
}

#[test]
fn publication_restores_all_old_outputs_if_an_install_fails() {
    let temp = tempfile::tempdir().unwrap();
    let stage = temp.path().join("stage");
    let output = temp.path().join("output");
    fs::create_dir(&stage).unwrap();
    fs::create_dir(&output).unwrap();
    for name in PRODUCTS {
        fs::write(output.join(name), format!("old {name}")).unwrap();
    }
    fs::write(stage.join(PRODUCTS[0]), b"new").unwrap();
    assert!(publish(&stage, &output, false).is_err());
    let error = publish(&stage, &output, true).unwrap_err();
    assert!(error.to_string().contains("已回滚"));
    for name in PRODUCTS {
        assert_eq!(
            fs::read_to_string(output.join(name)).unwrap(),
            format!("old {name}")
        );
    }
    assert_eq!(fs::read_dir(&output).unwrap().count(), PRODUCTS.len());
}

#[test]
fn rust_writes_verified_products_and_cancellation_preserves_them() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = request(temp.path());
    request.crs = Some(r#"PROJCRS["CGCS2000",ID["EPSG",4547]]"#.into());
    let flag = AtomicBool::new(false);
    let mut progress = Vec::new();
    let files = run(&request, &flag, |event| progress.push(event), || Ok(())).unwrap();
    let fractions: Vec<_> = progress.iter().filter_map(|event| event.fraction).collect();
    assert_eq!(fractions.first(), Some(&0.0));
    assert!(progress.iter().any(|event| {
        event.stage.starts_with("已读取 0 个模型") && event.fraction == Some(0.0)
    }));
    assert!(fractions.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(fractions.iter().all(|&fraction| fraction < 1.0));
    let index = progress
        .iter()
        .position(|event| event.stage == "构建三角形索引")
        .unwrap();
    assert_eq!(progress[index].fraction, Some(0.20));
    assert_eq!(progress[index + 1].fraction, Some(0.20));
    assert!(progress.iter().any(|event| event.stage == "校验写出结果"));
    assert_eq!(files.len(), PRODUCTS.len());
    let original: Vec<_> = files.iter().map(|p| fs::read(p).unwrap()).collect();
    assert_eq!(&original[0][..4], b"II+\0");
    assert_eq!(&original[1][..4], b"II+\0");
    assert!(run(&request, &flag, |_| {}, || Ok(())).is_err());
    request.overwrite = true;
    assert!(
        run(
            &request,
            &flag,
            |p| {
                if p.stage.starts_with("栅格化") {
                    flag.store(true, Ordering::Relaxed);
                }
            },
            || Ok(())
        )
        .is_err()
    );
    for (path, bytes) in files.iter().zip(&original) {
        assert_eq!(fs::read(path).unwrap(), *bytes);
    }
    flag.store(false, Ordering::Relaxed);
    request.mode = Mode::Absolute;
    request.resolution = 0.5;
    let preview = preflight(&request, &flag, |_| {}).unwrap();
    assert_eq!(preview.origin, [0.; 3]);
    assert_eq!(preview.size, [8, 8]);
    let paths = run(&request, &flag, |_| {}, || Ok(())).unwrap();
    assert_eq!(paths, files);
    assert_ne!(fs::read(&paths[2]).unwrap(), original[2]);
    assert_eq!(
        fs::read_dir(&request.output).unwrap().count(),
        PRODUCTS.len()
    );
}

#[test]
fn chunked_and_sqlite_match_for_equal_height_textures_across_geometry_blocks() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let mut request = request(temp.path());
    request.input = request.input.with_file_name("tile_L9_0.osgb");
    let model = textured_fixture(request.input.parent().unwrap(), 33_000, [4., 4.]);
    fs::write(&request.input, model)?;
    let flag = AtomicBool::new(false);
    let stage = StagedRaster::new(&request)?;
    let paths = compute_with_storage(
        &request,
        &stage.output,
        stage.path(),
        &flag,
        |_| {},
        None,
        Storage::Disk(stage.path()),
    )?;
    stage.publish(false)?;
    request.overwrite = true;
    let stage = StagedRaster::new(&request)?;
    compute_with_storage(
        &request,
        &stage.output,
        stage.path(),
        &flag,
        |_| {},
        None,
        Storage::ChunkedDisk(stage.path()),
    )?;
    for (path, name) in paths.iter().zip(PRODUCTS) {
        assert_eq!(fs::read(path)?, fs::read(stage.path().join(name))?);
    }
    Ok(())
}

#[test]
fn local_bigtiff_preserves_pixels_and_overviews_across_tiles() -> Result<()> {
    use tiff::{
        decoder::{Decoder, DecodingResult},
        tags::Tag,
    };

    let temp = tempfile::tempdir()?;
    let mut request = request(temp.path());
    request.mode = Mode::Local;
    request.output = temp.path().join("local");
    request.resolution = 0.01;
    let cancelled = AtomicBool::new(false);
    let paths = run(&request, &cancelled, |_| {}, || Ok(()))?;
    let pool = rayon::ThreadPoolBuilder::new().num_threads(3).build()?;
    request.overwrite = true;
    let expected: Vec<_> = paths.iter().map(fs::read).collect::<std::io::Result<_>>()?;
    let parallel = run_with_pool(&request, &cancelled, |_| {}, || Ok(()), Some(&pool))?;
    assert_eq!(paths, parallel);
    for (path, bytes) in paths.iter().zip(expected) {
        assert_eq!(fs::read(path)?, bytes);
    }
    let stage = StagedRaster::new(&request)?;
    for storage in [
        Storage::Disk(stage.path()),
        Storage::ChunkedDisk(stage.path()),
    ] {
        let disk_paths = compute_with_storage(
            &request,
            &stage.output,
            stage.path(),
            &cancelled,
            |_| {},
            Some(&pool),
            storage,
        )?;
        assert_eq!(paths, disk_paths);
        for path in &paths {
            assert_eq!(
                fs::read(path)?,
                fs::read(stage.path().join(path.file_name().unwrap()))?
            );
        }
    }
    stage.publish(true)?;
    request.output = temp.path().join("cancelled");
    let result = run_with_pool(
        &request,
        &cancelled,
        |p| {
            if p.stage.starts_with("栅格化") {
                cancelled.store(true, Ordering::Relaxed);
            }
        },
        || panic!("取消后仍尝试发布"),
        Some(&pool),
    );
    assert!(result.unwrap_err().to_string().contains("已取消"));
    assert_eq!(fs::read_dir(&request.output)?.count(), 0);
    let base_dir = |path| -> Result<_> { Ok(Decoder::new(fs::File::open(path)?)?) };
    let mut dsm = base_dir(&paths[0])?;
    let mut dom = base_dir(&paths[1])?;
    assert_eq!(dsm.dimensions()?, (400, 400));
    assert_eq!(dom.dimensions()?, (400, 400));
    assert!(dsm.find_tag(Tag::GeoKeyDirectoryTag)?.is_none());
    assert_eq!(
        dsm.get_tag_f64_vec(Tag::ModelTiepointTag)?,
        [0., 0., 0., 112., 222., 0.]
    );
    assert_eq!(dsm.get_tag_ascii_string(Tag::GdalNodata)?, "-9999");
    let DecodingResult::F32(pixels) = dsm.read_image()? else {
        panic!("DSM 应为 Float32");
    };
    assert_eq!(&pixels[280 * 400 + 255..][..2], &[26., 26.]);
    assert_eq!(pixels[4 * 400 + 255], NODATA);
    let DecodingResult::U8(base) = dom.read_image()? else {
        panic!("DOM 应为 RGB");
    };
    dom.next_image()?;
    let base_mask = tiff_mask::Mask::read_tags(&mut dom.image_ifd())?;
    let mut mask_data = fs::File::open(&paths[1])?;
    let bits = [
        base_mask.read_chunk(&mut mask_data, 2)?,
        base_mask.read_chunk(&mut mask_data, 3)?,
    ];
    for x in [255, 256] {
        assert!(base_mask.valid(&bits[x / TILE], x % TILE, 280 % TILE));
    }
    for factor in [2, 4, 8, 16] {
        dsm.next_image()?;
        dom.next_image()?;
        let size = 400_u32.div_ceil(factor);
        assert_eq!(dsm.dimensions()?, (size, size));
        assert_eq!(dom.dimensions()?, (size, size));
        let DecodingResult::U8(overview) = dom.read_image()? else {
            panic!("DOM 概览应为 RGB");
        };
        dom.next_image()?;
        assert_eq!(dom.get_tag_u32(Tag::NewSubfileType)?, 5);
        let mask = tiff_mask::Mask::read_tags(&mut dom.image_ifd())?;
        let packed = mask.read_chunk(&mut mask_data, 0)?;
        let factor = factor as usize;
        for (ox, oy) in [(255 / factor, 280 / factor), (256 / factor, 280 / factor)] {
            let mut sum = [0_u32; 3];
            let mut count = 0_u32;
            for y in oy * factor..(oy + 1) * factor {
                for x in ox * factor..(ox + 1) * factor {
                    if base_mask.valid(&bits[x / TILE], x % TILE, y % TILE) {
                        let start = (y * 400 + x) * 3;
                        for (total, &value) in sum.iter_mut().zip(&base[start..start + 3]) {
                            *total += u32::from(value);
                        }
                        count += 1;
                    }
                }
            }
            assert!(count > 0 && mask.valid(&packed, ox, oy));
            let start = (oy * size as usize + ox) * 3;
            assert_eq!(
                &overview[start..start + 3],
                &sum.map(|value| ((value + count / 2) / count) as u8)
            );
        }
    }
    Ok(())
}

#[test]
fn small_budget_spills_pageable_geometry_and_preflight_stays_page_local() -> Result<()> {
    const CHILD: &str = "WEBGIS_TEST_OSGB_BUDGET_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let result = std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "dom::tests::small_budget_spills_pageable_geometry_and_preflight_stays_page_local",
            ])
            .env(CHILD, "1")
            .output()?;
        ensure!(
            result.status.success(),
            "{}{}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        return Ok(());
    }
    let temp = tempfile::tempdir()?;
    let request = request(temp.path());
    let root = request.input.parent().unwrap();
    let page = fixture_with_faces(root, 1_000);
    for id in 0..64 {
        fs::write(root.join(format!("tile_L9_{id}.osgb")), &page)?;
    }
    let flag = AtomicBool::new(false);
    memory::configure(memory::MIB);
    let summary = preflight_with_progress(&request, &flag, |event| {
        assert_ne!(event.phase, Phase::SpillGeometry)
    })?;
    assert_eq!(summary.selected.len(), 64);
    assert_eq!(summary.triangles, 128_000);
    memory::configure(16 * memory::MIB);
    let stage = StagedRaster::new(&request)?;
    let mut spilled = false;
    let mut raster_start = None;
    let products = compute_with_storage(
        &request,
        &stage.output,
        stage.path(),
        &flag,
        |event| {
            spilled |=
                event.phase == Phase::SpillGeometry && event.display.stage.contains("个三角形");
            if event.phase == Phase::Rasterize && raster_start.is_none() {
                raster_start = event.display.fraction;
            }
        },
        None,
        Storage::Auto(stage.path()),
    )?;
    assert!(spilled);
    assert_eq!(raster_start, Some(0.20));
    assert_eq!(fs::read_dir(stage.path())?.count(), PRODUCTS.len());
    stage.publish(false)?;
    assert!(products.iter().all(|path| path.is_file()));

    let mut request = request;
    request.overwrite = true;
    let original: Vec<_> = products
        .iter()
        .map(fs::read)
        .collect::<std::io::Result<_>>()?;
    for abort in [false, true] {
        let stage = StagedRaster::new(&request)?;
        let mut wrote_blocks = false;
        let result = compute_with_storage(
            &request,
            &stage.output,
            stage.path(),
            &flag,
            |event| {
                if event.phase == Phase::ReadModel && stage.path().join("mesh.blocks").is_file() {
                    wrote_blocks = true;
                    assert!(event.display.stage.contains("个几何块"));
                    if abort {
                        flag.store(true, Ordering::Relaxed);
                    }
                }
            },
            None,
            Storage::Chunked(stage.path()),
        );
        assert!(wrote_blocks);
        if abort {
            assert!(format!("{:#}", result.unwrap_err()).contains("已取消"));
            flag.store(false, Ordering::Relaxed);
        } else {
            assert_eq!(result?, products);
            for (name, bytes) in PRODUCTS.iter().zip(&original) {
                assert_eq!(fs::read(stage.path().join(name))?, *bytes);
            }
        }
        drop(stage);
        assert_eq!(fs::read_dir(&request.output)?.count(), PRODUCTS.len());
        for (path, bytes) in products.iter().zip(&original) {
            assert_eq!(fs::read(path)?, *bytes);
        }
    }

    memory::configure(128 * 1024);
    let error = preflight(&request, &flag, |_| {}).err().unwrap();
    assert!(error.is::<memory::BudgetError>());
    assert!(error.to_string().contains("tile_L9_0.osgb"));
    Ok(())
}
