use super::{Boundary, Reference, osgb, vector};
use crate::message;
use crate::task_pool::{self, ThreadPool};
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
            return Err(message!(
                contains_external_pages_select_the_model_folder,
                file.display()
            ));
        }
    }
    check_cancelled(cancelled)?;
    Ok(prepared.summary)
}

pub fn run(
    request: &Request,
    cancelled: &AtomicBool,
    progress: impl FnMut(Progress),
    before_publish: impl FnOnce() -> Result<(), String>,
) -> Result<Report, String> {
    run_with_pool(request, cancelled, progress, before_publish, None)
}

pub fn run_with_pool(
    request: &Request,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
    before_publish: impl FnOnce() -> Result<(), String>,
    pool: Option<&ThreadPool>,
) -> Result<Report, String> {
    check_cancelled(cancelled)?;
    let prepared = prepare(request)?;
    check_cancelled(cancelled)?;
    let output = &prepared.summary.output;
    let existing = output_non_empty(output)?;
    if existing == Some(true) && !request.allow_non_empty_output {
        return Err(message!(the_output_folder_is_not_empty_confirm_before_clipping).into());
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
        return Err(message!(model_entry_conflicts_with_metadata_xml).into());
    }
    write_file(&stage.join("metadata.xml"), &prepared.metadata)?;
    while !pending.is_empty() {
        let batch: Vec<_> = pending
            .drain(..pending.len().min(task_pool::width(pool)))
            .collect();
        check_cancelled(cancelled)?;
        progress(Progress {
            completed,
            discovered: seen.len(),
            file: batch[0].clone(),
        });
        let models = task_pool::map(pool, batch, |relative| {
            check_cancelled(cancelled)?;
            let file = ensure_input(&prepared.root, &relative)?;
            let input = fs::read(&file).map_err(|err| file_error(&file, err))?;
            let clipped =
                osgb::rewrite_cancelable(&input, Some(&prepared.boundary), Some(cancelled))
                    .map_err(|err| message!(clip_osgb_failed, file_error(&file, err)))?;
            let verified = osgb::inspect(&clipped.bytes, Some(cancelled))
                .map_err(|err| file_error(&file, err))?;
            if verified.triangles != clipped.info.triangles
                || verified.pages != clipped.info.pages
                || verified.textures != clipped.info.textures
            {
                return Err(message!(output_validation_failed, file.display()));
            }
            Ok((relative, file, clipped))
        })?;
        for (relative, file, clipped) in models {
            check_cancelled(cancelled)?;
            if prepared.single && !clipped.info.pages.is_empty() {
                return Err(message!(
                    contains_external_pages_select_the_model_folder,
                    file.display()
                ));
            }
            for name in &clipped.info.pages {
                let path =
                    resolve_reference(&relative, name).map_err(|err| file_error(&file, err))?;
                if vector::extension(&path) != "osgb" {
                    return Err(message!(
                        references_a_non_osgb_page_name,
                        file.display(),
                        name = name
                    ));
                }
                if seen.insert(path.clone()) {
                    pending.push_back(path);
                }
            }
            for name in &clipped.info.textures {
                check_cancelled(cancelled)?;
                let path =
                    resolve_reference(&relative, name).map_err(|err| file_error(&file, err))?;
                if vector::extension(&path) == "osgb" || path == Path::new("metadata.xml") {
                    return Err(message!(
                        texture_path_in_conflicts_with_a_model_or_metadata_file_name,
                        file.display(),
                        name = name
                    ));
                }
                if textures.insert(path.clone()) {
                    let source = ensure_input(&prepared.root, &path)?;
                    let target = stage.join(path);
                    create_parent(&target)?;
                    let mut input =
                        fs::File::open(&source).map_err(|err| file_error(&source, err))?;
                    let mut out = fs::File::options()
                        .create_new(true)
                        .write(true)
                        .open(&target)
                        .map_err(|err| file_error(&target, err))?;
                    if io::copy(&mut input, &mut out).map_err(|err| file_error(&target, err))? == 0
                    {
                        return Err(message!(texture_file_is_empty, source.display()));
                    }
                }
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
    }
    check_cancelled(cancelled)?;
    if triangles == 0 {
        return Err(message!(the_model_does_not_intersect_the_clip_boundary).into());
    }
    before_publish()?;
    if existing.is_some() {
        let non_empty = directory_has_entries(output, Some(stage))?;
        if non_empty && !request.allow_non_empty_output {
            return Err(message!(the_output_folder_is_not_empty_confirm_before_clipping).into());
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
                        .map_err(|_| message!(the_tile_entry_must_be_within_the_model_folder))?
                        .to_path_buf(),
                ]
            }
            None => auto_entries(&model)?,
        };
        (model, entries, reference, metadata, false)
    } else {
        if !model.is_file() || vector::extension(&model) != "osgb" {
            return Err(
                message!(select_an_osgb_file_or_a_model_folder_containing_metadata_xml).into(),
            );
        }
        let reference = request.reference.clone().ok_or(message!(
            single_file_mode_requires_the_model_crs_and_xyz_origin
        ))?;
        let metadata = reference.xml().into_bytes();
        let root = model
            .parent()
            .ok_or(message!(model_has_no_parent_folder))?
            .to_path_buf();
        let entries = vec![PathBuf::from(
            model.file_name().ok_or(message!(model_has_no_file_name))?,
        )];
        (root, entries, reference, metadata, true)
    };
    if entries.iter().any(|path| vector::extension(path) != "osgb") {
        return Err(message!(the_tile_entry_must_be_an_osgb_file).into());
    }
    let output = output_path(&request.output)?;
    if !single && output.starts_with(&root) {
        return Err(message!(save_the_clipped_outputs_outside_the_model_folder).into());
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
        return Err(message!(output_location_must_be_an_absolute_path).into());
    }
    let name = path
        .file_name()
        .ok_or(message!(enter_a_name_for_the_new_output_folder))?;
    let parent = real_path(
        path.parent()
            .ok_or(message!(output_location_has_no_parent_folder))?,
    )?;
    if !parent.is_dir() {
        return Err(message!(the_parent_of_the_output_path_is_not_a_folder).into());
    }
    Ok(parent.join(name))
}

fn output_non_empty(path: &Path) -> Result<Option<bool>, String> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => directory_has_entries(path, None).map(Some),
        Ok(_) => Err(message!(
            output_path_exists_and_is_not_a_regular_folder,
            path.display()
        )),
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
        message!(
            output_location_already_exists_choose_another_location,
            path.display()
        )
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
            return Err(message!(
                err_could_not_clean_up_some_outputs,
                cleanup_errors.join("；"),
                err = err
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
        _ => {
            return Err(message!(
                the_folder_contains_multiple_top_level_osgb_files_select_the_tile_entry_manually
            )
            .into());
        }
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
        return Err(message!(no_standard_tile_entry_found_choose_an_osgb_file_manually).into());
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
        return Err(message!(
            unsupported_external_reference_path_name,
            name = name
        ));
    }
    let mut path = scene.parent().unwrap_or(Path::new("")).to_path_buf();
    for part in Path::new(name).components() {
        match part {
            Component::Normal(part) => path.push(part),
            Component::CurDir => {}
            Component::ParentDir if path.pop() => {}
            _ => {
                return Err(message!(
                    external_reference_is_outside_the_input_folder_name,
                    name = name
                ));
            }
        }
    }
    if path.as_os_str().is_empty() {
        return Err(message!(invalid_external_reference_name, name = name));
    }
    Ok(path)
}

fn ensure_input(root: &Path, relative: &Path) -> Result<PathBuf, String> {
    let candidate = root.join(relative);
    let real = real_path(&candidate)?;
    if !real.starts_with(root) || !real.is_file() {
        return Err(message!(
            input_dependency_is_not_a_file_within_the_model_folder,
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
    let scene = osgb::inspect(&bytes, Some(cancelled))
        .map_err(|err| message!(read_osgb_failed, file_error(&file, err)))?;
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
    message!(path_error, path.display(), err)
}

fn check_cancelled(cancelled: &AtomicBool) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err(message!(clipping_canceled).into());
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
