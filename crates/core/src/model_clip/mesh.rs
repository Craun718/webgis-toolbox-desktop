use geo::{Coord, MultiPolygon, Orient, Polygon, TriangulateDelaunay, Validation, unary_union};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};

type Index = GeomWithData<Rectangle<[f64; 2]>, usize>;
pub(super) type Weights = [f64; 3];

pub struct Boundary {
    triangles: Vec<[Coord; 3]>,
    index: RTree<Index>,
}

impl Boundary {
    pub fn new(polygons: &[Polygon]) -> Result<Self, String> {
        if polygons.is_empty() {
            return Err("裁剪范围没有多边形".into());
        }
        for polygon in polygons {
            if std::iter::once(polygon.exterior())
                .chain(polygon.interiors())
                .flat_map(|ring| &ring.0)
                .any(|p| !p.x.is_finite() || !p.y.is_finite())
            {
                return Err("裁剪范围包含非有限坐标".into());
            }
            polygon
                .check_validation()
                .map_err(|err| format!("无效多边形：{err}"))?;
        }
        let oriented: Vec<_> = polygons
            .iter()
            .map(|polygon| polygon.orient(geo::orient::Direction::Default))
            .collect();
        let merged: MultiPolygon = unary_union(&oriented);
        let mut triangles = Vec::new();
        for polygon in &merged.0 {
            let parts = polygon
                .constrained_triangulation(geo::triangulate_delaunay::DelaunayTriangulationConfig {
                    snap_radius: 1e-10,
                })
                .map_err(|err| format!("范围三角剖分失败：{err}"))?;
            for part in parts {
                let mut corners = part.to_array();
                if side(corners[0], corners[1], corners[2]) < 0.0 {
                    corners.swap(1, 2);
                }
                triangles.push(corners);
            }
        }
        if triangles.is_empty() {
            return Err("裁剪范围面积为零".into());
        }
        let index = RTree::bulk_load(
            triangles
                .iter()
                .enumerate()
                .map(|(i, triangle)| GeomWithData::new(Rectangle::from_aabb(bounds(triangle)), i))
                .collect(),
        );
        Ok(Self { triangles, index })
    }

    pub(super) fn clip(&self, xy: [Coord; 3]) -> Vec<[Weights; 3]> {
        let bbox = bounds(&xy);
        let mut result = Vec::new();
        for candidate in self.index.locate_in_envelope_intersecting(&bbox) {
            let triangle = self.triangles[candidate.data];
            let mut ring = vec![[1., 0., 0.], [0., 1., 0.], [0., 0., 1.]];
            for i in 0..3 {
                let a = triangle[i];
                let b = triangle[(i + 1) % 3];
                let values = xy.map(|point| side(a, b, point));
                // For a projected wall on a shared edge, only one of the two
                // incident triangles owns the edge. Exterior edges own themselves.
                if values == [0.; 3]
                    && self
                        .index
                        .locate_in_envelope_intersecting(&bounds(&[a, b]))
                        .any(|other| {
                            other.data < candidate.data
                                && self.triangles[other.data].contains(&a)
                                && self.triangles[other.data].contains(&b)
                        })
                {
                    ring.clear();
                    break;
                }
                ring = half_plane(&ring, values);
                if ring.len() < 3 {
                    break;
                }
            }
            for pair in ring.get(1..).unwrap_or_default().windows(2) {
                let [a, b, c] = [ring[0], pair[0], pair[1]];
                let area = (b[1] - a[1]) * (c[2] - a[2]) - (c[1] - a[1]) * (b[2] - a[2]);
                if area > 0. {
                    result.push([a, b, c]);
                }
            }
            if xy[0] == xy[1] && xy[0] == xy[2] && !result.is_empty() {
                break;
            }
        }
        result
    }
}

fn bounds<const N: usize>(points: &[Coord; N]) -> AABB<[f64; 2]> {
    AABB::from_points(&points.map(|p| [p.x, p.y]))
}

fn side(a: Coord, b: Coord, point: Coord) -> f64 {
    (b.x - a.x) * (point.y - a.y) - (b.y - a.y) * (point.x - a.x)
}

fn half_plane(ring: &[Weights], values: Weights) -> Vec<Weights> {
    let mut clipped = Vec::with_capacity(ring.len() + 1);
    for (&a, &b) in ring.iter().zip(ring.iter().cycle().skip(1)) {
        let da = interpolate(values, a);
        let db = interpolate(values, b);
        if da >= 0. {
            clipped.push(a);
        }
        if (da < 0. && db > 0.) || (da > 0. && db < 0.) {
            let t = da / (da - db);
            clipped.push(std::array::from_fn(|i| a[i] + t * (b[i] - a[i])));
        }
    }
    clipped
}

pub(super) fn interpolate(values: Weights, weights: Weights) -> f64 {
    values.into_iter().zip(weights).map(|(v, w)| v * w).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{line_string, polygon};

    fn area(pieces: Vec<[Weights; 3]>) -> f64 {
        pieces
            .into_iter()
            .map(|[a, b, c]| (b[1] - a[1]) * (c[2] - a[2]) - (c[1] - a[1]) * (b[2] - a[2]))
            .sum()
    }

    #[test]
    fn vertical_triangle_on_triangulation_edge_is_kept_once() {
        let boundary = Boundary::new(&[polygon![
            (x: 0., y: 0.), (x: 4., y: 0.), (x: 4., y: 4.), (x: 0., y: 4.),
        ]])
        .unwrap();
        let first = boundary.triangles[0];
        let second = boundary.triangles[1];
        let shared: Vec<_> = first.into_iter().filter(|c| second.contains(c)).collect();
        assert_eq!(shared.len(), 2);
        assert_eq!(area(boundary.clip([shared[0], shared[1], shared[0]])), 1.);
    }

    #[test]
    fn union_respects_concavity_holes_and_disjoint_polygons() {
        let concave_with_hole = Polygon::new(
            line_string![(x:0., y:0.), (x:8., y:0.), (x:8., y:4.),
                (x:4., y:4.), (x:4., y:8.), (x:0., y:8.), (x:0., y:0.)],
            vec![
                line_string![(x:1., y:1.), (x:3., y:1.), (x:3., y:3.), (x:1., y:3.), (x:1., y:1.)],
            ],
        );
        let overlap = polygon![(x:3., y:2.), (x:3., y:4.), (x:5., y:4.), (x:5., y:2.)];
        let disjoint = polygon![(x:10., y:0.), (x:11., y:0.), (x:11., y:1.), (x:10., y:1.)];
        let boundary = Boundary::new(&[concave_with_hole, overlap, disjoint]).unwrap();
        let result = boundary.clip([
            Coord { x: -1., y: -1. },
            Coord { x: 25., y: -1. },
            Coord { x: -1., y: 25. },
        ]);
        assert!((area(result) * 338. - 45.).abs() < 1e-6);
    }
}
