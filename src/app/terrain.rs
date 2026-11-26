mod mesh;

use super::{Outcome, PathPicker, field, path_field, required_path};
use anyhow::{Context, Result};
use ctb_rs::{
    cache::CachedRasterSource,
    geotiff::GeoTiffRasterSource,
    grid::{GlobalGeodeticGrid, GlobalMercatorGrid, TileGrid},
    raster::RasterSource,
    sampling::ResamplingMethod::Average,
    tileset::{
        HeightmapTilesetOptions, TileWriteProgress, TilesetPlan, terrain_path,
        write_heightmap_tileset_with_factory, write_layer_json,
    },
};
use eframe::egui::{ComboBox, Context as EguiContext, Ui};
use flate2::{Compression, write::GzEncoder};
use rayon::prelude::*;
use std::{
    io::Write,
    sync::{Arc, Mutex},
};
use tiles_toolbox::tiles::publish_directory;
use webgis_core::{i18n, message};
use webgis_toolbox::dom::{Phase, TaskProgress};

use super::task::Worker;

#[derive(Clone, Copy, Default, PartialEq, Eq)]
enum Profile {
    #[default]
    Geodetic,
    Mercator,
}

impl Profile {
    fn name(self) -> &'static str {
        match self {
            Self::Geodetic => "WGS84 (EPSG:4326)",
            Self::Mercator => "Mercator (EPSG:3857)",
        }
    }

    fn grid(self) -> Result<Box<dyn TileGrid>> {
        match self {
            Self::Geodetic => Ok(Box::new(GlobalGeodeticGrid::new(65)?)),
            Self::Mercator => Ok(Box::new(GlobalMercatorGrid::new(256)?)),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Format {
    Heightmap,
    #[default]
    QuantizedMesh,
}

impl Format {
    fn name(self) -> &'static str {
        match self {
            Self::Heightmap => "heightmap",
            Self::QuantizedMesh => "quantized-mesh",
        }
    }
}

pub(super) struct Form {
    format: Format,
    gzip: bool,
    input: String,
    output: String,
    profile: Profile,
    min_zoom: String,
    max_zoom: String,
    generate_layer_json: bool,
}

impl Default for Form {
    fn default() -> Self {
        Self {
            format: Format::default(),
            gzip: true,
            input: String::new(),
            output: String::new(),
            profile: Profile::default(),
            min_zoom: String::new(),
            max_zoom: String::new(),
            generate_layer_json: true,
        }
    }
}

impl Form {
    pub(super) fn ui(&mut self, ui: &mut Ui) {
        path_field(
            ui,
            message!(terrain_dem),
            &mut self.input,
            message!(single_band_geotiff_or_vrt),
            PathPicker::File("GeoTIFF / VRT", &["tif", "tiff", "vrt"]),
        );
        path_field(
            ui,
            message!(tile_output_folder),
            &mut self.output,
            message!(new_folder_2),
            PathPicker::OutputFolder("terrain"),
        );
        ui.horizontal(|ui| {
            ui.label(message!(terrain_profile));
            ComboBox::from_id_salt("terrain-profile")
                .selected_text(self.profile.name())
                .show_ui(ui, |ui| {
                    for profile in [Profile::Geodetic, Profile::Mercator] {
                        ui.selectable_value(&mut self.profile, profile, profile.name());
                    }
                });
        });
        ui.horizontal(|ui| {
            ui.label(message!(terrain_format));
            ComboBox::from_id_salt("terrain-format")
                .selected_text(self.format.name())
                .show_ui(ui, |ui| {
                    for format in [Format::Heightmap, Format::QuantizedMesh] {
                        ui.selectable_value(&mut self.format, format, format.name());
                    }
                });
        });
        ui.checkbox(&mut self.gzip, message!(terrain_gzip));
        field(ui, message!(terrain_min_zoom), &mut self.min_zoom, "0");
        field(
            ui,
            message!(terrain_max_zoom),
            &mut self.max_zoom,
            message!(zoom_from_source_resolution),
        );
        ui.checkbox(
            &mut self.generate_layer_json,
            message!(terrain_generate_layer_json),
        );
        ui.label(message!(terrain_output_description));
    }

    pub(super) fn start(&self, ctx: &EguiContext, name: &str) -> Result<Worker> {
        let input = required_path(message!(terrain_dem), &self.input)?;
        let output = required_path(message!(tile_output_folder), &self.output)?;
        let profile = self.profile;
        let format = self.format;
        let gzip = self.gzip;
        let generate_layer_json = self.generate_layer_json;
        let options = HeightmapTilesetOptions {
            resume: false,
            start_zoom: parse_zoom(&self.max_zoom, message!(terrain_max_zoom))?,
            end_zoom: parse_zoom(&self.min_zoom, message!(terrain_min_zoom))?,
            resampling: Average,
            worker_count: std::thread::available_parallelism().map_or(1, |count| count.get()),
        };
        Worker::start(ctx, name, false, move |task| {
            let grid = profile.grid()?;
            let language = i18n::language();
            publish_directory(&output, |staging| {
                task.update(TaskProgress::new(
                    Phase::GenerateTerrain,
                    message!(generate_terrain_tiles),
                    None,
                ));
                let shared_cache = GeoTiffRasterSource::new_shared_block_cache(&input).ok();
                let source_factory = || -> Result<Box<dyn RasterSource>, _> {
                    // 与 ctb-tile 一致，原生共享缓存可用时不再叠加转换后的块缓存。
                    match &shared_cache {
                        Some(cache) => Ok(Box::new(GeoTiffRasterSource::open_with_shared_cache(
                            &input,
                            Arc::clone(cache),
                        )?)),
                        None => Ok(Box::new(CachedRasterSource::new_with_nodata_cache(
                            GeoTiffRasterSource::open(&input)?,
                            64,
                            64,
                        ))),
                    }
                };
                let last_completed = Mutex::new(0);
                let progress = |event: TileWriteProgress| {
                    let mut last = last_completed.lock().unwrap();
                    if event.completed > *last {
                        *last = event.completed;
                        i18n::with_language(language, || {
                            task.update(TaskProgress::new(
                                Phase::GenerateTerrain,
                                message!(terrain_tile_progress, event.completed, event.total),
                                Some(0.95 * (event.completed as f32 / event.total as f32)),
                            ));
                        });
                    }
                };
                let plan = if format == Format::QuantizedMesh {
                    let source = source_factory()?;
                    let plan = TilesetPlan::from_raster_with_tile_grid(
                        source.metadata(),
                        grid.as_ref(),
                        options.start_zoom,
                        options.end_zoom,
                    )?;
                    let tiles: Vec<_> = plan
                        .levels
                        .iter()
                        .flat_map(|level| level.tiles.iter().copied())
                        .collect();
                    tiles.par_iter().try_for_each_init(
                        || source_factory(),
                        |source, tile| -> Result<()> {
                            let source = source.as_ref().map_err(|e| anyhow::anyhow!("{e}"))?;
                            let bytes = mesh::encode(source.as_ref(), grid.as_ref(), *tile)?;
                            let path = terrain_path(staging, *tile);
                            std::fs::create_dir_all(path.parent().unwrap())?;
                            std::fs::write(path, encode_payload(&bytes, gzip)?)?;
                            // Serialize completion accounting together with progress delivery.
                            let mut completed = last_completed.lock().unwrap();
                            *completed += 1;
                            i18n::with_language(language, || {
                                task.update(TaskProgress::new(
                                    Phase::GenerateTerrain,
                                    message!(terrain_tile_progress, *completed, tiles.len()),
                                    Some(0.95 * (*completed as f32 / tiles.len() as f32)),
                                ))
                            });
                            Ok(())
                        },
                    )?;
                    plan
                } else {
                    let plan = write_heightmap_tileset_with_factory(
                        &source_factory,
                        grid.as_ref(),
                        staging,
                        options,
                        Some(&progress),
                    )?;
                    if !gzip {
                        for tile in plan.levels.iter().flat_map(|level| &level.tiles) {
                            let path = terrain_path(staging, *tile);
                            let terrain = ctb_rs::terrain::HeightmapTerrain::read_gzip(&path)?;
                            std::fs::write(path, terrain.encode_raw()?)?;
                        }
                    }
                    plan
                };
                if generate_layer_json {
                    write_layer_json(staging, grid.as_ref(), &plan)?;
                    if format == Format::QuantizedMesh {
                        let path = staging.join("layer.json");
                        let mut layer: serde_json::Value =
                            serde_json::from_slice(&std::fs::read(&path)?)?;
                        layer["format"] = "quantized-mesh-1.0".into();
                        std::fs::write(path, serde_json::to_vec_pretty(&layer)?)?;
                    }
                }
                task.publish_with_progress(Some(0.95))
            })?;
            Ok(Outcome::Artifacts {
                paths: vec![output],
                details: None,
            })
        })
    }
}

fn encode_payload(bytes: &[u8], gzip: bool) -> Result<Vec<u8>> {
    if !gzip {
        return Ok(bytes.to_vec());
    }
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

fn parse_zoom(value: &str, label: &str) -> Result<Option<u8>> {
    let value = value.trim();
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        value
            .parse()
            .with_context(|| message!(invalid_terrain_zoom, label))?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn gzip_option_controls_payload() -> Result<()> {
        use std::io::Read;
        let raw = b"terrain payload";
        assert_eq!(encode_payload(raw, false)?, raw);
        let compressed = encode_payload(raw, true)?;
        assert_eq!(&compressed[..2], &[0x1f, 0x8b]);
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(compressed.as_slice()).read_to_end(&mut decoded)?;
        assert_eq!(decoded, raw);
        Ok(())
    }
    #[test]
    fn layer_json_is_enabled_by_default() {
        let form = Form::default();
        assert!(form.generate_layer_json);
        assert!(form.gzip);
        assert_eq!(form.format, Format::QuantizedMesh);
    }
}
