# WebGIS 工具箱

[English](README.md) | 简体中文

Linux 上的中文 egui 桌面应用，支持 OSGB 转 DOM / DSM、瓦片处理和 OSGB 模型裁剪。

```sh
cargo build --locked
./target/debug/webgis-toolbox
```

应用使用 Glow，支持 Wayland 和 X11。界面优先加载系统的思源黑体、Noto Sans CJK 或文泉驿微米黑，也可通过 `WEBGIS_CJK_FONT` 指定字体文件。构建使用 Rust 工具链和系统链接器。编译、启动、所有工具及完整测试套件均无需 GDAL 库或命令行工具。

- **OSGB 转 DOM / DSM**支持预检所选文件、LOD、坐标模式、范围和栅格尺寸。目录内存在 ContextCapture LOD 命名文件时，每个模型组选最高级别的全部分片。相对原点模式从 `metadata.xml` 读取坐标系及原点，支持手动覆盖。绝对坐标模式直接使用模型坐标，本地模式生成无 CRS 的栅格。默认像元尺寸为 0.05 个坐标单位。
- **瓦片处理**支持 XYZ / TMS 单级拼接、生成金字塔及 STACTA，也可将带坐标系的 8 位 GeoTIFF 重投影为 256×256 WebMercatorQuad PNG 切片（默认 6–18 级）并生成 STACTA。从已有 XYZ 瓦片生成索引时，索引写到瓦片源目录。TMS 会将瓦片转换为 XYZ 行号后写入新目录，并生成索引。
- **OSGB 模型裁剪**支持单文件和分页目录，可为目录指定入口路径。目录模式读取模型的 `metadata.xml`，单文件模式需填写坐标系和原点。裁剪范围支持 SHP、ZIP 内 SHP、KML 和 GeoJSON。ZIP 含多个 SHP 时先读取图层列表，再选择一个图层。

## 栅格格式与坐标系

OSGB 栅格化支持 ContextCapture v131 小端、无流压缩的 OSGB 及 JPEG、PNG 内嵌或外部纹理。绝对和相对坐标模式接受 `EPSG:代码`，也接受带顶层 EPSG `AUTHORITY`（WKT1）或 `ID`（WKT2）的 WKT。本地模式使用模型单位写入地理定位，栅格不含 CRS。

转换写出 `dsm.tif`、`dom.tif` 和 `run-report.json`。DSM 为 Float32，NoData 为 −9999。DOM 为 RGB，使用内部 1 位掩膜标记有效纹理。两种栅格均为 256 × 256 分块、DEFLATE 压缩的 BigTIFF，尺寸允许时生成 2、4、8 和 16 倍内部概览，并按有效像元求均值。覆盖同名产物需明确确认。OSGB 栅格化、地图切片和模型裁剪均可在发布前取消。

地图切片读取条带或分块存储的 Classic TIFF 和 BigTIFF，支持 8 位灰度、灰度 Alpha、RGB 和 RGBA，以及未压缩、LZW、DEFLATE、JPEG 编码。影像需内嵌 EPSG GeoKey，并具备像元比例与控制点或仿射矩阵，支持 PixelIsArea 和 PixelIsPoint。放大采用双线性插值，缩小采用面积平均。Alpha、内部掩膜及 NoData 参与重采样，全透明瓦片省略。单个条带或瓦片的解码上限为 64 MiB，使用较小数据块的大影像可分块读取。

坐标系使用内置 EPSG 定义，覆盖 EPSG:4326、3857、4490、4547，以及 WGS 84 UTM 分带（如 EPSG:32649）。遇到不支持的坐标系、所需外部格网或缺少地理定位时，会提示输入文件及原因。

## 测试

```sh
cargo test --workspace --locked
```

测试使用 Rust 检查 TIFF 标签、像元及 STACTA 索引，覆盖坐标模式、分块边缘、旋转仿射、点像元定位、重采样、掩膜、NoData、取消和发布回滚。

## 许可证

本项目采用 [Apache License 2.0](LICENSE)。
