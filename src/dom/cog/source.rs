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
use webgis_core::message;

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
            message!(invalid_tiff_image_or_block_dimensions)
        );
        let tiled = decoder.get_chunk_type() == tiff::decoder::ChunkType::Tile;
        let mut tags = decoder.image_ifd();
        let photo = tags.get_tag_unsigned::<u16>(Tag::PhotometricInterpretation)?;
        let samples = tags.find_tag_unsigned(Tag::SamplesPerPixel)?.unwrap_or(1);
        let bits_per_sample = tags
            .find_tag_unsigned_vec::<u16>(Tag::BitsPerSample)?
            .unwrap_or_default();
        let bits = *bits_per_sample
            .first()
            .context(message!(missing_tiff_bitspersample))?;
        let format = tags
            .find_tag_unsigned_vec::<u16>(Tag::SampleFormat)?
            .unwrap_or_else(|| vec![1]);
        let colors = match (bits, photo, samples) {
            (8, 0 | 1, 1 | 2) => 1,
            (8, 2, 3 | 4) => 3,
            (32, 1, 1) => 1,
            _ => anyhow::bail!(message!(unsupported_cog_pixel_type)),
        };
        ensure!(
            bits_per_sample.iter().all(|&value| value == bits)
                && !format.is_empty()
                && format
                    .iter()
                    .all(|&value| value == if bits == 32 { 3 } else { 1 }),
            message!(unsupported_tiff_pixel_type)
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
            message!(a_tiff_block_exceeds_64_mib_use_smaller_strips_or_tiles)
        );
        let extra = tags
            .find_tag_unsigned_vec::<u16>(Tag::ExtraSamples)?
            .unwrap_or_default();
        let alpha = match (samples - colors, extra.as_slice()) {
            (0, []) => None,
            (1, [kind @ (1 | 2)]) => Some(*kind),
            _ => anyhow::bail!(message!(extra_tiff_bands_must_be_marked_as_alpha)),
        };
        let planar = match tags
            .find_tag_unsigned::<u16>(Tag::PlanarConfiguration)?
            .unwrap_or(1)
        {
            1 => false,
            2 => true,
            _ => anyhow::bail!(message!(invalid_tiff_band_layout)),
        };
        ensure!(
            tags.find_tag_unsigned::<u16>(Tag::Orientation)?
                .unwrap_or(1)
                == 1,
            message!(only_tiff_pixels_with_a_top_left_origin_are_supported)
        );
        let nodata = ascii(&mut tags, Tag::GdalNodata)?;
        if let Some(text) = &nodata {
            text.trim()
                .parse::<f64>()
                .context(message!(invalid_tiff_nodata_value))?;
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
            message!(tiff_block_count_exceeds_the_decoding_limit)
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
                let len = plane
                    .checked_mul(pixel_bytes)
                    .context(message!(tiff_block_size_overflow))?;
                ensure!(
                    len <= MAX_CHUNK,
                    message!(a_tiff_block_exceeds_64_mib_use_smaller_strips_or_tiles)
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
            .with_context(|| message!(could_not_read_tiff_block_index, index = index))?;
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
        .context(message!(crs_geokey_is_missing_or_unreadable))?;
    ensure!(
        keys.len() >= 4 && keys[0] == 1,
        message!(invalid_geokey_directory)
    );
    let end = 4 + usize::from(keys[3]) * 4;
    ensure!(end <= keys.len(), message!(geokey_directory_is_incomplete));
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
    ensure!(
        code.is_some_and(|n| n > 0),
        message!(geokey_is_missing_an_embedded_crs_definition)
    );
    ensure!(
        matches!(key(1025), None | Some(1 | 2)),
        message!(unsupported_geotiff_raster_pixel_type)
    );
    for e in entries.chunks_exact(4) {
        let available = match e[1] {
            0 if e[2] == 1 => continue,
            34735 => keys.len(),
            34736 => doubles.as_ref().map_or(0, Vec::len),
            34737 => citation.as_ref().map_or(0, |s| s.len() + 1),
            _ => anyhow::bail!(message!(unsupported_geokey_parameter_tag, e[1])),
        };
        ensure!(
            usize::from(e[2]) + usize::from(e[3]) <= available,
            message!(geokey_parameter_is_out_of_range)
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
        _ => anyhow::bail!(message!(missing_valid_georeferencing_supply_pixel_scale_and_tie_points_or_an_affine_transformation_matrix)),
    };
    transform.inverse()?;
    Ok(geo)
}
