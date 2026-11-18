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

    if std::env::var_os("TILES_TEST_GDAL").is_some() {
        let output = gdal(root, "gdalinfo", &["-json", "xyz.tif"]);
        let info: Value = serde_json::from_str(&output).unwrap();
        assert!(
            info["coordinateSystem"]["wkt"]
                .as_str()
                .unwrap()
                .contains("3857")
        );
        assert_eq!(info["size"], serde_json::json!([32, 32]));
        assert_eq!(info["bands"][3]["colorInterpretation"], "Alpha");
        let gt = info["geoTransform"].as_array().unwrap();
        let half = std::f64::consts::PI * 6_378_137.0;
        for (value, expected) in
            gt.iter()
                .zip([-half, half / 32.0, 0.0, half / 2.0, 0.0, -half / 32.0])
        {
            assert!((value.as_f64().unwrap() - expected).abs() < 1e-7);
        }
        for path in [
            "xyz.tif",
            "tms.tif",
            "xyz/stacta.json",
            "exported/stacta.json",
        ] {
            let args = [
                "-oo",
                "SKIP_MISSING_METATILE=YES",
                "-valonly",
                path,
                "0",
                "0",
            ];
            let args = if path.ends_with(".tif") {
                &args[2..]
            } else {
                &args[..]
            };
            assert_eq!(gdal(root, "gdallocationinfo", args), "240\n0\n0\n128");
            let mut missing = args.to_vec();
            *missing.last_mut().unwrap() = "16";
            assert_eq!(gdal(root, "gdallocationinfo", &missing), "0\n0\n0\n0");
            if path.ends_with(".json") {
                assert_eq!(
                    gdal(
                        root,
                        "gdallocationinfo",
                        &[
                            "-oo",
                            "SKIP_MISSING_METATILE=YES",
                            "-valonly",
                            "-overview",
                            "1",
                            path,
                            "0",
                            "0"
                        ]
                    ),
                    "55\n66\n77\n255"
                );
            }
        }
    }
}

fn gdal(root: &Path, program: &str, args: &[&str]) -> String {
    let out = Command::new(program)
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap().trim().to_owned()
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

    if std::env::var_os("TILES_TEST_GDAL").is_some() {
        let path = "sparse/stacta.json";
        let read = |x| {
            gdal(
                root,
                "gdallocationinfo",
                &["-oo", "SKIP_MISSING_METATILE=YES", "-valonly", path, x, "0"],
            )
        };
        assert_eq!(read("0"), "0\n0\n0\n0");
        assert_eq!(read("16"), "240\n0\n0\n128");
    }

    tile(root, "high/30/134217728/0.png", RED);
    fail(
        root,
        &["stacta", "high", "--datetime", DATETIME],
        "超出 GDAL STACTA 的 32 位限制",
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
        if std::env::var_os("TILES_TEST_GDAL").is_some() {
            let path = format!("{format}/stacta.json");
            let pixel = gdal(
                root,
                "gdallocationinfo",
                &[
                    "-oo",
                    "SKIP_MISSING_METATILE=YES",
                    "-valonly",
                    &path,
                    "0",
                    "0",
                ],
            );
            if format == "webp" {
                assert_eq!(pixel, "240\n0\n0\n128");
            } else {
                let rgb: Vec<u8> = pixel.lines().map(|line| line.parse().unwrap()).collect();
                assert_eq!(rgb.len(), 3);
                assert!(
                    rgb[0].abs_diff(247) <= 3
                        && rgb[1].abs_diff(127) <= 3
                        && rgb[2].abs_diff(127) <= 3
                );
            }
        }
    }
}
