use super::{
    check_cancelled,
    geotiff::{Affine, Crs, geometry, write_geotags},
    tiff_mask::{Mask, bit},
};
use anyhow::{Context, Result, bail, ensure};
use flate2::{Compression, write::ZlibEncoder};
use std::{
    fs::File,
    io::{BufReader, Write},
    path::Path,
    sync::atomic::AtomicBool,
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    encoder::{DirectoryEncoder, TiffEncoder, TiffKindBig},
    tags::Tag,
};
use webgis_core::{message, osgb_memory as memory};

pub(super) const TILE: usize = 256;
pub(super) const NODATA: f32 = -9999.;
const PIXELS: usize = TILE * TILE;
const MASK_STRIDE: usize = TILE / 8;
const MASK_BYTES: usize = TILE * MASK_STRIDE;

#[derive(Clone, Copy)]
pub(super) struct RasterSpec {
    pub size: [usize; 2],
    pub transform: Affine,
    pub crs: Option<Crs>,
}

impl RasterSpec {
    fn overview(self, factor: usize) -> Self {
        let [x, dx, rx, y, ry, dy] = self.transform.0;
        let scale = factor as f64;
        Self {
            size: self.size.map(|n| n.div_ceil(factor)),
            transform: Affine([x, dx * scale, rx * scale, y, ry * scale, dy * scale]),
            ..self
        }
    }

    fn tile_grid(self) -> [usize; 2] {
        self.size.map(|n| n.div_ceil(TILE))
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum RasterKind {
    Dsm,
    Dom,
    Mask,
}

pub(super) type BigTiff = TiffEncoder<File, TiffKindBig>;
type Directory<'a> = DirectoryEncoder<'a, File, TiffKindBig>;

#[derive(Default)]
struct Blocks {
    offsets: Vec<u64>,
    counts: Vec<u64>,
}

impl Blocks {
    fn write(&mut self, dir: &mut Directory<'_>, raw: &[u8]) -> Result<()> {
        let mut compressor = ZlibEncoder::new(Vec::new(), Compression::fast());
        compressor.write_all(raw)?;
        let bytes = compressor.finish()?;
        self.offsets.push(dir.write_data(bytes.as_slice())?);
        self.counts.push(bytes.len() as u64);
        Ok(())
    }

    fn tags(
        &self,
        dir: &mut Directory<'_>,
        spec: RasterSpec,
        kind: RasterKind,
        reduced: bool,
    ) -> Result<()> {
        let [cols, rows] = spec.tile_grid();
        ensure!(
            self.offsets.len() == cols * rows,
            message!(written_tiff_block_count_does_not_match_raster_dimensions)
        );
        let (bits, samples, photo) = match kind {
            RasterKind::Dsm => (32_u16, 1_u16, 1_u16),
            RasterKind::Dom => (8, 3, 2),
            RasterKind::Mask => (1, 1, 4),
        };
        dir.write_tag(
            Tag::NewSubfileType,
            u32::from(reduced) | if kind == RasterKind::Mask { 4 } else { 0 },
        )?;
        dir.write_tag(Tag::ImageWidth, spec.size[0] as u32)?;
        dir.write_tag(Tag::ImageLength, spec.size[1] as u32)?;
        dir.write_tag(Tag::BitsPerSample, &[bits; 3][..usize::from(samples)])?;
        dir.write_tag(Tag::Compression, 8_u16)?;
        dir.write_tag(Tag::PhotometricInterpretation, photo)?;
        dir.write_tag(Tag::SamplesPerPixel, samples)?;
        dir.write_tag(Tag::PlanarConfiguration, 1_u16)?;
        dir.write_tag(Tag::TileWidth, TILE as u32)?;
        dir.write_tag(Tag::TileLength, TILE as u32)?;
        dir.write_tag(Tag::TileOffsets, self.offsets.as_slice())?;
        dir.write_tag(Tag::TileByteCounts, self.counts.as_slice())?;
        if kind == RasterKind::Dsm {
            dir.write_tag(Tag::SampleFormat, 3_u16)?;
            dir.write_tag(Tag::GdalNodata, "-9999")?;
        }
        if kind != RasterKind::Mask {
            write_geotags(dir, spec.crs, spec.transform, false)?;
        }
        Ok(())
    }
}

pub(super) struct LevelWriter<'a> {
    directory: Directory<'a>,
    kind: RasterKind,
    pixels: Blocks,
    mask: Blocks,
    buffer: Vec<u8>,
}

impl<'a> LevelWriter<'a> {
    pub fn new(file: &'a mut BigTiff, kind: RasterKind) -> Result<Self> {
        Ok(Self {
            directory: file.image_directory()?,
            kind,
            pixels: Blocks::default(),
            mask: Blocks::default(),
            buffer: Vec::new(),
        })
    }

    pub fn height(&mut self, [width, height]: [usize; 2], values: &[f32]) -> Result<()> {
        self.buffer.clear();
        let mut padded = [NODATA; TILE];
        for y in 0..TILE {
            if y < height {
                padded[..width].copy_from_slice(&values[y * width..(y + 1) * width]);
            } else if y == height {
                padded.fill(NODATA);
            }
            for value in padded {
                self.buffer.extend_from_slice(&value.to_ne_bytes());
            }
        }
        self.pixels.write(&mut self.directory, &self.buffer)
    }

    pub fn color(&mut self, [width, height]: [usize; 2], values: &[Vec<u8>; 4]) -> Result<()> {
        self.buffer.clear();
        self.buffer.resize(PIXELS * 3, 0);
        let mut mask = vec![0; MASK_BYTES];
        for y in 0..height {
            for x in 0..width {
                let input = y * width + x;
                if values[3][input] != 0 {
                    set_bit(&mut mask, x, y);
                    let offset = (y * TILE + x) * 3;
                    for (channel, samples) in values[..3].iter().enumerate() {
                        self.buffer[offset + channel] = samples[input];
                    }
                }
            }
        }
        self.rgb(&mask)
    }

    fn rgb(&mut self, mask: &[u8]) -> Result<()> {
        self.pixels.write(&mut self.directory, &self.buffer)?;
        self.mask.write(&mut self.directory, mask)
    }

    pub fn finish(mut self, spec: RasterSpec, reduced: bool) -> Result<Option<MaskDirectory>> {
        self.pixels
            .tags(&mut self.directory, spec, self.kind, reduced)?;
        self.directory.finish()?;
        Ok((self.kind == RasterKind::Dom).then_some(MaskDirectory(self.mask, spec, reduced)))
    }
}

pub(super) struct MaskDirectory(Blocks, RasterSpec, bool);

impl MaskDirectory {
    pub fn finish(self, file: &mut BigTiff) -> Result<()> {
        let mut dir = file.image_directory()?;
        self.0.tags(&mut dir, self.1, RasterKind::Mask, self.2)?;
        dir.finish()?;
        Ok(())
    }
}

fn set_bit(mask: &mut [u8], x: usize, y: usize) {
    mask[y * MASK_STRIDE + x / 8] |= 0x80 >> (x % 8);
}

fn decoder(path: &Path) -> Result<Decoder<BufReader<File>>> {
    Ok(Decoder::new(BufReader::new(File::open(path)?))?)
}

pub(super) fn wait_for_memory(cancelled: &AtomicBool, paths: &[&Path]) -> Result<()> {
    #[cfg(target_os = "linux")]
    if memory::pressure() {
        // Completed TIFF blocks can remain cgroup-charged as file cache.
        // Sync before advising the kernel to release those pages.
        for path in paths {
            if let Ok(file) = File::open(path) {
                file.sync_data()?;
                let _ = rustix::fs::fadvise(file, 0, None, rustix::fs::Advice::DontNeed);
            }
        }
    }
    memory::wait(cancelled).map_err(anyhow::Error::msg)
}

pub(super) fn overviews(
    file: &mut BigTiff,
    path: &Path,
    base: RasterSpec,
    kind: RasterKind,
    levels: &[usize],
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut original = decoder(path)?;
    let mask = if kind == RasterKind::Dom {
        original.next_image()?;
        let mask = Mask::read_tags(&mut original.image_ifd())?;
        original.seek_to_image(0)?;
        Some((mask, BufReader::new(File::open(path)?)))
    } else {
        None
    };
    let mut source = OverviewSource {
        spec: base,
        original,
        mask,
        cancelled,
    };
    for &factor in levels {
        let spec = base.overview(factor);
        let mut level = LevelWriter::new(file, kind)?;
        let [cols, rows] = spec.tile_grid();
        for y in 0..rows {
            for x in 0..cols {
                wait_for_memory(cancelled, &[path])?;
                let aggregate = source.aggregate([x, y], factor)?;
                if kind == RasterKind::Dsm {
                    let data: Vec<_> = aggregate
                        .iter()
                        .map(|pixel| {
                            if pixel.count == 0 {
                                NODATA
                            } else {
                                (pixel.sum[0] / f64::from(pixel.count)) as f32
                            }
                        })
                        .collect();
                    level.height([TILE, TILE], &data)?;
                } else {
                    level.buffer.clear();
                    level.buffer.resize(PIXELS * 3, 0);
                    let mut bits = vec![0; MASK_BYTES];
                    for (index, pixel) in aggregate.into_iter().enumerate() {
                        if pixel.count != 0 {
                            set_bit(&mut bits, index % TILE, index / TILE);
                            for channel in 0..3 {
                                level.buffer[index * 3 + channel] =
                                    (pixel.sum[channel] / f64::from(pixel.count)).round() as u8;
                            }
                        }
                    }
                    level.rgb(&bits)?;
                }
            }
        }
        if let Some(mask) = level.finish(spec, true)? {
            mask.finish(file)?;
        }
    }
    Ok(())
}

#[derive(Clone, Default)]
struct Average {
    sum: [f64; 3],
    count: u16,
}

struct OverviewSource<'a> {
    spec: RasterSpec,
    original: Decoder<BufReader<File>>,
    mask: Option<(Mask, BufReader<File>)>,
    cancelled: &'a AtomicBool,
}

impl OverviewSource<'_> {
    fn aggregate(&mut self, [x, y]: [usize; 2], factor: usize) -> Result<Vec<Average>> {
        let [cols, rows] = self.spec.tile_grid();
        let mut result = vec![Average::default(); PIXELS];
        // 每个输出块对应原图中 factor × factor 个块，直接汇总全分辨率有效像元。
        for tile_y in y * factor..((y + 1) * factor).min(rows) {
            for tile_x in x * factor..((x + 1) * factor).min(cols) {
                check_cancelled(self.cancelled)?;
                let index = tile_y * cols + tile_x;
                let chunk = self.original.read_chunk(u32::try_from(index)?)?;
                let width = TILE.min(self.spec.size[0] - tile_x * TILE);
                let row = (tile_y - y * factor) * TILE;
                let col = (tile_x - x * factor) * TILE;
                let destination =
                    |i: usize| ((row + i / width) / factor) * TILE + (col + i % width) / factor;
                match chunk {
                    DecodingResult::F32(data) => {
                        for (i, value) in data.into_iter().enumerate().filter(|(_, v)| *v != NODATA)
                        {
                            let pixel = &mut result[destination(i)];
                            pixel.sum[0] += f64::from(value);
                            pixel.count += 1;
                        }
                    }
                    DecodingResult::U8(data) => {
                        let (mask, reader) = self
                            .mask
                            .as_mut()
                            .context(message!(dom_is_missing_an_internal_mask))?;
                        let bits = mask.read_chunk(reader, index)?;
                        for (i, rgb) in data.chunks_exact(3).enumerate() {
                            if bit(&bits, MASK_STRIDE, i % width, i / width) {
                                let pixel = &mut result[destination(i)];
                                for (sum, value) in pixel.sum.iter_mut().zip(rgb) {
                                    *sum += f64::from(*value);
                                }
                                pixel.count += 1;
                            }
                        }
                    }
                    _ => bail!(message!(unexpected_tiff_block_pixel_type)),
                }
            }
        }
        Ok(result)
    }
}

pub(super) fn verify(
    path: &Path,
    spec: RasterSpec,
    kind: RasterKind,
    levels: &[usize],
    valid_pixels: u64,
    cancelled: &AtomicBool,
) -> Result<()> {
    let mut ds = decoder(path)?;
    let mut reader = BufReader::new(File::open(path)?);
    for factor in std::iter::once(1).chain(levels.iter().copied()) {
        let spec = spec.overview(factor);
        let reduced = u32::from(factor != 1);
        let [w, h] = spec.size.map(|n| n as u32);
        ensure!(
            ds.dimensions()? == (w, h) && ds.chunk_dimensions() == (TILE as u32, TILE as u32),
            message!(tiff_dimensions_or_block_size_failed_validation)
        );
        ensure!(
            ds.get_tag_u32(Tag::NewSubfileType)? == reduced
                && ds.get_tag_u32(Tag::Compression)? == 8,
            message!(tiff_overview_flags_or_compression_failed_validation)
        );
        if let Some(expected) = spec.crs {
            let (actual, transform) = geometry(&mut ds.image_ifd())?;
            ensure!(
                actual.code == expected.code && transform == spec.transform,
                message!(tiff_crs_or_georeferencing_failed_validation)
            );
        } else {
            ensure!(
                ds.find_tag(Tag::GeoKeyDirectoryTag)?.is_none(),
                message!(a_local_tiff_must_not_include_a_crs)
            );
            let scale = ds.get_tag_f64_vec(Tag::ModelPixelScaleTag)?;
            let tie = ds.get_tag_f64_vec(Tag::ModelTiepointTag)?;
            let [x, dx, _, y, _, dy] = spec.transform.0;
            ensure!(
                scale == [dx, -dy, 0.] && tie == [0., 0., 0., x, y, 0.],
                message!(local_tiff_georeferencing_failed_validation)
            );
        }
        let color_type = match kind {
            RasterKind::Dsm => tiff::ColorType::Gray(32),
            RasterKind::Dom => tiff::ColorType::RGB(8),
            RasterKind::Mask => unreachable!(),
        };
        ensure!(
            ds.colortype()? == color_type,
            message!(tiff_color_layout_failed_validation)
        );
        let count = ds.tile_count()?;
        let mut valid = 0_u64;
        if kind == RasterKind::Dsm {
            ensure!(
                ds.get_tag_ascii_string(Tag::GdalNodata)? == "-9999",
                message!(dsm_nodata_validation_failed)
            );
        }
        for index in 0..count {
            check_cancelled(cancelled)?;
            wait_for_memory(cancelled, &[path])?;
            match ds.read_chunk(index)? {
                DecodingResult::F32(data) if kind == RasterKind::Dsm => {
                    valid += data.iter().filter(|&&value| value != NODATA).count() as u64;
                }
                DecodingResult::U8(_) if kind == RasterKind::Dom => {}
                _ => bail!(message!(tiff_pixel_type_failed_validation)),
            }
        }
        if kind == RasterKind::Dom {
            ds.next_image()?;
            ensure!(
                ds.get_tag_u32(Tag::NewSubfileType)? == 4 | reduced,
                message!(dom_mask_flags_failed_validation)
            );
            let mask = Mask::read_tags(&mut ds.image_ifd())?;
            ensure!(
                mask.size == [w, h]
                    && mask.chunk == [TILE as u32; 2]
                    && mask.count() == count as usize,
                message!(dom_mask_dimensions_failed_validation)
            );
            for index in 0..mask.count() {
                check_cancelled(cancelled)?;
                wait_for_memory(cancelled, &[path])?;
                valid += mask
                    .read_chunk(&mut reader, index)?
                    .iter()
                    .map(|byte| u64::from(byte.count_ones()))
                    .sum::<u64>();
            }
        }
        ensure!(
            valid > 0 && (factor != 1 || valid == valid_pixels),
            message!(tiff_valid_pixel_count_failed_validation)
        );
        if factor != *levels.last().unwrap_or(&1) {
            ds.next_image()?;
        }
    }
    ensure!(!ds.more_images(), message!(tiff_contains_an_extra_ifd));
    Ok(())
}
