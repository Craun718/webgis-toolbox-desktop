use super::geotiff::{Affine, Crs, GeoKeys, geometry_with};
use anyhow::{Context, Result, ensure};
use proj4rs::proj::Proj;
use tiff::{decoder::IfdDecoder, tags::Tag};
use webgis_core::message;

const USER_DEFINED: u16 = 32767;

pub(super) enum TileCrs {
    Epsg(Crs),
    Geographic { projection: Proj, reason: String },
}

pub(super) fn geometry(tags: &mut IfdDecoder<'_>) -> Result<(TileCrs, Affine)> {
    geometry_with(tags, |tags, keys| {
        let reason = match keys.crs_code().0 {
            Some(USER_DEFINED) => message!(tile_crs_user_defined).to_owned(),
            Some(code) if code != 0 => {
                if crs_definitions::from_code(code).is_some() {
                    return keys.epsg().map(TileCrs::Epsg);
                }
                message!(no_built_in_definition_for_epsg_code, code = code)
            }
            _ => message!(crs_geokey_has_no_recognizable_epsg_identifier).to_owned(),
        };
        let projection = geographic_projection(tags, keys)
            .with_context(|| message!(tile_crs_fallback_failed, reason = reason))?;
        Ok(TileCrs::Geographic { projection, reason })
    })
}

fn geographic_projection(tags: &mut IfdDecoder<'_>, keys: &GeoKeys<'_>) -> Result<Proj> {
    let mut entries = std::collections::BTreeMap::new();
    for entry in keys.entries() {
        let id = entry[0];
        ensure!(
            entries.insert(id, entry).is_none(),
            message!(tile_crs_invalid_geokey, id = id)
        );
    }
    let invalid = |id| message!(tile_crs_invalid_geokey, id = id);
    let short = |id| -> Result<Option<u16>> {
        match entries.get(&id).copied() {
            None => Ok(None),
            Some([_, 0, 1, value]) => Ok(Some(*value)),
            _ => anyhow::bail!(invalid(id)),
        }
    };
    ensure!(
        short(1024)? == Some(2)
            && !entries
                .keys()
                .any(|id| (3072..=3096).contains(id) || (4096..=4099).contains(id)),
        message!(tile_crs_requires_geographic_2d)
    );
    short(2048)?;
    ensure!(
        matches!(short(2050)?, None | Some(USER_DEFINED)) && !entries.contains_key(&2062),
        message!(tile_crs_unsupported_datum)
    );
    ensure!(
        short(2054)? == Some(9102),
        message!(tile_crs_requires_degrees)
    );
    let doubles = tags
        .find_tag(Tag::GeoDoubleParamsTag)?
        .map(|value| value.into_f64_vec())
        .transpose()?
        .unwrap_or_default();
    let double = |id| -> Result<Option<f64>> {
        match entries.get(&id).copied() {
            None => Ok(None),
            Some([_, 34736, 1, index]) => Ok(Some(
                *doubles
                    .get(usize::from(*index))
                    .filter(|value| value.is_finite())
                    .with_context(|| invalid(id))?,
            )),
            _ => anyhow::bail!(invalid(id)),
        }
    };
    ensure!(
        double(2055)?.is_none_or(|value| close(value, std::f64::consts::PI / 180.)),
        invalid(2055)
    );
    let longitude = double(2061)?;
    let greenwich = match short(2051)? {
        Some(8901) => longitude.is_none_or(|value| value == 0.),
        None | Some(USER_DEFINED) => longitude == Some(0.),
        _ => false,
    };
    ensure!(greenwich, message!(tile_crs_requires_greenwich));
    let linear_units = short(2052)?;
    let metres_per_unit = double(2053)?;
    ensure!(
        match linear_units {
            None | Some(9001) => metres_per_unit.is_none_or(|value| value == 1.),
            Some(USER_DEFINED) => metres_per_unit == Some(1.),
            _ => false,
        },
        message!(tile_crs_requires_meter_axes)
    );
    let axis = double(2057)?
        .filter(|&value| value > 0.)
        .with_context(|| invalid(2057))?;
    let inverse_flattening = double(2059)?
        .filter(|&value| value > 1.)
        .with_context(|| invalid(2059))?;
    ensure!(
        double(2058)?.is_none_or(|minor| close(minor, axis * (1. - 1. / inverse_flattening))),
        invalid(2058)
    );
    // An authority ellipsoid is an additional constraint on the explicit parameters.
    let expected_flattening = match short(2056)? {
        None | Some(USER_DEFINED) => None,
        Some(1024 | 7019) => Some(298.257_222_101), // CGCS2000 / GRS 1980
        Some(7030) => Some(298.257_223_563),        // WGS 84
        _ => anyhow::bail!(invalid(2056)),
    };
    ensure!(
        expected_flattening
            .is_none_or(|expected| close(axis, 6_378_137.) && close(inverse_flattening, expected)),
        invalid(2056)
    );
    Proj::from_proj_string(&format!(
        "+proj=longlat +a={axis} +rf={inverse_flattening} +pm=0"
    ))
    .map_err(|error| anyhow::anyhow!("{error}"))
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() <= b.abs() * 1e-12
}
