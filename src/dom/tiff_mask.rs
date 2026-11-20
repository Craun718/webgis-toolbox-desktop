use anyhow::{Context, Result, bail, ensure};
use flate2::read::ZlibDecoder;
use std::io::{Read, Seek, SeekFrom};
use tiff::{decoder::IfdDecoder, tags::Tag};
use webgis_core::message;

/// TIFF 的 1 位掩膜独立于颜色 IFD，像元按行用位打包。
pub(super) struct Mask {
    pub size: [u32; 2],
    pub chunk: [u32; 2],
    tiled: bool,
    blocks: RawBlocks,
    reverse_bits: bool,
}

impl Mask {
    pub fn read_tags(tags: &mut IfdDecoder<'_>) -> Result<Self> {
        ensure!(
            tags.get_tag_u32(Tag::PhotometricInterpretation)? == 4,
            message!(invalid_tiff_mask_color_type)
        );
        ensure!(
            tags.find_tag_unsigned::<u16>(Tag::BitsPerSample)?
                .unwrap_or(1)
                == 1
                && tags
                    .find_tag_unsigned::<u16>(Tag::SamplesPerPixel)?
                    .unwrap_or(1)
                    == 1,
            message!(tiff_mask_must_be_a_single_band_1_bit_image)
        );
        let size = [
            tags.get_tag_u32(Tag::ImageWidth)?,
            tags.get_tag_u32(Tag::ImageLength)?,
        ];
        let tiled = tags.find_entry(Tag::TileOffsets).is_some();
        let chunk = if tiled {
            [
                tags.get_tag_u32(Tag::TileWidth)?,
                tags.get_tag_u32(Tag::TileLength)?,
            ]
        } else {
            [
                size[0],
                tags.find_tag_unsigned(Tag::RowsPerStrip)?
                    .unwrap_or(size[1])
                    .min(size[1]),
            ]
        };
        ensure!(
            size.into_iter().chain(chunk).all(|v| v > 0),
            message!(invalid_tiff_mask_dimensions)
        );
        let blocks = RawBlocks::new(tags, tiled)?;
        let count = u64::from(size[0].div_ceil(chunk[0])) * u64::from(size[1].div_ceil(chunk[1]));
        ensure!(
            blocks.offsets.len() as u64 == count,
            message!(invalid_tiff_mask_block_index)
        );
        let fill_order = tags.find_tag_unsigned::<u16>(Tag::FillOrder)?.unwrap_or(1);
        ensure!(
            matches!(fill_order, 1 | 2),
            message!(invalid_tiff_mask_bit_order)
        );
        Ok(Self {
            size,
            chunk,
            tiled,
            blocks,
            reverse_bits: fill_order == 2,
        })
    }

    pub fn count(&self) -> usize {
        self.blocks.offsets.len()
    }

    pub fn read_chunk(&self, reader: &mut (impl Read + Seek), index: usize) -> Result<Vec<u8>> {
        let rows = if self.tiled {
            self.chunk[1]
        } else {
            self.chunk[1].min(self.size[1] - index as u32 * self.chunk[1])
        };
        let size = u64::from(self.chunk[0].div_ceil(8)) * u64::from(rows);
        let size = usize::try_from(size)?;
        let mut data = self.blocks.read(reader, index, size)?;
        if self.reverse_bits {
            data.iter_mut()
                .for_each(|value| *value = value.reverse_bits());
        }
        Ok(data)
    }

    pub fn valid(&self, data: &[u8], x: usize, y: usize) -> bool {
        bit(data, self.chunk[0].div_ceil(8) as usize, x, y)
    }
}

pub(super) const MAX_CHUNK: usize = 64 * 1024 * 1024;

pub(super) struct RawBlocks {
    offsets: Vec<u64>,
    lengths: Vec<u64>,
    compression: u16,
}

impl RawBlocks {
    pub fn new(tags: &mut IfdDecoder<'_>, tiled: bool) -> Result<Self> {
        let (offset_tag, length_tag) = if tiled {
            (Tag::TileOffsets, Tag::TileByteCounts)
        } else {
            (Tag::StripOffsets, Tag::StripByteCounts)
        };
        let offsets = tags.get_tag_u64_vec(offset_tag)?;
        let lengths = tags.get_tag_u64_vec(length_tag)?;
        ensure!(
            offsets.len() == lengths.len(),
            message!(invalid_tiff_block_index)
        );
        let compression = tags.find_tag_unsigned(Tag::Compression)?.unwrap_or(1);
        ensure!(
            matches!(compression, 1 | 5 | 8 | 32946),
            message!(
                unsupported_tiff_block_compression_compression,
                compression = compression
            )
        );
        Ok(Self {
            offsets,
            lengths,
            compression,
        })
    }

    pub fn read(
        &self,
        reader: &mut (impl Read + Seek),
        index: usize,
        size: usize,
    ) -> Result<Vec<u8>> {
        let length = *self
            .lengths
            .get(index)
            .context(message!(tiff_block_is_out_of_range))?;
        ensure!(
            size <= MAX_CHUNK && length <= MAX_CHUNK as u64,
            message!(a_tiff_block_exceeds_the_memory_limit)
        );
        let mut data = vec![0; size];
        if self.offsets[index] == 0 && length == 0 {
            return Ok(data);
        }
        reader.seek(SeekFrom::Start(self.offsets[index]))?;
        let mut input = reader.take(length);
        match self.compression {
            1 => input.read_exact(&mut data)?,
            8 | 32946 => ZlibDecoder::new(input).read_exact(&mut data)?,
            5 => {
                let mut compressed = Vec::new();
                input.read_to_end(&mut compressed)?;
                let mut decoder =
                    weezl::decode::Configuration::with_tiff_size_switch(weezl::BitOrder::Msb, 8)
                        .with_yield_on_full_buffer(true)
                        .build();
                let mut src = compressed.as_slice();
                let mut dst = data.as_mut_slice();
                while !dst.is_empty() {
                    let result = decoder.decode_bytes(src, dst);
                    let status = result.status?;
                    src = &src[result.consumed_in..];
                    dst = &mut dst[result.consumed_out..];
                    ensure!(
                        dst.is_empty()
                            || matches!(status, weezl::LzwStatus::Ok)
                                && (result.consumed_in > 0 || result.consumed_out > 0),
                        message!(invalid_tiff_lzw_block_size)
                    );
                }
            }
            _ => bail!(message!(unsupported_tiff_compression)),
        }
        Ok(data)
    }
}

pub(super) fn bit(data: &[u8], stride: usize, x: usize, y: usize) -> bool {
    data[y * stride + x / 8] & (0x80 >> (x % 8)) != 0
}
