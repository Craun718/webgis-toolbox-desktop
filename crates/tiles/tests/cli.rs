use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use image::{ImageFormat, Rgba, RgbaImage};
use serde_json::Value;

const DATETIME: &str = "2026-11-18T00:00:00Z";
const RED: [u8; 4] = [240, 0, 0, 128];
const BLUE: [u8; 4] = [0, 0, 240, 255];

fn cli(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_tiles-toolbox"))
        .current_dir(root)
        .env("PATH", "")
        .args(args)
        .output()
        .unwrap()
}

fn run(root: &Path, args: &[&str]) {
    let output = cli(root, args);
    assert!(
        output.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn fail(root: &Path, args: &[&str], message: &str) {
    let output = cli(root, args);
    assert!(!output.status.success(), "{args:?}");
    assert!(
        String::from_utf8_lossy(&output.stderr).contains(message),
        "{output:?}"
    );
}

fn tile(root: &Path, relative: &str, color: [u8; 4]) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    RgbaImage::from_pixel(16, 16, Rgba(color))
        .save(path)
        .unwrap();
}

fn fixture(root: &Path) {
    for (z, x, y, color) in [
        (2, 0, 1, RED),
        (2, 1, 1, BLUE),
        (2, 1, 2, [0, 200, 0, 255]),
        (0, 0, 0, [55, 66, 77, 255]),
    ] {
        tile(root, &format!("xyz/{z}/{x}/{y}.png"), color);
        tile(
            root,
            &format!("tms/{z}/{x}/{}.png", (1 << z) - 1 - y),
            color,
        );
    }
}

fn pixel(root: &Path, path: &str, x: u32, y: u32) -> [u8; 4] {
    image::open(root.join(path))
        .unwrap()
        .into_rgba8()
        .get_pixel(x, y)
        .0
}

#[test]
fn mosaic_and_stacta_xyz_tms() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    run(
        root,
        &["mosaic", "xyz", "--zoom", "2", "--output", "xyz.tif"],
    );
    run(
        root,
        &[
            "mosaic", "tms", "--scheme", "tms", "--zoom", "2", "--output", "tms.tif",
        ],
    );
    let xyz = fs::read(root.join("xyz.tif")).unwrap();
    assert_eq!(&xyz[..4], b"II*\0");
    assert_eq!(xyz, fs::read(root.join("tms.tif")).unwrap());

    run(root, &["stacta", "xyz", "--datetime", DATETIME]);
    run(
        root,
        &[
            "stacta",
            "tms",
            "--scheme",
            "tms",
            "--output",
            "exported",
            "--datetime",
            DATETIME,
        ],
    );
    assert_eq!(
        fs::read(root.join("exported/2/0/1.png")).unwrap(),
        fs::read(root.join("tms/2/0/2.png")).unwrap()
    );
    assert!(!root.join("tms/stacta.json").exists());
    let index: Value =
        serde_json::from_slice(&fs::read(root.join("xyz/stacta.json")).unwrap()).unwrap();
    assert_eq!(index["id"], "xyz");
    assert_eq!(index["properties"]["datetime"], DATETIME);
    let matrices = index["properties"]["tiles:tile_matrix_sets"]["WebMercatorQuad"]["tileMatrix"]
        .as_array()
        .unwrap();
    assert_eq!(matrices.len(), 2);
    assert_eq!(matrices[0]["identifier"], "0");
    assert_eq!(matrices[1]["identifier"], "2");
    let bounds = &index["properties"]["tiles:tile_matrix_links"]["WebMercatorQuad"]["limits"]["2"];
    assert_eq!(
        bounds,
        &serde_json::json!({"min_tile_col": 0, "max_tile_col": 1, "min_tile_row": 1, "max_tile_row": 2})
    );

    let mut mosaic =
        tiff::decoder::Decoder::new(fs::File::open(root.join("xyz.tif")).unwrap()).unwrap();
    use tiff::{decoder::DecodingResult, tags::Tag};
    assert_eq!(mosaic.dimensions().unwrap(), (32, 32));
    assert_eq!(mosaic.get_tag_u16_vec(Tag::ExtraSamples).unwrap(), [2]);
    assert_eq!(
        mosaic.get_tag_u16_vec(Tag::GeoKeyDirectoryTag).unwrap(),
        [1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, 3857]
    );
    let half = std::f64::consts::PI * 6_378_137.0;
    let scale = mosaic.get_tag_f64_vec(Tag::ModelPixelScaleTag).unwrap();
    let tie = mosaic.get_tag_f64_vec(Tag::ModelTiepointTag).unwrap();
    assert!((scale[0] - half / 32.0).abs() < 1e-7);
    assert!((scale[1] - half / 32.0).abs() < 1e-7);
    assert!((tie[3] + half).abs() < 1e-7);
    assert!((tie[4] - half / 2.0).abs() < 1e-7);
    let DecodingResult::U8(pixels) = mosaic.read_image().unwrap() else {
        panic!("expected RGBA")
    };
    assert_eq!(&pixels[..4], &RED);
    assert_eq!(&pixels[16 * 32 * 4..][..4], &[0; 4]);
    for directory in ["xyz", "exported"] {
        let index = stacta_index(root, directory);
        assert_eq!(tile_asset(&index, 2, 0, 1), "./2/0/1.png");
        assert_eq!(stacta_pixel(root, directory, 2, 0, 1), Some(RED));
        assert_eq!(stacta_pixel(root, directory, 2, 0, 2), None);
        assert_eq!(
            stacta_pixel(root, directory, 0, 0, 0),
            Some([55, 66, 77, 255])
        );
    }
}

fn stacta_index(root: &Path, directory: &str) -> Value {
    serde_json::from_slice(&fs::read(root.join(directory).join("stacta.json")).unwrap()).unwrap()
}

fn tile_asset(index: &Value, zoom: u8, x: u32, y: u32) -> String {
    index["asset_templates"]["tiles"]["href"]
        .as_str()
        .unwrap()
        .replace("{TileMatrix}", &zoom.to_string())
        .replace("{TileCol}", &x.to_string())
        .replace("{TileRow}", &y.to_string())
}

fn stacta_pixel(root: &Path, directory: &str, zoom: u8, x: u32, y: u32) -> Option<[u8; 4]> {
    let path = root
        .join(directory)
        .join(tile_asset(&stacta_index(root, directory), zoom, x, y));
    path.exists()
        .then(|| image::open(path).unwrap().into_rgba8().get_pixel(0, 0).0)
}

#[test]
fn pyramid_resamples_directly_and_unifies_formats() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    let src = root.join("xyz/2/0/1.png");
    let original = fs::read(&src).unwrap();
    tile(root, "xyz/2/2/0.png", [210, 0, 255, 0]);

    run(
        root,
        &[
            "pyramid",
            "xyz",
            "--source-zoom",
            "2",
            "--zooms",
            "0-1,3,5",
            "--output",
            "png",
        ],
    );
    assert_eq!(fs::read(root.join("png/2/0/1.png")).unwrap(), original);
    assert_eq!(pixel(root, "png/1/0/0.png", 0, 8), RED);
    assert_eq!(pixel(root, "png/1/0/0.png", 0, 0), [0; 4]);
    assert_eq!(pixel(root, "png/3/1/2.png", 15, 8), [144, 0, 96, 160]);
    assert_eq!(pixel(root, "png/3/0/2.png", 8, 0), [240, 0, 0, 96]);
    assert!(!root.join("png/3/0/0.png").exists());
    assert!(!root.join("png/3/4/0.png").exists());
    assert!(!root.join("png/4").exists());
    assert_eq!(pixel(root, "png/5/3/11.png", 8, 8), RED);

    run(
        root,
        &[
            "pyramid",
            "tms",
            "--scheme",
            "tms",
            "--source-zoom",
            "2",
            "--zooms",
            "1,3",
            "--format",
            "webp",
            "--output",
            "webp",
        ],
    );
    assert_eq!(pixel(root, "webp/2/0/2.webp", 0, 0), RED);
    assert_eq!(pixel(root, "webp/3/1/5.webp", 15, 8), [144, 0, 96, 160]);
    assert_eq!(pixel(root, "webp/1/0/1.webp", 0, 8), RED);
    let bytes = fs::read(root.join("webp/3/1/5.webp")).unwrap();
    assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::WebP);

    run(
        root,
        &[
            "pyramid",
            "xyz",
            "--source-zoom",
            "2",
            "--zooms",
            "3",
            "--resampling",
            "nearest",
            "--format",
            "jpeg",
            "--background",
            "#000000",
            "--output",
            "jpeg",
        ],
    );
    for path in ["jpeg/2/0/1.jpg", "jpeg/3/1/2.jpg"] {
        let bytes = fs::read(root.join(path)).unwrap();
        assert_eq!(image::guess_format(&bytes).unwrap(), ImageFormat::Jpeg);
        let [r, g, b, a] = pixel(root, path, 15, 8);
        assert!(r.abs_diff(120) <= 3 && g <= 3 && b <= 3 && a == 255);
    }
    assert!(!root.join("jpeg/3/4/0.jpg").exists());
    assert_eq!(fs::read(src).unwrap(), original);

    run(
        root,
        &[
            "pyramid",
            "xyz",
            "--source-zoom",
            "2",
            "--zooms",
            "3",
            "--resampling",
            "average",
            "--output",
            "average",
        ],
    );
    assert_eq!(pixel(root, "average/3/1/2.png", 15, 8), RED);
}

#[test]
fn invalid_inputs_and_failed_publication() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);

    let before = fs::read(root.join("xyz/2/0/1.png")).unwrap();
    fs::write(root.join("xyz/2/1/2.png"), b"broken").unwrap();
    fail(
        root,
        &["mosaic", "xyz", "--zoom", "2", "--output", "bad.tif"],
        "2/1/2.png",
    );
    fail(
        root,
        &[
            "pyramid",
            "xyz",
            "--source-zoom",
            "2",
            "--zooms",
            "3",
            "--output",
            "bad",
        ],
        "2/1/2.png",
    );
    fail(
        root,
        &["stacta", "xyz", "--datetime", DATETIME],
        "2/1/2.png",
    );
    assert_eq!(fs::read_dir(root).unwrap().count(), 2);
    assert!(!root.join("xyz/stacta.json").exists());
    tile(root, "xyz/2/1/2.png", BLUE);

    fs::write(root.join("xyz/2/0/01.png"), &before).unwrap();
    fail(
        root,
        &["mosaic", "xyz", "--zoom", "2", "--output", "bad.tif"],
        "重复坐标",
    );
    fs::remove_file(root.join("xyz/2/0/01.png")).unwrap();
    tile(root, "xyz/2/0/4.png", RED);
    fail(
        root,
        &["mosaic", "xyz", "--zoom", "2", "--output", "bad.tif"],
        "行号越界",
    );
    fs::remove_file(root.join("xyz/2/0/4.png")).unwrap();

    fs::create_dir(root.join("existing")).unwrap();
    fs::write(root.join("existing.tif"), b"preserve me").unwrap();
    fail(
        root,
        &["mosaic", "xyz", "--zoom", "2", "--output", "existing.tif"],
        "目标已存在",
    );
    fail(
        root,
        &[
            "pyramid",
            "xyz",
            "--source-zoom",
            "2",
            "--zooms",
            "3",
            "--output",
            "existing",
        ],
        "目标已存在",
    );
    fail(
        root,
        &[
            "mosaic",
            "xyz",
            "--zoom",
            "2",
            "--output",
            "existing.tif/child",
        ],
        "检查目标",
    );
    assert_eq!(fs::read(root.join("existing.tif")).unwrap(), b"preserve me");
    assert_eq!(fs::read_dir(root.join("existing")).unwrap().count(), 0);

    fs::rename(root.join("xyz/2/0/1.png"), root.join("xyz/2/0/1.PNG")).unwrap();
    fail(
        root,
        &["stacta", "xyz", "--datetime", DATETIME],
        "扩展名或波段布局不一致",
    );
    fs::rename(root.join("xyz/2/0/1.PNG"), root.join("xyz/2/0/1.png")).unwrap();
    image::RgbImage::new(16, 16)
        .save(root.join("xyz/2/0/1.png"))
        .unwrap();
    fail(
        root,
        &["stacta", "xyz", "--datetime", DATETIME],
        "扩展名或波段布局不一致",
    );
    fs::write(root.join("xyz/2/0/1.png"), before).unwrap();
    run(root, &["stacta", "xyz", "--datetime", DATETIME]);
    fail(
        root,
        &["stacta", "xyz", "--datetime", DATETIME],
        "目标已存在",
    );
    fail(
        root,
        &["stacta", "xyz", "--datetime", "yesterday"],
        "invalid value",
    );
}

#[test]
fn stacta_opens_sparse_level_without_a_tile_at_the_origin() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    tile(root, "sparse/2/1/0.png", RED);
    tile(root, "sparse/2/0/1.png", BLUE);
    run(
        root,
        &[
            "stacta",
            "sparse",
            "--datetime",
            "2026-11-18T08:00:00+08:00",
            "--id",
            "sparse-map",
        ],
    );
    let index: Value =
        serde_json::from_slice(&fs::read(root.join("sparse/stacta.json")).unwrap()).unwrap();
    assert_eq!(index["id"], "sparse-map");
    assert_eq!(index["properties"]["datetime"], DATETIME);
    let bbox = index["bbox"].as_array().unwrap();
    assert_eq!(bbox[0], -180.0);
    assert_eq!(bbox[1], 0.0);
    assert_eq!(bbox[2], 0.0);
    assert!((bbox[3].as_f64().unwrap() - 85.0511287798).abs() < 1e-9);

    assert_eq!(stacta_pixel(root, "sparse", 2, 0, 0), None);
    assert_eq!(stacta_pixel(root, "sparse", 2, 1, 0), Some(RED));

    tile(root, "high/30/134217728/0.png", RED);
    fail(
        root,
        &["stacta", "high", "--datetime", DATETIME],
        "超出 STACTA 的 32 位兼容范围",
    );
}

#[test]
fn stacta_reads_webp_and_jpeg_pyramids() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    fixture(root);
    for format in ["jpeg", "webp"] {
        run(
            root,
            &[
                "pyramid",
                "xyz",
                "--source-zoom",
                "2",
                "--zooms",
                "0",
                "--format",
                format,
                "--output",
                format,
            ],
        );
        run(root, &["stacta", format, "--datetime", DATETIME]);
        let index = stacta_index(root, format);
        let asset = tile_asset(&index, 2, 0, 1);
        assert!(asset.ends_with(if format == "jpeg" { ".jpg" } else { ".webp" }));
        let pixel = stacta_pixel(root, format, 2, 0, 1).unwrap();
        if format == "webp" {
            assert_eq!(pixel, RED);
        } else {
            assert!(
                pixel[0].abs_diff(247) <= 3
                    && pixel[1].abs_diff(127) <= 3
                    && pixel[2].abs_diff(127) <= 3
                    && pixel[3] == 255
            );
        }
    }
}
