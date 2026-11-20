//! OpenSceneGraph v131 robust binary scene serialization.

use super::mesh::{Boundary, Weights, interpolate};
use geo::Coord;
use std::{
    collections::HashMap,
    ops::Range,
    sync::atomic::{AtomicBool, Ordering},
};

type Matrix = [[f64; 4]; 4];
const IDENTITY: Matrix = [
    [1., 0., 0., 0.],
    [0., 1., 0., 0.],
    [0., 0., 1., 0.],
    [0., 0., 0., 1.],
];

pub struct Scene {
    pub bytes: Vec<u8>,
    pub info: SceneInfo,
}

pub struct SceneInfo {
    pub pages: Vec<String>,
    pub textures: Vec<String>,
    pub triangles: usize,
}

/// Parse and rewrite a scene. A missing boundary performs a lossless round trip.
pub fn rewrite(bytes: &[u8], boundary: Option<&Boundary>) -> Result<Scene, String> {
    rewrite_cancelable(bytes, boundary, None)
}

pub(super) fn inspect(bytes: &[u8], cancelled: Option<&AtomicBool>) -> Result<SceneInfo, String> {
    Ok(parse(bytes, None, cancelled)?.info())
}

pub(super) fn rewrite_cancelable(
    bytes: &[u8],
    boundary: Option<&Boundary>,
    cancelled: Option<&AtomicBool>,
) -> Result<Scene, String> {
    let mut reader = parse(bytes, boundary, cancelled)?;
    reader.edits.sort_unstable_by_key(|(range, _)| range.start);
    let mut output = Vec::new();
    let mut pos = 0;
    for (range, value) in std::mem::take(&mut reader.edits) {
        output.extend_from_slice(&bytes[pos..range.start]);
        output.extend(value);
        pos = range.end;
    }
    output.extend_from_slice(&bytes[pos..]);
    Ok(Scene {
        bytes: output,
        info: reader.info(),
    })
}

pub struct RasterTriangle {
    pub xyz: [[f64; 3]; 3],
    pub uv: Option<[[f64; 2]; 3]>,
    pub texture: Option<u32>,
}

pub enum RasterImage {
    Embedded { name: String, bytes: Range<usize> },
    External(String),
}

#[derive(Default)]
pub struct RasterMesh {
    pub triangles: Vec<RasterTriangle>,
    pub images: HashMap<u32, RasterImage>,
    pub pages: Vec<String>,
    states: HashMap<u32, Option<u32>>,
    textures: HashMap<u32, u32>,
    active: Option<u32>,
}

pub fn raster_mesh(bytes: &[u8], cancelled: &AtomicBool) -> Result<RasterMesh, String> {
    let mut reader = read_scene(bytes, None, Some(cancelled), Some(RasterMesh::default()))?;
    let mut mesh = reader.raster.take().unwrap();
    mesh.pages = reader.pages;
    Ok(mesh)
}

fn parse<'a>(
    bytes: &'a [u8],
    boundary: Option<&'a Boundary>,
    cancelled: Option<&'a AtomicBool>,
) -> Result<Reader<'a>, String> {
    read_scene(bytes, boundary, cancelled, None)
}

fn read_scene<'a>(
    bytes: &'a [u8],
    boundary: Option<&'a Boundary>,
    cancelled: Option<&'a AtomicBool>,
    raster: Option<RasterMesh>,
) -> Result<Reader<'a>, String> {
    let mut reader = Reader {
        bytes,
        pos: 0,
        blocks: Vec::new(),
        edits: Vec::new(),
        ids: HashMap::new(),
        pages: Vec::new(),
        textures: Vec::new(),
        triangles: 0,
        boundary,
        cancelled,
        raster,
        object_id: 0,
        raster_coarse: false,
    };
    let result = reader.scene();
    result.map_err(|err| format!("OSGB 0x{:x}：{err}", reader.pos))?;
    Ok(reader)
}

struct Block {
    start: usize,
    end: usize,
    delta: i64,
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    blocks: Vec<Block>,
    edits: Vec<(Range<usize>, Vec<u8>)>,
    ids: HashMap<u32, String>,
    pages: Vec<String>,
    textures: Vec<String>,
    triangles: usize,
    boundary: Option<&'a Boundary>,
    cancelled: Option<&'a AtomicBool>,
    raster: Option<RasterMesh>,
    object_id: u32,
    raster_coarse: bool,
}

struct Array {
    range: Range<usize>,
    values: Vec<f64>,
    width: usize,
    scalar: usize,
    binding: u32,
}

impl Reader<'_> {
    fn info(self) -> SceneInfo {
        SceneInfo {
            triangles: self.triangles,
            pages: self.pages,
            textures: self.textures,
        }
    }

    fn check_cancelled(&self) -> Result<(), String> {
        if self
            .cancelled
            .is_some_and(|flag| flag.load(Ordering::Relaxed))
        {
            return Err("处理已取消".into());
        }
        Ok(())
    }
    fn scene(&mut self) -> Result<(), String> {
        if self.take(8)? != [0xa1, 0x0e, 0x91, 0x6c, 0x45, 0x45, 0xfb, 0x1a] {
            return Err("不支持的 OSGB 文件签名或字节序".into());
        }
        let kind = self.u32()?;
        let version = self.u32()?;
        if version != 131 {
            return Err(format!("不支持的 OSGB 序列化版本 {version}"));
        }
        let flags = self.u32()?;
        if kind != 1 || flags != 4 {
            return Err(format!("不支持的 OSGB 流类型 {kind} 或标志 {flags:#x}"));
        }
        let compressor = self.string()?;
        if compressor != "0" {
            return Err(format!("不支持的 OSGB 压缩方式 {compressor}"));
        }
        self.node(IDENTITY)?;
        if self.pos != self.bytes.len() {
            return Err("场景末尾有未识别的数据".into());
        }
        Ok(())
    }

    fn take(&mut self, size: usize) -> Result<&[u8], String> {
        let end = self.pos.checked_add(size).ok_or("偏移溢出")?;
        let limit = self
            .blocks
            .last()
            .map_or(self.bytes.len(), |block| block.end);
        if end > limit {
            return Err("数据截断或超出块长度".into());
        }
        let start = self.pos;
        self.pos = end;
        Ok(&self.bytes[start..end])
    }

    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn flag(&mut self) -> Result<bool, String> {
        match self.take(1)?[0] {
            0 => Ok(false),
            1 => Ok(true),
            value => Err(format!("无效布尔值 {value}")),
        }
    }

    fn string(&mut self) -> Result<String, String> {
        let size = self.u32()? as usize;
        String::from_utf8(self.take(size)?.to_vec()).map_err(|_| "无效 UTF-8 字符串".into())
    }

    fn begin(&mut self) -> Result<(), String> {
        if self.blocks.len() >= 128 {
            return Err("场景嵌套过深".into());
        }
        let start = self.pos;
        let size = self.u32()? as usize;
        let limit = self
            .blocks
            .last()
            .map_or(self.bytes.len(), |block| block.end);
        if !(4..=limit - start).contains(&size) {
            return Err("无效二进制块长度".into());
        }
        self.blocks.push(Block {
            start,
            end: start + size,
            delta: 0,
        });
        Ok(())
    }

    fn end(&mut self) -> Result<(), String> {
        let block = self.blocks.pop().unwrap();
        if self.pos != block.end {
            return Err(format!("块内剩余 {} 字节", block.end - self.pos));
        }
        if block.delta != 0 {
            let size = i32::try_from((block.end - block.start) as i64 + block.delta)
                .map_err(|_| "输出块长度溢出")?;
            self.edits
                .push((block.start..block.start + 4, size.to_le_bytes().to_vec()));
            if let Some(parent) = self.blocks.last_mut() {
                parent.delta += block.delta;
            }
        }
        Ok(())
    }

    fn edit_in(
        &mut self,
        block: usize,
        range: Range<usize>,
        replacement: Vec<u8>,
    ) -> Result<(), String> {
        // Array and primitive blocks have already been consumed when all
        // vertex attributes become available for interpolation.
        if block != self.blocks.last().unwrap().start {
            let old_size = u32::from_le_bytes(self.bytes[block..block + 4].try_into().unwrap());
            let delta = replacement.len() as i64 - range.len() as i64;
            let new_size = i32::try_from(i64::from(old_size) + delta)
                .map_err(|_| "数组块长度超出 OSGB 上限")?;
            self.edits
                .push((block..block + 4, new_size.to_le_bytes().to_vec()));
        }
        self.blocks.last_mut().unwrap().delta += replacement.len() as i64 - range.len() as i64;
        self.edits.push((range, replacement));
        Ok(())
    }

    fn object(&mut self) -> Result<(String, bool), String> {
        let name = self.string()?;
        if name == "NULL" {
            return Err("意外的空对象".into());
        }
        self.begin()?;
        let id = self.u32()?;
        self.object_id = id;
        let first = self.identify(id, &name)?;
        Ok((name, first))
    }

    fn identify(&mut self, id: u32, name: &str) -> Result<bool, String> {
        if let Some(previous) = self.ids.get(&id) {
            if previous != name {
                return Err(format!("对象 ID {id} 同时指向 {previous} 与 {name}"));
            }
            return Ok(false);
        }
        self.ids.insert(id, name.to_owned());
        Ok(true)
    }

    fn absent(&mut self, message: &str) -> Result<(), String> {
        if self.flag()? {
            return Err(message.into());
        }
        Ok(())
    }

    fn object_fields(&mut self) -> Result<(), String> {
        let length = self.u32()? as usize;
        self.take(length)?;
        self.u32()?;
        if self.flag()? {
            let (class, first) = self.object()?;
            if class != "osg::DefaultUserDataContainer" {
                return Err(format!("不支持的用户数据容器 {class}"));
            }
            if first {
                self.object_fields()?;
                self.absent("不支持自定义用户数据")?;
                if self.flag()? {
                    let count = self.u32()?;
                    self.begin()?;
                    for _ in 0..count {
                        self.string()?;
                    }
                    self.end()?;
                }
                self.absent("不支持自定义用户对象")?;
            }
            self.end()?;
        }
        Ok(())
    }

    fn node(&mut self, transform: Matrix) -> Result<(), String> {
        let inherited = self.raster.as_ref().and_then(|mesh| mesh.active);
        let result = self.node_body(transform);
        if let Some(mesh) = &mut self.raster {
            mesh.active = inherited;
        }
        result
    }

    fn node_body(&mut self, transform: Matrix) -> Result<(), String> {
        self.check_cancelled()?;
        let (class, first) = self.object()?;
        if !first {
            return Err(format!("不支持共用场景节点 {class}"));
        }
        self.object_fields()?;
        if class == "osg::Geometry" {
            self.geometry(transform)?;
            return self.end();
        }
        if self.flag()? {
            self.fixed_block(32)?;
        }
        for _ in 0..4 {
            self.absent("不支持场景节点回调")?;
        }
        self.flag()?;
        self.u32()?;
        self.state()?;
        match class.as_str() {
            "osg::Group" | "osg::Geode" => self.children(transform, false)?,
            "osg::LOD" => {
                self.children(transform, false)?;
                self.lod()?;
            }
            "osg::MatrixTransform" => self.matrix_children(transform)?,
            "osg::PagedLOD" => self.paged_children(transform)?,
            _ => return Err(format!("不支持的场景节点 {class}")),
        }
        self.end()
    }

    fn children(&mut self, transform: Matrix, paged: bool) -> Result<(), String> {
        if self.flag()? {
            let count = self.u32()?;
            if !paged || count > 0 {
                self.node_block(count, transform)?;
            }
        }
        Ok(())
    }

    fn node_block(&mut self, count: u32, transform: Matrix) -> Result<(), String> {
        self.begin()?;
        for _ in 0..count {
            self.node(transform)?;
        }
        self.end()
    }

    fn matrix_children(&mut self, transform: Matrix) -> Result<(), String> {
        // Children precede the matrix in the stream, so use the size-prefixed
        // child block to read the transform before visiting its geometry.
        let children = if self.flag()? {
            let count = self.u32()?;
            let start = self.pos;
            self.skip_block()?;
            Some((start, count))
        } else {
            None
        };
        let frame = self.u32()?;
        let mut local = IDENTITY;
        for value in local.iter_mut().flatten() {
            *value = f64::from_le_bytes(self.take(8)?.try_into().unwrap());
        }
        if frame > 2
            || local.iter().flatten().any(|v| !v.is_finite())
            || local.map(|row| row[3]) != [0., 0., 0., 1.]
        {
            return Err("不支持的 MatrixTransform 矩阵".into());
        }
        if let Some((start, count)) = children {
            let combined = if frame == 0 {
                multiply(local, transform)
            } else {
                local
            };
            let resume = self.pos;
            self.pos = start;
            self.node_block(count, combined)?;
            self.pos = resume;
        }
        Ok(())
    }

    fn skip_block(&mut self) -> Result<(), String> {
        self.begin()?;
        self.pos = self.blocks.last().unwrap().end;
        self.end()
    }

    fn paged_children(&mut self, transform: Matrix) -> Result<(), String> {
        self.lod()?;
        let mut has_external_page = false;
        let start = self.pos;
        if self.flag()? && self.flag()? {
            let path = self.string()?;
            if !path.is_empty() {
                return Err(format!("不支持 PagedLOD DatabasePath：{path}"));
            }
            if self.boundary.is_some() {
                self.edit_in(
                    self.blocks.last().unwrap().start,
                    start..self.pos,
                    vec![1, 0],
                )?;
            }
        }
        self.u32()?;
        self.flag()?;
        if self.flag()? {
            let count = self.u32()?;
            self.begin()?;
            for _ in 0..count {
                let start = self.pos;
                let name = self.string()?;
                if !name.is_empty() {
                    if transform != IDENTITY {
                        return Err("不支持变换节点下的外部分页".into());
                    }
                    let relative = self.portable_reference(name, start..self.pos)?;
                    has_external_page = true;
                    if !self.raster_coarse {
                        self.pages.push(relative);
                    }
                }
            }
            self.end()?;
            let count = self.u32()? as usize;
            self.fixed_block(count * 8)?;
        }
        // 外部分页承载此分支的细节，内嵌子节点只是远景替代。
        let inherited = self.raster_coarse;
        self.raster_coarse |= self.raster.is_some() && has_external_page;
        let result = self.children(transform, true);
        self.raster_coarse = inherited;
        result
    }

    fn fixed_block(&mut self, size: usize) -> Result<(), String> {
        self.begin()?;
        self.take(size)?;
        self.end()
    }

    fn lod(&mut self) -> Result<(), String> {
        self.u32()?;
        if self.flag()? {
            self.take(32)?;
        }
        self.u32()?;
        if self.flag()? {
            let count = self.u32()? as usize;
            self.fixed_block(count * 8)?;
        }
        Ok(())
    }

    fn array(&mut self) -> Result<(usize, Array), String> {
        let (class, first) = self.object()?;
        if !first {
            return Err(format!("不支持共用网格数组 {class}"));
        }
        let (width, scalar) = match class.as_str() {
            "osg::Vec2Array" => (2, 4),
            "osg::Vec3Array" => (3, 4),
            "osg::Vec4Array" => (4, 4),
            "osg::Vec4ubArray" => (4, 1),
            "osg::Vec2dArray" => (2, 8),
            "osg::Vec3dArray" => (3, 8),
            "osg::Vec4dArray" => (4, 8),
            _ => return Err(format!("不支持的网格数组 {class}")),
        };
        self.object_fields()?;
        let binding = self.u32()?;
        self.flag()?;
        self.flag()?;
        let start = self.pos;
        let count = self.u32()? as usize;
        let values: Vec<f64> = self
            .take(count * width * scalar)?
            .chunks_exact(scalar)
            .map(|chunk| match scalar {
                1 => f64::from(chunk[0]),
                4 => f64::from(f32::from_le_bytes(chunk.try_into().unwrap())),
                _ => f64::from_le_bytes(chunk.try_into().unwrap()),
            })
            .collect();
        if values.iter().any(|v| !v.is_finite()) {
            return Err("顶点数据包含非有限数值".into());
        }
        let range = start..self.pos;
        let block = self.blocks.last().unwrap().start;
        self.end()?;
        Ok((
            block,
            Array {
                range,
                values,
                width,
                scalar,
                binding,
            },
        ))
    }

    fn optional_array(&mut self) -> Result<Option<(usize, Array)>, String> {
        if self.flag()? {
            self.array().map(Some)
        } else {
            Ok(None)
        }
    }

    fn state(&mut self) -> Result<(), String> {
        if !self.flag()? {
            return Ok(());
        }
        let (class, first) = self.object()?;
        if class != "osg::StateSet" {
            return Err(format!("不支持的渲染状态 {class}"));
        }
        let id = self.object_id;
        let inherited = self.raster.as_ref().and_then(|mesh| mesh.active);
        if first {
            if let Some(mesh) = &mut self.raster {
                mesh.active = None;
            }
            self.object_fields()?;
            if self.flag()? {
                self.state_modes()?;
            }
            if self.flag()? {
                self.state_attributes(None)?;
            }
            for is_texture in [false, true] {
                if self.flag()? {
                    let count = self.u32()?;
                    self.begin()?;
                    for unit in 0..count {
                        if is_texture {
                            self.state_attributes(Some(unit))?;
                        } else {
                            self.state_modes()?;
                        }
                    }
                    self.end()?;
                }
            }
            self.absent("不支持 Shader Uniform")?;
            self.take(12)?;
            self.string()?;
            self.flag()?;
            self.absent("不支持渲染状态更新回调")?;
            self.absent("不支持渲染状态事件回调")?;
            if let Some(mesh) = &mut self.raster {
                mesh.states.insert(id, mesh.active);
            }
        }
        if let Some(mesh) = &mut self.raster {
            mesh.active = mesh.states[&id].or(inherited);
        }
        self.end()
    }

    fn state_modes(&mut self) -> Result<(), String> {
        let count = self.u32()? as usize;
        if count > 0 {
            self.fixed_block(count * 8)?;
        }
        Ok(())
    }

    fn state_attributes(&mut self, unit: Option<u32>) -> Result<(), String> {
        let count = self.u32()?;
        if count == 0 {
            return Ok(());
        }
        self.begin()?;
        for _ in 0..count {
            let (class, first) = self.object()?;
            match class.as_str() {
                "osg::Material" => {
                    if first {
                        self.object_fields()?;
                        self.absent("不支持材质更新回调")?;
                        self.absent("不支持材质事件回调")?;
                        self.u32()?;
                        for size in [32, 32, 32, 32, 8] {
                            if self.flag()? {
                                self.flag()?;
                                self.take(size)?;
                            }
                        }
                    }
                }
                "osg::Texture2D" => {
                    if self.raster.is_some() && unit != Some(0) {
                        return Err(format!("不支持纹理单元 {unit:?} 上的 Texture2D"));
                    }
                    let id = self.object_id;
                    let image = if first { Some(self.texture()?) } else { None };
                    if let Some(mesh) = &mut self.raster {
                        if let Some(image) = image {
                            mesh.textures.insert(id, image);
                        }
                        mesh.active = Some(mesh.textures[&id]);
                    }
                }
                _ => return Err(format!("不支持的材质属性 {class}")),
            }
            self.end()?;
            self.u32()?;
        }
        self.end()
    }

    fn texture(&mut self) -> Result<u32, String> {
        self.object_fields()?;
        self.absent("不支持纹理更新回调")?;
        self.absent("不支持纹理事件回调")?;
        for _ in 0..5 {
            if self.flag()? {
                self.u32()?;
            }
        }
        self.take(48)?;
        for _ in 0..3 {
            if self.flag()? {
                self.u32()?;
            }
        }
        self.take(13)?;
        self.absent("不支持纹理 ImageAttachment")?;
        if self.flag()? {
            let swizzle = self.string()?;
            if swizzle.len() != 4 {
                return Err("无效的纹理 Swizzle".into());
            }
            if self.raster.is_some() && !swizzle.eq_ignore_ascii_case("rgba") {
                return Err(format!("不支持的纹理通道映射 {swizzle}"));
            }
        }
        if !self.flag()? {
            return Err("Texture2D 缺少图像".into());
        }
        let image = self.image()?;
        self.take(8)?;
        Ok(image)
    }

    fn image(&mut self) -> Result<u32, String> {
        let class = self.string()?;
        if class != "osg::Image" {
            return Err(format!("不支持图像类型 {class}"));
        }
        let id = self.u32()?;
        if !self.identify(id, &class)? {
            return Ok(id);
        }
        let start = self.pos;
        let name = self.string()?;
        let end = self.pos;
        self.u32()?;
        match self.u32()? {
            0 => {
                if self.raster.is_some() {
                    return Err(format!("纹理 {name} 使用不支持的原始像素存储"));
                }
                self.take(36)?;
                let size = self.u32()? as usize;
                if size == 0 {
                    return Err(format!("纹理 {name} 没有像素数据"));
                }
                self.take(size)?;
                let mipmaps = self.u32()? as usize;
                self.take(mipmaps * 4)?;
            }
            1 => {
                let size = self.u32()? as usize;
                if size == 0 {
                    return Err(format!("纹理 {name} 没有内嵌数据"));
                }
                let start = self.pos;
                self.take(size)?;
                let end = self.pos;
                if let Some(mesh) = &mut self.raster {
                    mesh.images.insert(
                        id,
                        RasterImage::Embedded {
                            name,
                            bytes: start..end,
                        },
                    );
                }
            }
            2 | 3 => {
                if name.is_empty() {
                    return Err("外部纹理缺少文件名".into());
                }
                let relative = self.portable_reference(name, start..end)?;
                if let Some(mesh) = &mut self.raster {
                    mesh.images
                        .insert(id, RasterImage::External(relative.clone()));
                }
                self.textures.push(relative);
            }
            kind => return Err(format!("不支持的纹理存储方式 {kind}")),
        }
        self.object_fields()?;
        Ok(id)
    }

    fn portable_reference(&mut self, value: String, range: Range<usize>) -> Result<String, String> {
        let relative = value.replace('\\', "/");
        if self.boundary.is_some() && relative != value {
            let mut encoded = Vec::new();
            push_u32(&mut encoded, relative.len() as u32);
            encoded.extend_from_slice(relative.as_bytes());
            let block = self.blocks.last().unwrap().start;
            self.edit_in(block, range, encoded)?;
        }
        Ok(relative)
    }

    fn geometry(&mut self, transform: Matrix) -> Result<(), String> {
        self.state()?;
        if self.flag()? {
            self.fixed_block(48)?;
        }
        self.absent("不支持 Drawable 包围盒回调")?;
        self.absent("不支持 Drawable Shape")?;
        for _ in 0..3 {
            self.flag()?;
        }
        for _ in 0..4 {
            self.absent("不支持 Drawable 回调")?;
        }
        let count_offset = self.pos;
        let primitive_count = self.u32()?;
        let mut retained_primitives = primitive_count;
        let mut primitives = Vec::new();
        for _ in 0..primitive_count {
            let start = self.pos;
            let (class, first) = self.object()?;
            if !first {
                return Err(format!("不支持共用索引 {class}"));
            }
            let size = match class.as_str() {
                "osg::DrawElementsUByte" => 1,
                "osg::DrawElementsUShort" => 2,
                "osg::DrawElementsUInt" => 4,
                _ => return Err(format!("不支持的图元 {class}")),
            };
            let fields = self.pos - 4;
            self.object_fields()?;
            if self.u32()? > 1 || self.u32()? != 4 {
                return Err("仅支持非实例化的 GL_TRIANGLES 图元".into());
            }
            let header_end = self.pos;
            let count = self.u32()? as usize;
            let indices: Vec<usize> = self
                .take(count * size)?
                .chunks_exact(size)
                .map(|v| match size {
                    1 => usize::from(v[0]),
                    2 => usize::from(u16::from_le_bytes(v.try_into().unwrap())),
                    _ => u32::from_le_bytes(v.try_into().unwrap()) as usize,
                })
                .collect();
            if !count.is_multiple_of(3) {
                return Err("索引数量不是三的倍数".into());
            }
            self.end()?;
            primitives.push((start..self.pos, fields..header_end, indices));
        }
        let vertices = self.optional_array()?;
        if vertices.is_none() && primitive_count != 0 {
            return Err("缺少位置数组".into());
        }
        let mut arrays = Vec::new();
        for kind in 0..4 {
            if let Some(array) = self.optional_array()? {
                arrays.push((kind, array));
            }
        }
        for _ in 0..self.u32()? {
            arrays.push((4, self.array()?));
        }
        if self.u32()? != 0 {
            return Err("不支持自定义顶点属性".into());
        }
        let Some((vertex_block, vertices)) = vertices else {
            // 分页节点可以内嵌无图元的空几何。
            return Ok(());
        };
        if vertices.width != 3 {
            return Err("位置数组必须为三维".into());
        }
        let vertex_count = vertices.values.len() / 3;
        for (_, (_, array)) in &arrays {
            if array.binding == 1 {
                continue;
            } // BIND_OVERALL
            if !matches!(array.binding, 4 | u32::MAX)
                || array.values.len() / array.width != vertex_count
            {
                return Err(format!("不支持的顶点数组绑定 {}", array.binding));
            }
        }
        let uv = arrays
            .iter()
            .find(|(kind, _)| *kind == 4)
            .map(|(_, (_, array))| array);
        if self
            .raster
            .as_ref()
            .is_some_and(|mesh| mesh.active.is_some())
            && !uv.is_some_and(|array| array.width == 2 && array.binding != 1)
        {
            return Err("纹理网格缺少逐顶点 UV".into());
        }
        let mut samples = Vec::new();
        for (range, header, indices) in primitives {
            let first = samples.len();
            for (index, face) in indices.chunks_exact(3).enumerate() {
                if index % 4096 == 0 {
                    self.check_cancelled()?;
                }
                let ids = [face[0], face[1], face[2]];
                if ids.iter().any(|&id| id >= vertex_count) {
                    return Err("索引超出顶点范围".into());
                }
                if let Some(mesh) = &mut self.raster
                    && !self.raster_coarse
                {
                    let xyz = ids.map(|id| {
                        let v = &vertices.values[id * 3..id * 3 + 3];
                        std::array::from_fn(|axis| {
                            v[0] * transform[0][axis]
                                + v[1] * transform[1][axis]
                                + v[2] * transform[2][axis]
                                + transform[3][axis]
                        })
                    });
                    if xyz.iter().flatten().any(|v| !v.is_finite()) {
                        return Err("模型变换后的坐标超出范围".into());
                    }
                    let uv = uv
                        .filter(|array| array.width == 2 && array.binding != 1)
                        .map(|array| {
                            ids.map(|id| [array.values[id * 2], array.values[id * 2 + 1]])
                        });
                    mesh.triangles.push(RasterTriangle {
                        xyz,
                        uv,
                        texture: mesh.active,
                    });
                }
                if let Some(boundary) = self.boundary {
                    let xy = ids.map(|id| {
                        let v = &vertices.values[id * 3..id * 3 + 3];
                        Coord {
                            x: v[0] * transform[0][0]
                                + v[1] * transform[1][0]
                                + v[2] * transform[2][0]
                                + transform[3][0],
                            y: v[0] * transform[0][1]
                                + v[1] * transform[1][1]
                                + v[2] * transform[2][1]
                                + transform[3][1],
                        }
                    });
                    if xy.iter().any(|p| !p.x.is_finite() || !p.y.is_finite()) {
                        return Err("模型变换后的坐标超出范围".into());
                    }
                    for clipped in boundary.clip(xy) {
                        samples.extend(clipped.map(|weights| (ids, weights)));
                    }
                }
            }
            if self.boundary.is_some() {
                let last = u32::try_from(samples.len()).map_err(|_| "顶点数量超过 OSGB 上限")?;
                let first = first as u32;
                if last == first {
                    // OSG may dereference the first index even for zero indices.
                    retained_primitives -= 1;
                    self.edit_in(self.blocks.last().unwrap().start, range, Vec::new())?;
                    continue;
                }
                let mut primitive = Vec::new();
                let class = b"osg::DrawElementsUInt";
                push_u32(&mut primitive, class.len() as u32);
                primitive.extend_from_slice(class);
                let size = i32::try_from(4usize + header.len() + 4 + ((last - first) as usize) * 4)
                    .map_err(|_| "索引块长度超过 OSGB 上限")?;
                primitive.extend_from_slice(&size.to_le_bytes());
                primitive.extend_from_slice(&self.bytes[header]);
                push_u32(&mut primitive, last - first);
                for id in first..last {
                    push_u32(&mut primitive, id);
                }
                self.edit_in(self.blocks.last().unwrap().start, range, primitive)?;
                self.triangles += (last - first) as usize / 3;
            } else {
                self.triangles += indices.len() / 3;
            }
        }
        if self.boundary.is_some() {
            if retained_primitives != primitive_count {
                self.edit_in(
                    self.blocks.last().unwrap().start,
                    count_offset..count_offset + 4,
                    retained_primitives.to_le_bytes().to_vec(),
                )?;
            }
            self.write_array(vertex_block, vertices, &samples, false)?;
            for (kind, (block, array)) in arrays {
                if array.binding != 1 {
                    self.write_array(block, array, &samples, kind == 0)?;
                }
            }
        }
        Ok(())
    }

    fn write_array(
        &mut self,
        block: usize,
        array: Array,
        samples: &[([usize; 3], Weights)],
        normal: bool,
    ) -> Result<(), String> {
        let mut replacement = Vec::new();
        push_u32(&mut replacement, samples.len() as u32);
        for (index, &(ids, weights)) in samples.iter().enumerate() {
            if index % 4096 == 0 {
                self.check_cancelled()?;
            }
            let mut values = [0.; 4];
            for (i, value) in values[..array.width].iter_mut().enumerate() {
                *value = interpolate(ids.map(|id| array.values[id * array.width + i]), weights);
            }
            if normal {
                let length = values.iter().map(|v| v * v).sum::<f64>().sqrt();
                if length > 0. {
                    values.iter_mut().for_each(|v| *v /= length);
                }
            }
            for &value in &values[..array.width] {
                match array.scalar {
                    1 => replacement.push(value.round() as u8),
                    4 => replacement.extend_from_slice(&(value as f32).to_le_bytes()),
                    _ => replacement.extend_from_slice(&value.to_le_bytes()),
                }
            }
        }
        self.edit_in(block, array.range, replacement)
    }
}

fn push_u32(bytes: &mut Vec<u8>, value: u32) {
    bytes.extend_from_slice(&value.to_le_bytes());
}

fn multiply(a: Matrix, b: Matrix) -> Matrix {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..4).map(|k| a[i][k] * b[k][j]).sum()))
}
