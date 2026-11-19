use super::{
    check_cancelled,
    geotiff::{Affine, projection},
    tiff_output::TILE,
    tiff_source::Source,
};
use anyhow::{Result, ensure};
use image::{Rgba, RgbaImage};
use proj4rs::{adaptors::transform_vertex_3d, proj::Proj};
use std::sync::atomic::AtomicBool;
use tiles_toolbox::tiles::HALF_WORLD;

type Point = [f64; 2];
type Quad = [Point; 4];
const EARTH_RADIUS: f64 = 6_378_137.;
const MAX_LAT: f64 = 85.051_128_779_806_6;

pub(super) struct Grid {
    transform: Affine,
    inverse: Affine,
    projection: Option<(Proj, Proj)>,
    geographic: bool,
    central_lon: f64,
}

impl Grid {
    pub fn new(source: &Source) -> Result<Self> {
        let [width, height] = source.size.map(f64::from);
        Ok(Self {
            transform: source.transform,
            inverse: source.transform.inverse()?,
            projection: match source.crs.code {
                3857 | 4326 => None,
                code => Some((projection(code)?, projection(4326)?)),
            },
            geographic: source.crs.geographic,
            central_lon: source.transform.model([width / 2., height / 2.])[0],
        })
    }

    fn to_world(&self, point: Point) -> Option<Point> {
        let [mut x, mut y] = self.transform.model(point);
        if !self.geographic && self.projection.is_none() {
            return Some([x, y]);
        }
        if let Some((source, wgs84)) = &self.projection {
            let (x0, y0) = if self.geographic {
                (x.to_radians(), y.clamp(-90., 90.).to_radians())
            } else {
                (x, y)
            };
            let (lon, lat, _) = transform_vertex_3d(source, wgs84, (x0, y0, 0.)).ok()?;
            (x, y) = (lon.to_degrees(), lat.to_degrees());
        }
        if self.geographic {
            // 有些经纬度栅格使用 0–360，保留原经度所在的周期。
            let source_lon = self.transform.model(point)[0];
            x += 360. * ((source_lon - x) / 360.).round();
        }
        let lat = y.clamp(-MAX_LAT, MAX_LAT).to_radians();
        Some([
            EARTH_RADIUS * x.to_radians(),
            EARTH_RADIUS * lat.tan().asinh(),
        ])
    }

    fn source_pixel(&self, [x, y]: Point) -> Option<Point> {
        if !self.geographic && self.projection.is_none() {
            return Some(self.inverse.model([x, y]));
        }
        let (lon, lat) = (x / EARTH_RADIUS, (y / EARTH_RADIUS).sinh().atan());
        let (mut x, mut y) = (lon, lat);
        if let Some((source, wgs84)) = &self.projection {
            (x, y, _) = transform_vertex_3d(wgs84, source, (lon, lat, 0.)).ok()?;
        }
        if self.geographic {
            (x, y) = (x.to_degrees(), y.to_degrees());
            if (self.central_lon - x).abs() > 180. + 1e-10 {
                x += 360. * ((self.central_lon - x) / 360.).round();
            }
        }
        let p = self.inverse.model([x, y]);
        p.iter().all(|value| value.is_finite()).then_some(p)
    }

    pub fn bounds(&self, size: [u32; 2]) -> Result<Vec<[f64; 4]>> {
        let [w, h] = size.map(f64::from);
        let mut result = [
            f64::INFINITY,
            f64::INFINITY,
            f64::NEG_INFINITY,
            f64::NEG_INFINITY,
        ];
        let wrap_projected = !self.geographic && self.projection.is_some();
        let center = self.to_world([w / 2., h / 2.]).filter(|p| p[0].is_finite());
        let world = 2. * HALF_WORLD;
        for edge in 0..4 {
            for step in 0..=64 {
                let t = f64::from(step) / 64.;
                let p = match edge {
                    0 => [t * w, 0.],
                    1 => [w, t * h],
                    2 => [t * w, h],
                    _ => [0., t * h],
                };
                if let Some([mut x, y]) = self.to_world(p)
                    && x.is_finite()
                    && y.is_finite()
                {
                    if wrap_projected && let Some([center, _]) = center {
                        x += world * ((center - x) / world).round();
                    }
                    result[0] = result[0].min(x);
                    result[1] = result[1].min(y);
                    result[2] = result[2].max(x);
                    result[3] = result[3].max(y);
                }
            }
        }
        let [mut west, south, mut east, north] = result;
        let south = south.max(-HALF_WORLD);
        let north = north.min(HALF_WORLD);
        if west.is_finite() && east.is_finite() && (self.geographic || wrap_projected) {
            let offset = world * ((west + HALF_WORLD) / world).floor();
            west -= offset;
            east -= offset;
        }
        let mut ranges = Vec::new();
        if west.is_finite() && east.is_finite() && south < north {
            if (self.geographic || wrap_projected) && east > HALF_WORLD {
                let other_east = (east - world).min(HALF_WORLD);
                if other_east > -HALF_WORLD {
                    ranges.push([-HALF_WORLD, south, other_east, north]);
                }
            }
            west = west.max(-HALF_WORLD);
            east = east.min(HALF_WORLD);
            if west < east {
                ranges.push([west, south, east, north]);
            }
        }
        ensure!(!ranges.is_empty(), "影像不覆盖 WebMercatorQuad 范围");
        Ok(ranges)
    }

    pub fn render(
        &self,
        source: &mut Source,
        rgba: &mut RgbaImage,
        [left, top]: Point,
        pixel: f64,
        cancelled: &AtomicBool,
    ) -> Result<bool> {
        let mut upper = [None; TILE + 1];
        let mut lower = [None; TILE + 1];
        let edge = |out: &mut [Option<Point>], row: usize| {
            let y = top - row as f64 * pixel;
            for (column, point) in out.iter_mut().enumerate() {
                *point = self.source_pixel([left + column as f64 * pixel, y]);
            }
        };
        edge(&mut upper, 0);
        let mut visible = false;
        for y in 0..TILE {
            check_cancelled(cancelled)?;
            edge(&mut lower, y + 1);
            for x in 0..TILE {
                let mapped = [upper[x], upper[x + 1], lower[x + 1], lower[x]];
                let sample = if let [Some(a), Some(b), Some(c), Some(d)] = mapped
                    && [length(a, b), length(b, c), length(c, d), length(d, a)]
                        .into_iter()
                        .any(|n| n > 1.000_001)
                {
                    average(source, [a, b, c, d], cancelled)?
                } else if let Some(center) = self.source_pixel([
                    left + (x as f64 + 0.5) * pixel,
                    top - (y as f64 + 0.5) * pixel,
                ]) {
                    bilinear(source, center)?
                } else {
                    [0.; 4]
                };
                let [r, g, b, alpha] = sample;
                let opacity = alpha.round() as u8;
                let result = if opacity > 0 {
                    visible = true;
                    let [r, g, b] = [r, g, b].map(|value| (value * 255. / alpha).round() as u8);
                    [r, g, b, opacity]
                } else {
                    [0; 4]
                };
                rgba.put_pixel(x as u32, y as u32, Rgba(result));
            }
            std::mem::swap(&mut upper, &mut lower);
        }
        Ok(visible)
    }
}

fn length(a: Point, b: Point) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

fn bilinear(source: &mut Source, [x, y]: Point) -> Result<[f64; 4]> {
    let [width, height] = source.size.map(f64::from);
    if x < 0. || y < 0. || x >= width || y >= height {
        return Ok([0.; 4]);
    }
    let [x, y] = [
        (x - 0.5).clamp(0., width - 1.),
        (y - 0.5).clamp(0., height - 1.),
    ];
    let [x0, y0] = [x.floor() as u32, y.floor() as u32];
    let [x1, y1] = [
        (x0 + 1).min(source.size[0] - 1),
        (y0 + 1).min(source.size[1] - 1),
    ];
    let [dx, dy] = [x.fract(), y.fract()];
    let mut result = [0.; 4];
    for (py, wy) in [(y0, 1. - dy), (y1, dy)] {
        for (px, wx) in [(x0, 1. - dx), (x1, dx)] {
            let weight = wx * wy;
            if weight > 0. {
                accumulate(&mut result, source.pixel(px, py)?, weight);
            }
        }
    }
    Ok(result)
}

fn accumulate(result: &mut [f64; 4], sample: [f64; 4], weight: f64) {
    for (total, value) in result.iter_mut().zip(sample) {
        *total += value * weight;
    }
}

fn average(source: &mut Source, quad: Quad, cancelled: &AtomicBool) -> Result<[f64; 4]> {
    let area = polygon_area(&quad);
    if !area.is_finite() || area <= f64::EPSILON {
        return Ok([0.; 4]);
    }
    let min: Point =
        std::array::from_fn(|i| quad.iter().map(|p| p[i]).fold(f64::INFINITY, f64::min));
    let max: Point =
        std::array::from_fn(|i| quad.iter().map(|p| p[i]).fold(f64::NEG_INFINITY, f64::max));
    let rectangular = quad[0][1] == quad[1][1]
        && quad[1][0] == quad[2][0]
        && quad[2][1] == quad[3][1]
        && quad[3][0] == quad[0][0];
    let [start_x, start_y] = min.map(|v| v.floor() as u32);
    let [end_x, end_y] = std::array::from_fn(|i| (max[i].ceil() as u32).min(source.size[i]));
    if start_x >= end_x || start_y >= end_y {
        return Ok([0.; 4]);
    }
    let mut result = [0.; 4];
    for y in start_y..end_y {
        check_cancelled(cancelled)?;
        for x in start_x..end_x {
            let [x0, y0] = [f64::from(x), f64::from(y)];
            let overlap = if rectangular {
                (max[0].min(x0 + 1.) - min[0].max(x0)) * (max[1].min(y0 + 1.) - min[1].max(y0))
            } else {
                clip_area(quad, x0, y0)
            };
            if overlap > 0. {
                accumulate(&mut result, source.pixel(x, y)?, overlap);
            }
        }
    }
    Ok(result.map(|v| v / area))
}

fn polygon_area(points: &[Point]) -> f64 {
    if points.len() < 3 {
        return 0.;
    }
    let [ox, oy] = points[0];
    let area: f64 = points[1..]
        .windows(2)
        .map(|pair| {
            let [a, b] = [pair[0], pair[1]];
            (a[0] - ox) * (b[1] - oy) - (a[1] - oy) * (b[0] - ox)
        })
        .sum();
    area.abs() / 2.
}

// 将反投影所得的像元四边形裁到源像元方格，边缘按交叠面积加权。
fn clip_area(quad: Quad, x: f64, y: f64) -> f64 {
    let mut clipped = [[0.; 2]; 16];
    let mut next = [[0.; 2]; 16];
    clipped[..4].copy_from_slice(&quad);
    let mut len = 4;
    for (axis, boundary, sign) in [(0, x, 1.), (0, x + 1., -1.), (1, y, 1.), (1, y + 1., -1.)] {
        if len == 0 {
            break;
        }
        let mut previous = clipped[len - 1];
        let mut inside = (previous[axis] - boundary) * sign >= 0.;
        let mut count = 0;
        for &current in &clipped[..len] {
            let current_inside = (current[axis] - boundary) * sign >= 0.;
            if inside != current_inside {
                let t = (boundary - previous[axis]) / (current[axis] - previous[axis]);
                next[count][axis] = boundary;
                next[count][1 - axis] =
                    previous[1 - axis] + t * (current[1 - axis] - previous[1 - axis]);
                count += 1;
            }
            if current_inside {
                next[count] = current;
                count += 1;
            }
            inside = current_inside;
            previous = current;
        }
        std::mem::swap(&mut clipped, &mut next);
        len = count;
    }
    polygon_area(&clipped[..len])
}
