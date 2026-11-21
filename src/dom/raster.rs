mod disk;

use super::{Phase, Summary, TaskProgress, check_cancelled, is_osgb, tiff_output::NODATA};
use anyhow::{Context, Result, ensure};
use disk::DiskMesh;
use image::{ImageFormat, ImageReader, Limits, RgbaImage};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs,
    io::{Cursor, Read, Seek, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, atomic::AtomicBool},
};
use walkdir::WalkDir;
use webgis_core::message;
use webgis_core::model_clip::osgb::{self, RasterImage};
use webgis_core::osgb_memory::{self as memory, Reservation};

pub(super) const DISK_QUERY_BYTES: usize = 1024 * 1024;
const CACHE_BYTES: usize = 128 * 1024 * 1024;
const MAX_ENCODED_BYTES: u64 = 128 * 1024 * 1024;

pub struct Mesh {
    pub bounds: [f64; 6],
    pub triangles: Vec<Triangle>,
    pub images: Vec<Texture>,
    pub count: usize,
    image_count: usize,
    external: HashMap<PathBuf, usize>,
    pub texture_peak: usize,
    disk: Option<DiskMesh>,
    geometry: Vec<Reservation>,
    pending_pages: Vec<usize>,
    pending_bytes: usize,
    chunked: bool,
}

pub(super) use osgb::RasterTriangle as Triangle;

#[derive(Serialize, Deserialize)]
pub struct Texture {
    label: String,
    source: Source,
}

#[derive(Serialize, Deserialize)]
enum Source {
    Embedded { file: PathBuf, range: Range<usize> },
    External(PathBuf),
}

#[derive(Clone, Copy)]
pub enum Storage<'a> {
    Preview,
    Auto(&'a Path),
    Chunked(&'a Path),
    #[cfg(test)]
    ChunkedDisk(&'a Path),
    #[cfg(test)]
    Disk(&'a Path),
    #[cfg(test)]
    Memory,
}

impl<'a> Storage<'a> {
    pub fn automatic_stage(self) -> Option<&'a Path> {
        match self {
            Self::Auto(stage) | Self::Chunked(stage) => Some(stage),
            _ => None,
        }
    }

    fn chunked(self) -> bool {
        match self {
            Self::Chunked(_) => true,
            #[cfg(test)]
            Self::ChunkedDisk(_) => true,
            _ => false,
        }
    }
}

fn pending_geometry_limit(budget: usize) -> usize {
    (8 * memory::MIB).min(budget / 8)
}

impl Mesh {
    pub fn load(
        root: &Path,
        files: Vec<PathBuf>,
        origin: [f64; 3],
        cancelled: &AtomicBool,
        progress: &mut impl FnMut(TaskProgress),
        storage: Storage<'_>,
    ) -> Result<(Self, Vec<PathBuf>)> {
        let mut mesh = Self {
            bounds: [
                f64::INFINITY,
                f64::INFINITY,
                f64::INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
                f64::NEG_INFINITY,
            ],
            triangles: Vec::new(),
            images: Vec::new(),
            count: 0,
            image_count: 0,
            external: HashMap::new(),
            texture_peak: 0,
            disk: None,
            geometry: Vec::new(),
            pending_pages: Vec::new(),
            pending_bytes: 0,
            chunked: storage.chunked(),
        };
        #[cfg(test)]
        if let Storage::Disk(stage) | Storage::ChunkedDisk(stage) = storage {
            mesh.spill(stage, cancelled, progress)?;
        }
        let mut unread = files.len();
        let mut directory_count = None;
        let mut pending: Vec<_> = files.into_iter().rev().map(|file| (file, false)).collect();
        let mut active = HashSet::new();
        let mut visited = HashSet::new();
        let mut selected = Vec::new();
        let mut read_count = 0;
        let mut read_fraction = 0.0_f32;
        while let Some((file, finished)) = pending.pop() {
            check_cancelled(cancelled)?;
            if finished {
                active.remove(&file);
                visited.insert(file);
                continue;
            }
            unread -= 1;
            let file = file
                .canonicalize()
                .with_context(|| file.display().to_string())?;
            ensure!(
                file.starts_with(root),
                message!(external_page_is_outside_the_model_folder, file.display())
            );
            if visited.contains(&file) {
                continue;
            }
            ensure!(
                active.insert(file.clone()),
                message!(circular_osgb_page_reference, file.display())
            );
            // 分页引用会增加待处理数量。保持估算进度单调，留出最后一段给遍历结束。
            let known_total = read_count + unread + 1;
            let estimated_total = known_total.max(directory_count.unwrap_or(0));
            let estimate = read_count as f32 / estimated_total as f32;
            read_fraction = read_fraction.max(estimate.min(0.95));
            let stage_message = if mesh.chunked {
                let (blocks, bytes) = mesh
                    .disk
                    .as_ref()
                    .and_then(DiskMesh::block_stats)
                    .unwrap_or_default();
                message!(osgb_block_progress, read_count, blocks, bytes)
            } else {
                message!(read_model, read_count, file.display())
            };
            progress(TaskProgress::new(
                Phase::ReadModel,
                stage_message,
                Some(read_fraction),
            ));
            let context = || message!(could_not_read, file.display());
            if memory::pressure() {
                mesh.reclaim_disk_cache();
            }
            memory::wait(cancelled).map_err(anyhow::Error::msg)?;
            let estimate = memory::page_bytes(&file).with_context(context)?;
            if let Some(stage) = storage.automatic_stage()
                && mesh.disk.is_none()
                && !mesh.triangles.is_empty()
                && estimate > memory::available_budget()
            {
                mesh.spill(stage, cancelled, progress)?;
            }
            let page = memory::reserve(estimate, file.display())?;
            let bytes = memory::read_page(&file).with_context(context)?;
            let mut scene = match osgb::raster_mesh(&bytes, cancelled) {
                Err(osgb::RasterError::Budget(_))
                    if storage.automatic_stage().is_some()
                        && mesh.disk.is_none()
                        && !mesh.triangles.is_empty() =>
                {
                    if let Some(stage) = storage.automatic_stage() {
                        mesh.spill(stage, cancelled, progress)?;
                    }
                    osgb::raster_mesh(&bytes, cancelled)
                }
                result => result,
            }
            .with_context(context)?;
            if !scene.pages.is_empty() && directory_count.is_none() {
                // 深度优先遍历的队列看不到尚未展开的分支，先用目录里的文件数估算。
                let mut count = 0;
                for entry in WalkDir::new(root) {
                    check_cancelled(cancelled)?;
                    if let Ok(entry) = entry
                        && entry.file_type().is_file()
                        && is_osgb(entry.path())
                    {
                        count += 1;
                    }
                }
                directory_count = Some(count);
            }
            read_count += 1;
            pending.push((file.clone(), true));
            for name in scene.pages.drain(..).rev() {
                let path = Path::new(&name);
                ensure!(
                    !path.is_absolute() && !name.contains(':'),
                    message!(absolute_page_path_name_is_unsupported, name = name)
                );
                pending.push((file.with_file_name(path), false));
                unread += 1;
            }
            if scene.triangles.is_empty() {
                continue;
            }
            selected.push(file.clone());
            let used_textures: HashSet<_> = scene
                .triangles
                .iter()
                .filter_map(|tri| tri.texture.map(|id| id as u32))
                .collect();
            scene.images.retain(|id, _| used_textures.contains(id));
            let mut ids = HashMap::new();
            let mut external = HashMap::new();
            let mut images = Vec::new();
            let image_start = mesh.image_count;
            for (id, source) in scene.images.drain() {
                check_cancelled(cancelled)?;
                let source = match source {
                    RasterImage::Embedded { name, bytes: range } => {
                        ensure!(
                            range.len() as u64 <= MAX_ENCODED_BYTES,
                            message!(
                                embedded_texture_name_in_0_exceeds_the_size_limit,
                                file.display(),
                                name = name
                            )
                        );
                        Texture {
                            label: message!(embedded_texture_name_in, file.display(), name = name),
                            source: Source::Embedded {
                                file: file.clone(),
                                range: range.clone(),
                            },
                        }
                    }
                    RasterImage::External(name) => {
                        let path = texture_path(root, &file, &name).with_context(context)?;
                        if let Some(&index) = external.get(&path) {
                            ids.insert(id, index);
                            continue;
                        }
                        let previous = match &mesh.disk {
                            Some(disk) => disk.external_texture(&path)?,
                            None => mesh.external.get(&path).copied(),
                        };
                        if let Some(index) = previous {
                            ids.insert(id, index);
                            continue;
                        }
                        external.insert(path.clone(), image_start + images.len());
                        Texture {
                            label: message!(references, file.display(), path.display()),
                            source: Source::External(path),
                        }
                    }
                };
                let validate = || match &source.source {
                    Source::Embedded { range, .. } => source
                        .decode_bytes(&bytes[range.clone()])
                        .with_context(|| source.label.clone()),
                    Source::External(_) => source.decode(),
                };
                let decoded = match validate() {
                    Err(error) if error.is::<memory::BudgetError>() => {
                        if let Some(stage) = storage.automatic_stage()
                            && mesh.disk.is_none()
                            && !mesh.triangles.is_empty()
                        {
                            mesh.spill(stage, cancelled, progress)?;
                            validate()?
                        } else {
                            return Err(error);
                        }
                    }
                    result => result?,
                };
                mesh.texture_peak = mesh.texture_peak.max(decoded.decode_bytes);
                ids.insert(id, image_start + images.len());
                images.push(source);
            }
            for (index, tri) in scene.triangles.iter_mut().enumerate() {
                if index % 4096 == 0 {
                    check_cancelled(cancelled)?;
                }
                let xyz = tri.xyz.map(|v| std::array::from_fn(|i| v[i] + origin[i]));
                for v in xyz {
                    ensure!(
                        v.iter().all(|n| n.is_finite()) && (v[2] as f32).is_finite(),
                        message!(coordinates_out_of_range_in, file.display())
                    );
                    for (i, value) in v.into_iter().enumerate() {
                        mesh.bounds[i] = mesh.bounds[i].min(value);
                        mesh.bounds[i + 3] = mesh.bounds[i + 3].max(value);
                    }
                }
                let texture = tri
                    .texture
                    .map(|id| {
                        ids.get(&(id as u32))
                            .copied()
                            .context(message!(texture_object_was_not_found))
                    })
                    .transpose()
                    .with_context(context)?;
                if matches!(storage, Storage::Preview) {
                    mesh.count += 1;
                    continue;
                }
                tri.xyz = xyz;
                tri.texture = texture;
            }
            drop(bytes);
            drop(page);
            if !matches!(storage, Storage::Preview) {
                // Include vector growth and the in-memory R-tree build.
                let page_geometry = scene
                    .triangles
                    .capacity()
                    .saturating_mul(size_of::<Triangle>());
                let retained = page_geometry
                    .saturating_add(scene.triangles.capacity().saturating_mul(144))
                    .saturating_add(images.iter().map(Texture::descriptor_bytes).sum());
                if let Some(stage) = storage.automatic_stage()
                    && mesh.disk.is_none()
                    && (retained.saturating_sub(page_geometry)
                        > memory::available_budget().saturating_sub(memory::limit() / 4)
                        || (mesh.chunked
                            && mesh.pending_bytes.saturating_add(retained)
                                > pending_geometry_limit(memory::limit())))
                {
                    mesh.spill(stage, cancelled, progress)?;
                }
                let triangle_start = mesh.count;
                mesh.count += scene.triangles.len();
                mesh.image_count += images.len();
                if let Some(disk) = &mut mesh.disk {
                    disk.insert(
                        &scene.triangles,
                        &images,
                        triangle_start,
                        image_start,
                        cancelled,
                        None,
                    )?;
                } else {
                    let mut triangles = std::mem::take(&mut scene.triangles);
                    drop(scene);
                    mesh.geometry
                        .push(memory::reserve(retained, file.display())?);
                    mesh.external.extend(external);
                    if mesh.chunked {
                        mesh.pending_pages.push(triangles.len());
                        mesh.pending_bytes += retained;
                    }
                    mesh.triangles.append(&mut triangles);
                    mesh.images.append(&mut images);
                }
            }
        }
        ensure!(
            mesh.count != 0,
            message!(the_selected_model_contains_no_triangles)
        );
        if mesh.chunked {
            let (blocks, bytes) = mesh
                .disk
                .as_ref()
                .and_then(DiskMesh::block_stats)
                .unwrap_or_default();
            progress(TaskProgress::new(
                Phase::ReadModel,
                message!(osgb_block_progress, read_count, blocks, bytes),
                Some(1.0),
            ));
        }
        Ok((mesh, selected))
    }

    pub fn spill(
        &mut self,
        stage: &Path,
        cancelled: &AtomicBool,
        progress: &mut impl FnMut(TaskProgress),
    ) -> Result<()> {
        progress(TaskProgress::new(
            Phase::SpillGeometry,
            message!(osgb_disk_index),
            None,
        ));
        if self.disk.is_some() {
            return Ok(());
        }
        let mut disk = DiskMesh::new(stage, self.chunked)?;
        if self.chunked {
            disk.insert(&[], &self.images, 0, 0, cancelled, None)?;
            let mut first = 0;
            for count in self.pending_pages.drain(..) {
                let end = first + count;
                disk.insert(&self.triangles[first..end], &[], first, 0, cancelled, None)?;
                first = end;
            }
            self.pending_pages = Vec::new();
            self.pending_bytes = 0;
        } else {
            let total = self.triangles.len();
            disk.insert(
                &self.triangles,
                &self.images,
                0,
                0,
                cancelled,
                Some(&mut |done| {
                    progress(TaskProgress::new(
                        Phase::SpillGeometry,
                        message!(osgb_disk_index_progress, done, total),
                        None,
                    ));
                }),
            )?;
        }
        self.triangles = Vec::new();
        self.images = Vec::new();
        self.external = HashMap::new();
        self.geometry.clear();
        self.disk = Some(disk);
        Ok(())
    }

    pub fn reclaim_disk_cache(&self) {
        if let Some(disk) = &self.disk {
            disk.reclaim();
        }
    }

    pub fn is_on_disk(&self) -> bool {
        self.disk.is_some()
    }

    pub fn seal_disk(&mut self) {
        if let Some(disk) = &mut self.disk {
            disk.seal();
        }
    }

    pub fn disk_query_bytes(&self) -> Option<usize> {
        self.disk.as_ref().map(DiskMesh::query_bytes)
    }

    pub fn textures(&self) -> TextureCache<'_> {
        TextureCache::with_disk(&self.images, self.disk.as_ref())
    }

    pub fn render(
        &self,
        index: Option<&Index>,
        textures: &TextureCache<'_>,
        summary: &Summary,
        rect: [usize; 4],
        cancelled: &AtomicBool,
    ) -> Result<Block> {
        if let Some(disk) = &self.disk {
            render_candidates(textures, summary, rect, cancelled, |bounds, visit| {
                disk.visit(bounds, cancelled, visit)
            })
        } else {
            render(
                &self.triangles,
                index.unwrap(),
                textures,
                summary,
                rect,
                cancelled,
            )
        }
    }

    pub fn finish(self) -> Result<()> {
        if let Some(disk) = self.disk {
            disk.remove()?;
        }
        Ok(())
    }
}

fn texture_path(root: &Path, file: &Path, name: &str) -> Result<PathBuf> {
    let reference = Path::new(name);
    ensure!(
        !reference.is_absolute() && !name.contains(':'),
        message!(absolute_texture_path_name_is_unsupported, name = name)
    );
    let path = file
        .parent()
        .context(message!(missing_parent_folder_for_the_model))?
        .join(reference)
        .canonicalize()
        .with_context(|| message!(external_texture_name_was_not_found, name = name))?;
    ensure!(
        path.starts_with(root) && path.is_file(),
        message!(
            external_texture_is_outside_the_model_folder_name,
            name = name
        )
    );
    Ok(path)
}

impl Texture {
    fn external_key(&self) -> Option<&[u8]> {
        match &self.source {
            Source::External(path) => Some(path.as_os_str().as_encoded_bytes()),
            Source::Embedded { .. } => None,
        }
    }

    fn descriptor_bytes(&self) -> usize {
        let path = match &self.source {
            Source::Embedded { file, .. } => file,
            Source::External(path) => path,
        };
        (size_of::<Self>() + path.as_os_str().len() + self.label.len()).saturating_mul(2)
    }

    fn decode(&self) -> Result<DecodedTexture> {
        self.decode_inner().with_context(|| self.label.clone())
    }

    fn decode_inner(&self) -> Result<DecodedTexture> {
        let (mut file, offset, size) = match &self.source {
            Source::Embedded { file, range } => (
                fs::File::open(file)?,
                range.start as u64,
                range.len() as u64,
            ),
            Source::External(path) => {
                let file = fs::File::open(path)?;
                let size = file.metadata()?.len();
                (file, 0, size)
            }
        };
        ensure!(
            size <= MAX_ENCODED_BYTES,
            message!(external_texture_exceeds_the_size_limit)
        );
        let _encoded = memory::reserve(size as usize, &self.label)?;
        let mut bytes = vec![0; size as usize];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
        #[cfg(target_os = "linux")]
        if memory::limit() != usize::MAX {
            let _ = rustix::fs::fadvise(
                file,
                offset,
                std::num::NonZeroU64::new(size),
                rustix::fs::Advice::DontNeed,
            );
        }
        self.decode_bytes(&bytes)
    }

    fn decode_bytes(&self, bytes: &[u8]) -> Result<DecodedTexture> {
        let format = image::guess_format(bytes)?;
        ensure!(
            matches!(format, ImageFormat::Png | ImageFormat::Jpeg),
            message!(
                unsupported_texture_format_format_use_jpeg_or_png,
                format = format
            )
        );
        let reader = || {
            let mut limits = Limits::default();
            limits.max_image_width = Some(8192);
            limits.max_image_height = Some(8192);
            limits.max_alloc = Some((CACHE_BYTES * 2) as u64);
            let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
            reader.limits(limits);
            reader
        };
        let encoded_bytes = bytes.len();
        // The JPEG decoder reads the entire source into its own growing buffer.
        let decoder_input = if format == ImageFormat::Jpeg {
            encoded_bytes * 2
        } else {
            0
        };
        let header_bytes = 256 * 1024 + decoder_input;
        let _header = memory::reserve(header_bytes, &self.label)?;
        let (w, h) = reader().into_dimensions()?;
        let output_bytes = w as usize * h as usize * 4;
        ensure!(
            output_bytes <= CACHE_BYTES,
            message!(decoded_texture_exceeds_cache_capacity)
        );
        let reservation = memory::reserve(output_bytes, &self.label)?;
        let scratch_bytes = output_bytes * 3;
        let _scratch = memory::reserve(scratch_bytes, &self.label)?;
        Ok(DecodedTexture {
            pixels: reader().decode()?.into_rgba8(),
            decode_bytes: encoded_bytes + header_bytes + output_bytes + scratch_bytes,
            _reservation: reservation,
        })
    }
}

struct DecodedTexture {
    pixels: RgbaImage,
    decode_bytes: usize,
    _reservation: Reservation,
}

#[derive(Default)]
struct CacheState {
    entries: HashMap<usize, (Arc<DecodedTexture>, u64)>,
    loading: HashSet<usize>,
    decodes: usize,
    retries: usize,
    clock: u64,
    bytes: usize,
}

impl CacheState {
    fn evict_idle(&mut self) -> bool {
        let id = self
            .entries
            .iter()
            .filter(|(_, (image, _))| Arc::strong_count(image) == 1)
            .min_by_key(|(_, (_, used))| *used)
            .map(|(id, _)| *id);
        if let Some(id) = id {
            self.bytes -= self.entries.remove(&id).unwrap().0.pixels.as_raw().len();
        }
        id.is_some()
    }

    fn complete(
        &mut self,
        id: usize,
        result: Result<DecodedTexture>,
    ) -> Result<Arc<DecodedTexture>> {
        self.loading.remove(&id);
        let image = Arc::new(result?);
        let bytes = image.pixels.as_raw().len();
        while self.bytes + bytes > CACHE_BYTES && self.evict_idle() {}
        self.bytes += bytes;
        self.entries.insert(id, (Arc::clone(&image), self.clock));
        Ok(image)
    }
}

pub struct TextureCache<'a> {
    sources: &'a [Texture],
    disk: Option<&'a DiskMesh>,
    state: Mutex<CacheState>,
    retry: Mutex<()>,
}

impl<'a> TextureCache<'a> {
    #[cfg(test)]
    pub fn new(sources: &'a [Texture]) -> Self {
        Self::with_disk(sources, None)
    }

    fn with_disk(sources: &'a [Texture], disk: Option<&'a DiskMesh>) -> Self {
        Self {
            sources,
            disk,
            state: Mutex::new(CacheState::default()),
            retry: Mutex::new(()),
        }
    }

    pub fn trim_idle(&self) {
        let mut state = self.state.lock().unwrap();
        state
            .entries
            .retain(|_, (image, _)| Arc::strong_count(image) > 1);
        state.bytes = state
            .entries
            .values()
            .map(|(image, _)| image.pixels.as_raw().len())
            .sum();
    }

    fn get(&self, id: usize, cancelled: &AtomicBool) -> Result<Arc<DecodedTexture>> {
        self.get_with(id, cancelled, || {
            if let Some(disk) = self.disk {
                disk.texture(id)?.decode()
            } else {
                self.sources[id].decode()
            }
        })
    }

    fn get_with(
        &self,
        id: usize,
        cancelled: &AtomicBool,
        decode: impl Fn() -> Result<DecodedTexture>,
    ) -> Result<Arc<DecodedTexture>> {
        let pause = || std::thread::sleep(std::time::Duration::from_millis(5));
        loop {
            check_cancelled(cancelled)?;
            let mut state = self.state.lock().unwrap();
            state.clock += 1;
            let clock = state.clock;
            if let Some((image, used)) = state.entries.get_mut(&id) {
                *used = clock;
                return Ok(Arc::clone(image));
            }
            if state.retries == 0 && state.loading.insert(id) {
                state.decodes += 1;
                break;
            }
            drop(state);
            pause();
        }

        let result = decode();
        let mut state = self.state.lock().unwrap();
        state.decodes -= 1;
        if !matches!(&result, Err(error) if error.is::<memory::BudgetError>()) {
            return state.complete(id, result);
        }

        // Give a budget-limited decode exclusive admission after the
        // concurrent attempts finish. Other blocks can finish sampling
        // textures they already hold and release those reservations.
        state.retries += 1;
        drop(state);
        let _retry = self.retry.lock().unwrap();
        let result = (|| loop {
            check_cancelled(cancelled)?;
            let mut state = self.state.lock().unwrap();
            if state.decodes != 0 {
                drop(state);
                pause();
                continue;
            }
            while state.evict_idle() {}
            let has_live_images = !state.entries.is_empty();
            drop(state);
            match decode() {
                Err(error) if error.is::<memory::BudgetError>() && has_live_images => pause(),
                result => return result,
            }
        })();
        let mut state = self.state.lock().unwrap();
        state.retries -= 1;
        state.complete(id, result)
    }
}

type Envelope = AABB<[f64; 2]>;
type Index = RTree<GeomWithData<Rectangle<[f64; 2]>, usize>>;

fn envelope(xyz: [[f64; 3]; 3]) -> Envelope {
    let low = std::array::from_fn(|i| xyz.iter().map(|v| v[i]).fold(f64::INFINITY, f64::min));
    let high = std::array::from_fn(|i| xyz.iter().map(|v| v[i]).fold(f64::NEG_INFINITY, f64::max));
    AABB::from_corners(low, high)
}

pub fn index(triangles: &[Triangle]) -> Index {
    RTree::bulk_load(
        triangles
            .iter()
            .enumerate()
            .map(|(id, tri)| GeomWithData::new(Rectangle::from_aabb(envelope(tri.xyz)), id))
            .collect(),
    )
}

pub struct Block {
    heights: Vec<f64>,
    planes: [Vec<u8>; 4],
}

impl Block {
    pub fn into_planes(self) -> (Vec<f32>, [Vec<u8>; 4]) {
        (
            self.heights
                .into_iter()
                .map(|z| if z.is_finite() { z as f32 } else { NODATA })
                .collect(),
            self.planes,
        )
    }
}

pub fn render(
    triangles: &[Triangle],
    index: &Index,
    textures: &TextureCache<'_>,
    summary: &Summary,
    [x, y, w, h]: [usize; 4],
    cancelled: &AtomicBool,
) -> Result<Block> {
    render_candidates(
        textures,
        summary,
        [x, y, w, h],
        cancelled,
        |bounds, visit| {
            let mut hits: Vec<_> = index
                .locate_in_envelope_intersecting(&bounds)
                .map(|item| item.data)
                .collect();
            hits.sort_unstable();
            for id in hits {
                visit(&triangles[id])?;
            }
            Ok(())
        },
    )
}

fn render_candidates(
    textures: &TextureCache<'_>,
    summary: &Summary,
    [x, y, w, h]: [usize; 4],
    cancelled: &AtomicBool,
    candidates: impl FnOnce(Envelope, &mut dyn FnMut(&Triangle) -> Result<()>) -> Result<()>,
) -> Result<Block> {
    let mut block = Block {
        heights: vec![f64::NEG_INFINITY; w * h],
        planes: std::array::from_fn(|_| vec![0; w * h]),
    };
    let [west, _, _, _, north, _] = summary.bounds;
    let res = summary.resolution;
    let world_x = |column: usize| west + (column as f64 + 0.5) * res;
    let world_y = |row: usize| north - (row as f64 + 0.5) * res;
    let bounds = AABB::from_corners(
        [world_x(x), world_y(y + h - 1)],
        [world_x(x + w - 1), world_y(y)],
    );
    candidates(bounds, &mut |tri| {
        check_cancelled(cancelled)?;
        let [a, b, c] = tri.xyz;
        let (bx, by, cx, cy) = (b[0] - a[0], b[1] - a[1], c[0] - a[0], c[1] - a[1]);
        let det = bx * cy - by * cx;
        if det == 0. || !det.is_finite() {
            return Ok(());
        }
        let extent = envelope(tri.xyz);
        let min = extent.lower();
        let max = extent.upper();
        // Expand by a pixel to absorb roundoff at the bounding box edges.
        let x0 = (((min[0] - west) / res - 0.5).floor() as usize).max(x);
        let x1 = (((max[0] - west) / res - 0.5).ceil() as usize)
            .saturating_add(1)
            .min(x + w);
        let y0 = (((north - max[1]) / res - 0.5).floor() as usize).max(y);
        let y1 = (((north - min[1]) / res - 0.5).ceil() as usize)
            .saturating_add(1)
            .min(y + h);
        let mut image = None;
        for row in y0..y1 {
            let dy = world_y(row) - a[1];
            for col in x0..x1 {
                let dx = world_x(col) - a[0];
                let wb = (dx * cy - dy * cx) / det;
                let wc = (bx * dy - by * dx) / det;
                let wa = 1. - wb - wc;
                if wa.min(wb).min(wc) < -1e-12 {
                    continue;
                }
                let pixel = (row - y) * w + col - x;
                let z = wa * a[2] + wb * b[2] + wc * c[2];
                if z <= block.heights[pixel] || !z.is_finite() {
                    continue;
                }
                block.heights[pixel] = z;
                let rgba = match (tri.texture, tri.uv) {
                    (Some(id), Some([au, bu, cu])) => {
                        if image.is_none() {
                            image = Some(textures.get(id, cancelled)?);
                        }
                        sample(
                            &image.as_ref().unwrap().pixels,
                            wa * au[0] + wb * bu[0] + wc * cu[0],
                            wa * au[1] + wb * bu[1] + wc * cu[1],
                        )
                    }
                    _ => [0; 4],
                };
                for (plane, value) in block.planes.iter_mut().zip(rgba) {
                    plane[pixel] = value;
                }
            }
        }
        Ok(())
    })?;
    ensure!(
        !block.heights.iter().any(|&z| z as f32 == NODATA),
        message!(
            model_elevation_conflicts_with_dsm_nodata_value_nodata,
            NODATA = NODATA
        )
    );
    Ok(block)
}

fn sample(image: &RgbaImage, u: f64, v: f64) -> [u8; 4] {
    let x =
        (u.clamp(0., 1.) * f64::from(image.width()) - 0.5).clamp(0., f64::from(image.width() - 1));
    let y = ((1. - v.clamp(0., 1.)) * f64::from(image.height()) - 0.5)
        .clamp(0., f64::from(image.height() - 1));
    let (x0, y0) = (x.floor() as u32, y.floor() as u32);
    let (x1, y1) = (
        (x0 + 1).min(image.width() - 1),
        (y0 + 1).min(image.height() - 1),
    );
    let (dx, dy) = (x.fract(), y.fract());
    let blend = |channel| {
        let top = f64::from(image.get_pixel(x0, y0)[channel]) * (1. - dx)
            + f64::from(image.get_pixel(x1, y0)[channel]) * dx;
        let bottom = f64::from(image.get_pixel(x0, y1)[channel]) * (1. - dx)
            + f64::from(image.get_pixel(x1, y1)[channel]) * dx;
        top * (1. - dy) + bottom * dy
    };
    std::array::from_fn(|i| {
        if i == 3 {
            if blend(3) > 0. { 255 } else { 0 }
        } else {
            blend(i).round() as u8
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::Rgba;

    #[test]
    fn shared_cache_coalesces_simultaneous_requests_and_keeps_live_images() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let path = tmp.path().join("texture.png");
        RgbaImage::from_pixel(4, 4, Rgba([0, 40, 0, 255])).save(&path)?;
        let sources = [Texture {
            label: path.display().to_string(),
            source: Source::External(path),
        }];
        let cache = TextureCache::new(&sources);
        let cancelled = AtomicBool::new(false);
        let (first, second) = std::thread::scope(|scope| {
            let other = scope.spawn(|| cache.get(0, &cancelled));
            let first = cache.get(0, &cancelled)?;
            Ok::<_, anyhow::Error>((first, other.join().unwrap()?))
        })?;
        assert!(Arc::ptr_eq(&first, &second));
        cache.trim_idle();
        assert_eq!(cache.state.lock().unwrap().bytes, 4 * 4 * 4);
        drop((first, second));
        cache.trim_idle();
        assert_eq!(cache.state.lock().unwrap().bytes, 0);
        Ok(())
    }

    #[test]
    fn concurrent_texture_decodes_retry_after_reservations_are_released() -> Result<()> {
        use std::sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        };
        use std::time::Duration;
        const CHILD: &str = "WEBGIS_TEST_TEXTURE_BUDGET_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let result = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", "dom::raster::tests::concurrent_texture_decodes_retry_after_reservations_are_released"])
                .env(CHILD, "1")
                .output()?;
            ensure!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stdout)
            );
            return Ok(());
        }

        memory::configure(6 * memory::MIB);
        let cache = TextureCache::new(&[]);
        let cancelled = AtomicBool::new(false);
        let barrier = Barrier::new(2);
        let attempts = AtomicUsize::new(0);
        let load = |id| {
            // Each thread attempts its first reservation before either can
            // release scratch. This forces a retry under the shared budget.
            let first_attempt = AtomicBool::new(true);
            let image = cache.get_with(id, &cancelled, || {
                attempts.fetch_add(1, Ordering::Relaxed);
                let scratch = memory::reserve(4 * memory::MIB, "texture temporary buffer");
                if first_attempt.swap(false, Ordering::Relaxed) {
                    barrier.wait();
                }
                let _scratch = scratch?;
                let reservation = memory::reserve(2 * memory::MIB, "decoded texture");
                Ok(DecodedTexture {
                    pixels: RgbaImage::new(1, 1),
                    decode_bytes: 6 * memory::MIB,
                    _reservation: reservation?,
                })
            })?;
            std::thread::sleep(Duration::from_millis(30));
            drop(image);
            Ok::<_, anyhow::Error>(())
        };
        std::thread::scope(|scope| {
            let other = scope.spawn(|| load(1));
            load(0)?;
            other.join().unwrap()
        })?;
        assert!(attempts.load(Ordering::Relaxed) >= 3);
        cache.trim_idle();
        assert_eq!(memory::available_budget(), 6 * memory::MIB);
        Ok(())
    }
}
