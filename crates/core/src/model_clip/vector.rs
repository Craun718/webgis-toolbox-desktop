use geo::{Area, Contains, Coord, LineString, Polygon};
use quick_xml::{Reader, events::Event};
use serde_json::Value;
use shapefile::{PolygonRing, Shape, ShapeReader};
use std::{
    fs::{self, File},
    io::{Cursor, Read, Seek},
    path::Path,
};

const MAX_MEMBER: u64 = 256 * 1024 * 1024;

pub fn zip_layers(path: &Path) -> Result<Vec<String>, String> {
    let archive = open_zip(path)?;
    let mut layers: Vec<_> = archive
        .file_names()
        .filter(|name| extension(Path::new(name)) == "shp")
        .map(str::to_owned)
        .collect();
    layers.sort_unstable();
    Ok(layers)
}

pub(super) fn read(
    path: &Path,
    layer: Option<&str>,
    manual_crs: Option<&str>,
) -> Result<(Vec<Polygon>, String), String> {
    match extension(path).as_str() {
        "geojson" | "json" => Ok((geojson(&read_file(path)?)?, "EPSG:4326".into())),
        "kml" => Ok((kml(&read_file(path)?)?, "EPSG:4326".into())),
        "shp" => {
            let shp = File::open(path).map_err(|err| err.to_string())?;
            let projection = read_sidecar(path)?;
            Ok((shapes(shp)?, projection_or_manual(projection, manual_crs)?))
        }
        "zip" => {
            let mut archive = open_zip(path)?;
            let mut layers: Vec<_> = archive
                .file_names()
                .filter(|name| extension(Path::new(name)) == "shp")
                .map(str::to_owned)
                .collect();
            layers.sort_unstable();
            let layer = match (layer.filter(|s| !s.is_empty()), layers.as_slice()) {
                (None, [only]) => only,
                (Some(layer), _) if layers.iter().any(|name| name == layer) => layer,
                (_, []) => return Err("ZIP 中没有 Shapefile".into()),
                (None, _) => return Err("请选择 ZIP 中的 Shapefile 图层".into()),
                (Some(_), _) => return Err("ZIP 中找不到所选 Shapefile 图层".into()),
            };
            let prj_name = Path::new(layer)
                .with_extension("prj")
                .to_string_lossy()
                .replace('\\', "/");
            let prj_entries: Vec<_> = archive
                .file_names()
                .filter(|name| name.replace('\\', "/").eq_ignore_ascii_case(&prj_name))
                .map(str::to_owned)
                .collect();
            let projection = match prj_entries.as_slice() {
                [] => None,
                [name] => Some(
                    String::from_utf8(zip_member(&mut archive, name)?)
                        .map_err(|_| "PRJ 不是 UTF-8 文本")?,
                ),
                _ => return Err("ZIP 中有多个同名 PRJ".into()),
            };
            let bytes = zip_member(&mut archive, layer)?;
            Ok((
                shapes(Cursor::new(bytes))?,
                projection_or_manual(projection, manual_crs)?,
            ))
        }
        _ => Err("范围文件需要为 SHP、ZIP、KML 或 GeoJSON".into()),
    }
}

pub(super) fn extension(path: &Path) -> String {
    path.extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
}

fn projection_or_manual(
    projection: Option<String>,
    manual: Option<&str>,
) -> Result<String, String> {
    projection
        .filter(|s| !s.trim().is_empty())
        .or_else(|| manual.filter(|s| !s.trim().is_empty()).map(str::to_owned))
        .ok_or_else(|| "Shapefile 缺少 .prj，请填写范围坐标系".into())
}

fn read_sidecar(path: &Path) -> Result<Option<String>, String> {
    let desired = path.with_extension("prj");
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    for entry in fs::read_dir(parent).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        if entry
            .file_name()
            .eq_ignore_ascii_case(desired.file_name().unwrap_or_default())
        {
            return String::from_utf8(read_file(&entry.path())?)
                .map(Some)
                .map_err(|_| "PRJ 不是 UTF-8 文本".into());
        }
    }
    Ok(None)
}

fn open_zip(path: &Path) -> Result<zip::ZipArchive<File>, String> {
    let stream = File::open(path).map_err(|err| err.to_string())?;
    zip::ZipArchive::new(stream).map_err(|err| format!("无法读取 ZIP：{err}"))
}

fn zip_member(zip: &mut zip::ZipArchive<File>, name: &str) -> Result<Vec<u8>, String> {
    let member = zip
        .by_name(name)
        .map_err(|err| format!("ZIP 图层 {name}：{err}"))?;
    if !member.is_file() || member.size() > MAX_MEMBER {
        return Err(format!("不支持的 ZIP 条目或大小：{name}"));
    }
    read_limited(member, name)
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    read_limited(
        File::open(path).map_err(|err| err.to_string())?,
        &path.display().to_string(),
    )
}

fn read_limited(reader: impl Read, label: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_MEMBER + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("{label}：{err}"))?;
    if bytes.len() as u64 > MAX_MEMBER {
        return Err(format!("{label} 超过 256 MiB"));
    }
    Ok(bytes)
}

fn shapes<R: Read + Seek>(input: R) -> Result<Vec<Polygon>, String> {
    let mut reader = ShapeReader::new(input).map_err(|err| format!("无效 Shapefile：{err}"))?;
    let mut polygons = Vec::new();
    for shape in reader.iter_shapes() {
        let shape = shape.map_err(|err| format!("读取 Shapefile 失败：{err}"))?;
        match shape {
            Shape::Polygon(p) => polygons.extend(shape_rings(p.rings())?),
            Shape::PolygonM(p) => polygons.extend(shape_rings(p.rings())?),
            Shape::PolygonZ(p) => polygons.extend(shape_rings(p.rings())?),
            Shape::NullShape => {}
            _ => return Err("Shapefile 需要为面图层".into()),
        }
    }
    Ok(polygons)
}

fn shape_rings<P>(rings: &[PolygonRing<P>]) -> Result<Vec<Polygon>, String>
where
    P: Copy,
    Coord: From<P>,
{
    let mut shells = Vec::new();
    let mut holes = Vec::new();
    for ring in rings {
        let line = LineString::new(ring.points().iter().copied().map(Coord::from).collect());
        match ring {
            PolygonRing::Outer(_) => shells.push(Polygon::new(line, Vec::new())),
            PolygonRing::Inner(_) => holes.push(line),
        }
    }
    for hole in holes {
        let first = hole.0.first().ok_or("Shapefile 含空内环")?;
        let idx = shells
            .iter()
            .enumerate()
            .filter(|(_, shell)| Polygon::new(shell.exterior().clone(), vec![]).contains(first))
            .min_by(|(_, a), (_, b)| a.unsigned_area().total_cmp(&b.unsigned_area()))
            .map(|(i, _)| i)
            .ok_or("Shapefile 的内环没有匹配外环")?;
        shells[idx].interiors_push(hole);
    }
    Ok(shells)
}

fn geojson(bytes: &[u8]) -> Result<Vec<Polygon>, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| format!("无效 GeoJSON：{err}"))?;
    let mut polygons = Vec::new();
    collect_geojson(&value, &mut polygons)?;
    Ok(polygons)
}

fn collect_geojson(value: &Value, polygons: &mut Vec<Polygon>) -> Result<(), String> {
    if value.get("crs").is_some_and(|crs| !crs.is_null()) {
        return Err("GeoJSON 必须使用 WGS84 经纬度，且不能包含旧版 CRS 声明".into());
    }
    match value["type"].as_str() {
        Some("Feature") if value["geometry"].is_null() => {}
        Some("Feature") => collect_geojson(&value["geometry"], polygons)?,
        Some("FeatureCollection") | Some("GeometryCollection") => {
            let key = if value["type"] == "FeatureCollection" {
                "features"
            } else {
                "geometries"
            };
            for child in list(&value[key])? {
                collect_geojson(child, polygons)?;
            }
        }
        Some("Polygon") => polygons.push(json_polygon(&value["coordinates"])?),
        Some("MultiPolygon") => {
            for child in list(&value["coordinates"])? {
                polygons.push(json_polygon(child)?);
            }
        }
        Some("Point" | "MultiPoint" | "LineString" | "MultiLineString") => {}
        _ => return Err("无效的 GeoJSON 对象类型".into()),
    }
    Ok(())
}

fn list(value: &Value) -> Result<&[Value], String> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| "GeoJSON 中缺少数组".into())
}

fn json_polygon(value: &Value) -> Result<Polygon, String> {
    let mut rings = list(value)?
        .iter()
        .map(|ring| {
            list(ring)?
                .iter()
                .map(|point| {
                    let x = point[0].as_f64().ok_or("无效 GeoJSON 经度")?;
                    let y = point[1].as_f64().ok_or("无效 GeoJSON 纬度")?;
                    Ok(Coord { x, y })
                })
                .collect::<Result<Vec<_>, String>>()
                .map(LineString::new)
        })
        .collect::<Result<Vec<_>, String>>()?
        .into_iter();
    let outer = rings.next().ok_or("GeoJSON 多边形缺少外环")?;
    Ok(Polygon::new(outer, rings.collect()))
}

fn kml(bytes: &[u8]) -> Result<Vec<Polygon>, String> {
    let mut reader = Reader::from_reader(bytes);
    let mut stack = Vec::<Vec<u8>>::new();
    let mut polygons = Vec::new();
    let mut outer = None;
    let mut holes = Vec::new();
    let mut text = String::new();
    loop {
        match reader
            .read_event()
            .map_err(|err| format!("无效 KML：{err}"))?
        {
            Event::Start(tag) => {
                let name = tag.local_name().as_ref().to_vec();
                if name == b"coordinates" {
                    text.clear();
                }
                stack.push(name);
            }
            Event::Text(value) if stack.last().is_some_and(|name| name == b"coordinates") => {
                text.push_str(&value.decode().map_err(|err| err.to_string())?);
            }
            Event::CData(value) if stack.last().is_some_and(|name| name == b"coordinates") => {
                text.push_str(&value.decode().map_err(|err| err.to_string())?);
            }
            Event::End(_) => {
                let name = stack.pop().ok_or("KML 标签不匹配")?;
                if name == b"coordinates" && stack.len() >= 3 {
                    let suffix = &stack[stack.len() - 3..];
                    if suffix[0] == b"Polygon" && suffix[2] == b"LinearRing" {
                        let ring = parse_kml_ring(&text)?;
                        match suffix[1].as_slice() {
                            b"outerBoundaryIs" => {
                                if outer.replace(ring).is_some() {
                                    return Err("KML 多边形有多个外环".into());
                                }
                            }
                            b"innerBoundaryIs" => holes.push(ring),
                            _ => {}
                        }
                    }
                }
                if name == b"Polygon" {
                    polygons.push(Polygon::new(
                        outer.take().ok_or("KML 多边形缺少外环")?,
                        std::mem::take(&mut holes),
                    ));
                }
            }
            Event::DocType(_) => return Err("KML 不支持 DTD".into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err("KML XML 未闭合".into());
    }
    Ok(polygons)
}

fn parse_kml_ring(text: &str) -> Result<LineString, String> {
    text.split_whitespace()
        .map(|tuple| {
            let mut parts = tuple.split(',');
            let x = parts
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or("无效 KML 经度")?;
            let y = parts
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or("无效 KML 纬度")?;
            Ok(Coord { x, y })
        })
        .collect::<Result<Vec<_>, String>>()
        .map(LineString::new)
}
