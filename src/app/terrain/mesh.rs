//! Regular-grid quantized-mesh 1.0 encoder (CesiumGS/quantized-mesh).
use anyhow::Result;
use ctb_rs::{
    grid::{TileCoord, TileGrid},
    raster::{Crs, RasterSource, transform_coordinate},
    sampling::{ResamplingMethod, sample_at},
};

pub(super) fn encode(
    source: &dyn RasterSource,
    grid: &dyn TileGrid,
    tile: TileCoord,
) -> Result<Vec<u8>> {
    let bounds = grid.tile_bounds(tile)?;
    let (west, south) =
        transform_coordinate(bounds.min_x, bounds.min_y, &grid.crs(), &Crs::Epsg4326)?;
    let (east, north) =
        transform_coordinate(bounds.max_x, bounds.max_y, &grid.crs(), &Crs::Epsg4326)?;
    let mut heights = Vec::with_capacity(4225);
    let mut positions = Vec::with_capacity(4225);
    for row in 0..65 {
        for col in 0..65 {
            let x = west + (east - west) * col as f64 / 64.0;
            let y = south + (north - south) * row as f64 / 64.0;
            let (sx, sy) = transform_coordinate(x, y, &Crs::Epsg4326, &source.metadata().crs)?;
            let h = sample_at(source, sx, sy, ResamplingMethod::Bilinear)?;
            let h = if !h.is_finite() || source.metadata().no_data == Some(h) {
                0.0
            } else {
                h
            };
            heights.push(h);
            let (lon, lat) = (x, y);
            let (lon, lat) = (lon.to_radians(), lat.to_radians());
            let n = 6378137.0 / (1.0 - 0.0066943799901413165 * lat.sin().powi(2)).sqrt();
            positions.push([
                (n + h) * lat.cos() * lon.cos(),
                (n + h) * lat.cos() * lon.sin(),
                (n * (1.0 - 0.0066943799901413165) + h) * lat.sin(),
            ]);
        }
    }
    encode_grid(&heights, &positions)
}

fn encode_grid(heights: &[f64], positions: &[[f64; 3]]) -> Result<Vec<u8>> {
    let min = heights.iter().copied().fold(f64::INFINITY, f64::min) as f32;
    let max = heights.iter().copied().fold(f64::NEG_INFINITY, f64::max) as f32;
    let mut center = [0.0; 3];
    for axis in 0..3 {
        center[axis] = positions.iter().map(|p| p[axis]).sum::<f64>() / positions.len() as f64;
    }
    let radius = positions
        .iter()
        .map(|p| {
            (0..3)
                .map(|a| (p[a] - center[a]).powi(2))
                .sum::<f64>()
                .sqrt()
        })
        .fold(0.0, f64::max);
    let mut out = Vec::new();
    for v in center {
        out.extend(v.to_le_bytes());
    }
    out.extend(min.to_le_bytes());
    out.extend(max.to_le_bytes());
    for v in center {
        out.extend(v.to_le_bytes());
    }
    out.extend(radius.to_le_bytes());
    // A zero occlusion point conservatively disables horizon culling, including root tiles.
    for _ in 0..3 {
        out.extend(0_f64.to_le_bytes());
    }
    // Assign vertices in first-use order, as required by high-water-mark encoding.
    let mut order = Vec::new();
    let mut remap = vec![u16::MAX; 4225];
    let mut indices = Vec::new();
    for row in 0..64 {
        for col in 0..64 {
            let sw = row * 65 + col;
            for old in [sw, sw + 1, sw + 65, sw + 1, sw + 66, sw + 65] {
                if remap[old] == u16::MAX {
                    remap[old] = order.len() as u16;
                    order.push(old);
                }
                indices.push(remap[old]);
            }
        }
    }
    out.extend((order.len() as u32).to_le_bytes());
    for axis in 0..3 {
        let mut previous = 0_i32;
        for &old in &order {
            let value = match axis {
                0 => ((old % 65) as f64 * 32767.0 / 64.0).round() as i32,
                1 => ((old / 65) as f64 * 32767.0 / 64.0).round() as i32,
                _ if max == min => 0,
                _ => (((heights[old] - min as f64) / (max as f64 - min as f64) * 32767.0).round()
                    as i32)
                    .clamp(0, 32767),
            };
            let delta = value - previous;
            out.extend((((delta << 1) ^ (delta >> 31)) as u16).to_le_bytes());
            previous = value;
        }
    }
    out.extend((indices.len() as u32 / 3).to_le_bytes());
    let mut highest = 0;
    for index in indices {
        let code = highest - index;
        out.extend(code.to_le_bytes());
        if code == 0 {
            highest += 1;
        }
    }
    for edge in 0..4 {
        out.extend(65_u32.to_le_bytes());
        for i in 0..65 {
            let old = match edge {
                0 => i * 65,
                1 => i,
                2 => i * 65 + 64,
                _ => 64 * 65 + i,
            };
            out.extend(remap[old].to_le_bytes());
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mesh_indices_and_edges_are_valid() -> Result<()> {
        let bytes = encode_grid(&vec![12.0; 4225], &vec![[1.0, 2.0, 3.0]; 4225])?;
        let u32_at = |p| u32::from_le_bytes(bytes[p..p + 4].try_into().unwrap());
        let u16_at = |p| u16::from_le_bytes(bytes[p..p + 2].try_into().unwrap());
        assert_eq!(u32_at(88), 4225);
        let start = 92 + 4225 * 6;
        assert_eq!(u32_at(start), 8192);
        let mut highest = 0_u16;
        let mut decoded = Vec::new();
        for p in (start + 4..start + 4 + 8192 * 6).step_by(2) {
            let code = u16_at(p);
            assert!(code <= highest);
            decoded.push(highest - code);
            if code == 0 {
                highest += 1;
            }
        }
        assert_eq!(highest, 4225);
        // Independently decode vertex deltas, then check winding and edge coordinates.
        let mut coordinates = vec![[0_i32; 3]; 4225];
        for axis in 0..3 {
            let mut previous = 0;
            for (i, point) in coordinates.iter_mut().enumerate() {
                let code = u16_at(92 + (axis * 4225 + i) * 2) as i32;
                previous += (code >> 1) ^ -(code & 1);
                point[axis] = previous;
                assert!((0..=32767).contains(&previous));
            }
        }
        for tri in decoded.chunks_exact(3) {
            let [a, b, c] = [
                coordinates[tri[0] as usize],
                coordinates[tri[1] as usize],
                coordinates[tri[2] as usize],
            ];
            assert!((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0]) > 0);
        }

        for tri in decoded.chunks_exact(3) {
            assert!(tri[0] != tri[1] && tri[1] != tri[2] && tri[0] != tri[2]);
        }
        let edges = start + 4 + 8192 * 6;
        for edge in 0..4 {
            let p = edges + edge * 134;
            assert_eq!(u32_at(p), 65);
            for i in 0..65 {
                let index = u16_at(p + 4 + i * 2);
                assert!(index < 4225);
                let point = coordinates[index as usize];
                assert_eq!(
                    point[if edge == 0 || edge == 2 { 0 } else { 1 }],
                    if edge < 2 { 0 } else { 32767 }
                );
            }
        }
        assert_eq!(bytes.len(), edges + 4 * 134);
        Ok(())
    }
}
