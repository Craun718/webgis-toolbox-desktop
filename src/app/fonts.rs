use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use ab_glyph::{Font, FontRef};
use eframe::egui::{self, Context};
use webgis_core::i18n::Language;

pub(super) fn install_font(ctx: &Context, language: Language) -> bool {
    let japanese = language == Language::Japanese;
    let mut fonts = egui::FontDefinitions::default();
    let configured = std::env::var_os("WEBGIS_CJK_FONT").map(PathBuf::from);
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join("fonts")));
    let (candidates, primary_available) =
        candidates(configured, bundled.as_deref(), japanese, system_font);
    for (index, font) in candidates.into_iter().enumerate() {
        let name = format!("CJK {index}");
        fonts.font_data.insert(name.clone(), Arc::new(font));
        for family in fonts.families.values_mut() {
            family.push(name.clone());
        }
    }
    ctx.set_fonts(fonts);
    primary_available
}

fn candidates(
    configured: Option<PathBuf>,
    bundled: Option<&Path>,
    japanese: bool,
    system: impl FnOnce(bool) -> Option<egui::FontData>,
) -> (Vec<egui::FontData>, bool) {
    let mut candidates = Vec::new();
    if let Some(path) = configured {
        let preferred = if path
            .file_name()
            .is_some_and(|name| name == "NotoSansCJK-Regular.ttc")
            && !japanese
        {
            2
        } else {
            0
        };
        if let Ok(bytes) = std::fs::read(path) {
            let indices = std::iter::once(preferred)
                .chain(0..ttf_parser::fonts_in_collection(&bytes).unwrap_or(1));
            if let Some(index) = indices
                .into_iter()
                .find(|&index| supports(&bytes, index, japanese))
            {
                candidates.push(font_data(bytes, index));
            }
        }
    }
    if let Some(font) = system(japanese) {
        candidates.push(font);
    }
    let primary_available = if let Some(dir) = bundled {
        let primary = bundled_font(dir, japanese);
        let available = !candidates.is_empty() || primary.is_some();
        candidates.extend(primary);
        candidates.extend(bundled_font(dir, !japanese));
        available
    } else {
        !candidates.is_empty()
    };
    (candidates, primary_available)
}

fn supports(bytes: &[u8], index: u32, japanese: bool) -> bool {
    let sample = if japanese {
        "日本語あカ。ー"
    } else {
        "简体中文地图，。"
    };
    FontRef::try_from_slice_and_index(bytes, index)
        .is_ok_and(|font| sample.chars().all(|ch| font.glyph_id(ch).0 != 0))
}

fn font_data(bytes: Vec<u8>, index: u32) -> egui::FontData {
    let mut font = egui::FontData::from_owned(bytes);
    font.index = index;
    font
}

fn load(path: &Path, index: u32, japanese: bool) -> Option<egui::FontData> {
    let bytes = std::fs::read(path).ok()?;
    supports(&bytes, index, japanese).then(|| font_data(bytes, index))
}

fn bundled_font(dir: &Path, japanese: bool) -> Option<egui::FontData> {
    let name = if japanese {
        "NotoSansCJKjp-Regular.otf"
    } else {
        "NotoSansCJKsc-Regular.otf"
    };
    load(&dir.join(name), 0, japanese)
}

#[cfg(target_os = "linux")]
fn system_font(japanese: bool) -> Option<egui::FontData> {
    linux_candidates(japanese).find_map(|(path, index)| load(&path, index, japanese))
}

#[cfg(target_os = "linux")]
fn linux_candidates(japanese: bool) -> impl Iterator<Item = (PathBuf, u32)> {
    let family = if japanese {
        "Noto Sans CJK JP"
    } else {
        "Noto Sans CJK SC"
    };
    let system_match = std::process::Command::new("fc-match")
        .args(["-f", "%{family}\n%{file}\n%{index}", family])
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| {
            let output = String::from_utf8(output.stdout).ok()?;
            let mut lines = output.lines();
            let matches = lines.next()?.split(',').any(|name| name == family);
            let path = PathBuf::from(lines.next()?);
            let index = lines.next()?.parse().ok()?;
            matches.then_some((path, index))
        });
    let noto_index = if japanese { 0 } else { 2 };
    let noto = [
        "/usr/share/fonts/noto-cjk/NotoSansCJK-Regular.ttc",
        "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
    ]
    .map(|path| (PathBuf::from(path), noto_index));
    let chinese = [
        "/usr/share/fonts/adobe-source-han-sans/SourceHanSansCN-Regular.otf",
        "/usr/share/fonts/wenquanyi/wqy-microhei/wqy-microhei.ttc",
        "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
    ]
    .map(|path| (PathBuf::from(path), 0));
    system_match
        .into_iter()
        .chain(noto)
        .chain(chinese.into_iter().filter(move |_| !japanese))
}

#[cfg(any(target_os = "windows", target_os = "macos"))]
fn system_font(japanese: bool) -> Option<egui::FontData> {
    use fontdb::{Database, Style, Weight};
    use std::sync::OnceLock;

    static DATABASE: OnceLock<Database> = OnceLock::new();
    let database = DATABASE.get_or_init(|| {
        let mut database = Database::new();
        database.load_system_fonts();
        database
    });
    let preferred: &[&str] = if japanese {
        &[
            "Yu Gothic",
            "Meiryo",
            "Hiragino Sans",
            "Hiragino Kaku Gothic ProN",
            "Noto Sans CJK JP",
            "Noto Sans JP",
        ]
    } else {
        &[
            "Microsoft YaHei",
            "PingFang SC",
            "Noto Sans CJK SC",
            "Noto Sans SC",
            "Source Han Sans SC",
            "SimHei",
            "SimSun",
        ]
    };
    let mut faces: Vec<_> = database.faces().collect();
    faces.sort_by_key(|face| {
        let rank = preferred.iter().position(|name| {
            face.families
                .iter()
                .any(|(family, _)| family.eq_ignore_ascii_case(name))
        });
        (
            rank.unwrap_or(preferred.len()),
            face.style != Style::Normal,
            face.weight.0.abs_diff(Weight::NORMAL.0),
            &face.post_script_name,
            face.index,
        )
    });
    faces.into_iter().find_map(|face| {
        database
            .with_face_data(face.id, |bytes, index| {
                supports(bytes, index, japanese).then(|| font_data(bytes.to_vec(), index))
            })
            .flatten()
    })
}

#[cfg(not(any(target_os = "linux", target_os = "windows", target_os = "macos")))]
fn system_font(_: bool) -> Option<egui::FontData> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundled_fonts_cover_their_language() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts");
        assert!(bundled_font(&dir, false).is_some());
        assert!(bundled_font(&dir, true).is_some());
    }

    #[test]
    fn configured_system_and_bundled_fonts_are_prioritized() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts");
        let override_path = dir.join("NotoSansCJKjp-Regular.otf");
        let system = |_| Some(font_data(Vec::new(), 42));
        let (fonts, available) = candidates(Some(override_path), Some(&dir), true, system);
        assert!(available);
        assert_eq!(fonts.len(), 4);
        assert_eq!(fonts[0].index, 0);
        assert_eq!(fonts[1].index, 42);

        let invalid = tempfile::NamedTempFile::new().unwrap();
        let system = |_| Some(font_data(Vec::new(), 42));
        let (fonts, available) = candidates(Some(invalid.path().into()), Some(&dir), true, system);
        assert!(available);
        assert_eq!(fonts.len(), 3);
        assert_eq!(fonts[0].index, 42);

        let (_, available) = candidates(None, None, true, |_| None);
        assert!(!available);
    }

    #[test]
    fn configured_collection_uses_a_supported_nonzero_face() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts");
        let mut face = std::fs::read(dir.join("NotoSansCJKsc-Regular.otf")).unwrap();

        // Face zero is invalid. Face one is an OpenType face whose table
        // offsets, as required by TTC, are relative to the collection start.
        let mut collection = b"ttcf\0\x01\0\0\0\0\0\x02\0\0\0\0\0\0\0\x14".to_vec();
        let table_count = u16::from_be_bytes([face[4], face[5]]) as usize;
        for record in face[12..12 + table_count * 16].chunks_exact_mut(16) {
            let offset = u32::from_be_bytes(record[8..12].try_into().unwrap());
            record[8..12].copy_from_slice(&(offset + collection.len() as u32).to_be_bytes());
        }
        collection.extend(face);
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), collection).unwrap();
        let (fonts, available) = candidates(Some(file.path().into()), None, false, |_| None);
        assert!(available);
        assert_eq!(fonts[0].index, 1);
    }

    #[test]
    fn invalid_font_is_rejected() {
        assert!(!supports(b"invalid font", 0, false));
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("fonts");
        assert!(load(&dir.join("NotoSansCJKsc-Regular.otf"), 1, false).is_none());
    }
}
