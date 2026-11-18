# WebGIS 工具箱

[English](README.md) | 简体中文

Linux 上的中文 egui 桌面应用，支持 OSGB 转 DOM / DSM、瓦片处理和 OSGB 模型裁剪。

```sh
cargo build --locked
./target/debug/webgis-toolbox
```

应用使用 Glow，支持 Wayland 和 X11。界面优先加载系统的思源黑体、Noto Sans CJK 或文泉驿微米黑，也可通过 `WEBGIS_CJK_FONT` 指定字体文件。工作区内的 Rust 库提供三项处理功能，构建使用 Rust 工具链和常规系统链接器。

- **OSGB 转 DOM / DSM**支持预检所选文件、LOD、坐标模式、范围和栅格尺寸。目录内存在 ContextCapture LOD 命名文件时，每个模型组选最高级别的全部分片。相对原点模式从 `metadata.xml` 读取坐标系及原点，支持手动覆盖。绝对坐标模式直接使用模型坐标，本地模式生成无 CRS 的栅格。默认像元尺寸为 0.05 个坐标单位。
- **瓦片处理**支持 XYZ / TMS 单级拼接、生成金字塔及 STACTA，也可将带坐标系的 8 位 GeoTIFF 重投影为 256×256 WebMercatorQuad PNG 切片（默认 6–18 级）并生成 STACTA。GeoTIFF 切片使用 GDAL 3.9 或更新的动态运行库；XYZ 索引写到瓦片源目录。TMS 会将瓦片转换为 XYZ 行号后写入新目录，并生成索引。
- **OSGB 模型裁剪**支持单文件和分页目录，可为目录指定入口路径。目录模式读取模型的 `metadata.xml`，单文件模式需填写坐标系和原点。裁剪范围支持 SHP、ZIP 内 SHP、KML 和 GeoJSON。ZIP 含多个 SHP 时先读取图层列表，再选择一个图层。

## DOM / DSM 运行库

首次使用 DOM / DSM 时，应用动态加载 GDAL 3.9 或更新的预编译 `libgdal`。如运行库安装在非标准位置，可显式指定其路径。

```sh
WEBGIS_GDAL_LIBRARY=/绝对路径/libgdal.so ./target/debug/webgis-toolbox
```

常规瓦片和裁剪可独立于 GDAL 运行。GeoTIFF 地图切片使用同一 GDAL 3.9+ 运行库。支持的栅格化输入以 ContextCapture v131 小端、无流压缩的 OSGB 为基线，支持 JPEG / PNG 内嵌或外部纹理。其他序列化版本、未支持的场景或纹理格式会提示文件和原因。

DOM / DSM 写出 `dsm.tif`、`dom.tif` 和 `run-report.json`。DSM 为 Float32，NoData 为 -9999。DOM 为 RGB，内部掩膜标记有效纹理。两个栅格均为分块 Deflate BigTIFF，尺寸允许时生成内部概览。只有明确确认后才会覆盖同名产物。DOM / DSM、地图切片和裁剪可在发布前取消。

## 测试

```sh
cargo test --workspace --locked
cargo test --locked gdal_writes_verified_products_and_cancellation_preserves_them -- --ignored
```

第二条命令需要 GDAL 运行库。测试覆盖 LOD 选择、场景变换和原点偏移、高程遮挡、纹理方向、掩膜、NoData、分块一致性、取消和发布回滚。

## 许可证

本项目采用 [Apache License 2.0](LICENSE)。
