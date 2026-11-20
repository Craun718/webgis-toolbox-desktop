use super::{
    geotiff::{Affine, Crs, geometry},
    tiff_mask::{MAX_CHUNK, Mask, RawBlocks},
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::{HashSet, VecDeque},
    fs::File,
    io::BufReader,
    path::Path,
};
use tiff::{
    decoder::{Decoder, TiffCodingUnit},
    tags::{IfdPointer, Tag},
};
use webgis_core::message;

const CACHE_BYTES: usize = MAX_CHUNK;

pub(super) struct Source {
    pub size: [u32; 2],
    pub crs: Crs,
    pub transform: Affine,
    decoder: Decoder<BufReader<File>>,
    chunk: [u32; 2],
    chunk_cols: u32,
    colors: usize,
    samples: usize,
    photo: u16,
    alpha: u16,
    planar: bool,
    white_alpha: Option<WhiteAlpha>,
    nodata: Option<u8>,
    cache: Cache,
    mask: Option<SourceMask>,
}

pub(super) struct SourceMask {
    tags: Mask,
    reader: BufReader<File>,
    cache: Cache,
}

pub(super) struct WhiteAlpha {
    raw: RawBlocks,
    predictor: u16,
    tiled: bool,
    chunks_per_plane: usize,
}

impl WhiteAlpha {
    pub fn new(
        tags: &mut tiff::decoder::IfdDecoder<'_>,
        tiled: bool,
        chunks_per_plane: usize,
    ) -> Result<Self> {
        let predictor = tags.find_tag_unsigned(Tag::Predictor)?.unwrap_or(1);
        ensure!(
            matches!(predictor, 1 | 2),
            message!(unsupported_tiff_predictor_for_grayscale_with_alpha)
        );
        Ok(Self {
            raw: RawBlocks::new(tags, tiled)?,
            predictor,
            tiled,
            chunks_per_plane,
        })
    }

    pub fn read(
        &self,
        reader: &mut BufReader<File>,
        index: usize,
        chunk: [u32; 2],
        crop: [u32; 2],
        planar: bool,
    ) -> Result<Vec<u8>> {
        let [width, height] = if self.tiled { chunk } else { crop };
        let planes = if planar { 2 } else { 1 };
        let samples = 2 / planes;
        let stride = width as usize * samples;
        let size = stride
            .checked_mul(height as usize)
            .context(message!(tiff_block_size_overflow))?;
        let mut result = Vec::new();
        for plane in 0..planes {
            let mut bytes = self
                .raw
                .read(reader, index + plane * self.chunks_per_plane, size)?;
            if self.predictor == 2 {
                for row in bytes.chunks_exact_mut(stride) {
                    for i in samples..row.len() {
                        row[i] = row[i].wrapping_add(row[i - samples]);
                    }
                }
            }
            for row in bytes.chunks_exact(stride).take(crop[1] as usize) {
                result.extend_from_slice(&row[..crop[0] as usize * samples]);
            }
        }
        Ok(result)
    }
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let reader = BufReader::new(File::open(path)?);
        let mut limits = tiff::decoder::Limits::default();
        limits.decoding_buffer_size = MAX_CHUNK;
        limits.intermediate_buffer_size = MAX_CHUNK;
        limits.ifd_value_size = MAX_CHUNK;
        let mut decoder = Decoder::new(reader)?.with_limits(limits);
        let (width, height) = decoder.dimensions()?;
        let size = [width, height];
        let (crs, transform) = geometry(&mut decoder.image_ifd())?;
        let chunk = decoder.chunk_dimensions().into();
        let [cw, ch]: [u32; 2] = chunk;
        ensure!(cw > 0 && ch > 0, message!(invalid_tiff_block_dimensions));
        let expected_chunks = u64::from(width.div_ceil(cw)) * u64::from(height.div_ceil(ch));
        let tiled = decoder.get_chunk_type() == tiff::decoder::ChunkType::Tile;
        let mut tags = decoder.image_ifd();
        let photo: u16 = tags.get_tag_unsigned(Tag::PhotometricInterpretation)?;
        let samples = tags
            .find_tag_unsigned::<usize>(Tag::SamplesPerPixel)?
            .unwrap_or(1);
        let colors = match (photo, samples) {
            (0 | 1, 1 | 2) => 1,
            (2, 3 | 4) | (6, 3) => 3,
            _ => anyhow::bail!(message!(
                only_8_bit_grayscale_grayscale_with_alpha_rgb_or_rgba_geotiff_is_supported
            )),
        };
        let bits = tags.get_tag_u16_vec(Tag::BitsPerSample)?;
        let format = tags.find_tag_unsigned_vec::<u16>(Tag::SampleFormat)?;
        ensure!(
            bits.iter().all(|&bits| bits == 8)
                && format.is_none_or(|values| values.iter().all(|&value| value == 1)),
            message!(only_unsigned_8_bit_geotiff_pixels_are_supported)
        );
        let extra = tags
            .find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)?
            .unwrap_or_default();
        let alpha = match (samples - colors, extra.as_slice()) {
            (0, []) => 0,
            (1, [kind @ (1 | 2)]) => *kind,
            _ => anyhow::bail!(message!(extra_tiff_bands_must_be_marked_as_alpha)),
        };
        let planar = tags
            .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)?
            .unwrap_or(1)
            == 2;
        let compression = tags
            .find_tag_unsigned::<u16>(Tag::Compression)?
            .unwrap_or(1);
        ensure!(
            matches!(compression, 1 | 5 | 7 | 8 | 32946),
            message!(
                unsupported_tiff_compression_compression,
                compression = compression
            )
        );
        ensure!(
            photo != 6 || compression == 7,
            message!(only_jpeg_compressed_ycbcr_images_are_supported)
        );
        let nodata = tags
            .find_tag(Tag::GdalNodata)?
            .map(|value| value.into_string())
            .transpose()?
            .map(|text| text.trim().trim_end_matches('\0').parse::<f64>())
            .transpose()
            .context(message!(invalid_tiff_nodata_value))?
            .filter(|n| (0.0..=255.0).contains(n) && n.fract() == 0.)
            .map(|n| n as u8);
        let sub_ifds = tags
            .find_tag(Tag::SubIfd)?
            .map(ifd_pointers)
            .transpose()?
            .unwrap_or_default();
        // 通用解码器只能反转单波段 WhiteIsZero。双波段保留 Alpha，单独反转灰度。
        let white_alpha = if photo == 0 && samples == 2 {
            Some(WhiteAlpha::new(
                &mut tags,
                tiled,
                usize::try_from(expected_chunks)?,
            )?)
        } else {
            None
        };
        let mask = SourceMask::open(path, &mut decoder, size, sub_ifds, false)?;
        Ok(Self {
            size,
            crs,
            transform,
            decoder,
            chunk,
            chunk_cols: width.div_ceil(cw),
            colors,
            samples,
            photo,
            alpha,
            planar,
            white_alpha,
            nodata,
            cache: Cache::new(),
            mask,
        })
    }

    pub fn pixel(&mut self, x: u32, y: u32) -> Result<[f64; 4]> {
        let [cw, ch] = self.chunk;
        let chunk_index = y / ch * self.chunk_cols + x / cw;
        let width = cw.min(self.size[0] - x / cw * cw);
        let rows = ch.min(self.size[1] - y / ch * ch);
        let plane = usize::try_from(u64::from(width) * u64::from(rows))?;
        let index = (y % ch) as usize * width as usize + (x % cw) as usize;
        let decoder = &mut self.decoder;
        let data = self.cache.get(chunk_index, || {
            let unit = TiffCodingUnit(chunk_index);
            let len = plane
                .checked_mul(self.samples)
                .context(message!(tiff_block_size_overflow))?;
            ensure!(
                len <= MAX_CHUNK,
                message!(a_tiff_block_exceeds_64_mib_use_smaller_strips_or_tiles)
            );
            if let Some(white) = &self.white_alpha {
                return white.read(
                    decoder.inner(),
                    chunk_index as usize,
                    self.chunk,
                    [width, rows],
                    self.planar,
                );
            }
            let mut data = vec![0; len];
            if self.planar {
                decoder.read_coding_unit_bytes(unit, &mut data)?;
            } else {
                decoder.read_chunk_bytes(chunk_index, &mut data)?;
            }
            Ok(data)
        })?;
        let sample = |channel| {
            if self.planar {
                data[channel * plane + index]
            } else {
                data[index * self.samples + channel]
            }
        };
        let rgb = if self.colors == 1 {
            [sample(0); 3]
        } else {
            [sample(0), sample(1), sample(2)]
        };
        let nodata = self.photo != 6
            && self.nodata.is_some_and(|nodata| {
                // WhiteIsZero 的单波段解码结果由 TIFF 解码器反转。
                let value = if self.photo == 0 && self.white_alpha.is_none() {
                    255 - nodata
                } else {
                    nodata
                };
                rgb == [value; 3]
            });
        let opacity = if self.alpha == 0 {
            255
        } else {
            sample(self.colors)
        };
        if nodata || opacity == 0 {
            return Ok([0.; 4]);
        }
        if let Some(mask) = &mut self.mask
            && !mask.valid(x, y)?
        {
            return Ok([0.; 4]);
        }
        let alpha = f64::from(opacity);
        let rgb = match self.photo {
            0 if self.white_alpha.is_some() => {
                let white = if self.alpha == 1 { opacity } else { 255 };
                [f64::from(white.saturating_sub(rgb[0])); 3]
            }
            6 => {
                // JPEG 解码器保留了 YCbCr 分量，TIFF 的 JPEG 系数使用全范围。
                let [y, cb, cr] = rgb.map(f64::from);
                let (cb, cr) = (cb - 128., cr - 128.);
                [
                    y + 1.402 * cr,
                    y - 0.344_136 * cb - 0.714_136 * cr,
                    y + 1.772 * cb,
                ]
                .map(|v| v.clamp(0., 255.))
            }
            _ => rgb.map(f64::from),
        };
        if self.photo == 6
            && self
                .nodata
                .is_some_and(|nodata| rgb.iter().all(|value| value.round() as u8 == nodata))
        {
            return Ok([0.; 4]);
        }
        // 对关联 Alpha，像元已经预乘。其他颜色先预乘再交给采样器。
        let weight = if self.alpha == 1 { 1. } else { alpha / 255. };
        Ok([rgb[0] * weight, rgb[1] * weight, rgb[2] * weight, alpha])
    }
}

pub(super) fn ifd_pointers(value: tiff::decoder::ifd::Value) -> Result<Vec<IfdPointer>> {
    Ok(value
        .into_u64_vec()?
        .into_iter()
        .filter(|&n| n != 0)
        .map(IfdPointer)
        .collect())
}

fn find_mask(
    decoder: &mut Decoder<BufReader<File>>,
    size: [u32; 2],
    sub_ifds: Vec<IfdPointer>,
    single_image: bool,
) -> Result<Option<Mask>> {
    let mut next = decoder.image_ifd().directory().next();
    let mut seen = HashSet::new();
    let mut mask = None;
    let mut mask_pointer = None;
    let mut sub_ifds = sub_ifds.into_iter();
    loop {
        let (pointer, linked) = if let Some(pointer) = sub_ifds.next() {
            (pointer, false)
        } else if let Some(pointer) = next.take() {
            (pointer, true)
        } else {
            break;
        };
        if linked {
            ensure!(
                seen.insert(pointer.0),
                message!(tiff_ifd_chain_contains_a_duplicate_reference)
            );
        }
        let dir = decoder.read_directory(pointer)?;
        if linked {
            next = dir.next();
        }
        let mut tags = decoder.read_directory_tags(&dir);
        let flags = tags
            .find_tag_unsigned::<u32>(Tag::NewSubfileType)?
            .unwrap_or(0);
        if flags & 4 != 0 {
            let same_size = tags.get_tag_u32(Tag::ImageWidth)? == size[0]
                && tags.get_tag_u32(Tag::ImageLength)? == size[1];
            if same_size && flags & 1 == 0 {
                if mask_pointer == Some(pointer.0) {
                    continue;
                }
                ensure!(
                    mask.is_none(),
                    message!(tiff_contains_multiple_full_resolution_masks)
                );
                mask = Some(Mask::read_tags(&mut tags)?);
                mask_pointer = Some(pointer.0);
                if !single_image {
                    break;
                }
            } else {
                ensure!(
                    !single_image || flags & 1 != 0,
                    message!(tiff_image_dimensions_differ_from_its_internal_mask)
                );
            }
        } else if flags & 1 == 0 {
            ensure!(
                !single_image,
                message!(tiff_with_multiple_full_resolution_images_is_unsupported)
            );
            if linked {
                break;
            }
        }
    }
    Ok(mask)
}

impl SourceMask {
    pub fn open(
        path: &Path,
        decoder: &mut Decoder<BufReader<File>>,
        size: [u32; 2],
        sub_ifds: Vec<IfdPointer>,
        single_image: bool,
    ) -> Result<Option<Self>> {
        find_mask(decoder, size, sub_ifds, single_image)?
            .map(|tags| -> Result<_> {
                Ok(Self {
                    tags,
                    reader: BufReader::new(File::open(path)?),
                    cache: Cache::new(),
                })
            })
            .transpose()
    }

    pub fn valid(&mut self, x: u32, y: u32) -> Result<bool> {
        let [cw, ch] = self.tags.chunk;
        let cols = self.tags.size[0].div_ceil(cw);
        let index = y / ch * cols + x / cw;
        let bits = self.cache.get(index, || {
            self.tags.read_chunk(&mut self.reader, index as usize)
        })?;
        Ok(self.tags.valid(bits, (x % cw) as usize, (y % ch) as usize))
    }
}

pub(super) struct Cache {
    entries: VecDeque<(u32, Vec<u8>)>,
    bytes: usize,
}

impl Cache {
    pub fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }

    pub fn get(&mut self, index: u32, load: impl FnOnce() -> Result<Vec<u8>>) -> Result<&[u8]> {
        if let Some(position) = self.entries.iter().position(|(id, _)| *id == index) {
            if position != 0 {
                let entry = self.entries.remove(position).unwrap();
                self.entries.push_front(entry);
            }
        } else {
            let data = load()?;
            while self.bytes + data.len() > CACHE_BYTES || self.entries.len() >= 64 {
                self.bytes -= self.entries.pop_back().unwrap().1.len();
            }
            self.bytes += data.len();
            self.entries.push_front((index, data));
        }
        Ok(&self.entries.front().unwrap().1)
    }
}
