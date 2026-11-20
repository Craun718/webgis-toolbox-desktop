use std::{fs::File, path::Path};
use webgis_core::message;
use webgis_core::task_pool::{self, ThreadPool};

use anyhow::{Context, Result};
use tiff_core::{Compression, ExtraSample, PhotometricInterpretation, Tag, TagValue};
use tiff_writer::{ImageBuilder, TiffWriter, WriteOptions};

use crate::tiles::{Bounds, HALF_WORLD, Level, publish_file, tile_span};

pub fn run(level: &Level, zoom: u8, output: &Path) -> Result<()> {
    run_with_pool(level, zoom, output, None)
}

pub fn run_with_pool(
    level: &Level,
    zoom: u8,
    output: &Path,
    pool: Option<&ThreadPool>,
) -> Result<()> {
    run_with_publish(level, zoom, output, pool, || Ok(()))
}

pub fn run_with_publish(
    level: &Level,
    zoom: u8,
    output: &Path,
    pool: Option<&ThreadPool>,
    before_publish: impl FnOnce() -> Result<()>,
) -> Result<()> {
    publish_file(output, |file| {
        write(file, level, zoom, pool)?;
        before_publish()
    })
}

fn write(file: &mut File, level: &Level, zoom: u8, pool: Option<&ThreadPool>) -> Result<()> {
    let (&first_coord, first_tile) = level.first_key_value().unwrap();
    let first = first_tile.decode(None)?.image.into_rgba8();
    let size = first.width();
    let bounds = Bounds::of(level);
    let cols = bounds.max_x - bounds.min_x + 1;
    let rows = bounds.max_y - bounds.min_y + 1;
    let width = cols
        .checked_mul(size)
        .context(message!(mosaic_width_exceeds_the_tiff_limit))?;
    let height = rows
        .checked_mul(size)
        .context(message!(mosaic_height_exceeds_the_tiff_limit))?;
    let span = tile_span(zoom);
    let resolution = span / f64::from(size);
    let left = -HALF_WORLD + f64::from(bounds.min_x) * span;
    let top = HALF_WORLD - f64::from(bounds.min_y) * span;
    let image = ImageBuilder::new(width, height)
        .sample_type::<u8>()
        .samples_per_pixel(4)
        .photometric(PhotometricInterpretation::Rgb)
        .extra_samples(vec![ExtraSample::UnassociatedAlpha])
        .compression(Compression::Deflate)
        .tiles(size, size)
        .tag(Tag::new(
            33550,
            TagValue::Double(vec![resolution, resolution, 0.0]),
        ))
        .tag(Tag::new(
            33922,
            TagValue::Double(vec![0.0, 0.0, 0.0, left, top, 0.0]),
        ))
        // Projected、PixelIsArea、EPSG:3857。
        .tag(Tag::new(
            34735,
            TagValue::Short(vec![
                1, 1, 0, 3, 1024, 0, 1, 1, 1025, 0, 1, 1, 3072, 0, 1, 3857,
            ]),
        ));
    let mut writer = TiffWriter::new(file, WriteOptions::auto())?;
    let handle = writer.add_image(image)?;
    let blank = vec![0_u8; (size * size * 4) as usize];
    let coords = (bounds.min_y..=bounds.max_y)
        .flat_map(|y| (bounds.min_x..=bounds.max_x).map(move |x| (x, y)));
    let mut first = Some(first);
    for batch in task_pool::batches(coords, pool) {
        let decoded = task_pool::map(pool, batch, |coord| {
            let image = level
                .get(&coord)
                .filter(|_| coord != first_coord)
                .map(|tile| tile.rgba(size))
                .transpose()?;
            Ok::<_, anyhow::Error>((coord, image))
        })?;
        for ((x, y), image) in decoded {
            let block = (y - bounds.min_y) as usize * cols as usize + (x - bounds.min_x) as usize;
            let image = if (x, y) == first_coord {
                first.take()
            } else {
                image
            };
            writer.write_block(
                &handle,
                block,
                image.as_ref().map_or(&blank, |image| image.as_raw()),
            )?;
        }
    }
    writer.finish()?;
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::{Read, Seek, SeekFrom};

    use super::*;
    use crate::tiles::{Index, Scheme};

    #[test]
    fn auto_selects_bigtiff_when_ifd_exceeds_classic_offset_range() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("0/0");
        std::fs::create_dir_all(&source).unwrap();
        image::RgbaImage::from_pixel(16, 16, image::Rgba([31, 63, 127, 128]))
            .save(source.join("0.png"))
            .unwrap();
        let index = Index::scan(dir.path(), Scheme::Xyz).unwrap();

        let output = dir.path().join("big.tif");
        publish_file(&output, |file| {
            // 扩展稀疏文件，让真实 IFD 位置跨过 32 位偏移，无需分配 4 GiB 像素。
            file.set_len(1_u64 << 32)?;
            write(file, index.level(0)?, 0, None)
        })
        .unwrap();

        let mut file = File::open(&output).unwrap();
        let mut header = [0; 16];
        file.read_exact(&mut header).unwrap();
        assert_eq!(&header[..4], b"II+\0");
        let ifd_offset = u64::from_le_bytes(header[8..16].try_into().unwrap());
        assert!(ifd_offset >= 1_u64 << 32);
        assert!(ifd_offset < file.seek(SeekFrom::End(0)).unwrap());

        let mut decoder = tiff::decoder::Decoder::new(File::open(output).unwrap()).unwrap();
        let tiff::decoder::DecodingResult::U8(pixels) = decoder.read_chunk(0).unwrap() else {
            panic!("expected RGBA");
        };
        assert_eq!(&pixels[..4], &[31, 63, 127, 128]);
    }
}
