use super::{Progress, Summary, check_cancelled, gdal::NODATA};
use anyhow::{Context, Result, anyhow, ensure};
use image::{ImageFormat, ImageReader, Limits, RgbaImage};
use rstar::{
    AABB, RTree,
    primitives::{GeomWithData, Rectangle},
};
use std::{
    collections::HashMap,
    fs,
    io::{Cursor, Read, Seek, SeekFrom},
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, atomic::AtomicBool},
};
use webgis_core::model_clip::osgb::{self, RasterImage};

const CACHE_BYTES: usize = 128 * 1024 * 1024;
const MAX_ENCODED_BYTES: u64 = 128 * 1024 * 1024;

pub struct Mesh {
    pub bounds: [f64; 6],
    pub triangles: Vec<Triangle>,
    pub images: Vec<Texture>,
}

pub struct Triangle {
    xyz: [[f64; 3]; 3],
    uv: Option<[[f64; 2]; 3]>,
    texture: Option<usize>,
}

pub struct Texture {
    label: String,
    source: Source,
}

enum Source {
    Embedded {
        file: Arc<PathBuf>,
        range: Range<usize>,
    },
    External(PathBuf),
}

impl Mesh {
    pub fn load(
        root: &Path,
        files: &[PathBuf],
        origin: [f64; 3],
        cancelled: &AtomicBool,
        progress: &mut impl FnMut(Progress),
    ) -> Result<Self> {
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
        };
        let mut external = HashMap::new();
        for (i, file) in files.iter().enumerate() {
            check_cancelled(cancelled)?;
            progress(Progress {
                stage: format!("读取模型 {}", file.display()),
                fraction: Some(i as f32 / files.len() as f32),
            });
            let context = || file.display().to_string();
            let bytes = fs::read(file).with_context(context)?;
            let scene = osgb::raster_mesh(&bytes, cancelled)
                .map_err(|err| anyhow!(err))
                .with_context(context)?;
            if files.len() == 1 && scene.triangles.is_empty() && !scene.pages.is_empty() {
                anyhow::bail!("{} 仅包含外部分页，请选择模型目录", file.display());
            }
            let mut ids = HashMap::new();
            let encoded_file = Arc::new(file.clone());
            for (id, source) in scene.images {
                check_cancelled(cancelled)?;
                let source = match source {
                    RasterImage::Embedded { name, bytes: range } => {
                        ensure!(
                            range.len() as u64 <= MAX_ENCODED_BYTES,
                            "{} 的内嵌纹理 {name} 超出大小限制",
                            file.display()
                        );
                        Texture {
                            label: format!("{} 的内嵌纹理 {name}", file.display()),
                            source: Source::Embedded {
                                file: encoded_file.clone(),
                                range: range.clone(),
                            },
                        }
                    }
                    RasterImage::External(name) => {
                        let path = texture_path(root, file, &name).with_context(context)?;
                        if let Some(&index) = external.get(&path) {
                            ids.insert(id, index);
                            continue;
                        }
                        external.insert(path.clone(), mesh.images.len());
                        Texture {
                            label: format!("{} 引用的 {}", file.display(), path.display()),
                            source: Source::External(path),
                        }
                    }
                };
                match &source.source {
                    Source::Embedded { range, .. } => source
                        .decode_bytes(&bytes[range.clone()])
                        .with_context(|| source.label.clone())?,
                    Source::External(_) => source.decode()?,
                };
                ids.insert(id, mesh.images.len());
                mesh.images.push(source);
            }
            for (index, tri) in scene.triangles.into_iter().enumerate() {
                if index % 4096 == 0 {
                    check_cancelled(cancelled)?;
                }
                let xyz = tri.xyz.map(|v| std::array::from_fn(|i| v[i] + origin[i]));
                for v in xyz {
                    ensure!(
                        v.iter().all(|n| n.is_finite()) && (v[2] as f32).is_finite(),
                        "{} 的坐标超出范围",
                        file.display()
                    );
                    for (i, value) in v.into_iter().enumerate() {
                        mesh.bounds[i] = mesh.bounds[i].min(value);
                        mesh.bounds[i + 3] = mesh.bounds[i + 3].max(value);
                    }
                }
                let texture = tri
                    .texture
                    .map(|id| ids.get(&id).copied().context("找不到纹理对象"))
                    .transpose()
                    .with_context(context)?;
                mesh.triangles.push(Triangle {
                    xyz,
                    uv: tri.uv,
                    texture,
                });
            }
        }
        ensure!(!mesh.triangles.is_empty(), "所选模型中没有三角形");
        Ok(mesh)
    }
}

fn texture_path(root: &Path, file: &Path, name: &str) -> Result<PathBuf> {
    let reference = Path::new(name);
    ensure!(
        !reference.is_absolute() && !name.contains(':'),
        "不支持绝对纹理路径 {name}"
    );
    let path = file
        .parent()
        .context("模型缺少父目录")?
        .join(reference)
        .canonicalize()
        .with_context(|| format!("找不到外部纹理 {name}"))?;
    ensure!(
        path.starts_with(root) && path.is_file(),
        "外部纹理不在模型目录内：{name}"
    );
    Ok(path)
}

impl Texture {
    fn decode(&self) -> Result<RgbaImage> {
        self.decode_inner().with_context(|| self.label.clone())
    }

    fn decode_inner(&self) -> Result<RgbaImage> {
        let bytes = match &self.source {
            Source::Embedded { file, range } => {
                let mut file = fs::File::open(file.as_path())?;
                file.seek(SeekFrom::Start(range.start as u64))?;
                let mut data = vec![0; range.len()];
                file.read_exact(&mut data)?;
                data
            }
            Source::External(path) => {
                let mut data = Vec::new();
                fs::File::open(path)?
                    .take(MAX_ENCODED_BYTES + 1)
                    .read_to_end(&mut data)?;
                ensure!(
                    data.len() as u64 <= MAX_ENCODED_BYTES,
                    "外部纹理超出大小限制"
                );
                data
            }
        };
        self.decode_bytes(&bytes)
    }

    fn decode_bytes(&self, bytes: &[u8]) -> Result<RgbaImage> {
        let format = image::guess_format(bytes)?;
        ensure!(
            matches!(format, ImageFormat::Png | ImageFormat::Jpeg),
            "不支持的纹理格式 {format:?}，需要 JPEG 或 PNG"
        );
        let mut limits = Limits::default();
        limits.max_image_width = Some(8192);
        limits.max_image_height = Some(8192);
        limits.max_alloc = Some((CACHE_BYTES * 2) as u64);
        let mut reader = ImageReader::with_format(Cursor::new(bytes), format);
        reader.limits(limits);
        let decoder = reader.into_decoder()?;
        use image::ImageDecoder;
        let (w, h) = decoder.dimensions();
        ensure!(
            u64::from(w) * u64::from(h) * 4 <= CACHE_BYTES as u64,
            "解码纹理超出缓存容量"
        );
        Ok(image::DynamicImage::from_decoder(decoder)?.into_rgba8())
    }
}

pub struct TextureCache<'a> {
    sources: &'a [Texture],
    entries: HashMap<usize, (Arc<RgbaImage>, u64)>,
    clock: u64,
    bytes: usize,
}

impl<'a> TextureCache<'a> {
    pub fn new(sources: &'a [Texture]) -> Self {
        Self {
            sources,
            entries: HashMap::new(),
            clock: 0,
            bytes: 0,
        }
    }

    fn get(&mut self, id: usize) -> Result<Arc<RgbaImage>> {
        self.clock += 1;
        if let Some((image, used)) = self.entries.get_mut(&id) {
            *used = self.clock;
            return Ok(Arc::clone(image));
        }
        let image = Arc::new(self.sources[id].decode()?);
        let bytes = image.as_raw().len();
        while self.bytes + bytes > CACHE_BYTES {
            let id = *self
                .entries
                .iter()
                .min_by_key(|(_, (_, used))| *used)
                .unwrap()
                .0;
            self.bytes -= self.entries.remove(&id).unwrap().0.as_raw().len();
        }
        self.bytes += bytes;
        self.entries.insert(id, (Arc::clone(&image), self.clock));
        Ok(image)
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
    textures: &mut TextureCache<'_>,
    summary: &Summary,
    [x, y, w, h]: [usize; 4],
    cancelled: &AtomicBool,
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
    let mut hits: Vec<_> = index
        .locate_in_envelope_intersecting(&bounds)
        .map(|item| item.data)
        .collect();
    hits.sort_unstable();
    for id in hits {
        check_cancelled(cancelled)?;
        let tri = &triangles[id];
        let [a, b, c] = tri.xyz;
        let (bx, by, cx, cy) = (b[0] - a[0], b[1] - a[1], c[0] - a[0], c[1] - a[1]);
        let det = bx * cy - by * cx;
        if det == 0. || !det.is_finite() {
            continue;
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
                            image = Some(textures.get(id)?);
                        }
                        sample(
                            image.as_ref().unwrap(),
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
    }
    ensure!(
        !block.heights.iter().any(|&z| z as f32 == NODATA),
        "模型高程与 DSM NoData 值 {NODATA} 冲突"
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
