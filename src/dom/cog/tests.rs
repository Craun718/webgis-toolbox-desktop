use super::*;
use crate::dom::{
    geotiff::{Affine, Crs, geometry, write_geotags},
    tiff_output::{BigTiff, LevelWriter, NODATA, RasterKind, RasterSpec},
};
use std::{
    fs::{self, File},
    io::{BufReader, Read, Seek, SeekFrom},
    sync::atomic::Ordering,
};
use tiff::{
    decoder::{Decoder, DecodingResult},
    encoder::{TiffEncoder, colortype},
    tags::Tag,
};

const SIZE: [usize; 2] = [517, 263];
const TRANSFORM: Affine = Affine([123., 0.25, 0.03, 40., 0.02, -0.25]);

fn spec(size: [usize; 2]) -> Result<RasterSpec> {
    Ok(RasterSpec {
        size,
        transform: TRANSFORM,
        crs: Some(Crs::from_code(3857)?),
    })
}

fn rgba(path: &Path, size: [usize; 2], georeferenced: bool) -> Result<Vec<u8>> {
    let mut encoder = TiffEncoder::new(File::create(path)?)?;
    let mut image = encoder.new_image::<colortype::RGBA8>(size[0] as u32, size[1] as u32)?;
    image.rows_per_strip(13)?;
    image.encoder().write_tag(Tag::ExtraSamples, 2_u16)?;
    if georeferenced {
        write_geotags(image.encoder(), spec(size)?.crs, TRANSFORM, true)?;
        image
            .encoder()
            .write_tag(Tag::GeoAsciiParamsTag, "Test CRS|")?;
        image
            .encoder()
            .write_tag(Tag::GeoDoubleParamsTag, &[12.5, 6.25][..])?;
    }
    let bytes: Vec<_> = (0..size[0] * size[1])
        .flat_map(|i| {
            [
                i as u8,
                (i >> 3) as u8,
                90,
                if i % 7 == 0 { 0 } else { 180 },
            ]
        })
        .collect();
    image.write_data(&bytes)?;
    Ok(bytes)
}

fn dom_dsm(path: &Path, kind: RasterKind) -> Result<()> {
    let mut encoder = BigTiff::new_big(File::create(path)?)?;
    let mut level = LevelWriter::new(&mut encoder, kind)?;
    for row in 0..SIZE[1].div_ceil(TILE) {
        for col in 0..SIZE[0].div_ceil(TILE) {
            let [width, height] = [
                (SIZE[0] - col * TILE).min(TILE),
                (SIZE[1] - row * TILE).min(TILE),
            ];
            let count = width * height;
            if kind == RasterKind::Dom {
                let pixels = [
                    vec![col as u8 + 12; count],
                    vec![row as u8 + 20; count],
                    vec![74; count],
                    (0..count)
                        .map(|i| if i % 5 == 0 { 0 } else { 255 })
                        .collect(),
                ];
                level.color([width, height], &pixels)?;
            } else {
                let heights: Vec<_> = (0..count)
                    .map(|i| if i % 5 == 0 { NODATA } else { i as f32 * 0.125 })
                    .collect();
                level.height([width, height], &heights)?;
            }
        }
    }
    if let Some(mask) = level.finish(spec(SIZE)?, false)? {
        mask.finish(&mut encoder)?;
    }
    Ok(())
}

fn convert(input: &Path) -> Result<PathBuf> {
    let output = default_output(input);
    run(input, &output, &AtomicBool::new(false), |_| {}, || Ok(()))
}

fn decoder(path: &Path) -> Result<Decoder<BufReader<File>>> {
    Ok(Decoder::new(BufReader::new(File::open(path)?))?)
}

fn read_u8(decoder: &mut Decoder<BufReader<File>>) -> Result<Vec<u8>> {
    match decoder.read_image()? {
        DecodingResult::U8(bytes) => Ok(bytes),
        _ => anyhow::bail!("预期 8 位像元"),
    }
}

fn compare_full_resolution(input: &Path, output: &Path) -> Result<()> {
    let mut original = decoder(input)?;
    let mut result = decoder(output)?;
    let (_, before) = geometry(&mut original.image_ifd())?;
    let (_, after) = geometry(&mut result.image_ifd())?;
    assert_eq!(before, after);
    for tag in [
        Tag::GeoKeyDirectoryTag,
        Tag::ModelTransformationTag,
        Tag::GeoAsciiParamsTag,
        Tag::GeoDoubleParamsTag,
        Tag::ExtraSamples,
        Tag::GdalNodata,
    ] {
        assert_eq!(original.find_tag(tag)?, result.find_tag(tag)?);
    }
    let mut original = Source::open(input)?;
    let mut result = Source::open(output)?;
    assert_eq!(original.size, result.size);
    assert_eq!(original.mask.is_some(), result.mask.is_some());
    let [width, height] = result.size;
    let bytes = result.pixel_bytes();
    for y in 0..height {
        for x in 0..width {
            let (mut a, mut b) = ([0; 4], [0; 4]);
            original.pixel(x, y, &mut a[..bytes])?;
            result.pixel(x, y, &mut b[..bytes])?;
            assert_eq!(a, b, "像元 ({x}, {y})");
            if let (Some(a), Some(b)) = (&mut original.mask, &mut result.mask) {
                assert_eq!(a.valid(x, y)?, b.valid(x, y)?, "掩膜 ({x}, {y})");
            }
        }
    }
    Ok(())
}

struct Ifd {
    flags: u32,
    offsets: Vec<u64>,
    counts: Vec<u64>,
}

fn check_layout(path: &Path, size: [usize; 2], flags: &[u32]) -> Result<()> {
    let mut reader = File::open(path)?;
    let mut header = [0; 8];
    reader.read_exact(&mut header)?;
    assert_eq!(&header, b"II\x2b\0\x08\0\0\0");
    let mut decoded = decoder(path)?;
    let mut ifds = Vec::new();
    let mut last_ifd_end = 0;
    let mut last_value_end = 0;
    let mut first_index = u64::MAX;
    for &flag in flags {
        let pointer = decoded.ifd_pointer().unwrap().0;
        reader.seek(SeekFrom::Start(pointer))?;
        reader.read_exact(&mut header)?;
        let entries = u64::from_le_bytes(header);
        let end = pointer + 16 + entries * 20;
        assert!(pointer >= last_ifd_end);
        last_ifd_end = end;
        for index in 0..entries {
            reader.seek(SeekFrom::Start(pointer + 8 + index * 20))?;
            let mut entry = [0; 20];
            reader.read_exact(&mut entry)?;
            let ty = u16::from_le_bytes(entry[2..4].try_into()?);
            let count = u64::from_le_bytes(entry[4..12].try_into()?);
            let bytes = count
                * match ty {
                    2 => 1,
                    3 => 2,
                    4 => 4,
                    12 | 16 => 8,
                    _ => panic!("字段类型 {ty}"),
                };
            if bytes > 8 {
                let address = u64::from_le_bytes(entry[12..20].try_into()?);
                assert!(address >= end);
                last_value_end = last_value_end.max(address + bytes);
                let tag = u16::from_le_bytes(entry[..2].try_into()?);
                if tag == Tag::TileOffsets.to_u16() || tag == Tag::TileByteCounts.to_u16() {
                    first_index = first_index.min(address);
                } else {
                    last_ifd_end = last_ifd_end.max(address + bytes);
                }
            }
        }
        assert_eq!(decoded.get_tag_u32(Tag::NewSubfileType)?, flag);
        assert_eq!(decoded.chunk_dimensions(), (256, 256));
        ifds.push(Ifd {
            flags: flag,
            offsets: decoded.get_tag_u64_vec(Tag::TileOffsets)?,
            counts: decoded.get_tag_u64_vec(Tag::TileByteCounts)?,
        });
        if decoded.more_images() {
            decoded.next_image()?;
        }
    }
    assert!(!decoded.more_images());
    let images: Vec<_> = ifds.iter().filter(|dir| dir.flags & 4 == 0).collect();
    let masks: Vec<_> = ifds.iter().filter(|dir| dir.flags & 4 != 0).collect();
    let mut blocks = Vec::new();
    for level in (0..images.len()).rev() {
        let count: usize = size
            .map(|n| n.div_ceil(1 << level).div_ceil(TILE))
            .iter()
            .product();
        let image = images[level];
        assert_eq!(image.offsets.len(), count);
        for i in 0..count {
            blocks.push((image.offsets[i], image.counts[i]));
            if let Some(mask) = masks.get(level) {
                blocks.push((mask.offsets[i], mask.counts[i]));
            }
        }
    }
    assert!(first_index >= last_ifd_end);
    assert!(blocks[0].0 - 4 >= last_ifd_end.max(last_value_end));
    let mut next = blocks[0].0;
    for (offset, length) in blocks {
        assert_eq!(offset, next);
        reader.seek(SeekFrom::Start(offset - 4))?;
        let mut leader = [0; 4];
        reader.read_exact(&mut leader)?;
        assert_eq!(u64::from(u32::from_le_bytes(leader)), length);
        reader.seek(SeekFrom::Start(offset + length - 4))?;
        reader.read_exact(&mut header)?;
        assert_eq!(header[..4], header[4..]);
        next += length + 8;
    }
    assert_eq!(reader.metadata()?.len(), next - 4);
    Ok(())
}

#[test]
fn converts_stripped_rgba_with_odd_dimensions_and_preserves_hidden_samples() -> Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("rgba.TIFF");
    let bytes = rgba(&input, SIZE, true)?;
    let output = convert(&input)?;
    assert_eq!(output, root.path().join("rgba_cog.tif"));
    let mut result = decoder(&output)?;
    assert_eq!(read_u8(&mut result)?, bytes);
    for factor in [2, 4] {
        result.next_image()?;
        let [width, height] = SIZE.map(|v| v.div_ceil(factor));
        assert_eq!(result.dimensions()?, (width as u32, height as u32));
        let overview = read_u8(&mut result)?;
        for y in 0..height {
            for x in 0..width {
                let target = (y * width + x) * 4;
                let origin = (y * factor * SIZE[0] + x * factor) * 4;
                assert_eq!(overview[target..target + 4], bytes[origin..origin + 4]);
            }
        }
    }
    compare_full_resolution(&input, &output)?;
    check_layout(&output, SIZE, &[0, 1, 1])
}

#[test]
fn converts_toolbox_dom_and_dsm_with_internal_mask_and_nodata() -> Result<()> {
    let root = tempfile::tempdir()?;
    for (name, kind, flags) in [
        ("dom.tif", RasterKind::Dom, &[0, 4, 1, 1, 5, 5][..]),
        ("dsm.tif", RasterKind::Dsm, &[0, 1, 1][..]),
    ] {
        let input = root.path().join(name);
        dom_dsm(&input, kind)?;
        let output = convert(&input)?;
        compare_full_resolution(&input, &output)?;
        check_layout(&output, SIZE, flags)?;
        if kind == RasterKind::Dom {
            let mut overview = decoder(&output)?;
            let mut file = File::open(&output)?;
            let mut source = Source::open(&input)?.mask.unwrap();
            for (ifd, factor) in [(4, 2), (5, 4)] {
                overview.seek_to_image(ifd)?;
                let mask = crate::dom::tiff_mask::Mask::read_tags(&mut overview.image_ifd())?;
                let [width, height] = mask.size;
                assert_eq!([width, height], SIZE.map(|v| (v as u32).div_ceil(factor)));
                for row in 0..height.div_ceil(TILE as u32) {
                    for col in 0..width.div_ceil(TILE as u32) {
                        let tile = mask.read_chunk(
                            &mut file,
                            (row * width.div_ceil(TILE as u32) + col) as usize,
                        )?;
                        for y in 0..TILE.min(height as usize - row as usize * TILE) {
                            for x in 0..TILE.min(width as usize - col as usize * TILE) {
                                let sx = (col * TILE as u32 + x as u32) * factor;
                                let sy = (row * TILE as u32 + y as u32) * factor;
                                assert_eq!(mask.valid(&tile, x, y), source.valid(sx, sy)?);
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

#[test]
fn small_image_has_only_one_ifd_and_inline_index() -> Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("small.tif");
    let size = [15, 19];
    let pixels = rgba(&input, size, true)?;
    let output = convert(&input)?;
    assert_eq!(read_u8(&mut decoder(&output)?)?, pixels);
    check_layout(&output, size, &[0])
}

#[test]
fn rejects_unsupported_inputs_and_cleans_up_cancelled_or_conflicting_output() -> Result<()> {
    let root = tempfile::tempdir()?;
    let input = root.path().join("source.tif");
    rgba(&input, SIZE, true)?;
    let output = default_output(&input);
    let cancel = AtomicBool::new(false);
    let start = |input: &Path, output: &Path| run(input, output, &cancel, |_| {}, || Ok(()));
    assert!(format!("{:#}", start(&input, &input).unwrap_err()).contains("源文件"));
    fs::write(&output, b"existing")?;
    assert!(
        start(&input, &output)
            .unwrap_err()
            .to_string()
            .contains("已存在")
    );
    assert_eq!(fs::read(&output)?, b"existing");

    let missing_geo = root.path().join("local.tif");
    rgba(&missing_geo, [3, 5], false)?;
    assert!(format!("{:#}", convert(&missing_geo).unwrap_err()).contains("GeoKey"));
    let gray16 = root.path().join("gray16.tif");
    let mut encoder = TiffEncoder::new(File::create(&gray16)?)?;
    let mut image = encoder.new_image::<colortype::Gray16>(2, 2)?;
    write_geotags(image.encoder(), spec([2, 2])?.crs, TRANSFORM, false)?;
    image.write_data(&[0; 4])?;
    assert!(format!("{:#}", convert(&gray16).unwrap_err()).contains("仅支持 8 位"));

    let cancelled = root.path().join("cancelled.tif");
    let files = || fs::read_dir(root.path()).unwrap().count();
    let before = files();
    let error = run(
        &input,
        &cancelled,
        &cancel,
        |event| {
            if event.fraction.is_some_and(|fraction| fraction > 0.) {
                cancel.store(true, Ordering::Relaxed);
            }
        },
        || panic!("取消后进入发布"),
    )
    .unwrap_err();
    assert!(error.to_string().contains("取消"));
    assert!(!cancelled.exists());
    assert_eq!(files(), before);

    cancel.store(false, Ordering::Relaxed);
    let conflict = root.path().join("conflict.tif");
    assert!(
        run(
            &input,
            &conflict,
            &cancel,
            |_| {},
            || {
                fs::write(&conflict, b"concurrent")?;
                Ok(())
            }
        )
        .is_err()
    );
    assert_eq!(fs::read(&conflict)?, b"concurrent");
    assert_eq!(files(), before + 1);
    Ok(())
}

#[test]
fn preserves_white_is_zero_gray_alpha_and_associated_alpha_tag() -> Result<()> {
    use tiff::{
        encoder::Compression,
        tags::{PhotometricInterpretation, SampleFormat},
    };
    struct GrayAlpha;
    impl colortype::ColorType for GrayAlpha {
        type Inner = u8;
        const TIFF_VALUE: PhotometricInterpretation = PhotometricInterpretation::WhiteIsZero;
        const BITS_PER_SAMPLE: &'static [u16] = &[8, 8];
        const SAMPLE_FORMAT: &'static [SampleFormat] = &[SampleFormat::Uint; 2];

        fn horizontal_predict(row: &[u8], output: &mut Vec<u8>) {
            output.extend_from_slice(&row[..2]);
            output.extend(row[2..].iter().zip(row).map(|(a, b)| a.wrapping_sub(*b)));
        }
    }
    let root = tempfile::tempdir()?;
    let input = root.path().join("grey-alpha.tiff");
    let mut encoder = TiffEncoder::new(File::create(&input)?)?
        .with_compression(Compression::Lzw)
        .with_predictor(tiff::encoder::Predictor::Horizontal);
    let mut image = encoder.new_image::<GrayAlpha>(3, 2)?;
    image.rows_per_strip(1)?;
    write_geotags(image.encoder(), spec([3, 2])?.crs, TRANSFORM, false)?;
    image.encoder().write_tag(Tag::ExtraSamples, 1_u16)?;
    let samples = [[10, 50], [0, 0], [50, 60], [90, 94], [1, 118], [65, 69]];
    image.write_data(&samples.concat())?;
    let output = convert(&input)?;
    let mut result = Source::open(&output)?;
    assert_eq!(result.photo, 0);
    assert_eq!(result.alpha, Some(1));
    for (i, expected) in samples.into_iter().enumerate() {
        let mut actual = [0; 2];
        result.pixel(i as u32 % 3, i as u32 / 3, &mut actual)?;
        assert_eq!(actual, expected);
    }
    Ok(())
}
