use super::{TRIANGLE_BYTES, pack_triangle, reclaim, unpack_triangle};
use crate::dom::raster::{Envelope, Triangle, check_cancelled, envelope};
use anyhow::Result;
use rstar::{Envelope as _, RTree, RTreeObject};
use std::{
    fs::{self, File},
    io::{BufWriter, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use webgis_core::{
    message,
    osgb_memory::{self as memory, Reservation},
};

const BLOCK_TRIANGLES: usize = 4 * memory::MIB / TRIANGLE_BYTES;
const BUFFER_BYTES: usize = 64 * 1024;
const READ_TRIANGLES: usize = BUFFER_BYTES / TRIANGLE_BYTES;

struct Block {
    bounds: Envelope,
    offset: u64,
    first: usize,
    count: usize,
}

impl RTreeObject for Block {
    type Envelope = Envelope;

    fn envelope(&self) -> Envelope {
        self.bounds
    }
}

pub(super) struct Blocks {
    path: PathBuf,
    writer: Option<(BufWriter<File>, Reservation)>,
    index: RTree<Block>,
    bytes: u64,
    // Includes descriptor, R-tree nodes and their spare capacity, and the
    // reservation list. This also covers transient allocations during insertion.
    reservations: Vec<Reservation>,
}

impl Blocks {
    pub fn new(stage: &Path) -> Result<Self> {
        let path = stage.join("mesh.blocks");
        let reservation = memory::reserve(BUFFER_BYTES, message!(osgb_disk_index))?;
        let writer = BufWriter::with_capacity(BUFFER_BYTES, File::create(&path)?);
        Ok(Self {
            path,
            writer: Some((writer, reservation)),
            index: RTree::new(),
            bytes: 0,
            reservations: Vec::new(),
        })
    }

    pub fn insert(
        &mut self,
        triangles: &[Triangle],
        first: usize,
        cancelled: &AtomicBool,
    ) -> Result<()> {
        if triangles.is_empty() {
            return Ok(());
        }
        self.reservations.push(memory::reserve(
            triangles.len().div_ceil(BLOCK_TRIANGLES) * 512,
            message!(osgb_disk_index),
        )?);
        let (writer, _) = self.writer.as_mut().unwrap();
        for (chunk, triangles) in triangles.chunks(BLOCK_TRIANGLES).enumerate() {
            let mut bounds = envelope(triangles[0].xyz);
            for group in triangles.chunks(4096) {
                check_cancelled(cancelled)?;
                for triangle in group {
                    bounds.merge(&envelope(triangle.xyz));
                    writer.write_all(&pack_triangle(triangle))?;
                }
            }
            writer.flush()?;
            self.index.insert(Block {
                bounds,
                offset: self.bytes,
                first: first + chunk * BLOCK_TRIANGLES,
                count: triangles.len(),
            });
            self.bytes += (triangles.len() * TRIANGLE_BYTES) as u64;
            if memory::pressure() {
                reclaim(&self.path);
                memory::wait(cancelled).map_err(anyhow::Error::msg)?;
            }
        }
        Ok(())
    }

    pub fn seal(&mut self) {
        self.writer = None;
    }

    pub fn stats(&self) -> (usize, u64) {
        (self.index.size(), self.bytes)
    }

    pub fn query_bytes(&self) -> usize {
        BUFFER_BYTES.saturating_add(self.index.size().saturating_mul(size_of::<&Block>()))
    }

    pub fn visit(
        &self,
        bounds: Envelope,
        cancelled: &AtomicBool,
        mut visit: impl FnMut(&Triangle) -> Result<()>,
    ) -> Result<()> {
        let mut candidates = Vec::with_capacity(self.index.size());
        candidates.extend(self.index.locate_in_envelope_intersecting(&bounds));
        candidates.sort_unstable_by_key(|block| block.first);
        if candidates.is_empty() {
            return Ok(());
        }
        let mut file = File::open(&self.path)?;
        let mut buffer = vec![0; READ_TRIANGLES * TRIANGLE_BYTES];
        for block in candidates {
            file.seek(SeekFrom::Start(block.offset))?;
            let mut remaining = block.count;
            while remaining != 0 {
                check_cancelled(cancelled)?;
                let count = remaining.min(READ_TRIANGLES);
                let bytes = &mut buffer[..count * TRIANGLE_BYTES];
                file.read_exact(bytes)?;
                for record in bytes.chunks_exact(TRIANGLE_BYTES) {
                    let triangle = unpack_triangle(record)?;
                    if bounds.intersects(&envelope(triangle.xyz)) {
                        visit(&triangle)?;
                    }
                }
                remaining -= count;
            }
        }
        Ok(())
    }

    pub fn reclaim(&self) {
        reclaim(&self.path);
    }

    pub fn remove(mut self) -> Result<()> {
        self.seal();
        fs::remove_file(self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_split_pages_and_visit_candidates_in_triangle_order() -> Result<()> {
        let stage = tempfile::tempdir()?;
        let mut blocks = Blocks::new(stage.path())?;
        let cancelled = AtomicBool::new(false);
        // The two X bounds round to the same f32. The triangle filter has
        // to distinguish them after the wider block envelope intersects.
        let x = 100_000_000.;
        let triangle = |x, z| Triangle {
            xyz: [[x, 0., z], [x + 0.25, 0., z], [x, 1., z]],
            uv: None,
            texture: None,
        };
        let mut page: Vec<_> = (0..=BLOCK_TRIANGLES)
            .map(|_| triangle(x + 1., 0.))
            .collect();
        page[BLOCK_TRIANGLES - 1] = triangle(x, 2.);
        page[BLOCK_TRIANGLES] = triangle(x, 1.);
        blocks.insert(&page, 0, &cancelled)?;
        blocks.insert(&[triangle(x, 3.)], page.len(), &cancelled)?;
        blocks.seal();
        assert_eq!(
            blocks.stats(),
            (3, ((page.len() + 1) * TRIANGLE_BYTES) as u64)
        );
        let mut descriptors: Vec<_> = blocks.index.iter().collect();
        descriptors.sort_unstable_by_key(|block| block.first);
        assert_eq!(descriptors[0].count, BLOCK_TRIANGLES);
        assert!(descriptors[0].count * TRIANGLE_BYTES <= 4 * memory::MIB);
        assert_eq!(
            descriptors[1].offset,
            (BLOCK_TRIANGLES * TRIANGLE_BYTES) as u64
        );
        assert_eq!(descriptors[2].first, page.len());
        let mut heights = Vec::new();
        blocks.visit(
            Envelope::from_corners([x, 0.], [x + 0.25, 1.]),
            &cancelled,
            |tri| {
                heights.push(tri.xyz[0][2]);
                Ok(())
            },
        )?;
        assert_eq!(heights, [2., 1., 3.]);
        blocks.remove()?;
        assert_eq!(fs::read_dir(stage.path())?.count(), 0);
        Ok(())
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn full_block_file_leaves_previous_output_and_discards_staging() -> Result<()> {
        use crate::dom::{Mode, PRODUCTS, Request, StagedRaster};

        let output = tempfile::tempdir()?;
        let old = output.path().join(PRODUCTS[0]);
        fs::write(&old, b"previous")?;
        let stage = StagedRaster::new(&Request {
            input: output.path().join("model.osgb"),
            output: output.path().to_owned(),
            metadata: None,
            crs: None,
            origin: None,
            mode: Mode::Local,
            resolution: 1.,
            overwrite: true,
        })?;
        let mut blocks = Blocks::new(stage.path())?;
        blocks.writer.as_mut().unwrap().0 =
            BufWriter::with_capacity(BUFFER_BYTES, File::options().write(true).open("/dev/full")?);
        let error = blocks
            .insert(
                &[Triangle {
                    xyz: [[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
                    uv: None,
                    texture: None,
                }],
                0,
                &AtomicBool::new(false),
            )
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::StorageFull
        );
        drop(blocks);
        drop(stage);
        assert_eq!(fs::read(&old)?, b"previous");
        assert_eq!(fs::read_dir(output.path())?.count(), 1);
        Ok(())
    }
}
