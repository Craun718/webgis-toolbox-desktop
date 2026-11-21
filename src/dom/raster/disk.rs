mod blocks;

use super::{Envelope, Texture, Triangle, envelope};
use anyhow::{Result, ensure};
use blocks::Blocks;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Mutex, atomic::AtomicBool},
};
use webgis_core::{
    message,
    osgb_memory::{self as memory, Reservation},
};

const WRITE_BATCH: usize = 4096;
const READ_BATCH: usize = 256;

pub(super) struct DiskMesh {
    connection: Mutex<Connection>,
    _reservation: Reservation,
    path: PathBuf,
    blocks: Option<Blocks>,
}

impl DiskMesh {
    pub fn new(stage: &Path, chunked: bool) -> Result<Self> {
        let free = rustix::fs::statvfs(stage)?;
        ensure!(
            free.f_bavail.saturating_mul(free.f_frsize) >= 1024 * 1024,
            message!(osgb_disk_full)
        );
        let cache_kib = (memory::available_budget() / 32 / 1024).clamp(128, 16 * 1024);
        let reservation =
            memory::reserve(cache_kib * 1024 + 384 * 1024, message!(osgb_disk_index))?;
        let path = stage.join("mesh.sqlite");
        let connection = Connection::open(&path)?;
        connection.pragma_update(None, "cache_size", -(cache_kib as i32))?;
        // The parent discards the entire staging directory on failure. This
        // database is never recovered or published.
        connection.execute_batch(
            "PRAGMA temp_store = FILE;
             PRAGMA journal_mode = OFF;
             PRAGMA synchronous = OFF;
             CREATE TABLE textures (id INTEGER PRIMARY KEY, data BLOB NOT NULL, external BLOB UNIQUE);",
        )?;
        if !chunked {
            connection.execute_batch(
                "CREATE TABLE triangles (id INTEGER PRIMARY KEY, data BLOB NOT NULL);
             CREATE VIRTUAL TABLE bounds USING rtree(id, min_x, max_x, min_y, max_y);",
            )?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
            _reservation: reservation,
            path,
            blocks: chunked.then(|| Blocks::new(stage)).transpose()?,
        })
    }

    pub fn insert(
        &mut self,
        triangles: &[Triangle],
        textures: &[Texture],
        first: usize,
        texture_start: usize,
        cancelled: &AtomicBool,
        mut progress: Option<&mut dyn FnMut(usize)>,
    ) -> Result<()> {
        let connection = self.connection.get_mut().unwrap();
        for (chunk, textures) in textures.chunks(WRITE_BATCH).enumerate() {
            super::check_cancelled(cancelled)?;
            let tx = connection.transaction()?;
            {
                let mut insert = tx.prepare_cached("INSERT INTO textures VALUES (?1, ?2, ?3)")?;
                for (index, texture) in textures.iter().enumerate() {
                    insert.execute(params![
                        texture_start + chunk * WRITE_BATCH + index,
                        serde_json::to_vec(texture)?,
                        texture.external_key()
                    ])?;
                }
            }
            tx.commit()?;
            if memory::pressure() {
                reclaim(&self.path);
                memory::wait(cancelled).map_err(anyhow::Error::msg)?;
            }
        }
        if let Some(blocks) = &mut self.blocks {
            return blocks.insert(triangles, first, cancelled);
        }
        for (chunk, triangles) in triangles.chunks(WRITE_BATCH).enumerate() {
            super::check_cancelled(cancelled)?;
            let tx = connection.transaction()?;
            {
                let mut insert = tx.prepare_cached("INSERT INTO triangles VALUES (?1, ?2)")?;
                let mut bbox =
                    tx.prepare_cached("INSERT INTO bounds VALUES (?1, ?2, ?3, ?4, ?5)")?;
                for (index, triangle) in triangles.iter().enumerate() {
                    let id = first + chunk * WRITE_BATCH + index;
                    let bounds = envelope(triangle.xyz);
                    let [x0, y0] = bounds.lower();
                    let [x1, y1] = bounds.upper();
                    insert.execute(params![id, pack_triangle(triangle).as_slice()])?;
                    bbox.execute(params![
                        id,
                        lower_edge(x0),
                        upper_edge(x1),
                        lower_edge(y0),
                        upper_edge(y1)
                    ])?;
                }
            }
            tx.commit()?;
            if let Some(progress) = progress.as_mut() {
                progress(chunk * WRITE_BATCH + triangles.len());
            }
            if memory::pressure() {
                reclaim(&self.path);
                memory::wait(cancelled).map_err(anyhow::Error::msg)?;
            }
        }
        Ok(())
    }

    pub fn texture(&self, id: usize) -> Result<Texture> {
        let bytes: Vec<u8> = self.connection.lock().unwrap().query_row(
            "SELECT data FROM textures WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    pub fn external_texture(&self, path: &Path) -> Result<Option<usize>> {
        Ok(self
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT id FROM textures WHERE external = ?1",
                [path.as_os_str().as_encoded_bytes()],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn visit(
        &self,
        bounds: Envelope,
        cancelled: &AtomicBool,
        mut visit: impl FnMut(&Triangle) -> Result<()>,
    ) -> Result<()> {
        if let Some(blocks) = &self.blocks {
            return blocks.visit(bounds, cancelled, visit);
        }
        let [x0, y0] = bounds.lower();
        let [x1, y1] = bounds.upper();
        let connection = Connection::open_with_flags(&self.path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.execute_batch(
            "PRAGMA cache_size = -128; PRAGMA temp_store = FILE; PRAGMA temp.cache_size = -128;",
        )?;
        let mut query = connection.prepare(
            "SELECT t.data FROM bounds b CROSS JOIN triangles t ON t.id = b.id
             WHERE b.min_x <= ?1 AND b.max_x >= ?2 AND b.min_y <= ?3 AND b.max_y >= ?4
             ORDER BY b.id",
        )?;
        let mut rows = query.query(params![
            rtree_coordinate(x1),
            rtree_coordinate(x0),
            rtree_coordinate(y1),
            rtree_coordinate(y0)
        ])?;
        // The sorter can use SQLite's temporary files. Reuse the buffer
        // across batches while keeping a single ordered cursor per block.
        let mut batch = Vec::with_capacity(READ_BATCH);
        loop {
            while batch.len() < READ_BATCH {
                let Some(row) = rows.next()? else { break };
                batch.push(unpack_triangle(row.get_ref(0)?.as_blob()?)?);
            }
            if batch.is_empty() {
                return Ok(());
            }
            use rstar::Envelope as _;
            for triangle in batch.drain(..) {
                if bounds.intersects(&envelope(triangle.xyz)) {
                    visit(&triangle)?;
                }
            }
        }
    }

    pub fn block_stats(&self) -> Option<(usize, u64)> {
        self.blocks.as_ref().map(|blocks| {
            let (count, bytes) = blocks.stats();
            (
                count,
                bytes + fs::metadata(&self.path).map_or(0, |file| file.len()),
            )
        })
    }

    pub fn query_bytes(&self) -> usize {
        self.blocks
            .as_ref()
            .map_or(super::DISK_QUERY_BYTES, Blocks::query_bytes)
    }

    pub fn seal(&mut self) {
        if let Some(blocks) = &mut self.blocks {
            blocks.seal();
        }
    }

    pub fn reclaim(&self) {
        if let Some(blocks) = &self.blocks {
            blocks.reclaim();
        }
        reclaim(&self.path);
    }

    pub fn remove(self) -> Result<()> {
        let Self {
            connection,
            path,
            _reservation,
            blocks,
        } = self;
        if let Some(blocks) = blocks {
            blocks.remove()?;
        }
        drop(connection);
        fs::remove_file(path)?;
        Ok(())
    }
}

const TRIANGLE_BYTES: usize = 129;

fn pack_triangle(triangle: &Triangle) -> [u8; TRIANGLE_BYTES] {
    let mut bytes = [0; TRIANGLE_BYTES];
    let uv = triangle.uv.unwrap_or_default();
    for (chunk, value) in bytes[..120]
        .chunks_exact_mut(8)
        .zip(triangle.xyz.iter().flatten().chain(uv.iter().flatten()))
    {
        chunk.copy_from_slice(&value.to_le_bytes());
    }
    bytes[120..128].copy_from_slice(
        &triangle
            .texture
            .map_or(u64::MAX, |id| id as u64)
            .to_le_bytes(),
    );
    bytes[128] = u8::from(triangle.uv.is_some());
    bytes
}

fn unpack_triangle(bytes: &[u8]) -> Result<Triangle> {
    let bytes: &[u8; TRIANGLE_BYTES] = bytes.try_into()?;
    let mut values = bytes[..120]
        .chunks_exact(8)
        .map(|chunk| f64::from_le_bytes(chunk.try_into().unwrap()));
    let mut array = || std::array::from_fn(|_| values.next().unwrap());
    let xyz = [array(), array(), array()];
    let uv = (bytes[128] != 0)
        .then(|| std::array::from_fn(|_| std::array::from_fn(|_| values.next().unwrap())));
    let texture = u64::from_le_bytes(bytes[120..128].try_into().unwrap());
    Ok(Triangle {
        xyz,
        uv,
        texture: (texture != u64::MAX).then_some(texture as usize),
    })
}

// Clamping is monotonic; the original double-precision test removes extra hits.
fn rtree_coordinate(value: f64) -> f64 {
    let cap = f64::from(f32::MAX) / 2.;
    value.clamp(-cap, cap)
}

fn lower_edge(value: f64) -> f64 {
    f64::from((rtree_coordinate(value) as f32).next_down())
}

fn upper_edge(value: f64) -> f64 {
    f64::from((rtree_coordinate(value) as f32).next_up())
}

fn reclaim(path: &Path) {
    #[cfg(target_os = "linux")]
    if let Ok(file) = fs::File::open(path) {
        let _ = file.sync_data();
        let _ = rustix::fs::fadvise(file, 0, None, rustix::fs::Advice::DontNeed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dom::{Mode, PRODUCTS, Request, StagedRaster};

    #[test]
    fn sqlite_full_leaves_existing_output_intact_and_releases_staging() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let output = directory.path().to_owned();
        let old = output.join(PRODUCTS[0]);
        fs::write(&old, b"previous")?;
        let request = Request {
            input: output.join("model.osgb"),
            output: output.clone(),
            metadata: None,
            crs: None,
            origin: None,
            mode: Mode::Local,
            resolution: 1.,
            overwrite: true,
        };
        let stage = StagedRaster::new(&request)?;
        let mut disk = DiskMesh::new(stage.path(), false)?;
        let baseline: usize =
            disk.connection
                .get_mut()
                .unwrap()
                .query_row("PRAGMA page_count", [], |row| row.get(0))?;
        disk.connection
            .get_mut()
            .unwrap()
            .pragma_update(None, "max_page_count", baseline)?;
        let triangles: Vec<_> = (0..512)
            .map(|_| Triangle {
                xyz: [[0., 0., 0.], [1., 0., 0.], [0., 1., 0.]],
                uv: None,
                texture: None,
            })
            .collect();
        let error = disk
            .insert(&triangles, &[], 0, 0, &AtomicBool::new(false), None)
            .unwrap_err();
        assert_eq!(
            error
                .downcast_ref::<rusqlite::Error>()
                .unwrap()
                .sqlite_error_code(),
            Some(rusqlite::ErrorCode::DiskFull)
        );
        drop(disk);
        drop(stage);
        assert_eq!(fs::read_dir(output)?.count(), 1);
        assert_eq!(fs::read(&old)?, b"previous");
        Ok(())
    }
}
