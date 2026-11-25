# WebGIS 工具箱

[English](README.md) | 简体中文

跨平台 egui 桌面应用，支持 OSGB 转 DOM / DSM、栅格处理、瓦片处理和 OSGB 模型裁剪。

克隆仓库后初始化固定版本的 `ctb-rs` 子模块。常规构建可跳过 Git LFS 样例栅格下载。

```sh
GIT_LFS_SKIP_SMUDGE=1 git submodule update --init --recursive
cargo build --locked
cargo run --locked
```

应用使用 Glow，在 Linux 上支持 Wayland 和 X11。应用在 Windows、macOS 和 Linux 上自动查找系统 CJK 字体。请将发布包的 `fonts/` 目录保留在可执行文件旁，以便离线回退。目录包含 Noto Sans CJK SC 和 JP（Sans 2.004）以及 SIL 开放字体许可证。可通过 `WEBGIS_CJK_FONT` 指定优先于系统和随包字体使用的字体文件。构建使用 Rust 工具链和系统链接器。编译、启动、所有工具及完整测试套件均无需 GDAL 库或命令行工具。

- **OSGB 转 DOM / DSM** 的模型输入可选择根 OSGB 文件或 `metadata.xml`。选择 `metadata.xml` 时优先从同目录的 `Block.osgb` 进入。转换器沿 PagedLOD 引用遍历场景树，保留各分支的叶节点几何，支持分支在不同 LOD 终止。无 `Block.osgb` 的目录会递归扫描文件，对符合 ContextCapture 命名约定的分片按模型组选最高 LOD。支持预检所选文件、LOD、坐标模式、范围和栅格尺寸。相对原点模式从 `metadata.xml` 读取坐标系及原点，支持手动覆盖。绝对坐标模式直接使用模型坐标，本地模式生成无 CRS 的栅格。默认像元尺寸为 0.05 个坐标单位。
- **栅格处理 → 生成 COG 云优化 GeoTIFF**。每次选择一个内嵌地理参考的 `.tif` 或 `.tiff`，然后指定保存位置，默认在源文件同目录生成 `<源文件名>_cog.tif`。支持条带或分块存储的 8 位灰度、灰度加 Alpha、RGB、RGBA 和单波段 Float32 高程 GeoTIFF。输出为 256 × 256 分块、无损 DEFLATE 压缩的 BigTIFF，内部概览采用最近邻抽样。保留原始分辨率的像元、Alpha、内部掩膜、NoData 和地理参考。任务可在发布前取消，输出须指定新文件路径。
- **栅格处理 → 生成地形瓦片**。选择单波段、北向上的 GeoTIFF 或 VRT DEM，并指定一个新目录。默认使用 WGS84（EPSG:4326），也可选择 Mercator（EPSG:3857）。最低缩放级别留空时从 0 开始，最高缩放级别留空时根据源数据分辨率推导。后台任务显示已完成与总瓦片数，运行至成功或报错。输出为 `{z}/{x}/{y}.terrain` 格式的 TMS heightmap-1.0 瓦片。`layer.json`、quantized-mesh 和瓦片发布服务留待后续集成。
- **瓦片处理**支持 XYZ / TMS 单级拼接、生成多级瓦片（基于一个源级别生成指定级别）和生成 STACTA，也可将带坐标系的 8 位 GeoTIFF 重投影为 256×256 WebMercatorQuad PNG 切片（默认 6–18 级）并生成 STACTA。从已有 XYZ 瓦片生成索引时，索引写到瓦片源目录。TMS 会将瓦片转换为 XYZ 行号后写入新目录，并生成索引。
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
