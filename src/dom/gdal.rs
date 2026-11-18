use anyhow::{Context, Result, bail, ensure};
use libloading::Library;
use std::{
    ffi::{CStr, CString, c_char, c_double, c_int, c_void},
    os::unix::ffi::OsStrExt,
    path::Path,
    ptr,
    sync::atomic::{AtomicBool, Ordering},
};

type Handle = *mut c_void;
type Strings = *const *const c_char;
type Callback = unsafe extern "C" fn(c_double, *const c_char, Handle) -> c_int;
const BYTE: c_int = 1;
const FLOAT32: c_int = 6;
const PER_DATASET: c_int = 2;
pub const NODATA: f32 = -9999.0;
pub const TILE: usize = 256;

macro_rules! gdal_api {
    ($($name:ident($($arg:ty),*) $(-> $ret:ty)?;)+) => {
        #[allow(non_snake_case)]
        pub struct Gdal {
            $($name: unsafe extern "C" fn($($arg),*) $(-> $ret)?,)+
            _library: Library,
        }
        impl Gdal {
            fn symbols(library: Library) -> Result<Self> {
                // The library owns these C functions for the lifetime of every dataset.
                unsafe { Ok(Self {
                    $($name: *library.get(concat!(stringify!($name), "\0").as_bytes())?,)+
                    _library: library,
                }) }
            }
        }
    }
}

gdal_api! {
    GDALVersionInfo(*const c_char) -> *const c_char;
    GDALAllRegister();
    GDALGetDriverByName(*const c_char) -> Handle;
    GDALCreate(Handle, *const c_char, c_int, c_int, c_int, c_int, Strings) -> Handle;
    GDALOpen(*const c_char, c_int) -> Handle;
    GDALClose(Handle) -> c_int;
    GDALSetGeoTransform(Handle, *mut c_double) -> c_int;
    GDALGetGeoTransform(Handle, *mut c_double) -> c_int;
    GDALSetSpatialRef(Handle, Handle) -> c_int;
    GDALGetSpatialRef(Handle) -> Handle;
    GDALGetRasterXSize(Handle) -> c_int;
    GDALGetRasterYSize(Handle) -> c_int;
    GDALGetRasterCount(Handle) -> c_int;
    GDALGetRasterBand(Handle, c_int) -> Handle;
    GDALGetRasterDataType(Handle) -> c_int;
    GDALGetRasterColorInterpretation(Handle) -> c_int;
    GDALGetMetadataItem(Handle, *const c_char, *const c_char) -> *const c_char;
    GDALSetRasterNoDataValue(Handle, c_double) -> c_int;
    GDALGetRasterNoDataValue(Handle, *mut c_int) -> c_double;
    GDALCreateDatasetMaskBand(Handle, c_int) -> c_int;
    GDALGetMaskBand(Handle) -> Handle;
    GDALGetMaskFlags(Handle) -> c_int;
    GDALGetOverviewCount(Handle) -> c_int;
    GDALGetBlockSize(Handle, *mut c_int, *mut c_int);
    GDALRasterIO(Handle, c_int, c_int, c_int, c_int, c_int, Handle, c_int, c_int, c_int, c_int, c_int) -> c_int;
    GDALBuildOverviews(Handle, *const c_char, c_int, *const c_int, c_int, *const c_int, Option<Callback>, Handle) -> c_int;
    CPLGetLastErrorMsg() -> *const c_char;
    CPLGetThreadLocalConfigOption(*const c_char, *const c_char) -> *const c_char;
    CPLSetThreadLocalConfigOption(*const c_char, *const c_char);
    OSRNewSpatialReference(*const c_char) -> Handle;
    OSRSetFromUserInput(Handle, *const c_char) -> c_int;
    OSRIsSameEx(Handle, Handle, Strings) -> c_int;
    OSRDestroySpatialReference(Handle);
}

impl Gdal {
    pub fn load() -> Result<&'static Self> {
        static API: std::sync::OnceLock<Gdal> = std::sync::OnceLock::new();
        if let Some(api) = API.get() {
            return Ok(api);
        }
        let candidates = match std::env::var_os("WEBGIS_GDAL_LIBRARY") {
            Some(path) => vec![path],
            None => std::iter::once("libgdal.so".into())
                .chain((35..=50).rev().map(|n| format!("libgdal.so.{n}").into()))
                .collect(),
        };
        let mut errors = Vec::new();
        for path in candidates {
            let result = (|| {
                // Only the OS loader or the explicitly configured library supplies symbols.
                let api = Self::symbols(unsafe { Library::new(&path)? })?;
                let version =
                    unsafe { CStr::from_ptr((api.GDALVersionInfo)(c"VERSION_NUM".as_ptr())) };
                ensure!(
                    version.to_str()?.parse::<u32>()? >= 3_090_000,
                    "版本低于 3.9"
                );
                Ok::<_, anyhow::Error>(api)
            })();
            match result {
                Ok(api) => {
                    let api = API.get_or_init(|| {
                        unsafe { (api.GDALAllRegister)() };
                        api
                    });
                    return Ok(api);
                }
                Err(error) => errors.push(format!("{}: {error}", path.to_string_lossy())),
            }
        }
        bail!(
            "无法加载 GDAL 3.9 或更新版本。请安装预编译运行库，或设置 WEBGIS_GDAL_LIBRARY=/绝对路径/libgdal.so。\n{}",
            errors.join("\n")
        )
    }

    fn error(&self, operation: &str) -> anyhow::Error {
        let detail = unsafe { CStr::from_ptr((self.CPLGetLastErrorMsg)()) }.to_string_lossy();
        anyhow::anyhow!("{operation}：{detail}")
    }

    fn check(&self, status: c_int, operation: &str) -> Result<()> {
        if status != 0 {
            return Err(self.error(operation));
        }
        Ok(())
    }

    pub fn srs(&self, definition: &str) -> Result<Srs<'_>> {
        let text = CString::new(definition)?;
        let handle = unsafe { (self.OSRNewSpatialReference)(ptr::null()) };
        ensure!(!handle.is_null(), "无法分配坐标系");
        let srs = Srs { api: self, handle };
        self.check(
            unsafe { (self.OSRSetFromUserInput)(srs.handle, text.as_ptr()) },
            "无法解析坐标系",
        )?;
        Ok(srs)
    }

    pub fn configure(&self) -> Config<'_> {
        let mut config = Config {
            api: self,
            old: Vec::new(),
        };
        for (key, value) in [
            (c"GDAL_TIFF_INTERNAL_MASK", c"YES"),
            (c"USE_RRD", c"NO"),
            (c"TIFF_USE_OVR", c"NO"),
            (c"COMPRESS_OVERVIEW", c"DEFLATE"),
        ] {
            unsafe {
                let previous = (self.CPLGetThreadLocalConfigOption)(key.as_ptr(), ptr::null());
                config.old.push((
                    key,
                    (!previous.is_null()).then(|| CStr::from_ptr(previous).to_owned()),
                ));
                (self.CPLSetThreadLocalConfigOption)(key.as_ptr(), value.as_ptr());
            }
        }
        config
    }

    pub fn create(
        &self,
        path: &Path,
        size: [i32; 2],
        mut transform: [f64; 6],
        srs: Option<&Srs<'_>>,
        rgb: bool,
    ) -> Result<Dataset<'_>> {
        let path = CString::new(path.as_os_str().as_bytes())?;
        let driver = unsafe { (self.GDALGetDriverByName)(c"GTiff".as_ptr()) };
        ensure!(!driver.is_null(), "GDAL 运行库未提供 GTiff 驱动");
        let options = [
            c"TILED=YES".as_ptr(),
            c"BLOCKXSIZE=256".as_ptr(),
            c"BLOCKYSIZE=256".as_ptr(),
            c"COMPRESS=DEFLATE".as_ptr(),
            c"BIGTIFF=YES".as_ptr(),
            if rgb {
                c"PHOTOMETRIC=RGB".as_ptr()
            } else {
                c"PREDICTOR=3".as_ptr()
            },
            ptr::null(),
        ];
        let (bands, kind) = if rgb { (3, BYTE) } else { (1, FLOAT32) };
        let handle = unsafe {
            (self.GDALCreate)(
                driver,
                path.as_ptr(),
                size[0],
                size[1],
                bands,
                kind,
                options.as_ptr(),
            )
        };
        let dataset = self.dataset(handle, "创建 GeoTIFF 失败")?;
        unsafe {
            self.check(
                (self.GDALSetGeoTransform)(handle, transform.as_mut_ptr()),
                "写入仿射变换失败",
            )?;
            if let Some(srs) = srs {
                self.check(
                    (self.GDALSetSpatialRef)(handle, srs.handle),
                    "写入坐标系失败",
                )?;
            }
            if rgb {
                self.check(
                    (self.GDALCreateDatasetMaskBand)(handle, PER_DATASET),
                    "创建 DOM 掩膜失败",
                )?;
            } else {
                self.check(
                    (self.GDALSetRasterNoDataValue)(dataset.band(1), f64::from(NODATA)),
                    "写入 DSM NoData 失败",
                )?;
            }
        }
        Ok(dataset)
    }

    fn dataset(&self, handle: Handle, operation: &str) -> Result<Dataset<'_>> {
        if handle.is_null() {
            Err(self.error(operation))
        } else {
            Ok(Dataset { api: self, handle })
        }
    }

    pub fn verify(
        &self,
        path: &Path,
        size: [i32; 2],
        transform: [f64; 6],
        srs: Option<&Srs<'_>>,
        levels: &[i32],
        rgb: bool,
    ) -> Result<()> {
        use std::io::Read;
        let mut signature = [0; 4];
        std::fs::File::open(path)?.read_exact(&mut signature)?;
        ensure!(
            matches!(&signature, b"II+\0" | b"MM\0+"),
            "BigTIFF 文件签名校验失败：{}",
            path.display()
        );
        let name = CString::new(path.as_os_str().as_bytes())?;
        let ds = self.dataset(
            unsafe { (self.GDALOpen)(name.as_ptr(), 0) },
            "重新打开 GeoTIFF 失败",
        )?;
        self.verify_open(&ds, size, transform, srs, levels, rgb)
            .with_context(|| path.display().to_string())?;
        ds.close()
    }

    fn verify_open(
        &self,
        ds: &Dataset<'_>,
        size: [i32; 2],
        transform: [f64; 6],
        srs: Option<&Srs<'_>>,
        levels: &[i32],
        rgb: bool,
    ) -> Result<()> {
        unsafe {
            let h = ds.handle;
            let compression =
                (self.GDALGetMetadataItem)(h, c"COMPRESSION".as_ptr(), c"IMAGE_STRUCTURE".as_ptr());
            ensure!(
                !compression.is_null() && CStr::from_ptr(compression) == c"DEFLATE",
                "GeoTIFF 压缩方式校验失败"
            );
            let (bands, data_type) = if rgb { (3, BYTE) } else { (1, FLOAT32) };
            ensure!(
                [(self.GDALGetRasterXSize)(h), (self.GDALGetRasterYSize)(h)] == size
                    && (self.GDALGetRasterCount)(h) == bands,
                "栅格尺寸或波段数校验失败"
            );
            let mut actual = [0.; 6];
            self.check(
                (self.GDALGetGeoTransform)(h, actual.as_mut_ptr()),
                "读取仿射变换失败",
            )?;
            ensure!(actual == transform, "仿射变换校验失败");
            let actual_srs = (self.GDALGetSpatialRef)(h);
            ensure!(
                match srs {
                    Some(srs) =>
                        !actual_srs.is_null()
                            && (self.OSRIsSameEx)(
                                srs.handle,
                                actual_srs,
                                [
                                    c"IGNORE_DATA_AXIS_TO_SRS_AXIS_MAPPING=YES".as_ptr(),
                                    ptr::null()
                                ]
                                .as_ptr()
                            ) != 0,
                    None => actual_srs.is_null(),
                },
                "坐标系校验失败"
            );
            for i in 1..=bands {
                let band = ds.band(i);
                let (mut x, mut y) = (0, 0);
                (self.GDALGetBlockSize)(band, &mut x, &mut y);
                ensure!(
                    (self.GDALGetRasterDataType)(band) == data_type
                        && (self.GDALGetOverviewCount)(band) == levels.len() as i32
                        && x == TILE as i32
                        && y == TILE as i32,
                    "波段格式或概览校验失败"
                );
            }
            if rgb {
                for (i, color) in [(1, 3), (2, 4), (3, 5)] {
                    ensure!(
                        (self.GDALGetRasterColorInterpretation)(ds.band(i)) == color,
                        "DOM RGB 波段顺序校验失败"
                    );
                }
                ensure!(
                    (self.GDALGetMaskFlags)(ds.band(1)) == PER_DATASET
                        && (self.GDALGetOverviewCount)((self.GDALGetMaskBand)(ds.band(1)))
                            == levels.len() as i32,
                    "DOM 掩膜或掩膜概览校验失败"
                );
            } else {
                let mut has_nodata = 0;
                ensure!(
                    (self.GDALGetRasterNoDataValue)(ds.band(1), &mut has_nodata)
                        == f64::from(NODATA)
                        && has_nodata != 0,
                    "DSM NoData 校验失败"
                );
            }
        }
        Ok(())
    }
}

pub struct Config<'a> {
    api: &'a Gdal,
    old: Vec<(&'static CStr, Option<CString>)>,
}
impl Drop for Config<'_> {
    fn drop(&mut self) {
        for (key, value) in &self.old {
            unsafe {
                (self.api.CPLSetThreadLocalConfigOption)(
                    key.as_ptr(),
                    value.as_ref().map_or(ptr::null(), |v| v.as_ptr()),
                )
            };
        }
    }
}

pub struct Srs<'a> {
    api: &'a Gdal,
    handle: Handle,
}
impl Drop for Srs<'_> {
    fn drop(&mut self) {
        unsafe { (self.api.OSRDestroySpatialReference)(self.handle) };
    }
}

pub struct Dataset<'a> {
    api: &'a Gdal,
    handle: Handle,
}
impl Dataset<'_> {
    fn band(&self, index: i32) -> Handle {
        unsafe { (self.api.GDALGetRasterBand)(self.handle, index) }
    }

    pub fn write_dsm(&self, rect: [i32; 4], values: &mut [f32]) -> Result<()> {
        self.write(self.band(1), rect, values, FLOAT32)
    }

    pub fn write_dom(&self, rect: [i32; 4], planes: &mut [Vec<u8>; 4]) -> Result<()> {
        for (i, plane) in planes[..3].iter_mut().enumerate() {
            self.write(self.band(i as i32 + 1), rect, plane, BYTE)?;
        }
        let mask = unsafe { (self.api.GDALGetMaskBand)(self.band(1)) };
        self.write(mask, rect, &mut planes[3], BYTE)
    }

    fn write<T>(
        &self,
        band: Handle,
        [x, y, w, h]: [i32; 4],
        data: &mut [T],
        kind: i32,
    ) -> Result<()> {
        assert_eq!(data.len(), w as usize * h as usize);
        self.api.check(
            unsafe {
                (self.api.GDALRasterIO)(
                    band,
                    1,
                    x,
                    y,
                    w,
                    h,
                    data.as_mut_ptr().cast(),
                    w,
                    h,
                    kind,
                    0,
                    0,
                )
            },
            "写入栅格块失败",
        )
    }

    pub fn overviews(&self, levels: &[i32], cancelled: &AtomicBool) -> Result<()> {
        super::check_cancelled(cancelled)?;
        if levels.is_empty() {
            return Ok(());
        }
        unsafe extern "C" fn progress(_: f64, _: *const c_char, data: Handle) -> c_int {
            let flag = unsafe { &*data.cast::<AtomicBool>() };
            i32::from(!flag.load(Ordering::Relaxed))
        }
        let result = unsafe {
            (self.api.GDALBuildOverviews)(
                self.handle,
                c"AVERAGE".as_ptr(),
                levels.len() as i32,
                levels.as_ptr(),
                0,
                ptr::null(),
                Some(progress),
                ptr::from_ref(cancelled).cast_mut().cast(),
            )
        };
        super::check_cancelled(cancelled)?;
        self.api.check(result, "构建内部概览失败")
    }

    pub fn close(mut self) -> Result<()> {
        let handle = std::mem::replace(&mut self.handle, ptr::null_mut());
        self.api
            .check(unsafe { (self.api.GDALClose)(handle) }, "关闭 GeoTIFF 失败")
    }
}

impl Drop for Dataset<'_> {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe { (self.api.GDALClose)(self.handle) };
        }
    }
}
