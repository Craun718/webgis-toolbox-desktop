use super::{
    geotiff::{Affine, Crs, geometry},
    tiff_mask::{MAX_CHUNK, Mask, RawBlocks},
};
use anyhow::{Context, Result, ensure};
use std::{collections::VecDeque, fs::File, io::BufReader, path::Path};
use tiff::{
    decoder::{Decoder, TiffCodingUnit},
    tags::{IfdPointer, Tag},
};

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

struct SourceMask {
    tags: Mask,
    reader: BufReader<File>,
    cache: Cache,
}

struct WhiteAlpha {
    raw: RawBlocks,
    predictor: u16,
    tiled: bool,
    chunks_per_plane: usize,
}

impl WhiteAlpha {
    fn read(
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
            .context("TIFF 块尺寸溢出")?;
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
        ensure!(cw > 0 && ch > 0, "TIFF 块尺寸无效");
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
            _ => anyhow::bail!("仅支持 8 位灰度、灰度 Alpha、RGB 或 RGBA GeoTIFF"),
        };
        let bits = tags.get_tag_u16_vec(Tag::BitsPerSample)?;
        let format = tags.find_tag_unsigned_vec::<u16>(Tag::SampleFormat)?;
        ensure!(
            bits.iter().all(|&bits| bits == 8)
                && format.is_none_or(|values| values.iter().all(|&value| value == 1)),
            "仅支持 8 位无符号 GeoTIFF 像元"
        );
        let extra = tags
            .find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)?
            .unwrap_or_default();
        let alpha = match (samples - colors, extra.as_slice()) {
            (0, []) => 0,
            (1, [kind @ (1 | 2)]) => *kind,
            _ => anyhow::bail!("额外的 TIFF 波段必须标记为 Alpha"),
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
            "不支持的 TIFF 压缩 {compression}"
        );
        ensure!(
            photo != 6 || compression == 7,
            "仅支持 JPEG 压缩的 YCbCr 影像"
        );
        let nodata = tags
            .find_tag(Tag::GdalNodata)?
            .map(|value| value.into_string())
            .transpose()?
            .map(|text| text.trim().trim_end_matches('\0').parse::<f64>())
            .transpose()
            .context("无效的 TIFF NoData 值")?
            .filter(|n| (0.0..=255.0).contains(n) && n.fract() == 0.)
            .map(|n| n as u8);
        let sub_ifds = tags
            .find_tag(Tag::SubIfd)?
            .map(ifd_pointers)
            .transpose()?
            .unwrap_or_default();
        // 通用解码器只能反转单波段 WhiteIsZero。双波段保留 Alpha，单独反转灰度。
        let white_alpha = if photo == 0 && samples == 2 {
            let predictor = tags.find_tag_unsigned(Tag::Predictor)?.unwrap_or(1);
            ensure!(matches!(predictor, 1 | 2), "不支持的灰度 Alpha TIFF 预测器");
            Some(WhiteAlpha {
                raw: RawBlocks::new(&mut tags, tiled)?,
                predictor,
                tiled,
                chunks_per_plane: usize::try_from(expected_chunks)?,
            })
        } else {
            None
        };
        let mask = find_mask(&mut decoder, size, sub_ifds)?
            .map(|tags| -> Result<_> {
                Ok(SourceMask {
                    tags,
                    reader: BufReader::new(File::open(path)?),
                    cache: Cache::new(),
                })
            })
            .transpose()?;
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
            let len = plane.checked_mul(self.samples).context("TIFF 块尺寸溢出")?;
            ensure!(
                len <= MAX_CHUNK,
                "单个 TIFF 块超过 64 MiB，请使用较小的条带或分块"
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

fn ifd_pointers(value: tiff::decoder::ifd::Value) -> Result<Vec<IfdPointer>> {
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
) -> Result<Option<Mask>> {
    for pointer in sub_ifds {
        let dir = decoder.read_directory(pointer)?;
        let mut tags = decoder.read_directory_tags(&dir);
        let flags = tags
            .find_tag_unsigned::<u32>(Tag::NewSubfileType)?
            .unwrap_or(0);
        if flags & 4 != 0
            && tags.get_tag_u32(Tag::ImageWidth)? == size[0]
            && tags.get_tag_u32(Tag::ImageLength)? == size[1]
        {
            return Ok(Some(Mask::read_tags(&mut tags)?));
        }
    }
    while decoder.more_images() {
        decoder.next_image()?;
        let flags = decoder
            .find_tag_unsigned::<u32>(Tag::NewSubfileType)?
            .unwrap_or(0);
        if flags == 0 {
            break;
        }
        if flags & 4 != 0 && decoder.dimensions()? == (size[0], size[1]) {
            let mask = Mask::read_tags(&mut decoder.image_ifd())?;
            decoder.seek_to_image(0)?;
            return Ok(Some(mask));
        }
    }
    decoder.seek_to_image(0)?;
    Ok(None)
}

impl SourceMask {
    fn valid(&mut self, x: u32, y: u32) -> Result<bool> {
        let [cw, ch] = self.tags.chunk;
        let cols = self.tags.size[0].div_ceil(cw);
        let index = y / ch * cols + x / cw;
        let bits = self.cache.get(index, || {
            self.tags.read_chunk(&mut self.reader, index as usize)
        })?;
        Ok(self.tags.valid(bits, (x % cw) as usize, (y % ch) as usize))
    }
}

struct Cache {
    entries: VecDeque<(u32, Vec<u8>)>,
    bytes: usize,
}

impl Cache {
    fn new() -> Self {
        Self {
            entries: VecDeque::new(),
            bytes: 0,
        }
    }

    fn get(&mut self, index: u32, load: impl FnOnce() -> Result<Vec<u8>>) -> Result<&[u8]> {
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
