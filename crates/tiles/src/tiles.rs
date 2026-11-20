use std::{
    collections::BTreeMap,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
};
use webgis_core::message;

use anyhow::{Context, Result, bail, ensure};
use clap::ValueEnum;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, RgbaImage};
use tempfile::{NamedTempFile, tempdir_in};

pub const MAX_ZOOM: u8 = 30;
pub const HALF_WORLD: f64 = std::f64::consts::PI * 6_378_137.0;

#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Scheme {
    Xyz,
    Tms,
}

impl Scheme {
    pub fn row(self, zoom: u8, row: u32) -> u32 {
        match self {
            Self::Xyz => row,
            Self::Tms => (1 << zoom) - 1 - row,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Png,
    #[value(alias = "jpg")]
    Jpeg,
    Webp,
}

impl Format {
    pub fn from_extension(ext: &str) -> Option<Self> {
        match ext.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "jpeg" | "jpg" => Some(Self::Jpeg),
            "webp" => Some(Self::Webp),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
        }
    }

    fn image_format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Webp => ImageFormat::WebP,
        }
    }
}

pub type Coord = (u32, u32);
pub type Level = BTreeMap<Coord, Tile>;

pub struct Tile {
    pub path: PathBuf,
    pub format: Format,
}

pub struct Decoded {
    pub image: DynamicImage,
    // PNG 的文件波段布局可能不同于解码后的像素布局。
    pub bands: Option<u8>,
}

impl Tile {
    pub fn extension(&self) -> &str {
        self.path.extension().and_then(|ext| ext.to_str()).unwrap()
    }

    pub fn decode(&self, expected_size: Option<u32>) -> Result<Decoded> {
        self.decode_inner(expected_size)
            .with_context(|| message!(read_tile, self.path.display()))
    }

    fn decode_inner(&self, expected_size: Option<u32>) -> Result<Decoded> {
        let mut reader = BufReader::new(File::open(&self.path)?);
        let header = reader.fill_buf()?;
        ensure!(
            image::guess_format(header)? == self.format.image_format(),
            message!(image_format_does_not_match_the_file_extension)
        );
        let png_bands = if self.format == Format::Png && header.get(24) == Some(&8) {
            match header.get(25) {
                Some(0) => Some(1),
                Some(2) => Some(3),
                Some(4) => Some(2),
                Some(6) => Some(4),
                _ => None,
            }
        } else {
            None
        };
        let mut reader = ImageReader::with_format(reader, self.format.image_format());
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(4096);
        limits.max_image_height = Some(4096);
        reader.limits(limits);
        let decoder = reader.into_decoder()?;
        let (w, h) = decoder.dimensions();
        ensure!(
            w == h && (16..=4096).contains(&w) && w.is_multiple_of(16),
            message!(tiles_must_be_square_with_sides_that_are_multiples_of_16_and_at_most_4096_pixels_found_w_h, w = w, h = h));
        if let Some(size) = expected_size {
            ensure!(
                size == w,
                message!(expected_tile_side_length_size_found_w, size = size, w = w)
            );
        }
        let color = decoder.color_type();
        ensure!(
            color.bits_per_pixel() == u16::from(color.channel_count()) * 8,
            message!(only_8_bit_images_are_supported)
        );
        let bands = if self.format == Format::Png {
            png_bands
        } else {
            Some(color.channel_count())
        };
        Ok(Decoded {
            image: DynamicImage::from_decoder(decoder)?,
            bands,
        })
    }

    pub fn rgba(&self, size: u32) -> Result<RgbaImage> {
        Ok(self.decode(Some(size))?.image.into_rgba8())
    }
}

pub struct Index(pub BTreeMap<u8, Level>);

impl Index {
    pub fn scan(root: &Path, scheme: Scheme) -> Result<Self> {
        let mut levels = BTreeMap::<u8, Level>::new();
        for z_dir in directories(root)? {
            let Some(z) = coordinate_directory(&z_dir)? else {
                continue;
            };
            ensure!(
                z <= u32::from(MAX_ZOOM),
                message!(zoom_level_out_of_bounds, z_dir.display())
            );
            let zoom = z as u8;
            let count = 1 << zoom;
            for x_dir in directories(&z_dir)? {
                let Some(x) = coordinate_directory(&x_dir)? else {
                    continue;
                };
                ensure!(x < count, message!(column_out_of_range, x_dir.display()));
                for entry in read_dir(&x_dir)? {
                    let path = entry?.path();
                    let Some(format) = path
                        .extension()
                        .and_then(|ext| ext.to_str())
                        .and_then(Format::from_extension)
                    else {
                        continue;
                    };
                    ensure!(path.is_file(), message!(tile_is_not_a_file, path.display()));
                    let row = parse_coordinate(
                        path.file_stem()
                            .and_then(|stem| stem.to_str())
                            .unwrap_or_default(),
                        &path,
                    )?;
                    ensure!(row < count, message!(row_out_of_range, path.display()));
                    let coord = (x, scheme.row(zoom, row));
                    let level = levels.entry(zoom).or_default();
                    if let Some(previous) = level.get(&coord) {
                        bail!(message!(
                            duplicate_coordinates_and,
                            previous.path.display(),
                            path.display()
                        ));
                    }
                    level.insert(coord, Tile { path, format });
                }
            }
        }
        ensure!(
            !levels.is_empty(),
            message!(no_z_x_y_ext_tiles_found, root.display())
        );
        Ok(Self(levels))
    }

    pub fn level(&self, zoom: u8) -> Result<&Level> {
        self.0
            .get(&zoom)
            .with_context(|| message!(input_folder_has_no_tiles_at_zoom_zoom, zoom = zoom))
    }
}

fn read_dir(path: &Path) -> Result<fs::ReadDir> {
    fs::read_dir(path).with_context(|| message!(read_folder, path.display()))
}

fn directories(path: &Path) -> Result<Vec<PathBuf>> {
    let mut dirs = Vec::new();
    for entry in read_dir(path)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort_unstable();
    Ok(dirs)
}

fn coordinate_directory(path: &Path) -> Result<Option<u32>> {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default();
    let digits = name.strip_prefix('-').unwrap_or(name);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    parse_coordinate(name, path).map(Some)
}

fn parse_coordinate(value: &str, path: &Path) -> Result<u32> {
    ensure!(
        !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()),
        message!(invalid_coordinate, path.display())
    );
    value
        .parse()
        .with_context(|| message!(coordinates_out_of_bounds, path.display()))
}

#[derive(Clone, Copy)]
pub struct Bounds {
    pub min_x: u32,
    pub min_y: u32,
    pub max_x: u32,
    pub max_y: u32,
}

impl Bounds {
    pub fn of(level: &Level) -> Self {
        let mut bounds = Self {
            min_x: u32::MAX,
            min_y: u32::MAX,
            max_x: 0,
            max_y: 0,
        };
        for &(x, y) in level.keys() {
            bounds.include(x, y);
        }
        bounds
    }

    pub fn include(&mut self, x: u32, y: u32) {
        self.min_x = self.min_x.min(x);
        self.max_x = self.max_x.max(x);
        self.min_y = self.min_y.min(y);
        self.max_y = self.max_y.max(y);
    }
}

pub fn tile_span(zoom: u8) -> f64 {
    2.0 * HALF_WORLD / f64::from(1_u32 << zoom)
}

pub fn relative_path(zoom: u8, x: u32, row: u32, ext: &str) -> PathBuf {
    PathBuf::from(zoom.to_string())
        .join(x.to_string())
        .join(format!("{row}.{ext}"))
}

pub fn make_parent(path: &Path) -> Result<()> {
    fs::create_dir_all(parent(path))
        .with_context(|| message!(create_folder, parent(path).display()))
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

fn ensure_absent(path: &Path) -> Result<()> {
    match path.symlink_metadata() {
        Ok(_) => bail!(message!(destination_already_exists, path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).with_context(|| message!(inspect_destination, path.display())),
    }
}

pub fn publish_file(output: &Path, write: impl FnOnce(&mut File) -> Result<()>) -> Result<()> {
    ensure_absent(output)?;
    let mut temp = NamedTempFile::new_in(parent(output))
        .with_context(|| message!(create_temporary_file_in, parent(output).display()))?;
    write(temp.as_file_mut()).with_context(|| message!(write, output.display()))?;
    temp.as_file().sync_all()?;
    temp.persist_noclobber(output)
        .map_err(|error| error.error)
        .with_context(|| message!(publish, output.display()))?;
    Ok(())
}

pub fn publish_directory(output: &Path, write: impl FnOnce(&Path) -> Result<()>) -> Result<()> {
    ensure_absent(output)?;
    let temp = tempdir_in(parent(output))
        .with_context(|| message!(create_temporary_folder_in, parent(output).display()))?;
    write(temp.path()).with_context(|| message!(write, output.display()))?;
    rename_directory(temp.path(), output)
        .with_context(|| message!(publish_the_destination_must_not_exist, output.display()))?;
    let _ = temp.keep();
    Ok(())
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox"
))]
fn rename_directory(from: &Path, to: &Path) -> std::io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(Into::into)
}

#[cfg(windows)]
fn rename_directory(from: &Path, to: &Path) -> std::io::Result<()> {
    // Windows 的目录重命名会拒绝任何已经存在的目标。
    fs::rename(from, to)
}

#[cfg(not(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox",
    windows
)))]
fn rename_directory(_from: &Path, _to: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        message!(this_platform_does_not_support_atomic_folder_publishing_without_overwrite),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staged_outputs_are_cleaned_after_write_failure() {
        use std::io::Write;

        let dir = tempfile::tempdir().unwrap();
        let result = publish_file(&dir.path().join("result.bin"), |temp| {
            temp.write_all(b"partial content")?;
            Err(std::io::Error::other("injected write failure").into())
        });
        assert!(result.is_err());
        let result = publish_directory(&dir.path().join("directory"), |temp| {
            fs::write(temp.join("tile"), "partial content")?;
            Err(std::io::Error::other("injected write failure").into())
        });
        assert!(result.is_err());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn existing_destination_at_publish_time_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("result.bin");
        let result = publish_file(&file, |_| {
            fs::write(&file, "another writer")?;
            Ok(())
        });
        assert!(result.is_err());
        assert_eq!(fs::read_to_string(&file).unwrap(), "another writer");

        let output = dir.path().join("directory");
        let result = publish_directory(&output, |temp| {
            fs::write(temp.join("tile"), "staged")?;
            fs::create_dir(&output)?;
            Ok(())
        });
        assert!(result.is_err());
        assert!(output.is_dir());
        assert_eq!(fs::read_dir(&output).unwrap().count(), 0);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 2);
    }
}
