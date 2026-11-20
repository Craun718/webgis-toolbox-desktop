use anyhow::{Context, Result, ensure};
use proj4rs::proj::Proj;
use regex::Regex;
use std::sync::LazyLock;
use tiff::{decoder::IfdDecoder, encoder::DirectoryEncoder, tags::Tag};
use webgis_core::message;

#[derive(Clone, Copy, Debug)]
pub(super) struct Crs {
    pub code: u16,
    pub geographic: bool,
}

impl Crs {
    pub fn parse(value: &str) -> Result<Self> {
        let value = value.trim().trim_start_matches('\u{feff}').trim();
        let code = match value.split_once(':') {
            Some((prefix, code)) if prefix.eq_ignore_ascii_case("EPSG") => code.trim().parse().ok(),
            _ => wkt_epsg(value),
        };
        Self::from_code(code.with_context(|| {
            message!(
                crs_has_no_recognizable_top_level_epsg_identifier_value,
                value = value
            )
        })?)
    }

    pub fn from_code(code: u16) -> Result<Self> {
        let proj = projection(code)?;
        Ok(Self {
            code,
            geographic: proj.is_latlong(),
        })
    }
}

pub(super) fn projection(code: u16) -> Result<Proj> {
    let definition = crs_definitions::from_code(code)
        .with_context(|| message!(no_built_in_definition_for_epsg_code, code = code))?;
    let grid = definition.proj4.split_ascii_whitespace().find(|part| {
        (part.starts_with("+nadgrids=") && *part != "+nadgrids=@null")
            || part.starts_with("+geoidgrids=")
            || *part == "+datum=NAD27"
    });
    ensure!(
        grid.is_none(),
        message!(
            epsg_code_requires_an_external_transformation_grid,
            code = code
        )
    );
    let text = definition.proj4.replace("+nadgrids=@null", "");
    let proj = Proj::from_proj_string(&text).map_err(|err| {
        anyhow::anyhow!(message!(
            could_not_use_epsg_code_err,
            code = code,
            err = err
        ))
    })?;
    ensure!(
        proj.has_forward() && proj.has_inverse() && !proj.is_geocent() && proj.is_normalized_axis(),
        message!(
            epsg_code_does_not_use_longitude_latitude_or_easting_northing_axes,
            code = code
        )
    );
    Ok(proj)
}

fn wkt_epsg(wkt: &str) -> Option<u16> {
    static ROOT: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(r"(?i)^(?:GEOGCS|PROJCS|GEOGCRS|GEODCRS|PROJCRS|GEOGRAPHICCRS|GEODETICCRS|PROJECTEDCRS)\s*[\[(]").unwrap()
    });
    static TOKEN: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r#""(?:[^"]|"")*"|[\[\]()]"#).unwrap());
    static ID: LazyLock<Regex> = LazyLock::new(|| {
        Regex::new(
            r#"(?i)\b(?:AUTHORITY|ID)\s*[\[(]\s*"EPSG"\s*,\s*(?:"([0-9]+)"|([0-9]+))\s*[]),]"#,
        )
        .unwrap()
    });
    ROOT.find(wkt)?;
    let mut depth = 0;
    let mut code = None;
    for token in TOKEN.find_iter(wkt) {
        match token.as_str() {
            "[" | "(" => depth += 1,
            "]" | ")" => {
                depth -= 1;
                if depth == 0 {
                    return wkt[token.end()..]
                        .trim()
                        .is_empty()
                        .then_some(code)
                        .flatten();
                }
            }
            _ => {}
        }
        if depth == 1
            && let Some(id) =
                ID.captures(wkt[token.end()..].trim_start_matches([',', ' ', '\t', '\r', '\n']))
            && id.get(0)?.start() == 0
        {
            code = id.get(1).or_else(|| id.get(2))?.as_str().parse().ok();
        }
    }
    None
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Affine(pub [f64; 6]);

impl Affine {
    pub fn model(self, [x, y]: [f64; 2]) -> [f64; 2] {
        let [a, b, c, d, e, f] = self.0;
        [a + b * x + c * y, d + e * x + f * y]
    }

    pub fn inverse(self) -> Result<Self> {
        let [a, b, c, d, e, f] = self.0;
        let det = b * f - c * e;
        ensure!(
            det.is_finite() && det != 0.0,
            message!(georeferencing_affine_matrix_is_not_invertible)
        );
        let inverse = Self([
            (c * d - f * a) / det,
            f / det,
            -c / det,
            (e * a - b * d) / det,
            -e / det,
            b / det,
        ]);
        ensure!(
            inverse.0.iter().all(|v| v.is_finite()),
            message!(georeferencing_affine_matrix_contains_invalid_values)
        );
        Ok(inverse)
    }
}

pub(super) fn geometry(tags: &mut IfdDecoder<'_>) -> Result<(Crs, Affine)> {
    let keys = tags
        .find_tag(Tag::GeoKeyDirectoryTag)?
        .context(message!(missing_crs_geokey))?
        .into_u16_vec()?;
    ensure!(
        keys.len() >= 4 && keys[0] == 1,
        message!(invalid_geokey_directory)
    );
    let end = 4 + usize::from(keys[3]) * 4;
    ensure!(end <= keys.len(), message!(geokey_directory_is_incomplete));
    let key = |id| {
        keys[4..end].chunks_exact(4).find_map(|entry| {
            (entry[0] == id && entry[1] == 0 && entry[2] == 1).then_some(entry[3])
        })
    };
    let (code, geographic) = match key(1024) {
        Some(1) => (key(3072), false),
        Some(2) => (key(2048), true),
        None => key(3072).map_or((key(2048), true), |code| (Some(code), false)),
        _ => (None, false),
    };
    let crs = Crs::from_code(
        code.filter(|&code| code != 32767 && code != 0)
            .context(message!(crs_geokey_has_no_recognizable_epsg_identifier))?,
    )?;
    ensure!(
        crs.geographic == geographic,
        message!(epsg_does_not_match_the_geokey_coordinate_type, crs.code)
    );
    let mut transform = if let Some(value) = tags.find_tag(Tag::ModelTransformationTag)? {
        let matrix = value.into_f64_vec()?;
        ensure!(
            matrix.len() == 16
                && matrix.iter().all(|v| v.is_finite())
                && matrix[12..] == [0., 0., 0., 1.],
            message!(invalid_modeltransformation_affine_matrix)
        );
        Affine([
            matrix[3], matrix[0], matrix[1], matrix[7], matrix[4], matrix[5],
        ])
    } else {
        let scale = tags
            .find_tag(Tag::ModelPixelScaleTag)?
            .context(message!(
                missing_georeferencing_modelpixelscale_or_modeltransformation
            ))?
            .into_f64_vec()?;
        let tie = tags
            .find_tag(Tag::ModelTiepointTag)?
            .context(message!(missing_georeferencing_modeltiepoint))?
            .into_f64_vec()?;
        ensure!(
            scale.len() == 3 && tie.len() >= 6 && tie.len() % 6 == 0,
            message!(invalid_pixel_scale_or_tie_points)
        );
        Affine([
            tie[3] - tie[0] * scale[0],
            scale[0],
            0.,
            tie[4] + tie[1] * scale[1],
            0.,
            -scale[1],
        ])
    };
    match key(1025) {
        None | Some(1) => {}
        Some(2) => {
            let [x, y] = transform.model([-0.5, -0.5]);
            transform.0[0] = x;
            transform.0[3] = y;
        }
        _ => anyhow::bail!(message!(unsupported_geotiff_raster_pixel_type)),
    }
    transform.inverse()?;
    Ok((crs, transform))
}

pub(super) fn write_geotags<W: std::io::Write + std::io::Seek, K: tiff::encoder::TiffKind>(
    dir: &mut DirectoryEncoder<'_, W, K>,
    crs: Option<Crs>,
    Affine([x, dx, rx, y, ry, dy]): Affine,
    point: bool,
) -> Result<()> {
    if rx == 0. && ry == 0. && dx > 0. && dy < 0. {
        dir.write_tag(Tag::ModelPixelScaleTag, &[dx, -dy, 0.][..])?;
        dir.write_tag(Tag::ModelTiepointTag, &[0., 0., 0., x, y, 0.][..])?;
    } else {
        dir.write_tag(
            Tag::ModelTransformationTag,
            &[dx, rx, 0., x, ry, dy, 0., y, 0., 0., 1., 0., 0., 0., 0., 1.][..],
        )?;
    }
    if let Some(crs) = crs {
        let (model, key) = if crs.geographic { (2, 2048) } else { (1, 3072) };
        dir.write_tag(
            Tag::GeoKeyDirectoryTag,
            &[
                1_u16,
                1,
                0,
                3,
                1024,
                0,
                1,
                model,
                1025,
                0,
                1,
                1 + u16::from(point),
                key,
                0,
                1,
                crs.code,
            ][..],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_only_top_level_wkt_epsg() {
        assert_eq!(
            Crs::parse(
                r#"PROJCS["test",GEOGCS["base",AUTHORITY["EPSG","4490"]],AUTHORITY["EPSG","4547"]]"#
            )
            .unwrap()
            .code,
            4547
        );
        assert_eq!(
            Crs::parse(r#"PROJCRS["test",BASEGEOGCRS["base",ID["EPSG",4490]],ID["EPSG",4547]]"#)
                .unwrap()
                .code,
            4547
        );
        assert!(Crs::parse(r#"PROJCRS["test",BASEGEOGCRS["base",ID["EPSG",4490]]]"#).is_err());
        assert!(
            Crs::parse("EPSG:4267")
                .unwrap_err()
                .to_string()
                .contains("格网")
        );
    }
}
