use super::{Boundary, Reference, osgb, vector};
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashSet, VecDeque},
    fs, io,
    path::{Component, Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub model: PathBuf,
    pub entry: Option<PathBuf>,
    pub reference: Option<Reference>,
    pub boundary: PathBuf,
    pub layer: Option<String>,
    pub boundary_crs: Option<String>,
    pub output: PathBuf,
    #[serde(default)]
    pub allow_non_empty_output: bool,
}

#[derive(Clone, Serialize)]
pub struct Preflight {
    pub reference: Reference,
    pub entries: Vec<PathBuf>,
    pub output: PathBuf,
    pub output_non_empty: bool,
}

pub struct Progress {
    pub completed: usize,
    pub discovered: usize,
    pub file: PathBuf,
}

#[derive(Clone, Serialize)]
pub struct Report {
    pub output: PathBuf,
    pub entries: Vec<PathBuf>,
    pub files: usize,
    pub triangles: usize,
}

struct Prepared {
    summary: Preflight,
    root: PathBuf,
    metadata: Vec<u8>,
    boundary: Boundary,
    single: bool,
}

pub fn preflight(request: &Request) -> Result<Preflight, String> {
    preflight_cancelable(request, &AtomicBool::new(false))
}

pub fn preflight_cancelable(
    request: &Request,
    cancelled: &AtomicBool,
) -> Result<Preflight, String> {
    check_cancelled(cancelled)?;
    let prepared = prepare(request)?;
    for entry in &prepared.summary.entries {
        check_cancelled(cancelled)?;
        let (file, scene) = read_scene(&prepared.root, entry, cancelled)?;
        for name in scene.pages.iter().chain(&scene.textures) {
            check_cancelled(cancelled)?;
            ensure_input(&prepared.root, &resolve_reference(entry, name)?)?;
        }
        if prepared.single && !scene.pages.is_empty() {
            return Err(format!("{} 含外部分页，请选择模型目录", file.display()));
        }
    }
    check_cancelled(cancelled)?;
    Ok(prepared.summary)
}

pub fn run(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
    before_publish: impl FnOnce() -> Result<(), String>,
) -> Result<Report, String> {
    check_cancelled(cancelled)?;
    let prepared = prepare(request)?;
    check_cancelled(cancelled)?;
    let output = &prepared.summary.output;
    let existing = output_non_empty(output)?;
    if existing == Some(true) && !request.allow_non_empty_output {
        return Err("输出目录非空，请确认后再开始裁剪".into());
    }
    let staging = tempfile::Builder::new()
        .prefix(".osgb-clip-")
        .tempdir_in(if existing.is_some() {
            output.as_path()
        } else {
            output.parent().unwrap()
        })
        .map_err(|err| file_error(output, err))?;
    let stage = staging.path();
    let mut pending: VecDeque<_> = prepared.summary.entries.iter().cloned().collect();
    let mut seen: HashSet<_> = pending.iter().cloned().collect();
    let mut textures = HashSet::new();
    let mut completed = 0;
    let mut triangles = 0;
    if seen.contains(Path::new("metadata.xml")) {
        return Err("模型入口与 metadata.xml 冲突".into());
    }
    write_file(&stage.join("metadata.xml"), &prepared.metadata)?;
    while let Some(relative) = pending.pop_front() {
        check_cancelled(cancelled)?;
        progress(Progress {
            completed,
            discovered: seen.len(),
            file: relative.clone(),
        });
        let file = ensure_input(&prepared.root, &relative)?;
        let input = fs::read(&file).map_err(|err| file_error(&file, err))?;
        let clipped = osgb::rewrite_cancelable(&input, Some(&prepared.boundary), Some(cancelled))
            .map_err(|err| file_error(&file, err))?;
        if prepared.single && !clipped.info.pages.is_empty() {
            return Err(format!("{} 含外部分页，请选择模型目录", file.display()));
        }
        for name in &clipped.info.pages {
            let path = resolve_reference(&relative, name).map_err(|err| file_error(&file, err))?;
            if vector::extension(&path) != "osgb" {
                return Err(format!("{} 引用了非 OSGB 分页 {name}", file.display()));
            }
            if seen.insert(path.clone()) {
                pending.push_back(path);
            }
        }
        for name in &clipped.info.textures {
            check_cancelled(cancelled)?;
            let path = resolve_reference(&relative, name).map_err(|err| file_error(&file, err))?;
            if vector::extension(&path) == "osgb" || path == Path::new("metadata.xml") {
                return Err(format!(
                    "{} 的纹理路径与模型或元数据冲突：{name}",
                    file.display()
                ));
            }
            if textures.insert(path.clone()) {
                let source = ensure_input(&prepared.root, &path)?;
                let target = stage.join(path);
                create_parent(&target)?;
                let mut input = fs::File::open(&source).map_err(|err| file_error(&source, err))?;
                let mut out = fs::File::options()
                    .create_new(true)
                    .write(true)
                    .open(&target)
                    .map_err(|err| file_error(&target, err))?;
                if io::copy(&mut input, &mut out).map_err(|err| file_error(&target, err))? == 0 {
                    return Err(format!("纹理文件为空：{}", source.display()));
                }
            }
        }
        let verified =
            osgb::inspect(&clipped.bytes, Some(cancelled)).map_err(|err| file_error(&file, err))?;
        if verified.triangles != clipped.info.triangles
            || verified.pages != clipped.info.pages
            || verified.textures != clipped.info.textures
        {
            return Err(format!("输出校验失败：{}", file.display()));
        }
        write_file(&stage.join(&relative), &clipped.bytes)?;
        completed += 1;
        triangles += clipped.info.triangles;
        progress(Progress {
            completed,
            discovered: seen.len(),
            file: relative,
        });
    }
    check_cancelled(cancelled)?;
    if triangles == 0 {
        return Err("模型与裁剪范围没有交集".into());
    }
    before_publish()?;
    if existing.is_some() {
        let non_empty = directory_has_entries(output, Some(stage))?;
        if non_empty && !request.allow_non_empty_output {
            return Err("输出目录非空，请确认后再开始裁剪".into());
        }
        publish_into_existing(stage, output)?;
    } else {
        publish(stage, output).map_err(|err| output_error(output, err))?;
    }
    Ok(Report {
        output: output.clone(),
        entries: prepared
            .summary
            .entries
            .iter()
            .map(|path| output.join(path))
            .collect(),
        files: completed,
        triangles,
    })
}

fn prepare(request: &Request) -> Result<Prepared, String> {
    let model = real_path(&request.model)?;
    let (root, entries, reference, metadata, single) = if model.is_dir() {
        let path = model.join("metadata.xml");
        let metadata = fs::read(&path).map_err(|err| file_error(&path, err))?;
        let reference = Reference::parse(&metadata).map_err(|err| file_error(&path, err))?;
        let entries = match &request.entry {
            Some(entry) => {
                let entry = if entry.is_absolute() {
                    entry.clone()
                } else {
                    model.join(entry)
                };
                let entry = real_path(&entry)?;
                vec![
                    entry
                        .strip_prefix(&model)
                        .map_err(|_| "瓦片入口必须位于模型目录内")?
                        .to_path_buf(),
                ]
            }
            None => auto_entries(&model)?,
        };
        (model, entries, reference, metadata, false)
    } else {
        if !model.is_file() || vector::extension(&model) != "osgb" {
            return Err("请选择 OSGB 文件或包含 metadata.xml 的模型目录".into());
        }
        let reference = request
            .reference
            .clone()
            .ok_or("单文件模式需要填写模型坐标系与 XYZ 原点")?;
        let metadata = reference.xml().into_bytes();
        let root = model.parent().ok_or("模型没有父目录")?.to_path_buf();
        let entries = vec![PathBuf::from(model.file_name().ok_or("模型没有文件名")?)];
        (root, entries, reference, metadata, true)
    };
    if entries.iter().any(|path| vector::extension(path) != "osgb") {
        return Err("瓦片入口必须为 OSGB 文件".into());
    }
    let output = output_path(&request.output)?;
    if !single && output.starts_with(&root) {
        return Err("请将裁剪结果保存到模型目录之外".into());
    }
    let (polygons, source) = vector::read(
        &request.boundary,
        request.layer.as_deref(),
        request.boundary_crs.as_deref(),
    )
    .map_err(|err| file_error(&request.boundary, err))?;
    let polygons = reference.local_polygons(polygons, &source)?;
    let boundary = Boundary::new(&polygons)?;
    Ok(Prepared {
        summary: Preflight {
            reference,
            entries,
            output_non_empty: output_non_empty(&output)?.unwrap_or(false),
            output,
        },
        root,
        metadata,
        boundary,
        single,
    })
}

fn output_path(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err("输出位置需要为绝对路径".into());
    }
    let name = path.file_name().ok_or("请输入新输出目录名称")?;
    let parent = real_path(path.parent().ok_or("输出位置缺少父目录")?)?;
    if !parent.is_dir() {
        return Err("输出位置的父路径不是目录".into());
    }
    Ok(parent.join(name))
}

fn output_non_empty(path: &Path) -> Result<Option<bool>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => directory_has_entries(path, None).map(Some),
        Ok(_) => Err(format!("输出路径已存在且不是普通目录：{}", path.display())),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(file_error(path, err)),
    }
}

fn directory_has_entries(path: &Path, ignored: Option<&Path>) -> Result<bool, String> {
    let entries = fs::read_dir(path).map_err(|err| file_error(path, err))?;
    for entry in entries {
        let entry = entry.map_err(|err| file_error(path, err))?;
        if Some(entry.path().as_path()) != ignored {
            return Ok(true);
        }
    }
    Ok(false)
}

fn output_error(path: &Path, err: io::Error) -> String {
    if err.kind() == io::ErrorKind::AlreadyExists || path.symlink_metadata().is_ok() {
        format!("输出位置已存在，请更换位置：{}", path.display())
    } else {
        file_error(path, err)
    }
}

fn publish_into_existing(stage: &Path, output: &Path) -> Result<(), String> {
    let mut files = Vec::new();
    let mut directories = Vec::new();
    let result = merge_output(stage, output, &mut files, &mut directories);
    if let Err(err) = result {
        let mut cleanup_errors = Vec::new();
        for path in files.iter().rev() {
            if let Err(error) = fs::remove_file(path) {
                cleanup_errors.push(file_error(path, error));
            }
        }
        for path in directories.iter().rev() {
            if let Err(error) = fs::remove_dir(path) {
                cleanup_errors.push(file_error(path, error));
            }
        }
        if !cleanup_errors.is_empty() {
            return Err(format!(
                "{err}。部分输出清理失败：{}",
                cleanup_errors.join("；")
            ));
        }
        return Err(err);
    }
    Ok(())
}

fn merge_output(
    stage: &Path,
    output: &Path,
    files: &mut Vec<PathBuf>,
    directories: &mut Vec<PathBuf>,
) -> Result<(), String> {
    for source in list_files(stage)? {
        let target = output.join(source.file_name().unwrap());
        if source.is_dir() {
            match fs::create_dir(&target) {
                Ok(()) => directories.push(target.clone()),
                Err(err)
                    if err.kind() == io::ErrorKind::AlreadyExists
                        && target
                            .symlink_metadata()
                            .is_ok_and(|metadata| metadata.is_dir()) => {}
                Err(err) => return Err(output_error(&target, err)),
            }
            merge_output(&source, &target, files, directories)?;
        } else {
            publish_file(&source, &target).map_err(|err| output_error(&target, err))?;
            files.push(target);
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn publish_file(from: &Path, to: &Path) -> io::Result<()> {
    publish(from, to)
}

#[cfg(windows)]
fn publish_file(from: &Path, to: &Path) -> io::Result<()> {
    // Windows rename can replace files. Open the destination exclusively.
    let mut source = fs::File::open(from)?;
    let mut target = fs::File::options().write(true).create_new(true).open(to)?;
    let result = io::copy(&mut source, &mut target);
    drop(target);
    if let Err(err) = result {
        fs::remove_file(to)?;
        return Err(err);
    }
    Ok(())
}

fn auto_entries(root: &Path) -> Result<Vec<PathBuf>, String> {
    let entries = list_files(root)?;
    if let Some(entry) = entries.iter().find(|path| {
        path.file_name()
            .is_some_and(|name| name.eq_ignore_ascii_case("Block.osgb"))
    }) {
        return Ok(vec![entry.strip_prefix(root).unwrap().into()]);
    }
    let top: Vec<_> = entries
        .iter()
        .filter(|p| p.is_file() && vector::extension(p) == "osgb")
        .collect();
    match top.as_slice() {
        [entry] => return Ok(vec![entry.strip_prefix(root).unwrap().into()]),
        [] => {}
        _ => return Err("目录存在多个顶级 OSGB，请手动选择瓦片入口".into()),
    }
    let data_dir = root.join("Data");
    let folders = if data_dir.is_dir() {
        list_files(&data_dir)?
    } else {
        entries
    };
    let mut tiles = Vec::new();
    for folder in folders.into_iter().filter(|path| path.is_dir()) {
        let name = folder.file_name().unwrap();
        let entry = list_files(&folder)?.into_iter().find(|path| {
            path.is_file()
                && vector::extension(path) == "osgb"
                && path
                    .file_stem()
                    .is_some_and(|stem| stem.eq_ignore_ascii_case(name))
        });
        if let Some(path) = entry {
            tiles.push(path.strip_prefix(root).unwrap().to_path_buf());
        }
    }
    if tiles.is_empty() {
        return Err("未找到标准瓦片入口，请手动选择 OSGB 文件".into());
    }
    tiles.sort_unstable();
    Ok(tiles)
}

fn list_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    fs::read_dir(dir)
        .map_err(|err| file_error(dir, err))?
        .map(|entry| {
            entry
                .map(|entry| entry.path())
                .map_err(|err| file_error(dir, err))
        })
        .collect()
}

fn resolve_reference(scene: &Path, name: &str) -> Result<PathBuf, String> {
    if name.is_empty() || name.contains([':', '\0']) {
        return Err(format!("不支持的外部引用路径 {name}"));
    }
    let mut path = scene.parent().unwrap_or(Path::new("")).to_path_buf();
    for part in Path::new(name).components() {
        match part {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir if path.pop() => {}
            _ => return Err(format!("外部引用超出输入目录：{name}")),
        }
    }
    if path.as_os_str().is_empty() {
        return Err(format!("无效外部引用：{name}"));
    }
    Ok(path)
}

fn ensure_input(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let candidate = root.join(relative);
    let real = real_path(&candidate)?;
    if !real.starts_with(root) || !real.is_file() {
        return Err(format!(
            "输入依赖不是模型目录内的文件：{}",
            candidate.display()
        ));
    }
    Ok(real)
}

fn real_path(path: &Path) -> Result<PathBuf, String> {
    fs::canonicalize(path).map_err(|err| file_error(path, err))
}

fn read_scene(
    root: &Path,
    entry: &Path,
    cancelled: &AtomicBool,
) -> Result<(PathBuf, osgb::SceneInfo), String> {
    let file = ensure_input(root, entry)?;
    let bytes = fs::read(&file).map_err(|err| file_error(&file, err))?;
    let scene = osgb::inspect(&bytes, Some(cancelled)).map_err(|err| file_error(&file, err))?;
    Ok((file, scene))
}

fn create_parent(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path.parent().unwrap()).map_err(|err| file_error(path, err))
}

fn write_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use io::Write;
    create_parent(path)?;
    let mut file = fs::File::options()
        .create_new(true)
        .write(true)
        .open(path)
        .map_err(|err| file_error(path, err))?;
    file.write_all(bytes).map_err(|err| file_error(path, err))
}

fn file_error(path: &Path, err: impl std::fmt::Display) -> String {
    format!("{}：{err}", path.display())
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("裁剪已取消".into());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish(from: &Path, to: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, from, CWD, to, RenameFlags::NOREPLACE).map_err(io::Error::from)
}

#[cfg(windows)]
fn publish(from: &Path, to: &Path) -> io::Result<()> {
    // MoveFileEx cannot replace an existing directory.
    fs::rename(from, to)
}
