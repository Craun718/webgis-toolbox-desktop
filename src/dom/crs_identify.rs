//! Conservative identification, not a general CRS registry lookup.

pub(super) const CGCS2000: u16 = 4490;
pub(super) const CHINA_2000_DATUM: u16 = 1043;
pub(super) const CGCS2000_ELLIPSOID: u16 = 1024;

pub(super) fn cgcs2000_crs_name(name: &str) -> bool {
    one_of(
        name,
        &[
            "GCS_CGCS_2000",
            "CGCS2000",
            "China Geodetic Coordinate System 2000",
        ],
    )
}

pub(super) fn china_2000_datum_name(name: &str) -> bool {
    one_of(name, &["D_CGCS_2000", "China 2000", "CGCS2000"])
}

pub(super) fn cgcs2000_wkt(wkt: &str) -> Option<u16> {
    let (kind, args) = node(wkt)?;
    if !one_of(
        kind,
        &["GEOGCRS", "GEODCRS", "GEOGRAPHICCRS", "GEODETICCRS"],
    ) || !cgcs2000_crs_name(quoted(args.first()?)?)
    {
        return None;
    }
    // Reject structures that could describe a different CRS (e.g. a datum
    // ensemble or a dynamic datum). The caller already handles explicit IDs.
    let fields = components(
        &args[1..],
        &[
            "DATUM",
            "GEODETICDATUM",
            "TRF",
            "PRIMEM",
            "CS",
            "AXIS",
            "ANGLEUNIT",
            "USAGE",
            "REMARK",
        ],
    )?;
    let datum = unique(&fields, &["DATUM", "GEODETICDATUM", "TRF"])?;
    if !china_2000_datum_name(quoted(datum.first()?)?) {
        return None;
    }
    let datum_fields = components(&datum[1..], &["ELLIPSOID", "ID"])?;
    let ellipsoid = unique(&datum_fields, &["ELLIPSOID"])?;
    if !quoted(ellipsoid.first()?)?.eq_ignore_ascii_case("CGCS2000")
        || !number(ellipsoid.get(1)?, 6_378_137.)
        || !number(ellipsoid.get(2)?, 298.257_222_101)
    {
        return None;
    }
    let ellipse_fields = components(&ellipsoid[3..], &["LENGTHUNIT", "ID"])?;
    if !valid_ids(&datum_fields, CHINA_2000_DATUM)
        || !valid_ids(&ellipse_fields, CGCS2000_ELLIPSOID)
        || !units(&ellipse_fields, "LENGTHUNIT", 1., false)
    {
        return None;
    }
    let prime = unique(&fields, &["PRIMEM"])?;
    if !quoted(prime.first()?)?.eq_ignore_ascii_case("Greenwich") || !number(prime.get(1)?, 0.) {
        return None;
    }
    let prime_fields = components(&prime[2..], &["ANGLEUNIT", "ID"])?;
    if !units(&prime_fields, "ANGLEUNIT", DEGREES, false) || !valid_ids(&prime_fields, 8901) {
        return None;
    }
    let cs = unique(&fields, &["CS"])?;
    if cs.len() != 2 || !cs[0].eq_ignore_ascii_case("ellipsoidal") || cs[1] != "2" {
        return None;
    }
    let axes: Vec<_> = fields
        .iter()
        .filter(|(kind, _)| kind.eq_ignore_ascii_case("AXIS"))
        .collect();
    let [(_, lat), (_, lon)] = axes.as_slice() else {
        return None;
    };
    let shared_unit = fields
        .iter()
        .any(|(kind, _)| kind.eq_ignore_ascii_case("ANGLEUNIT"));
    if !units(&fields, "ANGLEUNIT", DEGREES, false)
        || !axis(lat, "north", "1", shared_unit)
        || !axis(lon, "east", "2", shared_unit)
    {
        return None;
    }
    Some(CGCS2000)
}

const DEGREES: f64 = std::f64::consts::PI / 180.;

fn axis(args: &[&str], direction: &str, order: &str, shared_unit: bool) -> bool {
    let [name, actual, rest @ ..] = args else {
        return false;
    };
    if quoted(name).is_none() || !actual.eq_ignore_ascii_case(direction) {
        return false;
    }
    let Some(fields) = components(rest, &["ORDER", "ANGLEUNIT"]) else {
        return false;
    };
    let mut orders = fields
        .iter()
        .filter(|(kind, _)| kind.eq_ignore_ascii_case("ORDER"));
    orders.next().is_none_or(|(_, args)| args == &[order])
        && orders.next().is_none()
        && units(&fields, "ANGLEUNIT", DEGREES, !shared_unit)
}

fn units(fields: &[Field<'_>], name: &str, factor: f64, required: bool) -> bool {
    let mut matches = fields
        .iter()
        .filter(|(kind, _)| kind.eq_ignore_ascii_case(name));
    matches.next().map_or(!required, |(_, args)| {
        let [name, value, rest @ ..] = args.as_slice() else {
            return false;
        };
        let accepted_ids: &[u16] = if factor == 1. { &[9001] } else { &[9102, 9122] };
        quoted(name).is_some()
            && number(value, factor)
            && components(rest, &["ID"])
                .is_some_and(|ids| accepted_ids.iter().any(|&code| valid_ids(&ids, code)))
    }) && matches.next().is_none()
}

type Field<'a> = (&'a str, Vec<&'a str>);

fn components<'a>(args: &[&'a str], allowed: &[&str]) -> Option<Vec<Field<'a>>> {
    args.iter()
        .map(|arg| {
            let field = node(arg)?;
            one_of(field.0, allowed).then_some(field)
        })
        .collect()
}

fn unique<'a, 'b>(fields: &'b [Field<'a>], names: &[&str]) -> Option<&'b [&'a str]> {
    let mut matches = fields.iter().filter(|(kind, _)| one_of(kind, names));
    let (_, args) = matches.next()?;
    matches.next().is_none().then_some(args)
}

fn valid_ids(fields: &[Field<'_>], code: u16) -> bool {
    fields
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("ID"))
        .all(|(_, args)| {
            let [authority, id] = args.as_slice() else {
                return false;
            };
            quoted(authority).is_some_and(|name| name.eq_ignore_ascii_case("EPSG"))
                && quoted(id).unwrap_or(id).parse::<u16>().ok() == Some(code)
        })
}

fn quoted(value: &str) -> Option<&str> {
    value.strip_prefix('"')?.strip_suffix('"')
}

fn one_of(value: &str, aliases: &[&str]) -> bool {
    aliases
        .iter()
        .any(|alias| value.eq_ignore_ascii_case(alias))
}

fn number(value: &str, expected: f64) -> bool {
    value
        .parse::<f64>()
        .is_ok_and(|n| (n - expected).abs() <= expected.abs() * 1e-12)
}

// Read only the direct arguments of one WKT node. Quoted commas/brackets and
// nested nodes cannot be mistaken for siblings. Support either WKT delimiter.
fn node(value: &str) -> Option<Field<'_>> {
    let start = value.find(['[', '('])?;
    let (name, rest) = value.split_at(start);
    let name = name.trim();
    if name.is_empty() || !name.bytes().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    let close = |open| if open == b'[' { b']' } else { b')' };
    let mut brackets = vec![close(rest.as_bytes()[0])];
    let mut args = Vec::new();
    let mut in_string = false;
    let mut offset = 1;
    for (index, byte) in rest.bytes().enumerate().skip(1) {
        match byte {
            b'"' => in_string = !in_string,
            _ if in_string => {}
            b'[' | b'(' if brackets.len() < 32 => brackets.push(close(byte)),
            b'[' | b'(' => return None,
            b']' | b')' => {
                if brackets.pop()? != byte {
                    return None;
                }
                if brackets.is_empty() {
                    let arg = rest[offset..index].trim();
                    if arg.is_empty() || !rest[index + 1..].trim().is_empty() {
                        return None;
                    }
                    args.push(arg);
                    return Some((name, args));
                }
            }
            b',' if brackets.len() == 1 => {
                let arg = rest[offset..index].trim();
                if arg.is_empty() {
                    return None;
                }
                args.push(arg);
                offset = index + 1;
            }
            _ => {}
        }
    }
    None
}
