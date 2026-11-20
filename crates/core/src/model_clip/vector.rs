use crate::message;
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
    shapefile_layers(&open_zip(path)?)
}

fn shapefile_layers(archive: &zip::ZipArchive<File>) -> Result<Vec<String>, String> {
    let mut layers = matching_zip_names(archive, |name| extension(Path::new(name)) == "shp")?;
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
            let shp = File::open(path).map_err(|err| {
                message!(operation_error, message!(open_input, path.display()), err)
            })?;
            let projection = read_sidecar(path)?;
            Ok((shapes(shp)?, projection_or_manual(projection, manual_crs)?))
        }
        "zip" => {
            let mut archive = open_zip(path)?;
            let layers = shapefile_layers(&archive)?;
            let layer = match (layer.filter(|s| !s.is_empty()), layers.as_slice()) {
                (None, [only]) => only,
                (Some(layer), _) if layers.iter().any(|name| name == layer) => layer,
                (_, []) => return Err(message!(no_shapefile_in_the_zip).into()),
                (None, _) => return Err(message!(select_a_shapefile_layer_in_the_zip).into()),
                (Some(_), _) => {
                    return Err(
                        message!(the_selected_shapefile_layer_was_not_found_in_the_zip).into(),
                    );
                }
            };
            let prj_name = Path::new(layer)
                .with_extension("prj")
                .to_string_lossy()
                .replace('\\', "/");
            let prj_entries = matching_zip_names(&archive, |name| {
                name.replace('\\', "/").eq_ignore_ascii_case(&prj_name)
            })?;
            let projection = match prj_entries.as_slice() {
                [] => None,
                [name] => Some(
                    String::from_utf8(zip_member(&mut archive, name)?)
                        .map_err(|_| message!(prj_is_not_utf_8_text))?,
                ),
                _ => return Err(message!(zip_contains_more_than_one_prj_with_the_same_name).into()),
            };
            let bytes = zip_member(&mut archive, layer)?;
            Ok((
                shapes(Cursor::new(bytes))?,
                projection_or_manual(projection, manual_crs)?,
            ))
        }
        _ => Err(message!(boundary_file_must_be_shp_zip_kml_or_geojson).into()),
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
        .ok_or_else(|| message!(shapefile_lacks_a_prj_file_enter_the_boundary_crs).into())
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
                .map_err(|_| message!(prj_is_not_utf_8_text).into());
        }
    }
    Ok(None)
}

fn open_zip(path: &Path) -> Result<zip::ZipArchive<File>, String> {
    let stream = File::open(path)
        .map_err(|err| message!(operation_error, message!(open_input, path.display()), err))?;
    zip::ZipArchive::new(stream).map_err(|err| message!(could_not_read_zip_err, err = err))
}

fn matching_zip_names(
    archive: &zip::ZipArchive<File>,
    matches: impl Fn(&str) -> bool,
) -> Result<Vec<String>, String> {
    let mut names = Vec::new();
    for name in archive.file_names() {
        let name = name.map_err(|err| message!(could_not_read_zip_entry_name_err, err = err))?;
        if matches(&name) {
            names.push(name.into_owned());
        }
    }
    Ok(names)
}

fn zip_member(zip: &mut zip::ZipArchive<File>, name: &str) -> Result<Vec<u8>, String> {
    let member = zip
        .by_name(name)
        .map_err(|err| message!(zip_layer_name_err, name = name, err = err))?;
    if !member.is_file() || member.size() > MAX_MEMBER {
        return Err(message!(unsupported_zip_entry_or_size_name, name = name));
    }
    read_limited(member, name)
}

fn read_file(path: &Path) -> Result<Vec<u8>, String> {
    read_limited(
        File::open(path)
            .map_err(|err| message!(operation_error, message!(open_input, path.display()), err))?,
        &path.display().to_string(),
    )
}

fn read_limited(reader: impl Read, label: &str) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_MEMBER + 1)
        .read_to_end(&mut bytes)
        .map_err(|err| message!(operation_error, message!(read_input, label), err))?;
    if bytes.len() as u64 > MAX_MEMBER {
        return Err(message!(label_exceeds_256_mib, label = label));
    }
    Ok(bytes)
}

fn shapes<R: Read + Seek>(input: R) -> Result<Vec<Polygon>, String> {
    let mut reader =
        ShapeReader::new(input).map_err(|err| message!(invalid_shapefile_err, err = err))?;
    let mut polygons = Vec::new();
    for shape in reader.iter_shapes() {
        let shape = shape.map_err(|err| message!(could_not_read_shapefile_err, err = err))?;
        match shape {
            Shape::Polygon(p) => polygons.extend(shape_rings(p.rings())?),
            Shape::PolygonM(p) => polygons.extend(shape_rings(p.rings())?),
            Shape::PolygonZ(p) => polygons.extend(shape_rings(p.rings())?),
            Shape::NullShape => {}
            _ => return Err(message!(shapefile_must_be_a_polygon_layer).into()),
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
        let first = hole
            .0
            .first()
            .ok_or(message!(shapefile_contains_an_empty_inner_ring))?;
        let idx = shells
            .iter()
            .enumerate()
            .filter(|(_, shell)| Polygon::new(shell.exterior().clone(), vec![]).contains(first))
            .min_by(|(_, a), (_, b)| a.unsigned_area().total_cmp(&b.unsigned_area()))
            .map(|(i, _)| i)
            .ok_or(message!(shapefile_inner_ring_has_no_matching_outer_ring))?;
        shells[idx].interiors_push(hole);
    }
    Ok(shells)
}

fn geojson(bytes: &[u8]) -> Result<Vec<Polygon>, String> {
    let value: Value =
        serde_json::from_slice(bytes).map_err(|err| message!(invalid_geojson_err, err = err))?;
    let mut polygons = Vec::new();
    collect_geojson(&value, &mut polygons)?;
    Ok(polygons)
}

fn collect_geojson(value: &Value, polygons: &mut Vec<Polygon>) -> Result<(), String> {
    if value.get("crs").is_some_and(|crs| !crs.is_null()) {
        return Err(message!(
            geojson_must_use_wgs84_longitude_and_latitude_without_a_legacy_crs_declaration
        )
        .into());
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
        _ => return Err(message!(invalid_geojson_object_type).into()),
    }
    Ok(())
}

fn list(value: &Value) -> Result<&[Value], String> {
    value
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| message!(missing_array_in_geojson).into())
}

fn json_polygon(value: &Value) -> Result<Polygon, String> {
    let mut rings = list(value)?
        .iter()
        .map(|ring| {
            list(ring)?
                .iter()
                .map(|point| {
                    let x = point[0]
                        .as_f64()
                        .ok_or(message!(invalid_geojson_longitude))?;
                    let y = point[1]
                        .as_f64()
                        .ok_or(message!(invalid_geojson_latitude))?;
                    Ok(Coord { x, y })
                })
                .collect::<Result<Vec<_>, String>>()
                .map(LineString::new)
        })
        .collect::<Result<Vec<_>, String>>()?
        .into_iter();
    let outer = rings
        .next()
        .ok_or(message!(geojson_polygon_is_missing_an_outer_ring))?;
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
            .map_err(|err| message!(invalid_kml_err, err = err))?
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
                let name = stack.pop().ok_or(message!(mismatched_kml_tags))?;
                if name == b"coordinates" && stack.len() >= 3 {
                    let suffix = &stack[stack.len() - 3..];
                    if suffix[0] == b"Polygon" && suffix[2] == b"LinearRing" {
                        let ring = parse_kml_ring(&text)?;
                        match suffix[1].as_slice() {
                            b"outerBoundaryIs" => {
                                if outer.replace(ring).is_some() {
                                    return Err(
                                        message!(kml_polygon_has_multiple_outer_rings).into()
                                    );
                                }
                            }
                            b"innerBoundaryIs" => holes.push(ring),
                            _ => {}
                        }
                    }
                }
                if name == b"Polygon" {
                    polygons.push(Polygon::new(
                        outer
                            .take()
                            .ok_or(message!(kml_polygon_is_missing_an_outer_ring))?,
                        std::mem::take(&mut holes),
                    ));
                }
            }
            Event::DocType(_) => return Err(message!(dtd_is_unsupported_in_kml).into()),
            Event::Eof => break,
            _ => {}
        }
    }
    if !stack.is_empty() {
        return Err(message!(unclosed_element_in_kml_xml).into());
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
                .ok_or(message!(invalid_kml_longitude))?;
            let y = parts
                .next()
                .and_then(|v| v.parse().ok())
                .ok_or(message!(invalid_kml_latitude))?;
            Ok(Coord { x, y })
        })
        .collect::<Result<Vec<_>, String>>()
        .map(LineString::new)
}
