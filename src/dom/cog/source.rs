use super::super::{
    geotiff::Affine,
    tiff_mask::MAX_CHUNK,
    tiff_source::{Cache, SourceMask, WhiteAlpha, ifd_pointers},
};
use super::writer::Field;
use anyhow::{Context, Result, ensure};
use std::{fs::File, io::BufReader, path::Path};
use tiff::{
    decoder::{Decoder, IfdDecoder, TiffCodingUnit},
    tags::Tag,
};

pub(super) struct Source {
    pub size: [u32; 2],
    pub bits: u16,
    pub samples: u16,
    pub photo: u16,
    pub alpha: Option<u16>,
    pub nodata: Option<String>,
    pub geo: Vec<Field>,
    pub mask: Option<SourceMask>,
    decoder: Decoder<BufReader<File>>,
    chunk: [u32; 2],
    planar: bool,
    white_alpha: Option<WhiteAlpha>,
    cache: Cache,
}

impl Source {
    pub fn open(path: &Path) -> Result<Self> {
        let mut limits = tiff::decoder::Limits::default();
        limits.decoding_buffer_size = MAX_CHUNK;
        limits.intermediate_buffer_size = MAX_CHUNK;
        limits.ifd_value_size = MAX_CHUNK;
        let mut decoder = Decoder::new(BufReader::new(File::open(path)?))?.with_limits(limits);
        let size: [u32; 2] = decoder.dimensions()?.into();
        let chunk: [u32; 2] = decoder.chunk_dimensions().into();
        ensure!(
            size.into_iter().chain(chunk).all(|n| n > 0),
            "TIFF 图像或块尺寸无效"
        );
        let tiled = decoder.get_chunk_type() == tiff::decoder::ChunkType::Tile;
        let mut tags = decoder.image_ifd();
        let photo = tags.get_tag_unsigned::<u16>(Tag::PhotometricInterpretation)?;
        let samples = tags.find_tag_unsigned(Tag::SamplesPerPixel)?.unwrap_or(1);
        let bits_per_sample = tags
            .find_tag_unsigned_vec::<u16>(Tag::BitsPerSample)?
            .unwrap_or_default();
        let bits = *bits_per_sample.first().context("缺少 TIFF BitsPerSample")?;
        let format = tags
            .find_tag_unsigned_vec::<u16>(Tag::SampleFormat)?
            .unwrap_or_else(|| vec![1]);
        let colors = match (bits, photo, samples) {
            (8, 0 | 1, 1 | 2) => 1,
            (8, 2, 3 | 4) => 3,
            (32, 1, 1) => 1,
            _ => anyhow::bail!("仅支持 8 位灰度、灰度 Alpha、RGB、RGBA 或单波段 Float32 GeoTIFF"),
        };
        ensure!(
            bits_per_sample.iter().all(|&value| value == bits)
                && !format.is_empty()
                && format
                    .iter()
                    .all(|&value| value == if bits == 32 { 3 } else { 1 }),
            "不支持的 TIFF 像元类型"
        );
        let decoded_width = if tiled { chunk[0] } else { size[0] };
        let decoded_rows = if tiled {
            chunk[1]
        } else {
            chunk[1].min(size[1])
        };
        let bytes_per_row = u64::from(decoded_width) * u64::from(bits / 8 * samples);
        ensure!(
            bytes_per_row <= MAX_CHUNK as u64 / u64::from(decoded_rows),
            "单个 TIFF 块超过 64 MiB，请使用较小的条带或分块"
        );
        let extra = tags
            .find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)?
            .unwrap_or_default();
        let alpha = match (samples - colors, extra.as_slice()) {
            (0, []) => None,
            (1, [kind @ (1 | 2)]) => Some(*kind),
            _ => anyhow::bail!("额外的 TIFF 波段必须标记为 Alpha"),
        };
        let planar = match tags
            .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)?
            .unwrap_or(1)
        {
            1 => false,
            2 => true,
            _ => anyhow::bail!("无效的 TIFF 波段排列"),
        };
        ensure!(
            tags.find_tag_unsigned::<u16>(Tag::Orientation)?
                .unwrap_or(1)
                == 1,
            "仅支持左上角为原点的 TIFF 像元排列"
        );
        let nodata = ascii(&mut tags, Tag::GdalNodata)?;
        if let Some(text) = &nodata {
            text.trim()
                .parse::<f64>()
                .context("无效的 TIFF NoData 值")?;
        }
        let geo = geotags(&mut tags)?;
        let sub_ifds = tags
            .find_tag(Tag::SubIfd)?
            .map(ifd_pointers)
            .transpose()?
            .unwrap_or_default();
        let chunks = u64::from(size[0].div_ceil(chunk[0])) * u64::from(size[1].div_ceil(chunk[1]));
        ensure!(
            chunks <= u64::from(u32::MAX) / if planar { u64::from(samples) } else { 1 },
            "TIFF 块数超出解码范围"
        );
        let white_alpha = if photo == 0 && samples == 2 {
            Some(WhiteAlpha::new(&mut tags, tiled, chunks as usize)?)
        } else {
            None
        };
        let mask = SourceMask::open(path, &mut decoder, size, sub_ifds, true)?;
        Ok(Self {
            size,
            bits,
            samples,
            photo,
            alpha,
            nodata,
            geo,
            mask,
            decoder,
            chunk,
            planar,
            white_alpha,
            cache: Cache::new(),
        })
    }

    pub fn pixel_bytes(&self) -> usize {
        usize::from(self.bits / 8 * self.samples)
    }

    // 返回原始样本，不应用 Alpha、NoData 或掩膜。
    pub fn pixel(&mut self, x: u32, y: u32, out: &mut [u8]) -> Result<()> {
        let [cw, ch] = self.chunk;
        let index = y / ch * self.size[0].div_ceil(cw) + x / cw;
        let width = cw.min(self.size[0] - x / cw * cw);
        let rows = ch.min(self.size[1] - y / ch * ch);
        let pixel_bytes = self.pixel_bytes();
        let plane = usize::try_from(u64::from(width) * u64::from(rows))?;
        let pos = (y % ch) as usize * width as usize + (x % cw) as usize;
        let data = self
            .cache
            .get(index, || {
                let len = plane.checked_mul(pixel_bytes).context("TIFF 块尺寸溢出")?;
                ensure!(
                    len <= MAX_CHUNK,
                    "单个 TIFF 块超过 64 MiB，请使用较小的条带或分块"
                );
                if let Some(white) = &self.white_alpha {
                    return white.read(
                        self.decoder.inner(),
                        index as usize,
                        self.chunk,
                        [width, rows],
                        self.planar,
                    );
                }
                let mut data = vec![0; len];
                if self.planar {
                    self.decoder
                        .read_coding_unit_bytes(TiffCodingUnit(index), &mut data)?;
                } else {
                    self.decoder.read_chunk_bytes(index, &mut data)?;
                }
                // 解码器自动反转单波段 WhiteIsZero，撤销反转以保留存储值。
                if self.photo == 0 {
                    data.iter_mut().for_each(|byte| *byte = !*byte);
                }
                Ok(data)
            })
            .with_context(|| format!("无法读取 TIFF 数据块 {index}"))?;
        if self.bits == 32 {
            let bytes = data[pos * 4..pos * 4 + 4].try_into()?;
            out.copy_from_slice(&u32::from_ne_bytes(bytes).to_le_bytes());
        } else if self.planar {
            for (channel, value) in out.iter_mut().enumerate() {
                *value = data[channel * plane + pos];
            }
        } else {
            out.copy_from_slice(&data[pos * pixel_bytes..(pos + 1) * pixel_bytes]);
        }
        Ok(())
    }
}

fn ascii(tags: &mut IfdDecoder<'_>, tag: Tag) -> Result<Option<String>> {
    Ok(tags
        .find_tag(tag)?
        .map(|value| value.into_string())
        .transpose()?)
}

fn geotags(tags: &mut IfdDecoder<'_>) -> Result<Vec<Field>> {
    let keys = tags
        .get_tag_u16_vec(Tag::GeoKeyDirectoryTag)
        .context("缺少或无法读取坐标系 GeoKey")?;
    ensure!(keys.len() >= 4 && keys[0] == 1, "无效的 GeoKey 目录");
    let end = 4 + usize::from(keys[3]) * 4;
    ensure!(end <= keys.len(), "GeoKey 目录不完整");
    let doubles = tags
        .find_tag(Tag::GeoDoubleParamsTag)?
        .map(|v| v.into_f64_vec())
        .transpose()?;
    let citation = ascii(tags, Tag::GeoAsciiParamsTag)?;
    let entries = &keys[4..end];
    let key = |id| {
        entries
            .chunks_exact(4)
            .find_map(|e| (e[0] == id && e[1] == 0 && e[2] == 1).then_some(e[3]))
    };
    let code = match key(1024) {
        Some(1) => key(3072),
        Some(2 | 3) => key(2048),
        None => key(3072).or_else(|| key(2048)),
        _ => None,
    };
    ensure!(code.is_some_and(|n| n > 0), "GeoKey 缺少内嵌坐标系定义");
    ensure!(
        matches!(key(1025), None | Some(1 | 2)),
        "不支持的 GeoTIFF 像元定位类型"
    );
    for e in entries.chunks_exact(4) {
        let available = match e[1] {
            0 if e[2] == 1 => continue,
            34735 => keys.len(),
            34736 => doubles.as_ref().map_or(0, Vec::len),
            34737 => citation.as_ref().map_or(0, |s| s.len() + 1),
            _ => anyhow::bail!("不支持的 GeoKey 参数标签 {}", e[1]),
        };
        ensure!(
            usize::from(e[2]) + usize::from(e[3]) <= available,
            "GeoKey 参数超出范围"
        );
    }
    let mut geo = vec![Field::shorts(Tag::GeoKeyDirectoryTag, &keys)];
    if let Some(values) = doubles {
        geo.push(Field::doubles(Tag::GeoDoubleParamsTag, &values));
    }
    if let Some(text) = citation {
        geo.push(Field::ascii(Tag::GeoAsciiParamsTag, &text));
    }
    let matrix = tags
        .find_tag(Tag::ModelTransformationTag)?
        .map(|v| v.into_f64_vec())
        .transpose()?;
    let scale = tags
        .find_tag(Tag::ModelPixelScaleTag)?
        .map(|v| v.into_f64_vec())
        .transpose()?;
    let tie = tags
        .find_tag(Tag::ModelTiepointTag)?
        .map(|v| v.into_f64_vec())
        .transpose()?;
    let transform = match (matrix, scale, tie) {
        (Some(m), None, None)
            if m.len() == 16 && m.iter().all(|v| v.is_finite()) && m[12..] == [0., 0., 0., 1.] =>
        {
            geo.push(Field::doubles(Tag::ModelTransformationTag, &m));
            Affine([m[3], m[0], m[1], m[7], m[4], m[5]])
        }
        (None, Some(s), Some(t))
            if s.len() == 3
                && t.len() >= 6
                && t.len() % 6 == 0
                && s.iter().chain(&t).all(|v| v.is_finite()) =>
        {
            geo.push(Field::doubles(Tag::ModelPixelScaleTag, &s));
            geo.push(Field::doubles(Tag::ModelTiepointTag, &t));
            Affine([t[3] - t[0] * s[0], s[0], 0., t[4] + t[1] * s[1], 0., -s[1]])
        }
        _ => anyhow::bail!("缺少有效的地理定位：需要像元比例和控制点，或仿射变换矩阵"),
    };
    transform.inverse()?;
    Ok(geo)
}
