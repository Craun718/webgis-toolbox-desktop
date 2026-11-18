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
    append_u32(&mut indices, &[0, 4, 3, 0, 1, 2]);
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
    let mut children = geometry(10, 1., Texture::File("lower.jpg"));
    children.extend(geometry(20, 4., Texture::Inline(png.get_ref())));
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
        input,
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
fn renders_highest_surface_texture_and_mask_consistently_across_blocks() {
    let temp = tempfile::tempdir().unwrap();
    let request = request(temp.path());
    let flag = AtomicBool::new(false);
    let (summary, mesh) = prepare(&request, &flag, &mut |_| {}).unwrap();
    assert_eq!(summary.selected, [request.input.join("tile_L9_0.osgb")]);
    assert_eq!(summary.bounds, [112., 218., 23., 116., 222., 26.]);
    assert_eq!(summary.size, [4, 4]);
    let index = raster::index(&mesh.triangles);
    let mut textures = TextureCache::new(&mesh.images);
    let mut render = |rect| {
        raster::render(
            &mesh.triangles,
            &index,
            &mut textures,
            &summary,
            rect,
            &flag,
        )
        .unwrap()
        .into_planes()
    };
    let (z, pixels) = render([0, 0, 4, 4]);
    assert_eq!(z[0], 26.);
    assert_eq!(z[3], gdal::NODATA);
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
    assert!(prepare(&request, &flag, &mut |_| {}).is_err());
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
#[ignore = "需要 GDAL 3.9+ 运行库"]
fn gdal_writes_verified_products_and_cancellation_preserves_them() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = request(temp.path());
    request.crs = Some("+proj=tmerc +lat_0=0 +lon_0=111 +k=0.9997 +x_0=500000 +y_0=0 +ellps=GRS80 +units=m +type=crs".into());
    let flag = AtomicBool::new(false);
    let files = run(&request, &flag, |_| {}, || Ok(())).unwrap();
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
