# WebGIS Toolbox

English | [简体中文](README.zh-CN.md)

WebGIS Toolbox is a Linux desktop application for converting OSGB models to DOM and DSM rasters, processing GeoTIFFs and map tiles, and clipping OSGB models.

## Build and run

Build with the Rust toolchain and a system linker. Building, running every tool, and running the workspace tests require no GDAL library or executables.

```sh
cargo build --locked
./target/debug/webgis-toolbox
```

The Chinese-language egui interface uses Glow and supports Wayland and X11. It looks for Source Han Sans, Noto Sans CJK, or WenQuanYi Micro Hei on the system. Set `WEBGIS_CJK_FONT` to use a specific font file.

## Tools

- **OSGB to DOM and DSM.** Select a root OSGB file or the model’s `metadata.xml`. Selecting `metadata.xml` scans its directory and subdirectories for OSGB files. Preview the selected files, level of detail (LOD), coordinate mode, extent, and raster dimensions. For inputs that follow the ContextCapture LOD naming convention, the converter uses all tiles at the highest LOD in each model group. Relative-origin mode reads the coordinate reference system (CRS) and origin from `metadata.xml` and accepts manual overrides. Absolute mode uses model coordinates directly. Local mode produces rasters without a CRS. The default pixel size is 0.05 coordinate units.
- **Raster processing → Generate Cloud Optimized GeoTIFF (COG).** Select one georeferenced `.tif` or `.tiff` and choose a save location. The default is `<source_stem>_cog.tif` beside the source. Supports stripped or tiled 8-bit grayscale, grayscale with alpha, RGB, RGBA, and single-band Float32 elevation GeoTIFFs. Output uses lossless DEFLATE, 256 × 256 BigTIFF tiles, and nearest-neighbor internal overviews. Original-resolution samples, alpha, internal masks, NoData, and georeferencing are preserved. The task can be cancelled before publication and requires a new output path.
- **Tile processing.** Mosaic a single XYZ or TMS level into a GeoTIFF, generate a tile pyramid, or build a STACTA index. The STACTA tool writes the index into the source directory for XYZ inputs. For TMS inputs it converts tile row numbers to XYZ and writes the tiles and index to a new directory. The map tiling tool reprojects georeferenced 8-bit GeoTIFFs into 256 × 256 WebMercatorQuad PNG tiles and writes a STACTA index. The default zoom range is 6–18.
- **OSGB model clipping.** Clip a single file or a paged model directory. Directory inputs read `metadata.xml` and accept an optional OSGB entry path. Single-file inputs require the model CRS and origin. Clipping boundaries can be SHP, zipped SHP, KML, or GeoJSON. If a ZIP contains multiple shapefiles, load the layer list and select the layer to use.

## Raster formats and coordinate systems

The OSGB raster converter supports little-endian ContextCapture v131 files with uncompressed streams and embedded or external JPEG and PNG textures. For absolute and relative modes, use `EPSG:<code>` or WKT with a top-level EPSG `AUTHORITY` (WKT1) or `ID` (WKT2). Local mode writes georeferencing in model units without a CRS.

Conversion produces `dsm.tif`, `dom.tif`, and `run-report.json`. The Float32 DSM uses a NoData value of −9999. The RGB DOM stores a 1-bit internal validity mask. Both are DEFLATE-compressed BigTIFFs with 256 × 256 tiles and internal 2×, 4×, 8×, and 16× overviews as dimensions permit. Overviews average valid pixels. Overwriting existing conversion outputs requires confirmation. Raster conversion, map tiling, and model clipping can be cancelled before publication.

Map tiling reads Classic TIFF and BigTIFF with strips or tiles, 8-bit grayscale, grayscale with alpha, RGB, or RGBA, and uncompressed, LZW, DEFLATE, or JPEG image data. The source needs an embedded EPSG GeoKey and pixel scale plus tiepoint or affine transformation tags. Both PixelIsArea and PixelIsPoint are supported. Tiling applies alpha, internal masks, and NoData during bilinear upsampling or area averaging and omits fully transparent tiles. The decoder limits each individual strip or tile to 64 MiB. Large rasters with smaller blocks are read incrementally.

CRSs come from a built-in EPSG registry and include EPSG:4326, 3857, 4490, 4547, and WGS 84 UTM zones such as EPSG:32649. Unsupported CRS definitions, required external grids, and missing georeferencing are reported with the input filename.

## Tests

```sh
cargo test --workspace --locked
```

Tests read TIFF tags, pixels, and STACTA indexes using Rust. Coverage includes coordinate modes, block boundaries, rotated and point-based georeferencing, resampling, masks, NoData, cancellation, and publication rollback.

## License

Licensed under the [Apache License 2.0](LICENSE).
