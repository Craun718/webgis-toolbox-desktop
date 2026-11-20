use super::{Phase, Source, TILE, TaskProgress, check_cancelled};
use anyhow::Result;
use flate2::{Compression, write::ZlibEncoder};
use std::{
    io::{Seek, SeekFrom, Write},
    sync::atomic::AtomicBool,
};
use tiff::tags::Tag;
use webgis_core::message;
use webgis_core::task_pool::{self, ThreadPool};

pub(super) struct Field {
    tag: Tag,
    kind: u16,
    count: u64,
    bytes: Vec<u8>,
}

impl Field {
    fn new(tag: Tag, kind: u16, count: u64, bytes: Vec<u8>) -> Self {
        Self {
            tag,
            kind,
            count,
            bytes,
        }
    }

    pub fn shorts(tag: Tag, values: &[u16]) -> Self {
        Self::new(
            tag,
            3,
            values.len() as u64,
            values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        )
    }

    fn long(tag: Tag, value: u32) -> Self {
        Self::new(tag, 4, 1, value.to_le_bytes().to_vec())
    }

    pub fn doubles(tag: Tag, values: &[f64]) -> Self {
        Self::new(
            tag,
            12,
            values.len() as u64,
            values.iter().flat_map(|v| v.to_le_bytes()).collect(),
        )
    }

    pub fn ascii(tag: Tag, value: &str) -> Self {
        let mut bytes = value.as_bytes().to_vec();
        bytes.push(0);
        Self::new(tag, 2, bytes.len() as u64, bytes)
    }
}

struct Level {
    size: [u32; 2],
    grid: [u32; 2],
    count: u64,
    image: Index,
    mask: Option<Index>,
}

#[derive(Clone, Copy, Default)]
struct Index {
    offsets: u64,
    lengths: u64,
}

impl Index {
    fn patch(self, out: &mut (impl Write + Seek), start: u64, blocks: &[[u64; 2]]) -> Result<()> {
        for (column, position) in [self.offsets, self.lengths].into_iter().enumerate() {
            out.seek(SeekFrom::Start(position + start * 8))?;
            for block in blocks {
                out.write_all(&block[column].to_le_bytes())?;
            }
        }
        Ok(())
    }
}

pub(super) fn write(
    out: &mut (impl Write + Seek),
    source: &mut Source,
    cancelled: &AtomicBool,
    progress: impl Fn(TaskProgress),
    pool: Option<&ThreadPool>,
) -> Result<()> {
    let mut levels = Vec::new();
    let mut size = source.size;
    let masked = source.mask.is_some();
    loop {
        let grid = size.map(|n| n.div_ceil(TILE as u32));
        levels.push(Level {
            size,
            grid,
            count: u64::from(grid[0]) * u64::from(grid[1]),
            image: Index::default(),
            mask: masked.then(Index::default),
        });
        if grid == [1, 1] {
            break;
        }
        size = size.map(|n| n.div_ceil(2));
    }
    let mut pos = directories(out, &mut levels, source)?;
    let total: u64 = levels.iter().map(|level| level.count).sum();
    let mut completed = 0;
    let mut buffers: Vec<_> = (0..task_pool::width(pool))
        .map(|_| {
            (
                vec![0; TILE * TILE * source.pixel_bytes()],
                vec![0; TILE * TILE / 8],
            )
        })
        .collect();
    let mut image_index = Vec::with_capacity(256);
    let mut mask_index = Vec::with_capacity(256);
    for (number, level) in levels.iter().enumerate().rev() {
        let mut start = 0;
        update(&progress, number, completed, total);
        for batch in task_pool::batches(0..level.count, pool) {
            let mut sampled = Vec::with_capacity(batch.len());
            for (index, (pixels, mask)) in batch.into_iter().zip(&mut buffers) {
                check_cancelled(cancelled)?;
                let col = (index % u64::from(level.grid[0])) as u32;
                let row = (index / u64::from(level.grid[0])) as u32;
                let origin = [col * TILE as u32, row * TILE as u32];
                sample(source, level.size, number, origin, pixels, mask)?;
                sampled.push((index, pixels.as_slice(), mask.as_slice()));
            }
            let blocks = task_pool::map(pool, sampled, |(index, pixels, mask)| {
                check_cancelled(cancelled)?;
                Ok::<_, anyhow::Error>((
                    index,
                    compress(pixels)?,
                    masked.then(|| compress(mask)).transpose()?,
                ))
            })?;
            for (index, pixels, mask) in blocks {
                check_cancelled(cancelled)?;
                image_index.push(block(out, &mut pos, &pixels)?);
                if let Some(mask) = mask {
                    mask_index.push(block(out, &mut pos, &mask)?);
                }
                if image_index.len() == image_index.capacity() || index + 1 == level.count {
                    level.image.patch(out, start, &image_index)?;
                    if let Some(mask) = level.mask {
                        mask.patch(out, start, &mask_index)?;
                    }
                    out.seek(SeekFrom::Start(pos))?;
                    start = index + 1;
                    image_index.clear();
                    mask_index.clear();
                }
                completed += 1;
                if completed % 16 == 0 || index + 1 == level.count {
                    update(&progress, number, completed, total);
                }
            }
        }
    }
    progress(TaskProgress::new(
        Phase::FlushCog,
        message!(flush_cog_write_buffer),
        Some(0.90),
    ));
    out.flush()?;
    Ok(())
}

fn update(progress: &impl Fn(TaskProgress), number: usize, completed: u64, total: u64) {
    let stage = if number == 0 {
        message!(write_full_resolution).to_owned()
    } else {
        message!(write_overview, 1_u32 << number)
    };
    progress(TaskProgress::new(
        Phase::WriteCogLevel(number),
        stage,
        Some(0.90 * (completed as f32 / total as f32)),
    ));
}

fn sample(
    source: &mut Source,
    size: [u32; 2],
    number: usize,
    [ox, oy]: [u32; 2],
    pixels: &mut [u8],
    mask: &mut [u8],
) -> Result<()> {
    pixels.fill(0);
    mask.fill(0);
    let width = (size[0] - ox).min(TILE as u32) as usize;
    let height = (size[1] - oy).min(TILE as u32) as usize;
    let bytes = source.pixel_bytes();
    // 每次从原图抽取同一网格位置，避免颜色、Alpha 和掩膜发生错位。
    for y in 0..height {
        let sy = (oy + y as u32) << number;
        for x in 0..width {
            let sx = (ox + x as u32) << number;
            let i = (y * TILE + x) * bytes;
            source.pixel(sx, sy, &mut pixels[i..i + bytes])?;
            if let Some(source_mask) = &mut source.mask
                && source_mask.valid(sx, sy)?
            {
                mask[y * (TILE / 8) + x / 8] |= 0x80 >> (x % 8);
            }
        }
    }
    Ok(())
}

fn compress(raw: &[u8]) -> Result<Vec<u8>> {
    let mut compressor = ZlibEncoder::new(Vec::new(), Compression::fast());
    compressor.write_all(raw)?;
    Ok(compressor.finish()?)
}

fn block(out: &mut impl Write, position: &mut u64, bytes: &[u8]) -> Result<[u64; 2]> {
    let length = u32::try_from(bytes.len())?;
    let offset = *position + 4;
    // GDAL 可通过块头和块尾识别连续的影像与掩膜数据。
    out.write_all(&length.to_le_bytes())?;
    out.write_all(bytes)?;
    out.write_all(&bytes[bytes.len() - 4..])?;
    *position += u64::from(length) + 8;
    Ok([offset, u64::from(length)])
}

fn directories(
    out: &mut (impl Write + Seek),
    levels: &mut [Level],
    source: &Source,
) -> Result<u64> {
    let masked = source.mask.is_some();
    let mut layout = String::from(
        "LAYOUT=IFDS_BEFORE_DATA\nBLOCK_ORDER=ROW_MAJOR\nBLOCK_LEADER=SIZE_AS_UINT4\nBLOCK_TRAILER=LAST_4_BYTES_REPEATED\nKNOWN_INCOMPATIBLE_EDITION=NO\n ",
    );
    if masked {
        layout.push_str("MASK_INTERLEAVED_WITH_IMAGERY=YES\n");
    }
    // 结构元数据的长度包含填充，使首个 IFD 紧接元数据并保持 8 字节对齐。
    const PREFIX_BYTES: usize = "GDAL_STRUCTURAL_METADATA_SIZE=000000 bytes\n".len();
    let first = align((16 + PREFIX_BYTES + layout.len()) as u64);
    let padding = first as usize - (16 + PREFIX_BYTES + layout.len());
    layout.extend(std::iter::repeat_n(' ', padding));
    let header = format!(
        "GDAL_STRUCTURAL_METADATA_SIZE={:06} bytes\n{layout}",
        layout.len()
    );
    out.write_all(b"II\x2b\0\x08\0\0\0")?;
    out.write_all(&first.to_le_bytes())?;
    out.write_all(header.as_bytes())?;

    // IFD 链顺序为原图、原图掩膜、影像概览、掩膜概览。
    let mut order = vec![(0, false)];
    if masked {
        order.push((0, true));
    }
    order.extend((1..levels.len()).map(|i| (i, false)));
    if masked {
        order.extend((1..levels.len()).map(|i| (i, true)));
    }
    let mut cursor = first;
    for (position, &(number, mask)) in order.iter().enumerate() {
        let level = &mut levels[number];
        let fields = fields(source, level.size, number, mask);
        let end = cursor + 16 + (fields.len() as u64 + 2) * 20;
        let index = if mask {
            level.mask.as_mut().unwrap()
        } else {
            &mut level.image
        };
        cursor = directory(
            out,
            fields,
            level.count,
            cursor,
            end,
            position + 1 == order.len(),
            index,
        )?;
    }
    // 单块索引内联，其余索引的值区域在最后一个 IFD 之后统一预留。
    for (number, mask) in order {
        let level = &mut levels[number];
        if level.count > 1 {
            let index = if mask {
                level.mask.as_mut().unwrap()
            } else {
                &mut level.image
            };
            for slot in [&mut index.offsets, &mut index.lengths] {
                out.seek(SeekFrom::Start(*slot))?;
                out.write_all(&cursor.to_le_bytes())?;
                *slot = cursor;
                cursor += level.count * 8;
            }
        }
    }
    out.seek(SeekFrom::Start(cursor))?;
    Ok(cursor)
}

fn fields(source: &Source, [width, height]: [u32; 2], number: usize, mask: bool) -> Vec<Field> {
    let (bits, samples, photo) = if mask {
        (1, 1, 4)
    } else {
        (source.bits, source.samples, source.photo)
    };
    let mut fields = vec![
        Field::long(
            Tag::NewSubfileType,
            u32::from(number != 0) | if mask { 4 } else { 0 },
        ),
        Field::long(Tag::ImageWidth, width),
        Field::long(Tag::ImageLength, height),
        Field::shorts(Tag::BitsPerSample, &[bits; 4][..usize::from(samples)]),
        Field::shorts(Tag::Compression, &[8]),
        Field::shorts(Tag::PhotometricInterpretation, &[photo]),
        Field::shorts(Tag::SamplesPerPixel, &[samples]),
        Field::shorts(Tag::PlanarConfiguration, &[1]),
        Field::long(Tag::TileWidth, TILE as u32),
        Field::long(Tag::TileLength, TILE as u32),
    ];
    if !mask {
        if let Some(alpha) = source.alpha {
            fields.push(Field::shorts(Tag::ExtraSamples, &[alpha]));
        }
        if bits == 32 {
            fields.push(Field::shorts(Tag::SampleFormat, &[3]));
        }
        if let Some(nodata) = &source.nodata {
            fields.push(Field::ascii(Tag::GdalNodata, nodata));
        }
        if number == 0 {
            fields.extend(
                source
                    .geo
                    .iter()
                    .map(|f| Field::new(f.tag, f.kind, f.count, f.bytes.clone())),
            );
        }
    }
    fields
}

fn directory(
    out: &mut (impl Write + Seek),
    mut fields: Vec<Field>,
    blocks: u64,
    start: u64,
    mut payload: u64,
    last: bool,
    index: &mut Index,
) -> Result<u64> {
    for tag in [Tag::TileOffsets, Tag::TileByteCounts] {
        fields.push(Field::new(tag, 16, blocks, Vec::new()));
    }
    fields.sort_unstable_by_key(|field| field.tag.to_u16());
    out.seek(SeekFrom::Start(start))?;
    out.write_all(&(fields.len() as u64).to_le_bytes())?;
    let mut values = Vec::new();
    for (i, field) in fields.into_iter().enumerate() {
        out.write_all(&field.tag.to_u16().to_le_bytes())?;
        out.write_all(&field.kind.to_le_bytes())?;
        out.write_all(&field.count.to_le_bytes())?;
        if field.tag == Tag::TileOffsets {
            index.offsets = start + 20 + i as u64 * 20;
        } else if field.tag == Tag::TileByteCounts {
            index.lengths = start + 20 + i as u64 * 20;
        }
        let slot = if field.bytes.len() > 8 {
            let offset = align(payload);
            values.resize(values.len() + (offset - payload) as usize, 0);
            payload = offset + field.bytes.len() as u64;
            values.extend(field.bytes);
            offset.to_le_bytes()
        } else {
            let mut slot = [0; 8];
            slot[..field.bytes.len()].copy_from_slice(&field.bytes);
            slot
        };
        out.write_all(&slot)?;
    }
    let next = align(payload);
    out.write_all(&(if last { 0 } else { next }).to_le_bytes())?;
    out.write_all(&values)?;
    Ok(next)
}

fn align(offset: u64) -> u64 {
    offset.next_multiple_of(8)
}
