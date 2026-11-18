# WebGIS Toolbox

English | [简体中文](README.zh-CN.md)

WebGIS Toolbox is a Linux desktop application for converting OSGB models to DOM and DSM rasters, processing map tiles, and clipping OSGB models.

## Build and run

Build with the Rust toolchain and a system linker.

```sh
cargo build --locked
./target/debug/webgis-toolbox
```

The Chinese-language egui interface uses Glow and supports Wayland and X11. It looks for Source Han Sans, Noto Sans CJK, or WenQuanYi Micro Hei on the system. Set `WEBGIS_CJK_FONT` to use a specific font file.

## Tools

- **OSGB to DOM and DSM.** Preview the selected files, level of detail (LOD), coordinate mode, extent, and raster dimensions. For inputs that follow the ContextCapture LOD naming convention, the converter uses all tiles at the highest LOD in each model group. Relative-origin mode reads the coordinate reference system (CRS) and origin from `metadata.xml` and accepts manual overrides. Absolute mode uses model coordinates directly. Local mode produces rasters without a CRS. The default pixel size is 0.05 coordinate units.
- **Tile processing.** Mosaic a single XYZ or TMS level into a GeoTIFF, generate a tile pyramid, or build a STACTA index. The STACTA tool writes the index into the source directory for XYZ inputs. For TMS inputs it converts tile row numbers to XYZ and writes the tiles and index to a new directory. The map tiling tool reprojects georeferenced 8-bit GeoTIFFs into 256 × 256 WebMercatorQuad PNG tiles and writes a STACTA index. The default zoom range is 6–18.
- **OSGB model clipping.** Clip a single file or a paged model directory. Directory inputs read `metadata.xml` and accept an optional OSGB entry path. Single-file inputs require the model CRS and origin. Clipping boundaries can be SHP, zipped SHP, KML, or GeoJSON. If a ZIP contains multiple shapefiles, load the layer list and select the layer to use.

## GDAL runtime and raster output

OSGB raster conversion and GeoTIFF map tiling load GDAL 3.9 or later at runtime. Set `WEBGIS_GDAL_LIBRARY` if the shared library is installed in a nonstandard location. The tile post-processing and clipping tools can run independently of GDAL.

```sh
WEBGIS_GDAL_LIBRARY=/path/to/libgdal.so ./target/debug/webgis-toolbox
```

The OSGB raster converter supports little-endian ContextCapture v131 files with uncompressed streams and embedded or external JPEG and PNG textures. Errors for unsupported versions, scenes, or textures identify the affected file.

Conversion produces `dsm.tif`, `dom.tif`, and `run-report.json`. The Float32 DSM uses a NoData value of −9999. The RGB DOM uses an internal mask to mark valid texture pixels. Both rasters are tiled, DEFLATE-compressed BigTIFFs with internal overviews when their dimensions allow. Overwriting existing conversion outputs requires confirmation. Raster conversion, map tiling, and model clipping can be cancelled before results are published.

## Tests

```sh
cargo test --workspace --locked
cargo test --locked gdal_writes_verified_products_and_cancellation_preserves_them -- --ignored
```

The GeoTIFF map tiling tests and the separately enabled DOM/DSM integration test require the GDAL runtime. Test coverage includes LOD selection, coordinate transforms and origin offsets, elevation occlusion, texture orientation, masks, NoData, block consistency, cancellation, and publication rollback.

## License

Licensed under the [Apache License 2.0](LICENSE).
