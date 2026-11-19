#![cfg(feature = "model-clip")]

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};
use webgis_core::model_clip::{Reference, Request, osgb, preflight, run, run_with_pool};

const WGS84_ESRI: &str = r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137,298.257223563]],PRIMEM["Greenwich",0],UNIT["Degree",0.0174532925199433]]"#;

#[test]
fn clips_the_same_nonzero_origin_mesh_with_all_four_vector_formats() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let mut request = fixture(root);
    let geojson = request.boundary.clone();
    let kml = root.join("range.kml");
    fs::write(
        &kml,
        r#"<k:kml xmlns:k="http://www.opengis.net/kml/2.2"><k:Placemark><k:Polygon>
        <k:outerBoundaryIs><k:LinearRing><k:coordinates>120,23,300 121,23,5
        121,24,30 120,24,600 120,23,300</k:coordinates></k:LinearRing></k:outerBoundaryIs>
        </k:Polygon></k:Placemark></k:kml>"#,
    )
    .unwrap();
    let shp = root.join("range.shp");
    let mut writer = shapefile::ShapeWriter::from_path(&shp).unwrap();
    let ring = [
        (120., 23.),
        (120., 24.),
        (121., 24.),
        (121., 23.),
        (120., 23.),
    ]
    .map(|(x, y)| shapefile::Point::new(x, y));
    writer
        .write_shape(&shapefile::Polygon::new(shapefile::PolygonRing::Outer(
            ring.to_vec(),
        )))
        .unwrap();
    writer.finalize().unwrap();
    drop(writer);
    fs::write(root.join("range.PRJ"), WGS84_ESRI).unwrap();

    let zip = root.join("range.zip");
    let mut archive = zip::ZipWriter::new(fs::File::create(&zip).unwrap());
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    for name in ["layers/first.shp", "layers/second.shp"] {
        archive.start_file(name, options).unwrap();
        archive.write_all(&fs::read(&shp).unwrap()).unwrap();
    }
    archive.finish().unwrap();
    assert_eq!(webgis_core::model_clip::zip_layers(&zip).unwrap().len(), 2);
    request.boundary = zip.clone();
    assert!(preflight(&request).err().unwrap().contains("选择 ZIP"));
    request.layer = Some("layers/second.shp".into());
    assert!(preflight(&request).err().unwrap().contains("缺少 .prj"));
    request.boundary_crs = Some("EPSG:4326".into());

    for (index, boundary) in [geojson, kml, shp, zip].into_iter().enumerate() {
        request.boundary = boundary;
        request.output = root.join(format!("output-{index}"));
        assert_eq!(
            preflight(&request).unwrap().entries,
            [PathBuf::from("mesh.osgb")]
        );
        let report = run(&request, &AtomicBool::new(false), |_| {}, || Ok(())).unwrap();
        system_reference_exists(&report.output);
        assert_eq!(report.triangles, 2);
        let bytes = fs::read(&report.entries[0]).unwrap();
        let checked = osgb::rewrite(&bytes, None).unwrap();
        assert_eq!(checked.bytes, bytes);
        assert_unit_square(&bytes);
    }
}

#[test]
fn cancellation_conflicts_and_invalid_input_leave_no_partial_output() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = fixture(temp.path());
    let cancel = AtomicBool::new(false);
    let error = run(
        &request,
        &cancel,
        |_| cancel.store(true, Ordering::Relaxed),
        || Ok(()),
    )
    .err()
    .unwrap();
    assert!(error.contains("已取消"));
    assert!(!request.output.exists());
    assert_no_staging(temp.path());

    let cancel = AtomicBool::new(false);
    let error = run(
        &request,
        &cancel,
        |_| {},
        || {
            fs::create_dir(&request.output).unwrap();
            fs::write(request.output.join("existing.txt"), "existing content").unwrap();
            Ok(())
        },
    )
    .err()
    .unwrap();
    assert!(error.contains("输出位置已存在"));
    assert_eq!(
        fs::read_to_string(request.output.join("existing.txt")).unwrap(),
        "existing content"
    );
    assert_no_staging(temp.path());

    request.output = temp.path().join("outside");
    fs::write(
        &request.boundary,
        r#"{"type":"Polygon","coordinates":[[[10,10],[11,10],[11,11],[10,10]]]}"#,
    )
    .unwrap();
    assert!(
        run(&request, &cancel, |_| {}, || Ok(()))
            .err()
            .unwrap()
            .contains("没有交集")
    );
    assert!(!request.output.exists());
    assert_no_staging(temp.path());

    let mut input = fs::read(&request.model).unwrap();
    input[12..16].copy_from_slice(&132u32.to_le_bytes());
    fs::write(&request.model, input).unwrap();
    assert!(
        preflight(&request)
            .err()
            .unwrap()
            .contains("序列化版本 132")
    );

    request.boundary = temp.path().join("bad.zip");
    fs::write(&request.boundary, b"not a zip").unwrap();
    assert!(preflight(&request).err().unwrap().contains("无法读取 ZIP"));
}

fn assert_unit_square(bytes: &[u8]) {
    let class = b"osg::Vec3Array";
    let start = bytes
        .windows(class.len())
        .position(|window| window == class)
        .unwrap()
        + class.len()
        + 4
        + 4
        + 9
        + 4
        + 2;
    let size = u32::from_le_bytes(bytes[start..start + 4].try_into().unwrap()) as usize;
    assert_eq!(size, 6);
    let vertices: Vec<[f64; 3]> = bytes[start + 4..start + 4 + size * 12]
        .chunks_exact(12)
        .map(|v| {
            std::array::from_fn(|i| {
                f64::from(f32::from_le_bytes(v[4 * i..4 * i + 4].try_into().unwrap()))
            })
        })
        .collect();
    for &[x, y, z] in &vertices {
        assert!((-1e-7..=1. + 1e-7).contains(&x));
        assert!((-1e-7..=1. + 1e-7).contains(&y));
        assert_eq!(z, 0.);
    }
    let area: f64 = vertices
        .chunks_exact(3)
        .map(|vertices| {
            let [a, b, c] = [vertices[0], vertices[1], vertices[2]];
            ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])) / 2.
        })
        .sum();
    assert!((area - 1.).abs() < 1e-7);
}

fn system_reference_exists(output: &Path) {
    let metadata = fs::read_to_string(output.join("metadata.xml")).unwrap();
    assert!(metadata.contains("<SRS>EPSG:4326</SRS>"));
    assert!(metadata.contains("<SRSOrigin>120,23,15</SRSOrigin>"));
}

fn assert_no_staging(root: &Path) {
    assert!(fs::read_dir(root).unwrap().all(|e| {
        !e.unwrap()
            .file_name()
            .to_string_lossy()
            .starts_with(".osgb-clip-")
    }));
}

fn fixture(root: &Path) -> Request {
    let model = root.join("mesh.osgb");
    fs::write(&model, triangle_scene()).unwrap();
    let boundary = root.join("shape.geojson");
    fs::write(
        &boundary,
        r#"{"type":"Polygon","coordinates":[[[120,23],[121,23],[121,24],[120,24],[120,23]]]}"#,
    )
    .unwrap();
    Request {
        model,
        entry: None,
        reference: Some(Reference {
            srs: "EPSG:4326".into(),
            origin: [120., 23., 15.],
        }),
        boundary,
        layer: None,
        boundary_crs: None,
        output: root.join("clipped"),
        allow_non_empty_output: false,
    }
}

fn triangle_scene() -> Vec<u8> {
    triangle_scene_with_texture(None)
}

fn triangle_scene_with_texture(texture: Option<&str>) -> Vec<u8> {
    let mut scene = vec![0xa1, 0x0e, 0x91, 0x6c, 0x45, 0x45, 0xfb, 0x1a];
    for n in [1u32, 131, 4, 1] {
        scene.extend(n.to_le_bytes());
    }
    scene.push(b'0');
    let mut geometry = vec![u8::from(texture.is_some())];
    if let Some(texture) = texture {
        geometry.extend(texture_state(texture));
    }
    geometry.extend([0, 0, 0, 1, 1, 0, 0, 0, 0, 0]);
    geometry.extend(1u32.to_le_bytes());
    let mut indices = Vec::new();
    for n in [0u32, 4, 3, 0, 1, 2] {
        indices.extend(n.to_le_bytes());
    }
    geometry.extend(object("osg::DrawElementsUInt", 2, indices));
    geometry.push(1);
    let mut vertices = Vec::new();
    vertices.extend(4u32.to_le_bytes());
    vertices.extend([0, 0]);
    vertices.extend(3u32.to_le_bytes());
    for v in [-2f32, -2., 0., 5., -2., 0., -2., 5., 0.] {
        vertices.extend(v.to_le_bytes());
    }
    geometry.extend(object("osg::Vec3Array", 3, vertices));
    geometry.extend([0; 12]);
    scene.extend(object("osg::Geometry", 1, geometry));
    scene
}

fn object(class: &str, id: u32, body: Vec<u8>) -> Vec<u8> {
    let mut bytes = (class.len() as u32).to_le_bytes().to_vec();
    bytes.extend(class.as_bytes());
    bytes.extend(((body.len() + 17) as u32).to_le_bytes());
    bytes.extend(id.to_le_bytes());
    bytes.extend(0u32.to_le_bytes());
    bytes.extend(2u32.to_le_bytes());
    bytes.push(0);
    bytes.extend(body);
    bytes
}

#[test]
fn applies_matrix_transform_in_model_space() {
    let temp = tempfile::tempdir().unwrap();
    let request = fixture(temp.path());
    let input = triangle_scene();
    let mut matrix = node_fields();
    matrix.push(1);
    matrix.extend(1u32.to_le_bytes());
    matrix.extend(((input.len() - 25 + 4) as u32).to_le_bytes());
    matrix.extend_from_slice(&input[25..]);
    matrix.extend(0u32.to_le_bytes());
    for value in [
        1f64, 0., 0., 0., 0., 1., 0., 0., 0., 0., 1., 0., 10., 0., 0., 1.,
    ] {
        matrix.extend(value.to_le_bytes());
    }
    let mut scene = input[..25].to_vec();
    scene.extend(object("osg::MatrixTransform", 4, matrix));
    fs::write(&request.model, scene).unwrap();
    fs::write(
        &request.boundary,
        r#"{"type":"Polygon","coordinates":[[[130,23],[131,23],[131,24],[130,24],[130,23]]]}"#,
    )
    .unwrap();
    let report = run(&request, &AtomicBool::new(false), |_| {}, || Ok(())).unwrap();
    assert_eq!(report.triangles, 2);
    assert_unit_square(&fs::read(&report.entries[0]).unwrap());
}

#[test]
fn preserves_relative_paging_and_reports_missing_tiles() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = fixture(temp.path());
    let model = temp.path().join("model");
    let tile = model.join("tiles/child.osgb");
    fs::create_dir_all(tile.parent().unwrap()).unwrap();
    fs::write(
        model.join("metadata.xml"),
        r#"<ModelMetadata version="1">
        <SRS>EPSG:4326</SRS><SRSOrigin>120,23,15</SRSOrigin></ModelMetadata>"#,
    )
    .unwrap();
    let input = triangle_scene();
    let mut lod = node_fields();
    lod.extend(0u32.to_le_bytes()); // CenterMode
    lod.push(0); // UserCenter
    lod.extend(0u32.to_le_bytes()); // RangeMode
    lod.push(0); // RangeList
    lod.extend([1, 0]); // DatabasePath present, empty
    lod.extend(0u32.to_le_bytes());
    lod.extend([0, 1]); // DisableExternalChildrenPaging, RangeDataList
    let names = [b"tiles\\child.osgb", b"tiles\\other.osgb"];
    lod.extend(2u32.to_le_bytes());
    lod.extend((4 + names.iter().map(|name| 4 + name.len() as u32).sum::<u32>()).to_le_bytes());
    for name in names {
        lod.extend((name.len() as u32).to_le_bytes());
        lod.extend(name);
    }
    lod.extend(2u32.to_le_bytes());
    lod.extend(20u32.to_le_bytes());
    for _ in names {
        lod.extend(0f32.to_le_bytes());
        lod.extend(1f32.to_le_bytes());
    }
    lod.push(0); // Children
    let mut scene = input[..25].to_vec();
    scene.extend(object("osg::PagedLOD", 4, lod));
    fs::write(model.join("Block.osgb"), scene).unwrap();
    request.model = model;
    assert!(preflight(&request).err().unwrap().contains("child.osgb"));
    let textured = triangle_scene_with_texture(Some("albedo.ppm"));
    fs::write(&tile, &textured).unwrap();
    fs::write(tile.with_file_name("other.osgb"), textured).unwrap();
    fs::write(
        tile.with_file_name("albedo.ppm"),
        b"P6\n1 1\n255\n\xff\x80\x00",
    )
    .unwrap();
    let cancelled = AtomicBool::new(false);
    let report = run(&request, &cancelled, |_| {}, || Ok(())).unwrap();
    assert_eq!(report.files, 3);
    assert_unit_square(&fs::read(report.output.join("tiles/child.osgb")).unwrap());
    let entry = fs::read(&report.entries[0]).unwrap();
    assert!(
        entry
            .windows(names[0].len())
            .any(|bytes| bytes == b"tiles/child.osgb")
    );

    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(3)
        .build()
        .unwrap();
    request.output = temp.path().join("parallel");
    let parallel = run_with_pool(&request, &cancelled, |_| {}, || Ok(()), Some(&pool)).unwrap();
    assert_eq!(
        (parallel.files, parallel.triangles),
        (report.files, report.triangles)
    );
    for path in [
        "Block.osgb",
        "metadata.xml",
        "tiles/child.osgb",
        "tiles/other.osgb",
        "tiles/albedo.ppm",
    ] {
        assert_eq!(
            fs::read(parallel.output.join(path)).unwrap(),
            fs::read(report.output.join(path)).unwrap()
        );
    }
    assert_eq!(
        fs::read_dir(parallel.output.join("tiles")).unwrap().count(),
        3
    );

    request.output = temp.path().join("cancelled");
    let error = run_with_pool(
        &request,
        &cancelled,
        |progress| {
            if progress.completed == 1 {
                cancelled.store(true, Ordering::Relaxed);
            }
        },
        || panic!("取消后仍尝试发布"),
        Some(&pool),
    )
    .err()
    .unwrap();
    assert!(error.contains("已取消"));
    assert!(!request.output.exists());
}

fn node_fields() -> Vec<u8> {
    let mut node = vec![0, 0, 0, 0, 0, 1];
    node.extend(u32::MAX.to_le_bytes());
    node.push(0);
    node
}

#[test]
fn completely_outside_geometry_omits_the_draw_call() {
    use geo::{Coord, LineString, Polygon};
    use webgis_core::model_clip::Boundary;
    let points = [(10., 10.), (11., 10.), (11., 11.), (10., 10.)].map(|(x, y)| Coord { x, y });
    let boundary =
        Boundary::new(&[Polygon::new(LineString::new(points.to_vec()), vec![])]).unwrap();
    let output = osgb::rewrite(&triangle_scene(), Some(&boundary)).unwrap();
    assert_eq!(output.info.triangles, 0);
    let class = b"osg::DrawElementsUInt";
    assert!(
        !output
            .bytes
            .windows(class.len())
            .any(|bytes| bytes == class)
    );
    assert_eq!(
        osgb::rewrite(&output.bytes, None).unwrap().info.triangles,
        0
    );
}

#[test]
fn external_texture_is_required_and_copied_with_its_relative_path() {
    let temp = tempfile::tempdir().unwrap();
    let request = fixture(temp.path());
    let path = "textures/albedo.ppm";
    fs::write(&request.model, triangle_scene_with_texture(Some(path))).unwrap();
    assert!(preflight(&request).err().unwrap().contains(path));
    let texture = temp.path().join(path);
    fs::create_dir_all(texture.parent().unwrap()).unwrap();
    let contents = b"P6\n1 1\n255\n\xff\x80\x00";
    fs::write(texture, contents).unwrap();
    let report = run(&request, &AtomicBool::new(false), |_| {}, || Ok(())).unwrap();
    assert_eq!(fs::read(report.output.join(path)).unwrap(), contents);
}

fn texture_state(name: &str) -> Vec<u8> {
    let mut texture = vec![0; 2 + 5 + 48 + 3 + 13 + 2];
    texture.push(1);
    texture.extend(10u32.to_le_bytes());
    texture.extend(b"osg::Image");
    texture.extend(6u32.to_le_bytes());
    texture.extend((name.len() as u32).to_le_bytes());
    texture.extend(name.as_bytes());
    texture.extend(0u32.to_le_bytes());
    texture.extend(2u32.to_le_bytes()); // External image storage
    texture.extend(0u32.to_le_bytes());
    texture.extend(2u32.to_le_bytes());
    texture.push(0);
    texture.extend([0; 8]);
    let mut attribute = object("osg::Texture2D", 5, texture);
    attribute.extend(1u32.to_le_bytes());
    let mut state = vec![0, 0, 0, 1];
    state.extend(1u32.to_le_bytes());
    state.extend(((attribute.len() + 12) as u32).to_le_bytes());
    state.extend(1u32.to_le_bytes());
    state.extend(((attribute.len() + 4) as u32).to_le_bytes());
    state.extend(attribute);
    state.extend([0; 17]);
    state.extend([1, 0, 0]);
    object("osg::StateSet", 4, state)
}

#[test]
fn selected_output_directory_requires_confirmation_and_preserves_existing_files() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = fixture(temp.path());
    let cancelled = AtomicBool::new(false);
    fs::create_dir(&request.output).unwrap();
    assert!(!preflight(&request).unwrap().output_non_empty);
    let existing = request.output.join("existing.txt");
    fs::write(&existing, "keep").unwrap();
    assert!(preflight(&request).unwrap().output_non_empty);
    assert!(
        run(&request, &cancelled, |_| {}, || Ok(()))
            .err()
            .unwrap()
            .contains("请确认")
    );
    request.allow_non_empty_output = true;
    let conflict = request.output.join("metadata.xml");
    fs::write(&conflict, "existing metadata").unwrap();
    assert!(
        run(&request, &cancelled, |_| {}, || Ok(()))
            .err()
            .unwrap()
            .contains("输出位置已存在")
    );
    assert_eq!(fs::read_to_string(&conflict).unwrap(), "existing metadata");
    assert!(!request.output.join("mesh.osgb").exists());
    fs::remove_file(conflict).unwrap();
    let report = run(&request, &cancelled, |_| {}, || Ok(())).unwrap();
    assert_eq!(report.output, request.output);
    assert!(report.entries[0].is_file());
    assert_eq!(fs::read_to_string(existing).unwrap(), "keep");
    assert_no_staging(temp.path());
}
