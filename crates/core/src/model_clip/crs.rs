use crate::message;
use geo::{Coord, LineString, Polygon};
use proj4rs::{adaptors::transform_vertex_3d, proj::Proj};
use quick_xml::{Reader, events::Event};
use serde::{Deserialize, Serialize};

#[derive(Clone, Deserialize, Serialize)]
pub struct Reference {
    pub srs: String,
    pub origin: [f64; 3],
}

impl Reference {
    pub(super) fn parse(metadata: &[u8]) -> Result<Self, String> {
        let mut reader = Reader::from_reader(metadata);
        let mut depth = 0;
        let mut srs = None;
        let mut origin = None;
        loop {
            match reader.read_event().map_err(|err| err.to_string())? {
                Event::Start(tag) if depth == 1 && tag.local_name().as_ref() == b"SRS" => {
                    if srs.is_some() {
                        return Err(message!(metadata_xml_contains_multiple_srs_elements).into());
                    }
                    srs = Some(xml_value(&mut reader, tag.name())?);
                }
                Event::Start(tag) if depth == 1 && tag.local_name().as_ref() == b"SRSOrigin" => {
                    if origin.is_some() {
                        return Err(
                            message!(metadata_xml_contains_multiple_srsorigin_elements).into()
                        );
                    }
                    origin = Some(coordinates(&xml_value(&mut reader, tag.name())?)?);
                }
                Event::Start(tag) => {
                    if depth == 0 && tag.local_name().as_ref() != b"ModelMetadata" {
                        return Err(message!(
                            metadata_xml_is_missing_its_modelmetadata_root_element
                        )
                        .into());
                    }
                    depth += 1;
                }
                Event::End(_) => depth -= 1,
                Event::DocType(_) => {
                    return Err(message!(dtd_is_unsupported_in_metadata_xml).into());
                }
                Event::Eof => break,
                _ => {}
            }
        }
        if depth != 0 {
            return Err(message!(unclosed_element_in_metadata_xml).into());
        }
        Ok(Self {
            srs: srs.ok_or(message!(metadata_xml_is_missing_srs))?,
            origin: origin.ok_or(message!(metadata_xml_is_missing_srsorigin))?,
        })
    }

    pub(super) fn xml(&self) -> String {
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<ModelMetadata version=\"1\">\n  <SRS>{}</SRS>\n  <SRSOrigin>{},{},{}</SRSOrigin>\n</ModelMetadata>\n",
            quick_xml::escape::escape(self.srs.trim()),
            self.origin[0],
            self.origin[1],
            self.origin[2]
        )
    }

    pub(super) fn local_polygons(
        &self,
        polygons: Vec<Polygon>,
        source: &str,
    ) -> Result<Vec<Polygon>, String> {
        if self.origin.iter().any(|v| !v.is_finite()) {
            return Err(message!(model_xyz_origin_coordinates_must_be_finite).into());
        }
        let source = projection(source)?;
        if source.is_geocent() || !source.is_normalized_axis() {
            return Err(message!(
                boundary_crs_must_use_longitude_latitude_or_projected_easting_northing_coordinates
            )
            .into());
        }
        let srs = self.srs.trim();
        let anchor = srs.strip_prefix("ENU:").map(coordinates::<2>).transpose()?;
        let target = if anchor.is_some() {
            projection("EPSG:4326")?
        } else {
            projection(srs)?
        };
        if target.is_geocent() || !target.is_normalized_axis() {
            return Err(message!(
                model_crs_must_use_longitude_latitude_projected_easting_northing_or_enu_coordinates
            )
            .into());
        }

        let enu = anchor.map(Enu::new).transpose()?;
        let ring = |line: LineString| -> Result<LineString, String> {
            line.0
                .into_iter()
                .map(|point| {
                    if !point.x.is_finite() || !point.y.is_finite() {
                        return Err(message!(boundary_contains_non_finite_coordinates).into());
                    }
                    let (x, y) = if source.is_latlong() {
                        check_lon_lat(point.x, point.y)?;
                        (point.x.to_radians(), point.y.to_radians())
                    } else {
                        (point.x, point.y)
                    };
                    let (x, y, _) =
                        transform_vertex_3d(&source, &target, (x, y, 0.)).map_err(|err| {
                            message!(
                                boundary_coordinate_transformation_failed_at_err,
                                point.x,
                                point.y,
                                err = err
                            )
                        })?;
                    let (x, y) = if let Some(enu) = &enu {
                        check_lon_lat(x.to_degrees(), y.to_degrees())?;
                        enu.horizontal(x, y)
                    } else if target.is_latlong() {
                        (x.to_degrees(), y.to_degrees())
                    } else {
                        (x, y)
                    };
                    let xy = Coord {
                        x: x - self.origin[0],
                        y: y - self.origin[1],
                    };
                    if !xy.x.is_finite() || !xy.y.is_finite() {
                        return Err(message!(transformed_coordinates_are_out_of_range).into());
                    }
                    Ok(xy)
                })
                .collect::<Result<Vec<_>, String>>()
                .map(LineString::new)
        };
        polygons
            .into_iter()
            .map(|polygon| {
                let (shell, holes) = polygon.into_inner();
                Ok(Polygon::new(
                    ring(shell)?,
                    holes.into_iter().map(&ring).collect::<Result<_, _>>()?,
                ))
            })
            .collect()
    }
}

fn xml_value(
    reader: &mut Reader<&[u8]>,
    name: quick_xml::name::QName<'_>,
) -> Result<String, String> {
    let mut result = String::new();
    loop {
        match reader.read_event().map_err(|err| err.to_string())? {
            Event::Text(text) => result.push_str(&text.decode().map_err(|err| err.to_string())?),
            Event::CData(text) => result.push_str(&text.decode().map_err(|err| err.to_string())?),
            Event::GeneralRef(entity) => {
                let escaped = format!("&{};", entity.decode().map_err(|err| err.to_string())?);
                result.push_str(
                    &quick_xml::escape::unescape(&escaped).map_err(|err| err.to_string())?,
                );
            }
            Event::End(tag) if tag.name() == name => return Ok(result.trim().to_owned()),
            _ => return Err(message!(srs_in_metadata_xml_must_be_text).into()),
        }
    }
}

fn coordinates<const N: usize>(value: &str) -> Result<[f64; N], String> {
    let values: Vec<f64> = value
        .split(',')
        .map(|part| part.trim().parse::<f64>())
        .collect::<Result<_, _>>()
        .map_err(|_| message!(invalid_coordinate_value, value = value))?;
    if values.iter().any(|v| !v.is_finite()) {
        return Err(message!(invalid_coordinate_value, value = value));
    }
    values
        .try_into()
        .map_err(|_| message!(expected_n_coordinate_values_value, N = N, value = value))
}

fn projection(value: &str) -> Result<Proj, String> {
    let value = value.trim().trim_start_matches('\u{feff}').trim_start();
    let definition = if value.contains('[') {
        proj4wkt::wkt_to_projstring(value)
            .map_err(|err| message!(could_not_parse_the_wkt_crs_err, err = err))?
    } else {
        value.to_owned()
    };
    if definition.contains("+nadgrids") || definition.contains("+geoidgrids") {
        return Err(message!(unsupported_crs_grid_file).into());
    }
    Proj::from_user_string(&definition)
        .map_err(|err| message!(could_not_use_crs_value_err, value = value, err = err))
}

fn check_lon_lat(lon: f64, lat: f64) -> Result<(), String> {
    if !(-180.0..=180.0).contains(&lon) || !(-90.0..=90.0).contains(&lat) {
        return Err(message!(
            invalid_longitude_and_latitude_lon_lat,
            lon = lon,
            lat = lat
        ));
    }
    Ok(())
}

struct Enu {
    origin: [f64; 3],
    east: [f64; 3],
    north: [f64; 3],
}

impl Enu {
    fn new([lat, lon]: [f64; 2]) -> Result<Self, String> {
        check_lon_lat(lon, lat)?;
        let (lon, lat) = (lon.to_radians(), lat.to_radians());
        let (sin_lon, cos_lon) = lon.sin_cos();
        let (sin_lat, cos_lat) = lat.sin_cos();
        Ok(Self {
            origin: ecef(lon, lat),
            east: [-sin_lon, cos_lon, 0.],
            north: [-sin_lat * cos_lon, -sin_lat * sin_lon, cos_lat],
        })
    }

    fn horizontal(&self, lon: f64, lat: f64) -> (f64, f64) {
        let xyz = ecef(lon, lat);
        let delta: [f64; 3] = std::array::from_fn(|i| xyz[i] - self.origin[i]);
        let dot = |axis: [f64; 3]| axis.into_iter().zip(delta).map(|(a, b)| a * b).sum();
        (dot(self.east), dot(self.north))
    }
}

fn ecef(lon: f64, lat: f64) -> [f64; 3] {
    const A: f64 = 6_378_137.;
    const E2: f64 = 0.006_694_379_990_141_316_5;
    let (sin_lat, cos_lat) = lat.sin_cos();
    let n = A / (1. - E2 * sin_lat * sin_lat).sqrt();
    [
        n * cos_lat * lon.cos(),
        n * cos_lat * lon.sin(),
        n * (1. - E2) * sin_lat,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enu_uses_latitude_then_longitude_and_applies_model_origin() {
        let reference = Reference {
            srs: "ENU:30,110".into(),
            origin: [10., 20., 99.],
        };
        let region = geo::Polygon::new(
            geo::LineString::from(vec![
                (110., 30.),
                (110.001, 30.),
                (110., 30.001),
                (110., 30.),
            ]),
            vec![],
        );
        let transformed = reference.local_polygons(vec![region], "EPSG:4326").unwrap();
        let exterior = &transformed[0].exterior().0;
        assert!((exterior[0].x + 10.).abs() < 1e-7);
        assert!((exterior[0].y + 20.).abs() < 1e-7);
        assert!((exterior[1].x - 86.486).abs() < 0.01);
        assert!((exterior[2].y - 90.852).abs() < 0.01);
    }
}
