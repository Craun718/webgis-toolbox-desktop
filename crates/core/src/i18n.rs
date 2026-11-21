//! Shared message catalog and task-local language selection.

use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    Chinese,
    English,
    Japanese,
}

impl Language {
    pub const ALL: [Self; 3] = [Self::Chinese, Self::English, Self::Japanese];

    pub const fn code(self) -> &'static str {
        match self {
            Self::Chinese => "zh-CN",
            Self::English => "en",
            Self::Japanese => "ja",
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Chinese => "简体中文",
            Self::English => "English",
            Self::Japanese => "日本語",
        }
    }

    pub fn from_code(code: &str) -> Self {
        Self::ALL
            .into_iter()
            .find(|language| language.code() == code)
            .unwrap_or_default()
    }
}

thread_local! {
    static LANGUAGE: Cell<Language> = const { Cell::new(Language::Chinese) };
}

pub fn language() -> Language {
    LANGUAGE.get()
}

/// Sets the language for work on this thread and restores it on return or unwind.
/// Capture and reapply the language when dispatching work onto other threads.
pub fn with_language<T>(language: Language, work: impl FnOnce() -> T) -> T {
    struct Restore(Language);

    impl Drop for Restore {
        fn drop(&mut self) {
            LANGUAGE.set(self.0);
        }
    }

    let _restore = Restore(LANGUAGE.replace(language));
    work()
}

#[doc(hidden)]
#[macro_export]
macro_rules! localized_template {
    ($zh:literal, $en:literal, $ja:literal $(,)?) => {
        match $crate::i18n::language() {
            $crate::i18n::Language::Chinese => $zh,
            $crate::i18n::Language::English => $en,
            $crate::i18n::Language::Japanese => $ja,
        }
    };
    ($zh:literal, $en:literal, $ja:literal; $($args:tt)+) => {
        match $crate::i18n::language() {
            $crate::i18n::Language::Chinese => format!($zh, $($args)+),
            $crate::i18n::Language::English => format!($en, $($args)+),
            $crate::i18n::Language::Japanese => format!($ja, $($args)+),
        }
    };
}

#[macro_export]
macro_rules! message {
    (osgb_raster_geometry) => {
        $crate::localized_template!("OSGB 页几何", "OSGB page geometry", "OSGB ページのジオメトリ")
    };
    (osgb_clip_page) => {
        $crate::localized_template!("OSGB 页裁剪", "OSGB page clipping", "OSGB ページの切り抜き")
    };
    (osgb_raster_buffers) => {
        $crate::localized_template!("栅格块和 TIFF 控制结构", "Raster blocks and TIFF control structures", "ラスターブロックと TIFF 管理構造")
    };
    (osgb_block_progress, $pages:expr, $blocks:expr, $bytes:expr) => {
        $crate::localized_template!("已读取 {} 页，已写入 {} 个几何块，暂存 {} 字节", "Read {} pages, wrote {} geometry blocks, {} staging bytes", "{} ページ読み込み、{} ジオメトリブロック書き込み、一時領域 {} バイト"; $pages, $blocks, $bytes)
    };
    (osgb_disk_index_progress, $done:expr, $total:expr) => {
        $crate::localized_template!("正在迁移几何和索引，已写入 {} / {} 个三角形", "Migrating geometry and index: {} / {} triangles written", "ジオメトリとインデックスを移行中：{} / {} 三角形を書き込み済み"; $done, $total)
    };
    (osgb_disk_index) => {
        $crate::localized_template!("几何和索引转为磁盘存储", "Spill geometry and index to disk", "ジオメトリとインデックスをディスクに保存")
    };
    (osgb_disk_full) => {
        $crate::localized_template!("OSGB 暂存文件系统空间不足。", "Insufficient space on the OSGB staging filesystem.", "OSGB の一時ファイル用ファイルシステムの空き容量が不足しています。")
    };
    (osgb_memory_pressure) => {
        $crate::localized_template!("系统内存持续不足，已停止 OSGB 任务。", "OSGB task stopped because available system memory stayed too low.", "システムの空きメモリ不足が続いたため、OSGB タスクを停止しました。")
    };
    (osgb_pressure_serial, $rss:expr, budget = $budget:expr) => {
        $crate::localized_template!("系统内存不足，暂停新增工作并将后续并发降为 1。峰值 RSS {} 字节，内存预算 {} 字节。", "System memory is low. Pausing new work and lowering concurrency to one. Peak RSS {} bytes, memory budget {} bytes.", "システムメモリが不足しています。新規処理を一時停止し、並列数を 1 に下げます。ピーク RSS {} バイト、メモリ予算 {} バイト。"; $rss, $budget)
    };
    (osgb_rss_limit) => {
        $crate::localized_template!("OSGB 工作进程超出内存预算，已停止任务。", "The OSGB worker exceeded its memory budget and was stopped.", "OSGB ワーカーがメモリ予算を超えたため停止しました。")
    };
    (osgb_worker_usage, $rss:expr, budget = $budget:expr) => {
        $crate::localized_template!("工作进程峰值 RSS {} 字节，内存预算 {} 字节。", "Worker peak RSS {} bytes, memory budget {} bytes.", "ワーカーのピーク RSS {} バイト、メモリ予算 {} バイト。"; $rss, $budget)
    };
    (osgb_worker_metrics, $rss:expr, budget = $budget:expr, ms = $ms:expr, staged = $staged:expr) => {
        $crate::localized_template!("工作进程峰值 RSS {} 字节，内存预算 {} 字节，已用时 {} 毫秒，暂存文件 {} 字节。", "Worker peak RSS {} bytes, budget {} bytes, elapsed {} ms, staging files {} bytes.", "ワーカーのピーク RSS {} バイト、メモリ予算 {} バイト、経過時間 {} ミリ秒、一時ファイル {} バイト。"; $rss, $budget, $ms, $staged)
    };
    (osgb_worker_parallelism, $width:expr) => {
        $crate::localized_template!("本批次并发数 {}。", "Current batch concurrency {}.", "現在のバッチの並列数は {} です。"; $width)
    };
    (osgb_reservation_failed, $unit:expr, bytes = $bytes:expr, available = $available:expr) => {
        $crate::localized_template!("{} 需要 {} 字节内存，剩余任务预算为 {} 字节。", "{} requires {} bytes of memory, with {} bytes left in the task budget.", "{} に {} バイトが必要ですが、残りのメモリ予算は {} バイトです。"; $unit, $bytes, $available)
    };
    (webgis_toolbox) => {
        $crate::localized_template!("WebGIS 工具箱", "WebGIS Toolbox", "WebGIS ツールボックス")
    };
    (create_cloud_optimized_geotiff) => {
        $crate::localized_template!("生成 COG 云优化 GeoTIFF", "Create Cloud Optimized GeoTIFF", "クラウド最適化 GeoTIFF の作成")
    };
    (osgb_to_dom_dsm) => {
        $crate::localized_template!("OSGB 转 DOM / DSM", "OSGB to DOM / DSM", "OSGB から DOM / DSM へ変換")
    };
    (clip_osgb_models) => {
        $crate::localized_template!("OSGB 模型裁剪", "Clip OSGB models", "OSGB モデルの切り抜き")
    };
    (settings) => {
        $crate::localized_template!("设置", "Settings", "設定")
    };
    (3d_models) => {
        $crate::localized_template!("三维模型", "3D Models", "3D モデル")
    };
    (raster_tools) => {
        $crate::localized_template!("栅格处理", "Raster Tools", "ラスター処理")
    };
    (tile_tools) => {
        $crate::localized_template!("瓦片处理", "Tile Tools", "タイル処理")
    };
    (scan_zip_layers) => {
        $crate::localized_template!("扫描 ZIP 图层", "Scan ZIP layers", "ZIP レイヤーの検索")
    };
    (preflight) => {
        $crate::localized_template!("预检", "Preflight", "事前確認")
    };
    (process) => {
        $crate::localized_template!("处理", "Process", "処理")
    };
    (generate_terrain_tiles) => {
        $crate::localized_template!("生成地形瓦片", "Generate Terrain Tiles", "地形タイルの生成")
    };
    (terrain_dem) => {
        $crate::localized_template!("高程 DEM", "Elevation DEM", "標高 DEM")
    };
    (single_band_geotiff_or_vrt) => {
        $crate::localized_template!("单波段 GeoTIFF 或 VRT", "Single-band GeoTIFF or VRT", "単バンドの GeoTIFF または VRT")
    };
    (terrain_profile) => {
        $crate::localized_template!("切片坐标系", "Tile coordinate system", "タイル座標系")
    };
    (terrain_min_zoom) => {
        $crate::localized_template!("最低缩放级别", "Minimum zoom", "最小ズームレベル")
    };
    (terrain_max_zoom) => {
        $crate::localized_template!("最高缩放级别", "Maximum zoom", "最大ズームレベル")
    };
    (zoom_from_source_resolution) => {
        $crate::localized_template!("根据源分辨率推导", "Derived from source resolution", "入力データの解像度から算出")
    };
    (terrain_output_description) => {
        $crate::localized_template!("输出 TMS heightmap-1.0 瓦片（{z}/{x}/{y}.terrain）。", "Writes TMS heightmap-1.0 tiles ({z}/{x}/{y}.terrain).", "TMS heightmap-1.0 タイル（{z}/{x}/{y}.terrain）を出力します。")
    };
    (terrain_tile_progress, $($args:tt)+) => {
        $crate::localized_template!("已生成 {} / {} 个地形瓦片", "Terrain tiles generated: {} / {}", "地形タイル生成済み {} / {}"; $($args)+)
    };
    (invalid_terrain_zoom, $($args:tt)+) => {
        $crate::localized_template!("{}必须是 0 到 255 的整数", "{} must be an integer from 0 to 255", "{}は 0 〜 255 の整数で指定してください"; $($args)+)
    };
    (source_geotiff) => {
        $crate::localized_template!("源 GeoTIFF", "Source GeoTIFF", "入力 GeoTIFF")
    };
    (output_cog) => {
        $crate::localized_template!("输出 COG", "Output COG", "出力 COG")
    };
    (clip_boundary) => {
        $crate::localized_template!("裁剪范围", "Clip boundary", "切り抜き範囲")
    };
    (check_model_boundary_and_output) => {
        $crate::localized_template!("预检模型、裁剪范围与输出", "Check model, boundary and output", "モデル・範囲・出力先の事前確認")
    };
    (clipping_processed_models_found, $($args:tt)+) => {
        $crate::localized_template!("裁剪 {}（已处理 {} / 已发现 {} 个模型）", "Clipping {} ({} processed / {} models found)", "{} を切り抜き中（{} 件処理済み / {} 件検出）"; $($args)+)
    };
    (wrote_osgb_files_with_triangles, $($args:tt)+) => {
        $crate::localized_template!("已写出 {} 个 OSGB 文件，共 {} 个三角形。", "Wrote {} OSGB files with {} triangles.", "OSGB ファイル {} 件、三角形 {} 個を書き出しました。"; $($args)+)
    };
    (about) => {
        $crate::localized_template!("关于", "About", "このアプリについて")
    };
    (version) => {
        $crate::localized_template!("版本号", "Version", "バージョン")
    };
    (author) => {
        $crate::localized_template!("作者", "Author", "作者")
    };
    (license) => {
        $crate::localized_template!("许可证类型", "License", "ライセンス")
    };
    (repository) => {
        $crate::localized_template!("项目地址", "Repository", "リポジトリ")
    };
    (run) => {
        $crate::localized_template!("开始处理", "Run", "実行")
    };
    (task_in_progress) => {
        $crate::localized_template!("任务正在进行", "Task in progress", "タスクを実行中")
    };
    (ok) => {
        $crate::localized_template!("确定", "OK", "OK")
    };
    (copy_path) => {
        $crate::localized_template!("复制路径", "Copy path", "パスをコピー")
    };
    (task_canceled) => {
        $crate::localized_template!("任务已取消", "Task canceled", "タスクをキャンセルしました")
    };
    (processing_failed) => {
        $crate::localized_template!("处理失败", "Processing failed", "処理に失敗しました")
    };
    (dom_dsm_preflight) => {
        $crate::localized_template!("DOM / DSM 预检", "DOM / DSM preflight", "DOM / DSM の事前確認")
    };
    (coordinate_mode_crs, $($args:tt)+) => {
        $crate::localized_template!("坐标模式　{}　　坐标系　{}", "Coordinate mode: {}    CRS: {}", "座標モード: {}　座標参照系: {}"; $($args)+)
    };
    (local) => {
        $crate::localized_template!("本地", "Local", "ローカル")
    };
    (metadata_value, $($args:tt)+) => {
        $crate::localized_template!("元数据　{}", "Metadata: {}", "メタデータ: {}"; $($args)+)
    };
    (coordinate_extent, $($args:tt)+) => {
        $crate::localized_template!("范围　X {x} 至 {east}　Y {y} 至 {north}　Z {z} 至 {high}", "Extent: X {x} to {east}  Y {y} to {north}  Z {z} to {high}", "範囲: X {x} ～ {east}　Y {y} ～ {north}　Z {z} ～ {high}"; $($args)+)
    };
    (origin, $($args:tt)+) => {
        $crate::localized_template!("原点　{:?}", "Origin: {:?}", "原点: {:?}"; $($args)+)
    };
    (raster_dimensions_and_pixel_size, $($args:tt)+) => {
        $crate::localized_template!("栅格　{w} × {h}　　像元尺寸　{}", "Raster: {w} × {h}    Pixel size: {}", "ラスター: {w} × {h}　ピクセルサイズ: {}"; $($args)+)
    };
    (selected_model_files_and_lods_triangles, $($args:tt)+) => {
        $crate::localized_template!("选定模型文件及 LOD（{} 个）　{} 个三角形", "Selected model files and LODs ({})    {} triangles", "選択したモデルと LOD（{} 件）　三角形 {} 個"; $($args)+)
    };
    (clip_preflight) => {
        $crate::localized_template!("裁剪预检", "Clip preflight", "切り抜きの事前確認")
    };
    (model_crs_origin, $($args:tt)+) => {
        $crate::localized_template!("模型坐标系　{}　　原点　{:?}", "Model CRS: {}    Origin: {:?}", "モデルの座標参照系: {}　原点: {:?}"; $($args)+)
    };
    (output_folder_value, $($args:tt)+) => {
        $crate::localized_template!("输出目录　{}", "Output folder: {}", "出力フォルダー: {}"; $($args)+)
    };
    (clip_nonempty_output_notice) => {
        $crate::localized_template!("输出目录非空。运行前需要确认，同名产物仍受保护。", "The output folder is not empty. Confirm before running. Existing files with the same names are protected.", "出力フォルダーにファイルがあります。実行前に確認してください。同名のファイルは保護されます。")
    };
    (model_entry) => {
        $crate::localized_template!("模型入口", "Model entry", "モデルの起点")
    };
    (found_shp_layers, $($args:tt)+) => {
        $crate::localized_template!("找到 {} 个 SHP 图层。", "Found {} SHP layers.", "SHP レイヤーを {} 件検出しました。"; $($args)+)
    };
    (processing_complete) => {
        $crate::localized_template!("处理完成", "Processing complete", "処理が完了しました")
    };
    (browse_folders) => {
        $crate::localized_template!("选择目录…", "Browse folders…", "フォルダーを選択…")
    };
    (browse_files) => {
        $crate::localized_template!("选择文件…", "Browse files…", "ファイルを選択…")
    };
    (save_as) => {
        $crate::localized_template!("选择保存位置…", "Save as…", "保存先を選択…")
    };
    (new_folder) => {
        $crate::localized_template!("指定新目录…", "New folder…", "新規フォルダー…")
    };
    (enter_a_value_for_label, $($args:tt)+) => {
        $crate::localized_template!("请填写{label}", "Enter a value for {label}", "{label}を入力してください"; $($args)+)
    };
    (relative_to_origin) => {
        $crate::localized_template!("相对原点", "Relative to origin", "原点からの相対座標")
    };
    (absolute_coordinates) => {
        $crate::localized_template!("绝对坐标", "Absolute coordinates", "絶対座標")
    };
    (local_coordinates_no_crs) => {
        $crate::localized_template!("本地坐标（无 CRS）", "Local coordinates (no CRS)", "ローカル座標（座標参照系なし）")
    };
    (input_model) => {
        $crate::localized_template!("模型输入", "Input model", "入力モデル")
    };
    (root_osgb_file_or_metadata_xml) => {
        $crate::localized_template!("根 OSGB 文件或 metadata.xml", "Root OSGB file or metadata.xml", "ルート OSGB ファイルまたは metadata.xml")
    };
    (output_folder) => {
        $crate::localized_template!("输出目录", "Output folder", "出力フォルダー")
    };
    (path_raster_output) => {
        $crate::localized_template!("/路径/栅格成果", "/path/raster-output", "/path/raster-output")
    };
    (path_mosaic_tif) => {
        $crate::localized_template!("/路径/mosaic.tif", "/path/mosaic.tif", "/path/mosaic.tif")
    };
    (metadata) => {
        $crate::localized_template!("元数据", "Metadata", "メタデータ")
    };
    (leave_blank_to_find_metadata_xml_automatically) => {
        $crate::localized_template!("留空自动发现 metadata.xml", "Leave blank to find metadata.xml automatically", "空欄の場合は metadata.xml を自動検出")
    };
    (coordinate_mode) => {
        $crate::localized_template!("坐标模式", "Coordinate mode", "座標モード")
    };
    (crs) => {
        $crate::localized_template!("坐标系", "CRS", "座標参照系")
    };
    (use_metadata_or_enter_epsg_wkt) => {
        $crate::localized_template!("留空使用元数据，或输入 EPSG / WKT", "Use metadata or enter EPSG / WKT", "空欄ならメタデータを使用。EPSG / WKT も入力可")
    };
    (xyz_origin) => {
        $crate::localized_template!("XYZ 原点", "XYZ origin", "XYZ 原点")
    };
    (use_metadata_or_enter_x_y_z) => {
        $crate::localized_template!("留空使用元数据，或输入 X, Y, Z", "Use metadata or enter X, Y, Z", "空欄ならメタデータを使用。X, Y, Z も入力可")
    };
    (pixel_size_crs_units) => {
        $crate::localized_template!("像元尺寸（坐标单位）", "Pixel size (CRS units)", "ピクセルサイズ（座標の単位）")
    };
    (overwrite_existing_rasters_with_the_same_names) => {
        $crate::localized_template!("确认覆盖输出目录中的同名栅格产物", "Overwrite existing rasters with the same names", "出力先の同名ラスターを上書きする")
    };
    (invalid_pixel_size) => {
        $crate::localized_template!("无效的像元尺寸", "Invalid pixel size", "ピクセルサイズが無効です")
    };
    (osgb_file_or_folder_containing_metadata_xml) => {
        $crate::localized_template!("OSGB 文件或包含 metadata.xml 的目录", "OSGB file or folder containing metadata.xml", "OSGB ファイルまたは metadata.xml を含むフォルダー")
    };
    (choose_the_folder_s_osgb_entry_manually) => {
        $crate::localized_template!("手选目录的 OSGB 入口", "Choose the folder's OSGB entry manually", "フォルダー内の OSGB の起点を手動で指定する")
    };
    (entry_file) => {
        $crate::localized_template!("入口文件", "Entry file", "起点ファイル")
    };
    (relative_path_within_the_folder_or_absolute_path) => {
        $crate::localized_template!("目录内相对路径或绝对路径", "Relative path within the folder or absolute path", "フォルダー内の相対パスまたは絶対パス")
    };
    (set_the_crs_and_origin_for_a_single_model_file) => {
        $crate::localized_template!("设置单文件的模型坐标系与原点", "Set the CRS and origin for a single model file", "単一モデルファイルの座標参照系と原点を指定する")
    };
    (model_crs) => {
        $crate::localized_template!("模型坐标系", "Model CRS", "モデルの座標参照系")
    };
    (epsg_wkt_enu_longitude_latitude) => {
        $crate::localized_template!("EPSG / WKT / ENU:经度,纬度", "EPSG / WKT / ENU:longitude,latitude", "EPSG / WKT / ENU:経度,緯度")
    };
    (model_xyz_origin) => {
        $crate::localized_template!("模型 XYZ 原点", "Model XYZ origin", "モデルの XYZ 原点")
    };
    (shp_zip_kml_or_geojson) => {
        $crate::localized_template!("SHP、ZIP、KML 或 GeoJSON", "SHP, ZIP, KML or GeoJSON", "SHP、ZIP、KML、GeoJSON")
    };
    (load_zip_layers) => {
        $crate::localized_template!("读取 ZIP 图层", "Load ZIP layers", "ZIP レイヤーを読み込む")
    };
    (select_a_layer) => {
        $crate::localized_template!("请选择图层", "Select a layer", "レイヤーを選択")
    };
    (boundary_crs) => {
        $crate::localized_template!("裁剪范围坐标系", "Boundary CRS", "範囲の座標参照系")
    };
    (leave_blank_to_use_the_embedded_crs) => {
        $crate::localized_template!("留空使用数据自带坐标系", "Leave blank to use the embedded CRS", "空欄の場合はデータに含まれる座標参照系を使用")
    };
    (absolute_path_with_an_existing_parent_folder) => {
        $crate::localized_template!("绝对路径，父目录须存在", "Absolute path with an existing parent folder", "親フォルダーが存在する絶対パス")
    };
    (write_results_to_a_nonempty_folder_and_protect_same_name_files) => {
        $crate::localized_template!("确认向非空目录发布裁剪结果（保护同名文件）", "Write results to a nonempty folder and protect same-name files", "既存のファイルがあるフォルダーへの出力を許可（同名ファイルを保護）")
    };
    (enter_the_model_crs) => {
        $crate::localized_template!("请填写模型坐标系", "Enter the model CRS", "モデルの座標参照系を入力してください")
    };
    (mosaic_to_geotiff) => {
        $crate::localized_template!("拼接 GeoTIFF", "Mosaic to GeoTIFF", "GeoTIFF モザイクの作成")
    };
    (generate_tile_pyramid) => {
        $crate::localized_template!("生成多级瓦片", "Generate tile pyramid", "タイルピラミッドの作成")
    };
    (generate_stacta) => {
        $crate::localized_template!("生成 STACTA", "Generate STACTA", "STACTA の作成")
    };
    (map_tiles) => {
        $crate::localized_template!("地图切片", "Map tiles", "地図タイル")
    };
    (georeferenced_tif_tiff_with_crs) => {
        $crate::localized_template!("带坐标系和地理定位的 .tif / .tiff", "Georeferenced .tif / .tiff with CRS", "座標参照系と位置情報のある .tif / .tiff")
    };
    (tile_root_folder) => {
        $crate::localized_template!("瓦片根目录", "Tile root folder", "タイルのルートフォルダー")
    };
    (folder_containing_z_x_y_images) => {
        $crate::localized_template!("包含 z/x/y 图像的目录", "Folder containing z/x/y images", "z/x/y 形式の画像を含むフォルダー")
    };
    (tile_row_scheme) => {
        $crate::localized_template!("瓦片行号", "Tile row scheme", "タイルの行番号方式")
    };
    (source_zoom) => {
        $crate::localized_template!("源级别", "Source zoom", "入力ズームレベル")
    };
    (output_geotiff) => {
        $crate::localized_template!("GeoTIFF 输出文件", "Output GeoTIFF", "出力 GeoTIFF")
    };
    (target_zoom_levels) => {
        $crate::localized_template!("目标级别", "Target zoom levels", "出力ズームレベル")
    };
    (e_g_0_2_4) => {
        $crate::localized_template!("例如 0-2,4", "e.g. 0-2,4", "例: 0-2,4")
    };
    (resampling) => {
        $crate::localized_template!("重采样", "Resampling", "リサンプリング")
    };
    (auto) => {
        $crate::localized_template!("自动", "Auto", "自動")
    };
    (nearest_neighbor) => {
        $crate::localized_template!("最近邻", "Nearest neighbor", "最近傍法")
    };
    (bilinear) => {
        $crate::localized_template!("双线性", "Bilinear", "バイリニア")
    };
    (average) => {
        $crate::localized_template!("平均", "Average", "平均")
    };
    (format) => {
        $crate::localized_template!("格式", "Format", "形式")
    };
    (jpeg_background) => {
        $crate::localized_template!("JPEG 背景", "JPEG background", "JPEG の背景色")
    };
    (tile_output_folder) => {
        $crate::localized_template!("瓦片输出目录", "Tile output folder", "タイルの出力フォルダー")
    };
    (new_folder_2) => {
        $crate::localized_template!("新目录", "New folder", "新規フォルダー")
    };
    (time_rfc_3339) => {
        $crate::localized_template!("时间（RFC 3339）", "Time (RFC 3339)", "日時（RFC 3339）")
    };
    (e_g_2026_01_01t00_00_00z) => {
        $crate::localized_template!("例如 2026-01-01T00:00:00Z", "e.g. 2026-01-01T00:00:00Z", "例: 2026-01-01T00:00:00Z")
    };
    (defaults_to_the_tile_root_folder_name) => {
        $crate::localized_template!("留空使用瓦片根目录名称", "Defaults to the tile root folder name", "空欄の場合はタイルのルートフォルダー名を使用")
    };
    (xyz_output_folder) => {
        $crate::localized_template!("XYZ 输出目录", "XYZ output folder", "XYZ の出力フォルダー")
    };
    (creates_stacta_json_in_the_input_folder) => {
        $crate::localized_template!("在源目录生成 stacta.json。", "Creates stacta.json in the input folder.", "入力フォルダーに stacta.json を作成します。")
    };
    (zoom_levels) => {
        $crate::localized_template!("级别", "Zoom levels", "ズームレベル")
    };
    (min) => {
        $crate::localized_template!("最小", "Min", "最小")
    };
    (max) => {
        $crate::localized_template!("最大", "Max", "最大")
    };
    (defaults_to_the_output_folder_name) => {
        $crate::localized_template!("留空使用输出目录名", "Defaults to the output folder name", "空欄の場合は出力フォルダー名を使用")
    };
    (prepare_map_tiles) => {
        $crate::localized_template!("准备地图切片", "Prepare map tiles", "地図タイルの準備")
    };
    (scan_tiles) => {
        $crate::localized_template!("扫描瓦片", "Scan tiles", "タイルを検索")
    };
    (time_must_follow_rfc_3339) => {
        $crate::localized_template!("时间须符合 RFC 3339", "Time must follow RFC 3339", "日時は RFC 3339 形式で入力してください")
    };
    (validate_tiles_and_generate_stacta) => {
        $crate::localized_template!("校验瓦片并生成 STACTA", "Validate tiles and generate STACTA", "タイルの検証と STACTA の作成")
    };
    (no_valid_tiles_at_these_zoom_levels, $($args:tt)+) => {
        $crate::localized_template!("以下级别没有有效瓦片：{}", "No valid tiles at these zoom levels: {}", "有効なタイルがないズームレベル: {}"; $($args)+)
    };
    (could_not_create_the_task_thread_pool) => {
        $crate::localized_template!("无法创建任务线程池", "Could not create the task thread pool", "タスク用のスレッドプールを作成できません")
    };
    (the_background_worker_exited_unexpectedly) => {
        $crate::localized_template!("后台处理线程异常退出", "The background worker exited unexpectedly", "バックグラウンド処理が異常終了しました")
    };
    (light) => {
        $crate::localized_template!("亮色", "Light", "ライト")
    };
    (dark) => {
        $crate::localized_template!("暗色", "Dark", "ダーク")
    };
    (system) => {
        $crate::localized_template!("跟随系统", "System", "システムに従う")
    };
    (settings_file_has_no_parent_folder) => {
        $crate::localized_template!("配置文件缺少父目录", "Settings file has no parent folder", "設定ファイルの親フォルダーがありません")
    };
    (user_configuration_folder_was_not_found) => {
        $crate::localized_template!("找不到用户配置目录", "User configuration folder was not found", "ユーザー設定フォルダーが見つかりません")
    };
    (could_not_write, $($args:tt)+) => {
        $crate::localized_template!("写入 {} 失败", "Could not write {}", "{} に書き込めません"; $($args)+)
    };
    (worker_threads) => {
        $crate::localized_template!("并行线程数", "Worker threads", "処理スレッド数")
    };
    (appearance) => {
        $crate::localized_template!("显示模式", "Appearance", "外観")
    };
    (language) => {
        $crate::localized_template!("语言", "Language", "言語")
    };
    (save_settings) => {
        $crate::localized_template!("保存设置", "Save settings", "設定を保存")
    };
    (settings_saved) => {
        $crate::localized_template!("保存成功", "Settings saved", "設定を保存しました")
    };
    (could_not_save_settings_err, $($args:tt)+) => {
        $crate::localized_template!("保存失败：{err:#}", "Could not save settings: {err:#}", "設定を保存できません: {err:#}"; $($args)+)
    };
    (prepare_task) => {
        $crate::localized_template!("准备任务", "Prepare task", "タスクの準備")
    };
    (task_started) => {
        $crate::localized_template!("任务开始", "Task started", "タスクを開始しました")
    };
    (cancel_requested) => {
        $crate::localized_template!("已请求取消，等待后台任务退出", "Cancellation requested. Waiting for the worker to exit.", "キャンセルを要求しました。処理の終了を待っています。")
    };
    (task_completed) => {
        $crate::localized_template!("任务已完成", "Task completed", "タスクが完了しました")
    };
    (task_failed) => {
        $crate::localized_template!("任务失败", "Task failed", "タスクに失敗しました")
    };
    (processing_canceled) => {
        $crate::localized_template!("处理已取消", "Processing canceled", "処理をキャンセルしました")
    };
    (save_results) => {
        $crate::localized_template!("保存结果", "Save results", "結果を保存")
    };
    (wait_for_cancel_before_closing) => {
        $crate::localized_template!("请等待任务取消完成后再关闭窗口。", "Wait for the task to finish canceling before closing the window.", "キャンセル処理が完了してからウィンドウを閉じてください。")
    };
    (cancel_before_closing) => {
        $crate::localized_template!("请先点击主界面的『取消任务』，等待任务停止后再关闭窗口。", "Click Cancel task in the main window and wait for the task to stop before closing.", "メインウィンドウの「タスクをキャンセル」を押し、処理が停止してから閉じてください。")
    };
    (wait_for_task_before_closing) => {
        $crate::localized_template!("请等待当前任务结束后再关闭窗口。", "Wait for the current task to finish before closing the window.", "タスクが終了してからウィンドウを閉じてください。")
    };
    (cancel_task) => {
        $crate::localized_template!("取消任务", "Cancel task", "タスクをキャンセル")
    };
    (canceling) => {
        $crate::localized_template!("正在取消", "Canceling", "キャンセル中")
    };
    (this_task_cannot_be_canceled) => {
        $crate::localized_template!("当前任务不支持取消", "This task cannot be canceled", "このタスクはキャンセルできません")
    };
    (close) => {
        $crate::localized_template!("关闭", "Close", "閉じる")
    };
    (copy_log) => {
        $crate::localized_template!("复制日志", "Copy log", "ログをコピー")
    };
    (application_started) => {
        $crate::localized_template!("启动软件", "Application started", "アプリを起動しました")
    };
    (application_exited) => {
        $crate::localized_template!("退出软件", "Application exited", "アプリを終了しました")
    };
    (application_failed_to_start_error, $($args:tt)+) => {
        $crate::localized_template!("启动失败：{error}", "Application failed to start: {error}", "アプリを起動できません: {error}"; $($args)+)
    };
    (missing_chinese_font) => {
        $crate::localized_template!("缺少所需字体。请安装 Noto Sans CJK SC 或设置 WEBGIS_CJK_FONT。", "Install Noto Sans CJK SC or set WEBGIS_CJK_FONT to a font path.", "Noto Sans CJK SC をインストールするか、WEBGIS_CJK_FONT にフォントのパスを指定してください。")
    };
    (missing_japanese_font) => {
        $crate::localized_template!("缺少所需字体。请安装 Noto Sans CJK JP 或设置 WEBGIS_CJK_FONT。", "Install Noto Sans CJK JP or set WEBGIS_CJK_FONT to a font path.", "Noto Sans CJK JP をインストールするか、WEBGIS_CJK_FONT にフォントのパスを指定してください。")
    };
    (scan_osgb_and_metadata) => {
        $crate::localized_template!("扫描 OSGB 与元数据", "Scan OSGB and metadata", "OSGB とメタデータを検索")
    };
    (read_model, $($args:tt)+) => {
        $crate::localized_template!("已读取 {} 个模型，正在读取 {}", "{} models read, reading {}", "{} 件のモデルを読み込み済み、{} を読み込み中"; $($args)+)
    };
    (build_triangle_index) => {
        $crate::localized_template!("构建三角形索引", "Build triangle index", "三角形インデックスを作成")
    };
    (rasterize_0_count_blocks, $($args:tt)+) => {
        $crate::localized_template!("栅格化 0/{count} 块", "Rasterize 0/{count} blocks", "ラスター化 0/{count} ブロック"; $($args)+)
    };
    (rasterize_done_count_blocks, $($args:tt)+) => {
        $crate::localized_template!("栅格化 {done}/{count} 块", "Rasterize {done}/{count} blocks", "ラスター化 {done}/{count} ブロック"; $($args)+)
    };
    (generate_internal_pyramid) => {
        $crate::localized_template!("生成内置金字塔", "Generate internal overviews", "内部ピラミッドを生成")
    };
    (build_internal_overviews) => {
        $crate::localized_template!("构建内部概览", "Build internal overviews", "内部オーバービューを作成")
    };
    (validate_written_files) => {
        $crate::localized_template!("校验写出结果", "Validate written files", "出力ファイルを検証")
    };
    (read_geotiff) => {
        $crate::localized_template!("读取 GeoTIFF", "Read GeoTIFF", "GeoTIFF を読み込む")
    };
    (write_full_resolution) => {
        $crate::localized_template!("写入原始分辨率", "Write full resolution", "元の解像度で書き込む")
    };
    (write_overview, $($args:tt)+) => {
        $crate::localized_template!("写入 {} 倍概览", "Write {}× overview", "{} 倍のオーバービューを書き込む"; $($args)+)
    };
    (flush_cog_write_buffer) => {
        $crate::localized_template!("刷新 COG 写入缓冲区", "Flush COG write buffer", "COG の書き込みバッファーを反映")
    };
    (generate_map_tiles) => {
        $crate::localized_template!("生成地图切片", "Generate map tiles", "地図タイルを生成")
    };
    (generate_map_tiles_at_zoom, $($args:tt)+) => {
        $crate::localized_template!("生成 {zoom} 级地图切片", "Generate map tiles at zoom {zoom}", "ズームレベル {zoom} の地図タイルを生成"; $($args)+)
    };
    (generate_stacta_index) => {
        $crate::localized_template!("生成 STACTA 索引", "Generate STACTA index", "STACTA インデックスを生成")
    };
    (id_cannot_be_empty) => {
        $crate::localized_template!("--id 不能为空", "--id cannot be empty", "--id を空にできません")
    };
    (output_is_required_for_tms_export_and_valid_only_for_tms) => {
        $crate::localized_template!("--output 仅用于 TMS 导出，且为必填项", "--output is required for TMS export and valid only for TMS", "--output は TMS のエクスポート専用で、指定が必須です")
    };
    (cog_output_cannot_point_to_the_source_file) => {
        $crate::localized_template!("COG 输出不能指向源文件", "COG output cannot point to the source file", "COG の出力先に入力ファイルを指定できません")
    };
    (dom_mask_dimensions_failed_validation) => {
        $crate::localized_template!("DOM 掩膜尺寸校验失败", "DOM mask dimensions failed validation", "DOM マスクのサイズ検証に失敗しました")
    };
    (dom_mask_flags_failed_validation) => {
        $crate::localized_template!("DOM 掩膜标记校验失败", "DOM mask flags failed validation", "DOM マスクのフラグ検証に失敗しました")
    };
    (dom_is_missing_an_internal_mask) => {
        $crate::localized_template!("DOM 缺少内部掩膜", "DOM is missing an internal mask", "DOM に内部マスクがありません")
    };
    (dsm_nodata_validation_failed) => {
        $crate::localized_template!("DSM NoData 校验失败", "DSM NoData validation failed", "DSM の NoData 検証に失敗しました")
    };
    (epsg_code_does_not_use_longitude_latitude_or_easting_northing_axes, $($args:tt)+) => {
        $crate::localized_template!("EPSG:{code} 不支持经纬度或东、北轴平面坐标", "EPSG:{code} does not use longitude/latitude or easting/northing axes", "EPSG:{code} は経度・緯度軸または東向き・北向きの平面座標軸に対応していません"; $($args)+)
    };
    (epsg_code_requires_an_external_transformation_grid, $($args:tt)+) => {
        $crate::localized_template!("EPSG:{code} 需要外部坐标转换格网", "EPSG:{code} requires an external transformation grid", "EPSG:{code} には外部の座標変換グリッドが必要です"; $($args)+)
    };
    (epsg_does_not_match_the_geokey_coordinate_type, $($args:tt)+) => {
        $crate::localized_template!("EPSG:{} 与 GeoKey 坐标类型不符", "EPSG:{} does not match the GeoKey coordinate type", "EPSG:{} が GeoKey の座標タイプと一致しません"; $($args)+)
    };
    (missing_array_in_geojson) => {
        $crate::localized_template!("GeoJSON 中缺少数组", "Missing array in GeoJSON", "GeoJSON に配列がありません")
    };
    (geojson_polygon_is_missing_an_outer_ring) => {
        $crate::localized_template!("GeoJSON 多边形缺少外环", "GeoJSON polygon is missing an outer ring", "GeoJSON ポリゴンに外周リングがありません")
    };
    (geojson_must_use_wgs84_longitude_and_latitude_without_a_legacy_crs_declaration) => {
        $crate::localized_template!("GeoJSON 必须使用 WGS84 经纬度，且不能包含旧版 CRS 声明", "GeoJSON must use WGS84 longitude and latitude without a legacy CRS declaration", "GeoJSON は WGS84 の経度・緯度を使用し、旧形式の CRS 宣言を含めないでください")
    };
    (geokey_parameter_is_out_of_range) => {
        $crate::localized_template!("GeoKey 参数超出范围", "GeoKey parameter is out of range", "GeoKey パラメーターが範囲外です")
    };
    (geokey_directory_is_incomplete) => {
        $crate::localized_template!("GeoKey 目录不完整", "GeoKey directory is incomplete", "GeoKey ディレクトリが不完全です")
    };
    (geokey_is_missing_an_embedded_crs_definition) => {
        $crate::localized_template!("GeoKey 缺少内嵌坐标系定义", "GeoKey is missing an embedded CRS definition", "GeoKey に座標参照系の定義がありません")
    };
    (unclosed_element_in_kml_xml) => {
        $crate::localized_template!("KML XML 未闭合", "Unclosed element in KML XML", "KML XML に閉じられていない要素があります")
    };
    (dtd_is_unsupported_in_kml) => {
        $crate::localized_template!("KML 不支持 DTD", "DTD is unsupported in KML", "KML の DTD はサポートされていません")
    };
    (kml_polygon_has_multiple_outer_rings) => {
        $crate::localized_template!("KML 多边形有多个外环", "KML polygon has multiple outer rings", "KML ポリゴンに複数の外周リングがあります")
    };
    (kml_polygon_is_missing_an_outer_ring) => {
        $crate::localized_template!("KML 多边形缺少外环", "KML polygon is missing an outer ring", "KML ポリゴンに外周リングがありません")
    };
    (mismatched_kml_tags) => {
        $crate::localized_template!("KML 标签不匹配", "Mismatched KML tags", "KML のタグが一致しません")
    };
    (circular_osgb_page_reference, $($args:tt)+) => {
        $crate::localized_template!("OSGB 分页存在循环引用：{}", "Circular OSGB page reference: {}", "OSGB のページ参照が循環しています: {}"; $($args)+)
    };
    (prj_is_not_utf_8_text) => {
        $crate::localized_template!("PRJ 不是 UTF-8 文本", "PRJ is not UTF-8 text", "PRJ が UTF-8 テキストではありません")
    };
    (stacta_requires_explicit_8_bit_grayscale_rgb_or_alpha_bands, $($args:tt)+) => {
        $crate::localized_template!("STACTA 需要 8 位显式灰度、RGB 或 Alpha 波段：{}", "STACTA requires explicit 8-bit grayscale, RGB or alpha bands: {}", "STACTA には明示的な 8 ビットのグレースケール、RGB、またはアルファバンドが必要です: {}"; $($args)+)
    };
    (stacta_requires_an_explicit_alpha_band, $($args:tt)+) => {
        $crate::localized_template!("STACTA 需要显式的 Alpha 波段：{}", "STACTA requires an explicit alpha band: {}", "STACTA には明示的なアルファバンドが必要です: {}"; $($args)+)
    };
    (shapefile_contains_an_empty_inner_ring) => {
        $crate::localized_template!("Shapefile 含空内环", "Shapefile contains an empty inner ring", "Shapefile に空の内周リングがあります")
    };
    (shapefile_inner_ring_has_no_matching_outer_ring) => {
        $crate::localized_template!("Shapefile 的内环没有匹配外环", "Shapefile inner ring has no matching outer ring", "Shapefile の内周リングに対応する外周リングがありません")
    };
    (shapefile_lacks_a_prj_file_enter_the_boundary_crs) => {
        $crate::localized_template!("Shapefile 缺少 .prj，请填写范围坐标系", "Shapefile lacks a .prj file. Enter the boundary CRS.", "Shapefile に .prj がありません。範囲の座標参照系を入力してください。")
    };
    (shapefile_must_be_a_polygon_layer) => {
        $crate::localized_template!("Shapefile 需要为面图层", "Shapefile must be a polygon layer", "Shapefile はポリゴンレイヤーである必要があります")
    };
    (invalid_tiff_lzw_block_size) => {
        $crate::localized_template!("TIFF LZW 块大小无效", "Invalid TIFF LZW block size", "TIFF の LZW ブロックサイズが無効です")
    };
    (tiff_pixel_type_failed_validation) => {
        $crate::localized_template!("TIFF 像元类型校验失败", "TIFF pixel type failed validation", "TIFF のピクセル型の検証に失敗しました")
    };
    (written_tiff_block_count_does_not_match_raster_dimensions) => {
        $crate::localized_template!("TIFF 写出块数与栅格尺寸不符", "Written TIFF block count does not match raster dimensions", "書き込んだ TIFF のブロック数がラスターのサイズと一致しません")
    };
    (tiff_contains_multiple_full_resolution_masks) => {
        $crate::localized_template!("TIFF 包含多个原始分辨率掩膜", "TIFF contains multiple full-resolution masks", "TIFF に元の解像度のマスクが複数あります")
    };
    (tiff_contains_an_extra_ifd) => {
        $crate::localized_template!("TIFF 包含额外的 IFD", "TIFF contains an extra IFD", "TIFF に余分な IFD があります")
    };
    (tiff_image_dimensions_differ_from_its_internal_mask) => {
        $crate::localized_template!("TIFF 原图与内部掩膜尺寸不一致", "TIFF image dimensions differ from its internal mask", "TIFF 画像と内部マスクのサイズが一致しません")
    };
    (invalid_tiff_image_or_block_dimensions) => {
        $crate::localized_template!("TIFF 图像或块尺寸无效", "Invalid TIFF image or block dimensions", "TIFF の画像またはブロックのサイズが無効です")
    };
    (tiff_crs_or_georeferencing_failed_validation) => {
        $crate::localized_template!("TIFF 坐标系或地理定位校验失败", "TIFF CRS or georeferencing failed validation", "TIFF の座標参照系または位置情報の検証に失敗しました")
    };
    (invalid_tiff_block_dimensions) => {
        $crate::localized_template!("TIFF 块尺寸无效", "Invalid TIFF block dimensions", "TIFF のブロックサイズが無効です")
    };
    (tiff_block_size_overflow) => {
        $crate::localized_template!("TIFF 块尺寸溢出", "TIFF block size overflow", "TIFF のブロックサイズが上限を超えています")
    };
    (tiff_block_count_exceeds_the_decoding_limit) => {
        $crate::localized_template!("TIFF 块数超出解码范围", "TIFF block count exceeds the decoding limit", "TIFF のブロック数がデコードの上限を超えています")
    };
    (invalid_tiff_block_index) => {
        $crate::localized_template!("TIFF 块索引无效", "Invalid TIFF block index", "TIFF のブロックインデックスが無効です")
    };
    (tiff_block_is_out_of_range) => {
        $crate::localized_template!("TIFF 块超出范围", "TIFF block is out of range", "TIFF のブロックが範囲外です")
    };
    (invalid_tiff_mask_block_index) => {
        $crate::localized_template!("TIFF 掩膜块索引无效", "Invalid TIFF mask block index", "TIFF マスクのブロックインデックスが無効です")
    };
    (invalid_tiff_mask_dimensions) => {
        $crate::localized_template!("TIFF 掩膜尺寸无效", "Invalid TIFF mask dimensions", "TIFF マスクのサイズが無効です")
    };
    (tiff_mask_must_be_a_single_band_1_bit_image) => {
        $crate::localized_template!("TIFF 掩膜必须为单波段 1 位影像", "TIFF mask must be a single-band 1-bit image", "TIFF マスクは 1 ビットの単一バンド画像である必要があります")
    };
    (tiff_valid_pixel_count_failed_validation) => {
        $crate::localized_template!("TIFF 有效像元数校验失败", "TIFF valid pixel count failed validation", "TIFF の有効ピクセル数の検証に失敗しました")
    };
    (tiff_overview_flags_or_compression_failed_validation) => {
        $crate::localized_template!("TIFF 概览标记或压缩方式校验失败", "TIFF overview flags or compression failed validation", "TIFF のオーバービューフラグまたは圧縮方式の検証に失敗しました")
    };
    (tiff_ifd_chain_contains_a_duplicate_reference) => {
        $crate::localized_template!("TIFF 的 IFD 链包含重复引用", "TIFF IFD chain contains a duplicate reference", "TIFF の IFD チェーンに重複参照があります")
    };
    (tiff_color_layout_failed_validation) => {
        $crate::localized_template!("TIFF 颜色布局校验失败", "TIFF color layout failed validation", "TIFF のカラー構成の検証に失敗しました")
    };
    (texture2d_is_missing_an_image) => {
        $crate::localized_template!("Texture2D 缺少图像", "Texture2D is missing an image", "Texture2D に画像がありません")
    };
    (the_selected_shapefile_layer_was_not_found_in_the_zip) => {
        $crate::localized_template!("ZIP 中找不到所选 Shapefile 图层", "The selected Shapefile layer was not found in the ZIP", "ZIP 内に選択した Shapefile レイヤーがありません")
    };
    (zip_contains_more_than_one_prj_with_the_same_name) => {
        $crate::localized_template!("ZIP 中有多个同名 PRJ", "ZIP contains more than one PRJ with the same name", "ZIP に同名の PRJ が複数あります")
    };
    (no_shapefile_in_the_zip) => {
        $crate::localized_template!("ZIP 中没有 Shapefile", "No Shapefile in the ZIP", "ZIP に Shapefile がありません")
    };
    (zip_layer_name_err, $($args:tt)+) => {
        $crate::localized_template!("ZIP 图层 {name}：{err}", "ZIP layer {name}: {err}", "ZIP レイヤー {name}: {err}"; $($args)+)
    };
    (dtd_is_unsupported_in_metadata_xml) => {
        $crate::localized_template!("metadata.xml 不支持 DTD", "DTD is unsupported in metadata.xml", "metadata.xml の DTD はサポートされていません")
    };
    (metadata_xml_contains_multiple_srs_elements) => {
        $crate::localized_template!("metadata.xml 包含多个 SRS", "metadata.xml contains multiple SRS elements", "metadata.xml に SRS 要素が複数あります")
    };
    (metadata_xml_contains_multiple_srsorigin_elements) => {
        $crate::localized_template!("metadata.xml 包含多个 SRSOrigin", "metadata.xml contains multiple SRSOrigin elements", "metadata.xml に SRSOrigin 要素が複数あります")
    };
    (no_osgb_files_in_the_metadata_xml_folder) => {
        $crate::localized_template!("metadata.xml 所在目录内没有 OSGB 文件", "No OSGB files in the metadata.xml folder", "metadata.xml のフォルダーに OSGB ファイルがありません")
    };
    (unclosed_element_in_metadata_xml) => {
        $crate::localized_template!("metadata.xml 未闭合", "Unclosed element in metadata.xml", "metadata.xml に閉じられていない要素があります")
    };
    (srs_in_metadata_xml_must_be_text) => {
        $crate::localized_template!("metadata.xml 的 SRS 字段必须为文本", "SRS in metadata.xml must be text", "metadata.xml の SRS はテキストである必要があります")
    };
    (crs_and_origin_fields_in_metadata_xml_must_be_text) => {
        $crate::localized_template!("metadata.xml 的坐标系与原点字段必须为文本", "CRS and origin fields in metadata.xml must be text", "metadata.xml の座標参照系と原点のフィールドはテキストである必要があります")
    };
    (metadata_xml_is_missing_its_modelmetadata_root_element) => {
        $crate::localized_template!("metadata.xml 缺少 ModelMetadata 根元素", "metadata.xml is missing its ModelMetadata root element", "metadata.xml にルート要素 ModelMetadata がありません")
    };
    (metadata_xml_is_missing_srs) => {
        $crate::localized_template!("metadata.xml 缺少 SRS", "metadata.xml is missing SRS", "metadata.xml に SRS がありません")
    };
    (metadata_xml_is_missing_srsorigin) => {
        $crate::localized_template!("metadata.xml 缺少 SRSOrigin", "metadata.xml is missing SRSOrigin", "metadata.xml に SRSOrigin がありません")
    };
    (err_could_not_clean_up_some_outputs, $($args:tt)+) => {
        $crate::localized_template!("{err}。部分输出清理失败：{}", "{err}. Could not clean up some outputs: {}", "{err}。一部の出力を削除できませんでした: {}"; $($args)+)
    };
    (label_exceeds_256_mib, $($args:tt)+) => {
        $crate::localized_template!("{label} 超过 256 MiB", "{label} exceeds 256 MiB", "{label} が 256 MiB を超えています"; $($args)+)
    };
    (contains_external_pages_select_the_model_folder, $($args:tt)+) => {
        $crate::localized_template!("{} 含外部分页，请选择模型目录", "{} contains external pages. Select the model folder.", "{} に外部ページがあります。モデルのフォルダーを選択してください。"; $($args)+)
    };
    (references_a_non_osgb_page_name, $($args:tt)+) => {
        $crate::localized_template!("{} 引用了非 OSGB 分页 {name}", "{} references a non-OSGB page {name}", "{} が OSGB 以外のページ {name} を参照しています"; $($args)+)
    };
    (references, $($args:tt)+) => {
        $crate::localized_template!("{} 引用的 {}", "{} references {}", "{} が参照する {}"; $($args)+)
    };
    (contains_no_valid_tiles, $($args:tt)+) => {
        $crate::localized_template!("{} 没有有效瓦片", "{} contains no valid tiles", "{} に有効なタイルがありません"; $($args)+)
    };
    (embedded_texture_name_in_0_exceeds_the_size_limit, $($args:tt)+) => {
        $crate::localized_template!("{} 的内嵌纹理 {name} 超出大小限制", "Embedded texture {name} in {0} exceeds the size limit", "{} の埋め込みテクスチャ {name} がサイズ上限を超えています"; $($args)+)
    };
    (embedded_texture_name_in, $($args:tt)+) => {
        $crate::localized_template!("{} 的内嵌纹理 {name}", "Embedded texture {name} in {}", "{} の埋め込みテクスチャ {name}"; $($args)+)
    };
    (invalid_crs_in, $($args:tt)+) => {
        $crate::localized_template!("{} 的坐标系无效", "Invalid CRS in {}", "{} の座標参照系が無効です"; $($args)+)
    };
    (coordinates_out_of_range_in, $($args:tt)+) => {
        $crate::localized_template!("{} 的坐标超出范围", "Coordinates out of range in {}", "{} の座標が範囲外です"; $($args)+)
    };
    (texture_path_in_conflicts_with_a_model_or_metadata_file_name, $($args:tt)+) => {
        $crate::localized_template!("{} 的纹理路径与模型或元数据冲突：{name}", "Texture path in {} conflicts with a model or metadata file: {name}", "{} のテクスチャパスがモデルまたはメタデータと競合しています: {name}"; $($args)+)
    };
    (lacks_srs_specify_the_crs_or_select_local_mode, $($args:tt)+) => {
        $crate::localized_template!("{} 缺少 SRS，请指定坐标系或选择本地模式", "{} lacks SRS. Specify the CRS or select local mode.", "{} に SRS がありません。座標参照系を指定するかローカルモードを選択してください。"; $($args)+)
    };
    (drawable_shape_is_unsupported) => {
        $crate::localized_template!("不支持 Drawable Shape", "Drawable Shape is unsupported", "Drawable Shape はサポートされていません")
    };
    (drawable_bounding_box_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持 Drawable 包围盒回调", "Drawable bounding-box callbacks are unsupported", "Drawable のバウンディングボックスコールバックはサポートされていません")
    };
    (drawable_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持 Drawable 回调", "Drawable callbacks are unsupported", "Drawable のコールバックはサポートされていません")
    };
    (unsupported_pagedlod_databasepath_path, $($args:tt)+) => {
        $crate::localized_template!("不支持 PagedLOD DatabasePath：{path}", "Unsupported PagedLOD DatabasePath: {path}", "サポートされていない PagedLOD DatabasePath: {path}"; $($args)+)
    };
    (shader_uniform_is_unsupported) => {
        $crate::localized_template!("不支持 Shader Uniform", "Shader Uniform is unsupported", "Shader Uniform はサポートされていません")
    };
    (shared_scene_node_class_is_unsupported, $($args:tt)+) => {
        $crate::localized_template!("不支持共用场景节点 {class}", "Shared scene node {class} is unsupported", "共有シーンノード {class} はサポートされていません"; $($args)+)
    };
    (shared_indices_class_are_unsupported, $($args:tt)+) => {
        $crate::localized_template!("不支持共用索引 {class}", "Shared indices {class} are unsupported", "共有インデックス {class} はサポートされていません"; $($args)+)
    };
    (shared_mesh_array_class_is_unsupported, $($args:tt)+) => {
        $crate::localized_template!("不支持共用网格数组 {class}", "Shared mesh array {class} is unsupported", "共有メッシュ配列 {class} はサポートされていません"; $($args)+)
    };
    (tiff_with_multiple_full_resolution_images_is_unsupported) => {
        $crate::localized_template!("不支持包含多个原始分辨率影像的 TIFF", "TIFF with multiple full-resolution images is unsupported", "元の解像度の画像が複数ある TIFF はサポートされていません")
    };
    (external_pages_under_transform_nodes_are_unsupported) => {
        $crate::localized_template!("不支持变换节点下的外部分页", "External pages under transform nodes are unsupported", "変換ノード配下の外部ページはサポートされていません")
    };
    (unsupported_image_type_class, $($args:tt)+) => {
        $crate::localized_template!("不支持图像类型 {class}", "Unsupported image type {class}", "サポートされていない画像タイプ {class}"; $($args)+)
    };
    (scene_node_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持场景节点回调", "Scene-node callbacks are unsupported", "シーンノードのコールバックはサポートされていません")
    };
    (material_event_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持材质事件回调", "Material event callbacks are unsupported", "マテリアルのイベントコールバックはサポートされていません")
    };
    (material_update_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持材质更新回调", "Material update callbacks are unsupported", "マテリアルの更新コールバックはサポートされていません")
    };
    (render_state_event_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持渲染状态事件回调", "Render-state event callbacks are unsupported", "描画状態のイベントコールバックはサポートされていません")
    };
    (render_state_update_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持渲染状态更新回调", "Render-state update callbacks are unsupported", "描画状態の更新コールバックはサポートされていません")
    };
    (unsupported_geokey_parameter_tag, $($args:tt)+) => {
        $crate::localized_template!("不支持的 GeoKey 参数标签 {}", "Unsupported GeoKey parameter tag {}", "サポートされていない GeoKey パラメータータグ {}"; $($args)+)
    };
    (unsupported_geotiff_raster_pixel_type) => {
        $crate::localized_template!("不支持的 GeoTIFF 像元定位类型", "Unsupported GeoTIFF raster pixel type", "サポートされていない GeoTIFF のピクセル位置タイプ")
    };
    (unsupported_matrixtransform_matrix) => {
        $crate::localized_template!("不支持的 MatrixTransform 矩阵", "Unsupported MatrixTransform matrix", "サポートされていない MatrixTransform 行列")
    };
    (unsupported_osgb_compression_method_compressor, $($args:tt)+) => {
        $crate::localized_template!("不支持的 OSGB 压缩方式 {compressor}", "Unsupported OSGB compression method {compressor}", "サポートされていない OSGB 圧縮方式 {compressor}"; $($args)+)
    };
    (unsupported_osgb_serialization_version_version, $($args:tt)+) => {
        $crate::localized_template!("不支持的 OSGB 序列化版本 {version}", "Unsupported OSGB serialization version {version}", "サポートされていない OSGB シリアライズバージョン {version}"; $($args)+)
    };
    (unsupported_osgb_file_signature_or_byte_order) => {
        $crate::localized_template!("不支持的 OSGB 文件签名或字节序", "Unsupported OSGB file signature or byte order", "サポートされていない OSGB のファイル署名またはバイト順序")
    };
    (unsupported_osgb_stream_type_kind_or_flags_flags, $($args:tt)+) => {
        $crate::localized_template!("不支持的 OSGB 流类型 {kind} 或标志 {flags:#x}", "Unsupported OSGB stream type {kind} or flags {flags:#x}", "サポートされていない OSGB のストリームタイプ {kind} またはフラグ {flags:#x}"; $($args)+)
    };
    (unsupported_tiff_pixel_type) => {
        $crate::localized_template!("不支持的 TIFF 像元类型", "Unsupported TIFF pixel type", "サポートされていない TIFF ピクセル型")
    };
    (unsupported_tiff_compression_compression, $($args:tt)+) => {
        $crate::localized_template!("不支持的 TIFF 压缩 {compression}", "Unsupported TIFF compression {compression}", "サポートされていない TIFF 圧縮方式 {compression}"; $($args)+)
    };
    (unsupported_tiff_compression) => {
        $crate::localized_template!("不支持的 TIFF 压缩", "Unsupported TIFF compression", "サポートされていない TIFF 圧縮方式")
    };
    (unsupported_tiff_block_compression_compression, $($args:tt)+) => {
        $crate::localized_template!("不支持的 TIFF 数据块压缩 {compression}", "Unsupported TIFF block compression {compression}", "サポートされていない TIFF のブロック圧縮方式 {compression}"; $($args)+)
    };
    (unsupported_zip_entry_or_size_name, $($args:tt)+) => {
        $crate::localized_template!("不支持的 ZIP 条目或大小：{name}", "Unsupported ZIP entry or size: {name}", "サポートされていない ZIP エントリーまたはサイズ: {name}"; $($args)+)
    };
    (unsupported_primitive_class, $($args:tt)+) => {
        $crate::localized_template!("不支持的图元 {class}", "Unsupported primitive {class}", "サポートされていないプリミティブ {class}"; $($args)+)
    };
    (unsupported_scene_node_class, $($args:tt)+) => {
        $crate::localized_template!("不支持的场景节点 {class}", "Unsupported scene node {class}", "サポートされていないシーンノード {class}"; $($args)+)
    };
    (unsupported_external_reference_path_name, $($args:tt)+) => {
        $crate::localized_template!("不支持的外部引用路径 {name}", "Unsupported external reference path {name}", "サポートされていない外部参照パス {name}"; $($args)+)
    };
    (unsupported_material_attribute_class, $($args:tt)+) => {
        $crate::localized_template!("不支持的材质属性 {class}", "Unsupported material attribute {class}", "サポートされていないマテリアル属性 {class}"; $($args)+)
    };
    (unsupported_render_state_class, $($args:tt)+) => {
        $crate::localized_template!("不支持的渲染状态 {class}", "Unsupported render state {class}", "サポートされていない描画状態 {class}"; $($args)+)
    };
    (unsupported_tiff_predictor_for_grayscale_with_alpha) => {
        $crate::localized_template!("不支持的灰度 Alpha TIFF 预测器", "Unsupported TIFF predictor for grayscale with alpha", "サポートされていないグレースケール＋アルファ TIFF の予測方式")
    };
    (unsupported_user_data_container_class, $($args:tt)+) => {
        $crate::localized_template!("不支持的用户数据容器 {class}", "Unsupported user data container {class}", "サポートされていないユーザーデータコンテナー {class}"; $($args)+)
    };
    (unsupported_texture_storage_method_kind, $($args:tt)+) => {
        $crate::localized_template!("不支持的纹理存储方式 {kind}", "Unsupported texture storage method {kind}", "サポートされていないテクスチャ格納方式 {kind}"; $($args)+)
    };
    (unsupported_texture_format_format_use_jpeg_or_png, $($args:tt)+) => {
        $crate::localized_template!("不支持的纹理格式 {format:?}，需要 JPEG 或 PNG", "Unsupported texture format {format:?}. Use JPEG or PNG.", "テクスチャ形式 {format:?} はサポートされていません。JPEG または PNG を使用してください。"; $($args)+)
    };
    (unsupported_texture_channel_mapping_swizzle, $($args:tt)+) => {
        $crate::localized_template!("不支持的纹理通道映射 {swizzle}", "Unsupported texture channel mapping {swizzle}", "サポートされていないテクスチャチャンネルの割り当て {swizzle}"; $($args)+)
    };
    (unsupported_mesh_array_class, $($args:tt)+) => {
        $crate::localized_template!("不支持的网格数组 {class}", "Unsupported mesh array {class}", "サポートされていないメッシュ配列 {class}"; $($args)+)
    };
    (unsupported_vertex_array_binding, $($args:tt)+) => {
        $crate::localized_template!("不支持的顶点数组绑定 {}", "Unsupported vertex array binding {}", "サポートされていない頂点配列バインディング {}"; $($args)+)
    };
    (texture_imageattachment_is_unsupported) => {
        $crate::localized_template!("不支持纹理 ImageAttachment", "Texture ImageAttachment is unsupported", "テクスチャの ImageAttachment はサポートされていません")
    };
    (texture_event_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持纹理事件回调", "Texture event callbacks are unsupported", "テクスチャのイベントコールバックはサポートされていません")
    };
    (texture2d_on_texture_unit_unit_is_unsupported, $($args:tt)+) => {
        $crate::localized_template!("不支持纹理单元 {unit:?} 上的 Texture2D", "Texture2D on texture unit {unit:?} is unsupported", "テクスチャユニット {unit:?} の Texture2D はサポートされていません"; $($args)+)
    };
    (texture_update_callbacks_are_unsupported) => {
        $crate::localized_template!("不支持纹理更新回调", "Texture update callbacks are unsupported", "テクスチャの更新コールバックはサポートされていません")
    };
    (absolute_page_path_name_is_unsupported, $($args:tt)+) => {
        $crate::localized_template!("不支持绝对分页路径 {name}", "Absolute page path {name} is unsupported", "ページの絶対パス {name} はサポートされていません"; $($args)+)
    };
    (absolute_texture_path_name_is_unsupported, $($args:tt)+) => {
        $crate::localized_template!("不支持绝对纹理路径 {name}", "Absolute texture path {name} is unsupported", "テクスチャの絶対パス {name} はサポートされていません"; $($args)+)
    };
    (custom_user_objects_are_unsupported) => {
        $crate::localized_template!("不支持自定义用户对象", "Custom user objects are unsupported", "カスタムユーザーオブジェクトはサポートされていません")
    };
    (custom_user_data_is_unsupported) => {
        $crate::localized_template!("不支持自定义用户数据", "Custom user data is unsupported", "カスタムユーザーデータはサポートされていません")
    };
    (custom_vertex_attributes_are_unsupported) => {
        $crate::localized_template!("不支持自定义顶点属性", "Custom vertex attributes are unsupported", "カスタム頂点属性はサポートされていません")
    };
    (output_path_is_not_a_regular_file, $($args:tt)+) => {
        $crate::localized_template!("产物位置不是常规文件：{}", "Output path is not a regular file: {}", "出力先が通常のファイルではありません: {}"; $($args)+)
    };
    (a_sidecar_file_already_exists_at_the_output_location, $($args:tt)+) => {
        $crate::localized_template!("产物位置存在旁车文件：{}", "A sidecar file already exists at the output location: {}", "出力先に補助ファイルが存在します: {}"; $($args)+)
    };
    (output_already_exists_confirm_overwrite, $($args:tt)+) => {
        $crate::localized_template!("产物已存在，请确认覆盖：{}", "Output already exists. Confirm overwrite: {}", "出力が既に存在します。上書きを確認してください: {}"; $($args)+)
    };
    (only_8_bit_images_are_supported) => {
        $crate::localized_template!("仅支持 8 位影像", "Only 8-bit images are supported", "8 ビット画像のみサポートされています")
    };
    (only_unsigned_8_bit_geotiff_pixels_are_supported) => {
        $crate::localized_template!("仅支持 8 位无符号 GeoTIFF 像元", "Only unsigned 8-bit GeoTIFF pixels are supported", "符号なし 8 ビット GeoTIFF ピクセルのみサポートされています")
    };
    (only_8_bit_grayscale_grayscale_with_alpha_rgb_or_rgba_geotiff_is_supported) => {
        $crate::localized_template!("仅支持 8 位灰度、灰度 Alpha、RGB 或 RGBA GeoTIFF", "Only 8-bit grayscale, grayscale with alpha, RGB or RGBA GeoTIFF is supported", "8 ビットのグレースケール、グレースケール＋アルファ、RGB、RGBA の GeoTIFF のみサポートされています")
    };
    (unsupported_cog_pixel_type) => {
        $crate::localized_template!("仅支持 8 位灰度、灰度 Alpha、RGB、RGBA 或单波段 Float32 GeoTIFF", "Supported GeoTIFF types: 8-bit grayscale, grayscale with alpha, RGB, RGBA and single-band Float32", "対応する GeoTIFF は 8 ビットのグレースケール、グレースケール＋アルファ、RGB、RGBA、または単一バンドの Float32 です")
    };
    (only_jpeg_compressed_ycbcr_images_are_supported) => {
        $crate::localized_template!("仅支持 JPEG 压缩的 YCbCr 影像", "Only JPEG-compressed YCbCr images are supported", "JPEG 圧縮の YCbCr 画像のみサポートされています")
    };
    (only_tiff_pixels_with_a_top_left_origin_are_supported) => {
        $crate::localized_template!("仅支持左上角为原点的 TIFF 像元排列", "Only TIFF pixels with a top-left origin are supported", "左上を原点とする TIFF のピクセル配置のみサポートされています")
    };
    (only_non_instanced_gl_triangles_primitives_are_supported) => {
        $crate::localized_template!("仅支持非实例化的 GL_TRIANGLES 图元", "Only non-instanced GL_TRIANGLES primitives are supported", "インスタンス化されていない GL_TRIANGLES プリミティブのみサポートされています")
    };
    (position_arrays_must_be_three_dimensional) => {
        $crate::localized_template!("位置数组必须为三维", "Position arrays must be three-dimensional", "位置配列は 3 次元である必要があります")
    };
    (offset_overflow) => {
        $crate::localized_template!("偏移溢出", "Offset overflow", "オフセットが上限を超えています")
    };
    (pixel_size_is_below_coordinate_precision) => {
        $crate::localized_template!("像元尺寸低于坐标数值精度", "Pixel size is below coordinate precision", "ピクセルサイズが座標の数値精度を下回っています")
    };
    (write, $($args:tt)+) => {
        $crate::localized_template!("写入 {}", "Write {}", "{} に書き込む"; $($args)+)
    };
    (write_tile, $($args:tt)+) => {
        $crate::localized_template!("写入瓦片 {}", "Write tile {}", "タイル {} を書き込む"; $($args)+)
    };
    (resolution_must_be_finite_and_positive) => {
        $crate::localized_template!("分辨率必须为有限正数", "Resolution must be finite and positive", "解像度は有限の正の値である必要があります")
    };
    (tile_is_not_a_file, $($args:tt)+) => {
        $crate::localized_template!("切片不是文件：{}", "Tile is not a file: {}", "タイルがファイルではありません: {}"; $($args)+)
    };
    (tiles_must_be_square_with_sides_that_are_multiples_of_16_and_at_most_4096_pixels_found_w_h, $($args:tt)+) => {
        $crate::localized_template!("切片必须是边长为 16 的倍数且不超过 4096 的方形，实际为 {w}×{h}", "Tiles must be square with sides that are multiples of 16 and at most 4096 pixels. Found {w}×{h}.", "タイルは一辺が 16 の倍数かつ 4096 ピクセル以下の正方形である必要があります。実際のサイズ: {w}×{h}"; $($args)+)
    };
    (expected_tile_side_length_size_found_w, $($args:tt)+) => {
        $crate::localized_template!("切片边长应为 {size}，实际为 {w}", "Expected tile side length {size}, found {w}", "タイルの一辺は {size} である必要があります。実際の値: {w}"; $($args)+)
    };
    (column_out_of_range, $($args:tt)+) => {
        $crate::localized_template!("列号越界：{}", "Column out of range: {}", "列番号が範囲外です: {}"; $($args)+)
    };
    (create, $($args:tt)+) => {
        $crate::localized_template!("创建 {}", "Create {}", "{} を作成"; $($args)+)
    };
    (create_folder, $($args:tt)+) => {
        $crate::localized_template!("创建目录 {}", "Create folder {}", "フォルダー {} を作成"; $($args)+)
    };
    (a_tiff_block_exceeds_the_memory_limit) => {
        $crate::localized_template!("单个 TIFF 块超出内存限制", "A TIFF block exceeds the memory limit", "TIFF ブロックがメモリー上限を超えています")
    };
    (a_tiff_block_exceeds_64_mib_use_smaller_strips_or_tiles) => {
        $crate::localized_template!("单个 TIFF 块超过 64 MiB，请使用较小的条带或分块", "A TIFF block exceeds 64 MiB. Use smaller strips or tiles.", "TIFF ブロックが 64 MiB を超えています。より小さいストリップまたはタイルを使用してください。")
    };
    (single_file_mode_requires_the_model_crs_and_xyz_origin) => {
        $crate::localized_template!("单文件模式需要填写模型坐标系与 XYZ 原点", "Single-file mode requires the model CRS and XYZ origin", "単一ファイルモードではモデルの座標参照系と XYZ 原点の指定が必要です")
    };
    (origin_coordinates_must_be_finite_numbers) => {
        $crate::localized_template!("原点须为有限数值", "Origin coordinates must be finite numbers", "原点座標は有限の数値である必要があります")
    };
    (the_origin_must_contain_three_coordinates) => {
        $crate::localized_template!("原点须包含三个坐标", "The origin must contain three coordinates", "原点には 3 つの座標が必要です")
    };
    (publish, $($args:tt)+) => {
        $crate::localized_template!("发布 {}", "Publish {}", "{} を出力先に配置"; $($args)+)
    };
    (publish_the_destination_must_not_exist, $($args:tt)+) => {
        $crate::localized_template!("发布 {}（目标必须不存在）", "Publish {} (the destination must not exist)", "{} を出力先に配置（既存の出力先は指定できません）"; $($args)+)
    };
    (publishing_failed_changes_were_rolled_back) => {
        $crate::localized_template!("发布失败，已回滚", "Publishing failed. Changes were rolled back.", "出力に失敗したため、変更を元に戻しました")
    };
    (publishing_failed_error_rollback_failed_backup_retained_at, $($args:tt)+) => {
        $crate::localized_template!("发布失败：{error:#}。回滚失败：{}。备份保留于 {}", "Publishing failed: {error:#}. Rollback failed: {}. Backup retained at {}", "出力に失敗しました: {error:#}。復元に失敗しました: {}。バックアップの保存先: {}"; $($args)+)
    };
    (create_temporary_file_in, $($args:tt)+) => {
        $crate::localized_template!("在 {} 创建临时文件", "Create temporary file in {}", "{} に一時ファイルを作成"; $($args)+)
    };
    (create_temporary_folder_in, $($args:tt)+) => {
        $crate::localized_template!("在 {} 创建临时目录", "Create temporary folder in {}", "{} に一時フォルダーを作成"; $($args)+)
    };
    (georeferencing_affine_matrix_is_not_invertible) => {
        $crate::localized_template!("地理定位仿射矩阵不可逆", "Georeferencing affine matrix is not invertible", "位置情報のアフィン行列が可逆ではありません")
    };
    (georeferencing_affine_matrix_contains_invalid_values) => {
        $crate::localized_template!("地理定位仿射矩阵包含无效值", "Georeferencing affine matrix contains invalid values", "位置情報のアフィン行列に無効な値があります")
    };
    (scene_nesting_is_too_deep) => {
        $crate::localized_template!("场景嵌套过深", "Scene nesting is too deep", "シーンの入れ子が深すぎます")
    };
    (unrecognized_data_at_the_end_of_the_scene) => {
        $crate::localized_template!("场景末尾有未识别的数据", "Unrecognized data at the end of the scene", "シーンの末尾に認識できないデータがあります")
    };
    (crs_geokey_has_no_recognizable_epsg_identifier) => {
        $crate::localized_template!("坐标系 GeoKey 缺少可识别的 EPSG 标识", "CRS GeoKey has no recognizable EPSG identifier", "座標参照系の GeoKey に認識可能な EPSG 識別子がありません")
    };
    (crs_has_no_recognizable_top_level_epsg_identifier_value, $($args:tt)+) => {
        $crate::localized_template!("坐标系缺少可识别的顶层 EPSG 标识：{value}", "CRS has no recognizable top-level EPSG identifier: {value}", "座標参照系の最上位に認識可能な EPSG 識別子がありません: {value}"; $($args)+)
    };
    (coordinates_out_of_bounds, $($args:tt)+) => {
        $crate::localized_template!("坐标越界：{}", "Coordinates out of bounds: {}", "座標が範囲外です: {}"; $($args)+)
    };
    (transformed_coordinates_are_out_of_range) => {
        $crate::localized_template!("坐标转换结果超出范围", "Transformed coordinates are out of range", "変換後の座標が範囲外です")
    };
    (bytes_remain_in_the_block, $($args:tt)+) => {
        $crate::localized_template!("块内剩余 {} 字节", "{} bytes remain in the block", "ブロック内に {} バイト残っています"; $($args)+)
    };
    (copy_to, $($args:tt)+) => {
        $crate::localized_template!("复制 {} 到 {}", "Copy {} to {}", "{} を {} にコピー"; $($args)+)
    };
    (copy, $($args:tt)+) => {
        $crate::localized_template!("复制 {}", "Copy {}", "{} をコピー"; $($args)+)
    };
    (external_page_is_outside_the_model_folder, $($args:tt)+) => {
        $crate::localized_template!("外部分页不在模型目录内：{}", "External page is outside the model folder: {}", "外部ページがモデルのフォルダー外にあります: {}"; $($args)+)
    };
    (external_reference_is_outside_the_input_folder_name, $($args:tt)+) => {
        $crate::localized_template!("外部引用超出输入目录：{name}", "External reference is outside the input folder: {name}", "外部参照が入力フォルダーの外にあります: {name}"; $($args)+)
    };
    (external_texture_is_outside_the_model_folder_name, $($args:tt)+) => {
        $crate::localized_template!("外部纹理不在模型目录内：{name}", "External texture is outside the model folder: {name}", "外部テクスチャがモデルのフォルダー外にあります: {name}"; $($args)+)
    };
    (external_texture_has_no_file_name) => {
        $crate::localized_template!("外部纹理缺少文件名", "External texture has no file name", "外部テクスチャにファイル名がありません")
    };
    (external_texture_exceeds_the_size_limit) => {
        $crate::localized_template!("外部纹理超出大小限制", "External texture exceeds the size limit", "外部テクスチャがサイズ上限を超えています")
    };
    (image_format_does_not_match_the_file_extension) => {
        $crate::localized_template!("实际图像格式与扩展名不匹配", "Image format does not match the file extension", "画像形式がファイル拡張子と一致しません")
    };
    (object_id_id_points_to_both_previous_and_name, $($args:tt)+) => {
        $crate::localized_template!("对象 ID {id} 同时指向 {previous} 与 {name}", "Object ID {id} points to both {previous} and {name}", "オブジェクト ID {id} が {previous} と {name} の両方を参照しています"; $($args)+)
    };
    (unsupported_crs_grid_file) => {
        $crate::localized_template!("当前坐标转换不支持需要格网文件的坐标系", "CRS requiring a grid file is unsupported by coordinate transformation", "グリッドファイルが必要な座標参照系の変換はサポートされていません")
    };
    (this_platform_does_not_support_atomic_folder_publishing_without_overwrite) => {
        $crate::localized_template!("当前平台不支持无覆盖的原子目录发布", "This platform does not support atomic folder publishing without overwrite", "この環境では上書きなしのアトミックなフォルダー公開はサポートされていません")
    };
    (image_does_not_overlap_the_webmercatorquad_extent) => {
        $crate::localized_template!("影像不覆盖 WebMercatorQuad 范围", "Image does not overlap the WebMercatorQuad extent", "画像が WebMercatorQuad の範囲に重なっていません")
    };
    (unexpected_null_object) => {
        $crate::localized_template!("意外的空对象", "Unexpected null object", "予期しない null オブジェクト")
    };
    (the_selected_model_contains_no_triangles) => {
        $crate::localized_template!("所选模型中没有三角形", "The selected model contains no triangles", "選択したモデルに三角形がありません")
    };
    (file_extension_or_band_layout_differs_between_and, $($args:tt)+) => {
        $crate::localized_template!("扩展名或波段布局不一致：{} 与 {}", "File extension or band layout differs between {} and {}", "ファイル拡張子またはバンド構成が一致しません: {} と {}"; $($args)+)
    };
    (external_texture_name_was_not_found, $($args:tt)+) => {
        $crate::localized_template!("找不到外部纹理 {name}", "External texture {name} was not found", "外部テクスチャ {name} が見つかりません"; $($args)+)
    };
    (texture_object_was_not_found) => {
        $crate::localized_template!("找不到纹理对象", "Texture object was not found", "テクスチャオブジェクトが見つかりません")
    };
    (could_not_project_the_extent_of, $($args:tt)+) => {
        $crate::localized_template!("投影 {} 的覆盖范围失败", "Could not project the extent of {}", "{} の範囲を投影できません"; $($args)+)
    };
    (mosaic_width_exceeds_the_tiff_limit) => {
        $crate::localized_template!("拼接宽度超出 TIFF 限制", "Mosaic width exceeds the TIFF limit", "モザイクの幅が TIFF の上限を超えています")
    };
    (mosaic_height_exceeds_the_tiff_limit) => {
        $crate::localized_template!("拼接高度超出 TIFF 限制", "Mosaic height exceeds the TIFF limit", "モザイクの高さが TIFF の上限を超えています")
    };
    (data_is_truncated_or_exceeds_the_block_length) => {
        $crate::localized_template!("数据截断或超出块长度", "Data is truncated or exceeds the block length", "データが途中で切れているか、ブロック長を超えています")
    };
    (array_block_length_exceeds_the_osgb_limit) => {
        $crate::localized_template!("数组块长度超出 OSGB 上限", "Array block length exceeds the OSGB limit", "配列ブロックの長さが OSGB の上限を超えています")
    };
    (invalid_geojson_latitude) => {
        $crate::localized_template!("无效 GeoJSON 纬度", "Invalid GeoJSON latitude", "GeoJSON の緯度が無効です")
    };
    (invalid_geojson_longitude) => {
        $crate::localized_template!("无效 GeoJSON 经度", "Invalid GeoJSON longitude", "GeoJSON の経度が無効です")
    };
    (invalid_geojson_err, $($args:tt)+) => {
        $crate::localized_template!("无效 GeoJSON：{err}", "Invalid GeoJSON: {err}", "無効な GeoJSON: {err}"; $($args)+)
    };
    (invalid_kml_latitude) => {
        $crate::localized_template!("无效 KML 纬度", "Invalid KML latitude", "KML の緯度が無効です")
    };
    (invalid_kml_longitude) => {
        $crate::localized_template!("无效 KML 经度", "Invalid KML longitude", "KML の経度が無効です")
    };
    (invalid_kml_err, $($args:tt)+) => {
        $crate::localized_template!("无效 KML：{err}", "Invalid KML: {err}", "無効な KML: {err}"; $($args)+)
    };
    (invalid_shapefile_err, $($args:tt)+) => {
        $crate::localized_template!("无效 Shapefile：{err}", "Invalid Shapefile: {err}", "無効な Shapefile: {err}"; $($args)+)
    };
    (invalid_utf_8_string) => {
        $crate::localized_template!("无效 UTF-8 字符串", "Invalid UTF-8 string", "無効な UTF-8 文字列")
    };
    (invalid_binary_block_length) => {
        $crate::localized_template!("无效二进制块长度", "Invalid binary block length", "バイナリーブロックの長さが無効です")
    };
    (invalid_coordinate_value, $($args:tt)+) => {
        $crate::localized_template!("无效坐标 {value}", "Invalid coordinate {value}", "無効な座標 {value}"; $($args)+)
    };
    (invalid_coordinate, $($args:tt)+) => {
        $crate::localized_template!("无效坐标：{}", "Invalid coordinate: {}", "無効な座標: {}"; $($args)+)
    };
    (invalid_external_reference_name, $($args:tt)+) => {
        $crate::localized_template!("无效外部引用：{name}", "Invalid external reference: {name}", "無効な外部参照: {name}"; $($args)+)
    };
    (invalid_polygon_err, $($args:tt)+) => {
        $crate::localized_template!("无效多边形：{err}", "Invalid polygon: {err}", "無効なポリゴン: {err}"; $($args)+)
    };
    (invalid_boolean_value_value, $($args:tt)+) => {
        $crate::localized_template!("无效布尔值 {value}", "Invalid boolean value {value}", "無効な真偽値 {value}"; $($args)+)
    };
    (invalid_geojson_object_type) => {
        $crate::localized_template!("无效的 GeoJSON 对象类型", "Invalid GeoJSON object type", "GeoJSON のオブジェクトタイプが無効です")
    };
    (invalid_geokey_directory) => {
        $crate::localized_template!("无效的 GeoKey 目录", "Invalid GeoKey directory", "GeoKey ディレクトリが無効です")
    };
    (invalid_modeltransformation_affine_matrix) => {
        $crate::localized_template!("无效的 ModelTransformation 仿射矩阵", "Invalid ModelTransformation affine matrix", "ModelTransformation のアフィン行列が無効です")
    };
    (invalid_tiff_nodata_value) => {
        $crate::localized_template!("无效的 TIFF NoData 值", "Invalid TIFF NoData value", "TIFF の NoData 値が無効です")
    };
    (invalid_tiff_mask_bit_order) => {
        $crate::localized_template!("无效的 TIFF 掩膜位顺序", "Invalid TIFF mask bit order", "TIFF マスクのビット順序が無効です")
    };
    (invalid_tiff_mask_color_type) => {
        $crate::localized_template!("无效的 TIFF 掩膜颜色类型", "Invalid TIFF mask color type", "TIFF マスクのカラータイプが無効です")
    };
    (invalid_tiff_band_layout) => {
        $crate::localized_template!("无效的 TIFF 波段排列", "Invalid TIFF band layout", "TIFF のバンド構成が無効です")
    };
    (invalid_pixel_scale_or_tie_points) => {
        $crate::localized_template!("无效的像元比例或控制点", "Invalid pixel scale or tie points", "ピクセルスケールまたはタイポイントが無効です")
    };
    (invalid_texture_swizzle) => {
        $crate::localized_template!("无效的纹理 Swizzle", "Invalid texture swizzle", "テクスチャのスウィズルが無効です")
    };
    (invalid_zoom_level_value, $($args:tt)+) => {
        $crate::localized_template!("无效级别 {value:?}", "Invalid zoom level {value:?}", "無効なズームレベル {value:?}"; $($args)+)
    };
    (invalid_longitude_and_latitude_lon_lat, $($args:tt)+) => {
        $crate::localized_template!("无效经纬度 ({lon}, {lat})", "Invalid longitude and latitude ({lon}, {lat})", "無効な経度・緯度 ({lon}, {lat})"; $($args)+)
    };
    (could_not_infer_the_id_from_the_tile_root_folder_specify_id) => {
        $crate::localized_template!("无法从切片根目录推导 ID，请指定 --id", "Could not infer the ID from the tile root folder. Specify --id.", "タイルのルートフォルダーから ID を取得できません。--id を指定してください。")
    };
    (could_not_infer_the_stac_id_from_the_output_folder_enter_an_id) => {
        $crate::localized_template!("无法从输出目录推导 STAC ID，请填写 ID", "Could not infer the STAC ID from the output folder. Enter an ID.", "出力フォルダーから STAC ID を取得できません。ID を入力してください。")
    };
    (could_not_use_epsg_code_err, $($args:tt)+) => {
        $crate::localized_template!("无法使用 EPSG:{code}：{err}", "Could not use EPSG:{code}: {err}", "EPSG:{code} を使用できません: {err}"; $($args)+)
    };
    (could_not_use_crs_value_err, $($args:tt)+) => {
        $crate::localized_template!("无法使用坐标系 {value}：{err}", "Could not use CRS {value}: {err}", "座標参照系 {value} を使用できません: {err}"; $($args)+)
    };
    (could_not_publish_cog_to, $($args:tt)+) => {
        $crate::localized_template!("无法发布 COG 至 {}", "Could not publish COG to {}", "COG を {} に出力できません"; $($args)+)
    };
    (could_not_create_a_temporary_file_in, $($args:tt)+) => {
        $crate::localized_template!("无法在 {} 创建临时文件", "Could not create a temporary file in {}", "{} に一時ファイルを作成できません"; $($args)+)
    };
    (could_not_open, $($args:tt)+) => {
        $crate::localized_template!("无法打开 {}", "Could not open {}", "{} を開けません"; $($args)+)
    };
    (could_not_inspect_input_file, $($args:tt)+) => {
        $crate::localized_template!("无法检查输入文件：{}", "Could not inspect input file: {}", "入力ファイルを確認できません: {}"; $($args)+)
    };
    (could_not_parse_the_wkt_crs_err, $($args:tt)+) => {
        $crate::localized_template!("无法解析 WKT 坐标系：{err}", "Could not parse the WKT CRS: {err}", "WKT の座標参照系を解析できません: {err}"; $($args)+)
    };
    (could_not_read_tiff_block_index, $($args:tt)+) => {
        $crate::localized_template!("无法读取 TIFF 数据块 {index}", "Could not read TIFF block {index}", "TIFF ブロック {index} を読み込めません"; $($args)+)
    };
    (could_not_read_zip_entry_name_err, $($args:tt)+) => {
        $crate::localized_template!("无法读取 ZIP 条目名称：{err}", "Could not read ZIP entry name: {err}", "ZIP エントリー名を読み込めません: {err}"; $($args)+)
    };
    (could_not_read_zip_err, $($args:tt)+) => {
        $crate::localized_template!("无法读取 ZIP：{err}", "Could not read ZIP: {err}", "ZIP を読み込めません: {err}"; $($args)+)
    };
    (could_not_transform, $($args:tt)+) => {
        $crate::localized_template!("无法转换 {}", "Could not transform {}", "{} を変換できません"; $($args)+)
    };
    (output_folder_lock_failed) => {
        $crate::localized_template!("无法锁定输出目录，可能有另一个任务正在写入", "Could not lock the output folder. Another task may be writing to it.", "出力フォルダーをロックできません。別のタスクが書き込んでいる可能性があります。")
    };
    (minimum_zoom_cannot_exceed_maximum_zoom) => {
        $crate::localized_template!("最小级别不能大于最大级别", "Minimum zoom cannot exceed maximum zoom", "最小ズームレベルは最大ズームレベル以下である必要があります")
    };
    (the_topmost_surface_has_no_visible_texture_pixels_dom_cannot_be_generated) => {
        $crate::localized_template!("最高表面没有可见纹理像元，无法生成 DOM", "The topmost surface has no visible texture pixels. DOM cannot be generated.", "最上面に可視のテクスチャピクセルがないため、DOM を作成できません。")
    };
    (no_z_x_y_ext_tiles_found, $($args:tt)+) => {
        $crate::localized_template!("未找到 z/x/y.ext 切片：{}", "No z/x/y.ext tiles found: {}", "z/x/y.ext 形式のタイルが見つかりません: {}"; $($args)+)
    };
    (no_standard_tile_entry_found_choose_an_osgb_file_manually) => {
        $crate::localized_template!("未找到标准瓦片入口，请手动选择 OSGB 文件", "No standard tile entry found. Choose an OSGB file manually.", "標準のタイル起点が見つかりません。OSGB ファイルを手動で選択してください。")
    };
    (a_local_tiff_must_not_include_a_crs) => {
        $crate::localized_template!("本地 TIFF 不应包含 CRS", "A local TIFF must not include a CRS", "ローカル TIFF に座標参照系を含めることはできません")
    };
    (local_tiff_georeferencing_failed_validation) => {
        $crate::localized_template!("本地 TIFF 地理定位校验失败", "Local TIFF georeferencing failed validation", "ローカル TIFF の位置情報の検証に失敗しました")
    };
    (local_mode_does_not_accept_a_crs) => {
        $crate::localized_template!("本地模式不能指定坐标系", "Local mode does not accept a CRS", "ローカルモードでは座標参照系を指定できません")
    };
    (build_internal_overviews_for, $($args:tt)+) => {
        $crate::localized_template!("构建 {} 的内部概览", "Build internal overviews for {}", "{} の内部オーバービューを作成"; $($args)+)
    };
    (raster_dimensions_exceed_the_supported_limits) => {
        $crate::localized_template!("栅格尺寸超出支持范围", "Raster dimensions exceed the supported limits", "ラスターのサイズがサポート範囲を超えています")
    };
    (validate, $($args:tt)+) => {
        $crate::localized_template!("校验 {}", "Validate {}", "{} を検証"; $($args)+)
    };
    (grid) => {
        $crate::localized_template!("格网", "grid", "グリッド")
    };
    (inspect_destination, $($args:tt)+) => {
        $crate::localized_template!("检查目标 {}", "Inspect destination {}", "出力先 {} を確認"; $($args)+)
    };
    (model_xyz_origin_coordinates_must_be_finite) => {
        $crate::localized_template!("模型 XYZ 原点必须为有限数值", "Model XYZ origin coordinates must be finite", "モデルの XYZ 原点座標は有限である必要があります")
    };
    (the_model_does_not_intersect_the_clip_boundary) => {
        $crate::localized_template!("模型与裁剪范围没有交集", "The model does not intersect the clip boundary", "モデルが切り抜き範囲と重なっていません")
    };
    (model_entry_conflicts_with_metadata_xml) => {
        $crate::localized_template!("模型入口与 metadata.xml 冲突", "Model entry conflicts with metadata.xml", "モデルの起点が metadata.xml と競合しています")
    };
    (transformed_model_coordinates_are_out_of_range) => {
        $crate::localized_template!("模型变换后的坐标超出范围", "Transformed model coordinates are out of range", "変換後のモデル座標が範囲外です")
    };
    (model_crs_must_use_longitude_latitude_projected_easting_northing_or_enu_coordinates) => {
        $crate::localized_template!("模型坐标系需要使用经纬度、东、北轴的投影坐标或 ENU", "Model CRS must use longitude/latitude, projected easting/northing or ENU coordinates", "モデルの座標参照系には経度・緯度、東向き・北向きの投影座標、または ENU を使用してください")
    };
    (model_has_no_file_name) => {
        $crate::localized_template!("模型没有文件名", "Model has no file name", "モデルにファイル名がありません")
    };
    (model_has_no_valid_xy_extent) => {
        $crate::localized_template!("模型没有有效的 XY 范围", "Model has no valid XY extent", "モデルに有効な XY 範囲がありません")
    };
    (model_has_no_parent_folder) => {
        $crate::localized_template!("模型没有父目录", "Model has no parent folder", "モデルに親フォルダーがありません")
    };
    (the_model_folder_has_multiple_block_osgb_entries) => {
        $crate::localized_template!("模型目录包含多个 Block.osgb 入口", "The model folder has multiple Block.osgb entries", "モデルのフォルダーに Block.osgb の起点が複数あります")
    };
    (missing_parent_folder_for_the_model) => {
        $crate::localized_template!("模型缺少父目录", "Missing parent folder for the model", "モデルの親フォルダーがありません")
    };
    (model_elevation_conflicts_with_dsm_nodata_value_nodata, $($args:tt)+) => {
        $crate::localized_template!("模型高程与 DSM NoData 值 {NODATA} 冲突", "Model elevation conflicts with DSM NoData value {NODATA}", "モデルの標高が DSM の NoData 値 {NODATA} と競合しています"; $($args)+)
    };
    (no_pixel_centers_fall_on_the_model_surface_reduce_the_pixel_size) => {
        $crate::localized_template!("没有像元中心落在模型表面，请减小像元尺寸", "No pixel centers fall on the model surface. Reduce the pixel size.", "モデル表面にピクセル中心がありません。ピクセルサイズを小さくしてください。")
    };
    (no_built_in_definition_for_epsg_code, $($args:tt)+) => {
        $crate::localized_template!("没有内置的 EPSG:{code} 定义", "No built-in definition for EPSG:{code}", "EPSG:{code} の定義が組み込まれていません"; $($args)+)
    };
    (no_tile_zoom_levels_to_write_to_stacta) => {
        $crate::localized_template!("没有可写入 STACTA 的瓦片级别", "No tile zoom levels to write to STACTA", "STACTA に書き込めるタイルのズームレベルがありません")
    };
    (input_folder_has_no_tiles_at_zoom_zoom, $($args:tt)+) => {
        $crate::localized_template!("源目录没有级别 {zoom} 的切片", "Input folder has no tiles at zoom {zoom}", "入力フォルダーにズームレベル {zoom} のタイルがありません"; $($args)+)
    };
    (tile_pixel_size_must_be_positive) => {
        $crate::localized_template!("瓦片像元尺寸必须大于零", "Tile pixel size must be positive", "タイルのピクセルサイズは正の値である必要があります")
    };
    (the_tile_entry_must_be_an_osgb_file) => {
        $crate::localized_template!("瓦片入口必须为 OSGB 文件", "The tile entry must be an OSGB file", "タイルの起点は OSGB ファイルである必要があります")
    };
    (the_tile_entry_must_be_within_the_model_folder) => {
        $crate::localized_template!("瓦片入口必须位于模型目录内", "The tile entry must be within the model folder", "タイルの起点はモデルのフォルダー内に置く必要があります")
    };
    (tile_zoom_is_out_of_range_zoom, $($args:tt)+) => {
        $crate::localized_template!("瓦片级别超出范围：{zoom}", "Tile zoom is out of range: {zoom}", "タイルのズームレベルが範囲外です: {zoom}"; $($args)+)
    };
    (the_folder_contains_multiple_top_level_osgb_files_select_the_tile_entry_manually) => {
        $crate::localized_template!("目录存在多个顶级 OSGB，请手动选择瓦片入口", "The folder contains multiple top-level OSGB files. Select the tile entry manually.", "フォルダーに最上位の OSGB ファイルが複数あります。タイルの起点を手動で選択してください。")
    };
    (destination_already_exists, $($args:tt)+) => {
        $crate::localized_template!("目标已存在：{}", "Destination already exists: {}", "出力先が既に存在します: {}"; $($args)+)
    };
    (relative_path_templates_require_decimal_coordinates_without_leading_zeros, $($args:tt)+) => {
        $crate::localized_template!("相对路径模板需要十进制且无前导零的坐标：{}", "Relative path templates require decimal coordinates without leading zeros: {}", "相対パスのテンプレートには先頭にゼロのない 10 進数の座標が必要です: {}"; $($args)+)
    };
    (index_block_length_exceeds_the_osgb_limit) => {
        $crate::localized_template!("索引块长度超过 OSGB 上限", "Index block length exceeds the OSGB limit", "インデックスブロックの長さが OSGB の上限を超えています")
    };
    (index_count_is_not_a_multiple_of_three) => {
        $crate::localized_template!("索引数量不是三的倍数", "Index count is not a multiple of three", "インデックス数が 3 の倍数ではありません")
    };
    (index_is_outside_the_vertex_range) => {
        $crate::localized_template!("索引超出顶点范围", "Index is outside the vertex range", "インデックスが頂点の範囲外です")
    };
    (stacta_raster_size_out_of_range, $($args:tt)+) => {
        $crate::localized_template!("级别 {zoom} 的像元原点偏移或栅格尺寸超出 STACTA 的 32 位兼容范围", "Pixel origin offset or raster dimensions at zoom {zoom} exceed the STACTA 32-bit compatibility range", "ズームレベル {zoom} のピクセル原点オフセットまたはラスターサイズが STACTA の 32 ビット互換範囲を超えています"; $($args)+)
    };
    (invalid_tile_extent_at_zoom_zoom, $($args:tt)+) => {
        $crate::localized_template!("级别 {zoom} 的瓦片范围无效", "Invalid tile extent at zoom {zoom}", "ズームレベル {zoom} のタイル範囲が無効です"; $($args)+)
    };
    (zoom_levels_must_be_in_the_range_0_max_zoom, $($args:tt)+) => {
        $crate::localized_template!("级别必须在 0–{MAX_ZOOM} 范围内", "Zoom levels must be in the range 0–{MAX_ZOOM}", "ズームレベルは 0～{MAX_ZOOM} の範囲で指定してください"; $($args)+)
    };
    (reversed_zoom_range_item, $($args:tt)+) => {
        $crate::localized_template!("级别范围反向：{item}", "Reversed zoom range: {item}", "ズームレベル範囲の大小が逆です: {item}"; $($args)+)
    };
    (zoom_level_out_of_bounds, $($args:tt)+) => {
        $crate::localized_template!("级别越界：{}", "Zoom level out of bounds: {}", "ズームレベルが範囲外です: {}"; $($args)+)
    };
    (zoom_must_be_between_0_and_max_zoom, $($args:tt)+) => {
        $crate::localized_template!("级别须在 0–{MAX_ZOOM} 之间", "Zoom must be between 0 and {MAX_ZOOM}", "ズームレベルは 0～{MAX_ZOOM} の範囲で指定してください"; $($args)+)
    };
    (texture_name_uses_an_unsupported_raw_pixel_layout, $($args:tt)+) => {
        $crate::localized_template!("纹理 {name} 使用不支持的原始像素存储", "Texture {name} uses an unsupported raw pixel layout", "テクスチャ {name} の生ピクセルの格納方式はサポートされていません"; $($args)+)
    };
    (texture_name_has_no_pixel_data, $($args:tt)+) => {
        $crate::localized_template!("纹理 {name} 没有像素数据", "Texture {name} has no pixel data", "テクスチャ {name} にピクセルデータがありません"; $($args)+)
    };
    (texture_name_has_no_embedded_data, $($args:tt)+) => {
        $crate::localized_template!("纹理 {name} 没有内嵌数据", "Texture {name} has no embedded data", "テクスチャ {name} に埋め込みデータがありません"; $($args)+)
    };
    (texture_file_is_empty, $($args:tt)+) => {
        $crate::localized_template!("纹理文件为空：{}", "Texture file is empty: {}", "テクスチャファイルが空です: {}"; $($args)+)
    };
    (textured_mesh_is_missing_per_vertex_uv_coordinates) => {
        $crate::localized_template!("纹理网格缺少逐顶点 UV", "Textured mesh is missing per-vertex UV coordinates", "テクスチャ付きメッシュに頂点ごとの UV 座標がありません")
    };
    (absolute_coordinate_mode_does_not_accept_an_origin_offset) => {
        $crate::localized_template!("绝对坐标模式不能指定原点偏移", "Absolute coordinate mode does not accept an origin offset", "絶対座標モードでは原点オフセットを指定できません")
    };
    (encode, $($args:tt)+) => {
        $crate::localized_template!("编码 {}", "Encode {}", "{} をエンコード"; $($args)+)
    };
    (missing_srs_origin) => {
        $crate::localized_template!("缺少 SRSOrigin，请指定原点或改用绝对坐标／本地模式", "SRSOrigin is missing. Enter an origin or select absolute or local mode.", "SRSOrigin がありません。原点を指定するか、絶対座標モードまたはローカルモードを選択してください。")
    };
    (missing_tiff_bitspersample) => {
        $crate::localized_template!("缺少 TIFF BitsPerSample", "Missing TIFF BitsPerSample", "TIFF の BitsPerSample がありません")
    };
    (missing_position_array) => {
        $crate::localized_template!("缺少位置数组", "Missing position array", "位置配列がありません")
    };
    (missing_georeferencing_modelpixelscale_or_modeltransformation) => {
        $crate::localized_template!("缺少地理定位 ModelPixelScale 或 ModelTransformation", "Missing georeferencing ModelPixelScale or ModelTransformation", "位置情報の ModelPixelScale または ModelTransformation がありません")
    };
    (missing_georeferencing_modeltiepoint) => {
        $crate::localized_template!("缺少地理定位 ModelTiepoint", "Missing georeferencing ModelTiepoint", "位置情報の ModelTiepoint がありません")
    };
    (missing_crs_geokey) => {
        $crate::localized_template!("缺少坐标系 GeoKey", "Missing CRS GeoKey", "座標参照系の GeoKey がありません")
    };
    (crs_geokey_is_missing_or_unreadable) => {
        $crate::localized_template!("缺少或无法读取坐标系 GeoKey", "CRS GeoKey is missing or unreadable", "座標参照系の GeoKey がないか、読み込めません")
    };
    (missing_valid_georeferencing_supply_pixel_scale_and_tie_points_or_an_affine_transformation_matrix) => {
        $crate::localized_template!("缺少有效的地理定位：需要像元比例和控制点，或仿射变换矩阵", "Missing valid georeferencing. Supply pixel scale and tie points or an affine transformation matrix.", "有効な位置情報がありません。ピクセルスケールとタイポイント、またはアフィン変換行列が必要です。")
    };
    (input_has_no_parent_folder) => {
        $crate::localized_template!("缺少输入父目录", "Input has no parent folder", "入力の親フォルダーがありません")
    };
    (background_color_must_use_rrggbb) => {
        $crate::localized_template!("背景颜色必须是 #RRGGBB", "Background color must use #RRGGBB", "背景色は #RRGGBB 形式で指定してください")
    };
    (boundary_triangulation_failed_err, $($args:tt)+) => {
        $crate::localized_template!("范围三角剖分失败：{err}", "Boundary triangulation failed: {err}", "範囲の三角形分割に失敗しました: {err}"; $($args)+)
    };
    (boundary_contains_non_finite_coordinates) => {
        $crate::localized_template!("范围包含非有限坐标", "Boundary contains non-finite coordinates", "範囲に有限でない座標が含まれています")
    };
    (boundary_crs_must_use_longitude_latitude_or_projected_easting_northing_coordinates) => {
        $crate::localized_template!("范围坐标系需要使用经纬度或东、北轴的投影坐标", "Boundary CRS must use longitude/latitude or projected easting/northing coordinates", "範囲の座標参照系には経度・緯度、または東向き・北向きの投影座標を使用してください")
    };
    (boundary_coordinate_transformation_failed_at_err, $($args:tt)+) => {
        $crate::localized_template!("范围坐标转换失败 ({}, {})：{err}", "Boundary coordinate transformation failed at ({}, {}): {err}", "範囲の座標 ({}, {}) の変換に失敗しました: {err}"; $($args)+)
    };
    (boundary_file_must_be_shp_zip_kml_or_geojson) => {
        $crate::localized_template!("范围文件需要为 SHP、ZIP、KML 或 GeoJSON", "Boundary file must be SHP, ZIP, KML or GeoJSON", "範囲ファイルは SHP、ZIP、KML、GeoJSON のいずれかである必要があります")
    };
    (row_out_of_range, $($args:tt)+) => {
        $crate::localized_template!("行号越界：{}", "Row out of range: {}", "行番号が範囲外です: {}"; $($args)+)
    };
    (clipping_canceled) => {
        $crate::localized_template!("裁剪已取消", "Clipping canceled", "切り抜きをキャンセルしました")
    };
    (clip_boundary_contains_non_finite_coordinates) => {
        $crate::localized_template!("裁剪范围包含非有限坐标", "Clip boundary contains non-finite coordinates", "切り抜き範囲に有限でない座標が含まれています")
    };
    (the_clip_boundary_contains_no_polygons) => {
        $crate::localized_template!("裁剪范围没有多边形", "The clip boundary contains no polygons", "切り抜き範囲にポリゴンがありません")
    };
    (the_clip_boundary_has_zero_area) => {
        $crate::localized_template!("裁剪范围面积为零", "The clip boundary has zero area", "切り抜き範囲の面積がゼロです")
    };
    (decoded_texture_exceeds_cache_capacity) => {
        $crate::localized_template!("解码纹理超出缓存容量", "Decoded texture exceeds cache capacity", "デコードしたテクスチャがキャッシュ容量を超えています")
    };
    (save_the_clipped_outputs_outside_the_model_folder) => {
        $crate::localized_template!("请将裁剪结果保存到模型目录之外", "Save the clipped outputs outside the model folder", "切り抜き結果はモデルのフォルダー外に保存してください")
    };
    (enter_a_name_for_the_new_output_folder) => {
        $crate::localized_template!("请输入新输出目录名称", "Enter a name for the new output folder", "新しい出力フォルダーの名前を入力してください")
    };
    (select_a_tif_or_tiff_file) => {
        $crate::localized_template!("请选择 .tif 或 .tiff 文件", "Select a .tif or .tiff file", ".tif または .tiff ファイルを選択してください")
    };
    (select_an_osgb_file_or_a_model_folder_containing_metadata_xml) => {
        $crate::localized_template!("请选择 OSGB 文件或包含 metadata.xml 的模型目录", "Select an OSGB file or a model folder containing metadata.xml", "OSGB ファイルまたは metadata.xml を含むモデルのフォルダーを選択してください")
    };
    (select_a_shapefile_layer_in_the_zip) => {
        $crate::localized_template!("请选择 ZIP 中的 Shapefile 图层", "Select a Shapefile layer in the ZIP", "ZIP 内の Shapefile レイヤーを選択してください")
    };
    (select_a_root_osgb_file_or_metadata_xml) => {
        $crate::localized_template!("请选择根 OSGB 文件或 metadata.xml", "Select a root OSGB file or metadata.xml", "ルート OSGB ファイルまたは metadata.xml を選択してください")
    };
    (could_not_read_shapefile_err, $($args:tt)+) => {
        $crate::localized_template!("读取 Shapefile 失败：{err}", "Could not read Shapefile: {err}", "Shapefile を読み込めません: {err}"; $($args)+)
    };
    (could_not_read, $($args:tt)+) => {
        $crate::localized_template!("读取 {} 失败", "Could not read {}", "{} を読み込めません"; $($args)+)
    };
    (read_tile, $($args:tt)+) => {
        $crate::localized_template!("读取切片 {}", "Read tile {}", "タイル {} を読み込む"; $($args)+)
    };
    (read_tile_expected_side_length_size_found_width, $($args:tt)+) => {
        $crate::localized_template!("读取切片 {}：切片边长应为 {size}，实际为 {width}", "Read tile {}: expected side length {size}, found {width}", "タイル {} の読み込み: 一辺は {size} である必要があります。実際の値: {width}"; $($args)+)
    };
    (read_folder, $($args:tt)+) => {
        $crate::localized_template!("读取目录 {}", "Read folder {}", "フォルダー {} を読み込む"; $($args)+)
    };
    (input_dependency_is_not_a_file_within_the_model_folder, $($args:tt)+) => {
        $crate::localized_template!("输入依赖不是模型目录内的文件：{}", "Input dependency is not a file within the model folder: {}", "入力の依存ファイルがモデルのフォルダー内にありません: {}"; $($args)+)
    };
    (input_must_be_a_tif_or_tiff_file) => {
        $crate::localized_template!("输入必须是 .tif 或 .tiff 文件", "Input must be a .tif or .tiff file", "入力は .tif または .tiff ファイルである必要があります")
    };
    (input_file_does_not_exist, $($args:tt)+) => {
        $crate::localized_template!("输入文件不存在：{}", "Input file does not exist: {}", "入力ファイルが存在しません: {}"; $($args)+)
    };
    (output_location_already_exists_choose_another_location, $($args:tt)+) => {
        $crate::localized_template!("输出位置已存在，请更换位置：{}", "Output location already exists. Choose another location: {}", "出力先が既に存在します。別の場所を指定してください: {}"; $($args)+)
    };
    (the_parent_of_the_output_path_is_not_a_folder) => {
        $crate::localized_template!("输出位置的父路径不是目录", "The parent of the output path is not a folder", "出力先の親パスがフォルダーではありません")
    };
    (output_location_has_no_parent_folder) => {
        $crate::localized_template!("输出位置缺少父目录", "Output location has no parent folder", "出力先に親フォルダーがありません")
    };
    (output_location_must_be_an_absolute_path) => {
        $crate::localized_template!("输出位置需要为绝对路径", "Output location must be an absolute path", "出力先は絶対パスである必要があります")
    };
    (output_contains_unexpected_geotiff_sidecar_files) => {
        $crate::localized_template!("输出包含意外的 GeoTIFF 旁车文件", "Output contains unexpected GeoTIFF sidecar files", "出力に予期しない GeoTIFF 補助ファイルがあります")
    };
    (output_block_length_overflow) => {
        $crate::localized_template!("输出块长度溢出", "Output block length overflow", "出力ブロック長が上限を超えています")
    };
    (output_file_already_exists, $($args:tt)+) => {
        $crate::localized_template!("输出文件已存在：{}", "Output file already exists: {}", "出力ファイルが既に存在します: {}"; $($args)+)
    };
    (output_validation_failed, $($args:tt)+) => {
        $crate::localized_template!("输出校验失败：{}", "Output validation failed: {}", "出力の検証に失敗しました: {}"; $($args)+)
    };
    (the_output_folder_is_not_empty_confirm_before_clipping) => {
        $crate::localized_template!("输出目录非空，请确认后再开始裁剪", "The output folder is not empty. Confirm before clipping.", "出力フォルダーにファイルがあります。確認してから切り抜きを開始してください。")
    };
    (output_path_exists_and_is_not_a_regular_folder, $($args:tt)+) => {
        $crate::localized_template!("输出路径已存在且不是普通目录：{}", "Output path exists and is not a regular folder: {}", "出力先が既に存在し、通常のフォルダーではありません: {}"; $($args)+)
    };
    (could_not_sample_at_zoom_x_y, $($args:tt)+) => {
        $crate::localized_template!("采样 {} 的 {zoom}/{x}/{y} 失败", "Could not sample {} at {zoom}/{x}/{y}", "{} の {zoom}/{x}/{y} をサンプリングできません"; $($args)+)
    };
    (duplicate_coordinates_and, $($args:tt)+) => {
        $crate::localized_template!("重复坐标：{} 与 {}", "Duplicate coordinates: {} and {}", "座標が重複しています: {} と {}"; $($args)+)
    };
    (unexpected_tiff_block_pixel_type) => {
        $crate::localized_template!("重新读取原始 TIFF 块时发现意外的像元类型", "Unexpected pixel type when rereading the original TIFF block", "元の TIFF ブロックを再読み込みしたときに予期しないピクセル型が見つかりました")
    };
    (expected_n_coordinate_values_value, $($args:tt)+) => {
        $crate::localized_template!("需要 {N} 个坐标值：{value}", "Expected {N} coordinate values: {value}", "座標値が {N} 個必要です: {value}"; $($args)+)
    };
    (vertex_data_contains_non_finite_values) => {
        $crate::localized_template!("顶点数据包含非有限数值", "Vertex data contains non-finite values", "頂点データに有限でない値が含まれています")
    };
    (vertex_count_exceeds_the_osgb_limit) => {
        $crate::localized_template!("顶点数量超过 OSGB 上限", "Vertex count exceeds the OSGB limit", "頂点数が OSGB の上限を超えています")
    };
    (extra_tiff_bands_must_be_marked_as_alpha) => {
        $crate::localized_template!("额外的 TIFF 波段必须标记为 Alpha", "Extra TIFF bands must be marked as alpha", "TIFF の追加バンドはアルファとして指定する必要があります")
    };
    (status_error, $($args:tt)+) => {
        $crate::localized_template!("{status}：{error:#}", "{status}: {error:#}", "{status}: {error:#}"; $($args)+)
    };
    (progress_percentage, $($args:tt)+) => {
        $crate::localized_template!("{}（{:.0}%）", "{} ({:.0}%)", "{}（{:.0}%）"; $($args)+)
    };
    (could_not_start_the_background_worker) => {
        $crate::localized_template!("无法启动后台处理线程", "Could not start the background worker", "バックグラウンド処理を開始できません")
    };
    (could_not_resolve_output_path, $($args:tt)+) => {
        $crate::localized_template!("无法确认输出路径 {}", "Could not resolve output path {}", "出力パス {} を解決できません"; $($args)+)
    };
    (tiff_dimensions_or_block_size_failed_validation) => {
        $crate::localized_template!("TIFF 尺寸或块大小校验失败", "TIFF dimensions or block size failed validation", "TIFF の画像またはブロックのサイズ検証に失敗しました")
    };
    (clip_osgb_failed, $($args:tt)+) => {
        $crate::localized_template!("裁剪 OSGB 失败：{}", "Could not clip OSGB: {}", "OSGB の切り抜きに失敗しました: {}"; $($args)+)
    };
    (read_osgb_failed, $($args:tt)+) => {
        $crate::localized_template!("读取 OSGB 失败：{}", "Could not read OSGB: {}", "OSGB を読み込めません: {}"; $($args)+)
    };
    (operation_error, $($args:tt)+) => {
        $crate::localized_template!("{}失败：{}", "{} failed: {}", "{}に失敗しました: {}"; $($args)+)
    };
    (write_cog, $($args:tt)+) => {
        $crate::localized_template!("写入 COG {}", "Write COG {}", "COG {} を書き込む"; $($args)+)
    };
    (osgb_offset_error, $($args:tt)+) => {
        $crate::localized_template!("OSGB 0x{:x}：{err}", "OSGB at 0x{:x}: {err}", "OSGB の位置 0x{:x}: {err}"; $($args)+)
    };
    (path_error, $($args:tt)+) => {
        $crate::localized_template!("{}：{}", "{}: {}", "{}: {}"; $($args)+)
    };
    (read_input, $($args:tt)+) => {
        $crate::localized_template!("读取 {}", "Read {}", "{} の読み込み"; $($args)+)
    };
    (open_input, $($args:tt)+) => {
        $crate::localized_template!("打开 {}", "Open {}", "{} を開く処理"; $($args)+)
    };
    (error_cause, $($args:tt)+) => {
        $crate::localized_template!("：{}", ": {}", ": {}"; $($args)+)
    };
}
