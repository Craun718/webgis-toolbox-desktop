use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub(super) enum Selection<'a> {
    Folder,
    File,
    Save(Option<&'a str>),
}

type Filter<'a> = (&'a str, &'a [&'a str]);

#[cfg(not(target_os = "linux"))]
pub(super) fn select(
    title: &str,
    directory: Option<&Path>,
    filter: Option<Filter<'_>>,
    selection: Selection<'_>,
) -> Option<PathBuf> {
    let mut dialog = rfd::FileDialog::new().set_title(title);
    if let Some(directory) = directory {
        dialog = dialog.set_directory(directory);
    }
    if let Some((name, extensions)) = filter {
        dialog = dialog.add_filter(name, extensions);
    }
    match selection {
        Selection::Folder => dialog.pick_folder(),
        Selection::File => dialog.pick_file(),
        Selection::Save(Some(name)) => dialog.set_file_name(name).save_file(),
        Selection::Save(None) => dialog.save_file(),
    }
}

#[cfg(target_os = "linux")]
pub(super) fn select(
    title: &str,
    directory: Option<&Path>,
    filter: Option<Filter<'_>>,
    selection: Selection<'_>,
) -> Option<PathBuf> {
    use ashpd::desktop::file_chooser::{FileFilter, OpenFileRequest, SaveFileRequest};

    let filters = filter.map(|(name, extensions)| {
        extensions
            .iter()
            .fold(FileFilter::new(name), |filter, ext| {
                filter.glob(&format!("*.{ext}"))
            })
    });
    let request = pollster::block_on(async {
        match selection {
            Selection::Save(name) => {
                SaveFileRequest::default()
                    .title(title)
                    .current_name(name)
                    .filters(filters)
                    .current_folder::<&Path>(directory)?
                    .send()
                    .await
            }
            Selection::Folder | Selection::File => {
                OpenFileRequest::default()
                    .title(title)
                    .directory(matches!(selection, Selection::Folder))
                    .multiple(false)
                    .filters(filters)
                    .current_folder::<&Path>(directory)?
                    .send()
                    .await
            }
        }
    });
    match request {
        Ok(request) => request.response().ok()?.uris().first()?.to_file_path().ok(),
        // rfd's Zenity fallback omits the title and initial folder.
        Err(_) => zenity(title, directory, filter, selection),
    }
}

#[cfg(target_os = "linux")]
fn zenity(
    title: &str,
    directory: Option<&Path>,
    filter: Option<Filter<'_>>,
    selection: Selection<'_>,
) -> Option<PathBuf> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, process::Command};

    let mut command = Command::new("zenity");
    command.args(["--file-selection", "--title", title]);
    let name = match selection {
        Selection::Folder => {
            command.arg("--directory");
            None
        }
        Selection::File => None,
        Selection::Save(name) => {
            command.args(["--save", "--confirm-overwrite"]);
            name
        }
    };
    if let Some((label, extensions)) = filter {
        let patterns = extensions
            .iter()
            .map(|ext| format!("*.{ext}"))
            .collect::<Vec<_>>();
        command.args([
            "--file-filter",
            &format!("{label} | {}", patterns.join(" ")),
        ]);
    }
    match (directory, name) {
        (Some(dir), Some(name)) => {
            command.arg("--filename").arg(dir.join(name));
        }
        (Some(dir), None) => {
            let mut start = dir.as_os_str().to_owned();
            start.push("/");
            command.arg("--filename").arg(start);
        }
        (None, Some(name)) => {
            command.args(["--filename", name]);
        }
        (None, None) => {}
    }
    let result = command.output().ok()?;
    let path = result.stdout.strip_suffix(b"\n").unwrap_or(&result.stdout);
    (result.status.success() && !path.is_empty()).then(|| OsString::from_vec(path.to_vec()).into())
}
