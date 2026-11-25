use super::{Outcome, PathPicker, field, path_field, required_path};
use anyhow::{Context, Result};
use ctb_rs::{
    cache::CachedRasterSource,
    geotiff::GeoTiffRasterSource,
    grid::{GlobalGeodeticGrid, GlobalMercatorGrid, TileGrid},
    raster::RasterSource,
    sampling::ResamplingMethod::Average,
    tileset::{HeightmapTilesetOptions, TileWriteProgress, write_heightmap_tileset_with_factory},
};
use eframe::egui::{ComboBox, Context as EguiContext, Ui};
use std::sync::{Arc, Mutex};
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

#[derive(Default)]
pub(super) struct Form {
    input: String,
    output: String,
    profile: Profile,
    min_zoom: String,
    max_zoom: String,
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
        field(ui, message!(terrain_min_zoom), &mut self.min_zoom, "0");
        field(
            ui,
            message!(terrain_max_zoom),
            &mut self.max_zoom,
            message!(zoom_from_source_resolution),
        );
        ui.label(message!(terrain_output_description));
    }

    pub(super) fn start(&self, ctx: &EguiContext, name: &str) -> Result<Worker> {
        let input = required_path(message!(terrain_dem), &self.input)?;
        let output = required_path(message!(tile_output_folder), &self.output)?;
        let profile = self.profile;
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
                write_heightmap_tileset_with_factory(
                    &source_factory,
                    grid.as_ref(),
                    staging,
                    options,
                    Some(&progress),
                )?;
                task.publish_with_progress(Some(0.95))
            })?;
            Ok(Outcome::Artifacts {
                paths: vec![output],
                details: None,
            })
        })
    }
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
