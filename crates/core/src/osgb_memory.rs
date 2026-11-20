//! Shared reservations for an isolated OSGB task.
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed},
};

pub const MIB: usize = 1024 * 1024;
pub const HEADROOM: usize = 256 * MIB;
static LIMIT: AtomicUsize = AtomicUsize::new(usize::MAX);
static USED: AtomicUsize = AtomicUsize::new(0);
static PRESSURE: AtomicBool = AtomicBool::new(false);
static SERIAL: AtomicBool = AtomicBool::new(false);

pub fn configure(limit: usize) {
    LIMIT.store(limit, Relaxed);
}

pub fn limit() -> usize {
    LIMIT.load(Relaxed)
}

pub fn pressure() -> bool {
    PRESSURE.load(Relaxed)
}

pub fn set_pressure(active: bool) {
    PRESSURE.store(active, Relaxed);
    if active {
        SERIAL.store(true, Relaxed);
    }
}

pub fn width(maximum: usize) -> usize {
    if SERIAL.load(Relaxed) { 1 } else { maximum }
}

pub fn available_budget() -> usize {
    limit().saturating_sub(USED.load(Relaxed))
}

pub struct Reservation(usize);

impl Drop for Reservation {
    fn drop(&mut self) {
        USED.fetch_sub(self.0, Relaxed);
    }
}

#[derive(Debug)]
pub struct BudgetError(String);

impl std::fmt::Display for BudgetError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for BudgetError {}

pub fn reserve(bytes: usize, unit: impl std::fmt::Display) -> Result<Reservation, BudgetError> {
    USED.fetch_update(Relaxed, Relaxed, |used| {
        used.checked_add(bytes).filter(|&total| total <= limit())
    })
    .map_err(|used| {
        BudgetError(crate::message!(
            osgb_reservation_failed,
            unit,
            bytes = bytes,
            available = limit().saturating_sub(used)
        ))
    })?;
    Ok(Reservation(bytes))
}

// The parser may expand packed vertices and indices into native arrays, triangles,
// edits and verification buffers. RSS monitoring backs up this estimate.
pub fn page_bytes(path: &Path) -> std::io::Result<usize> {
    let size = usize::try_from(fs::metadata(path)?.len()).unwrap_or(usize::MAX);
    Ok(size.saturating_mul(12).saturating_add(64 * 1024))
}

pub fn read_page(path: &Path) -> std::io::Result<Vec<u8>> {
    #[cfg(all(feature = "model-clip", target_os = "linux"))]
    if limit() != usize::MAX {
        use std::io::Read;
        // Limit growth to the size of the file when it was opened. Each page
        // is traversed once, so its file cache need not stay cgroup-charged.
        let file = fs::File::open(path)?;
        let length = file.metadata()?.len();
        let mut bytes = Vec::with_capacity(usize::try_from(length).unwrap_or(usize::MAX));
        (&file).take(length).read_to_end(&mut bytes)?;
        let _ = rustix::fs::fadvise(&file, 0, None, rustix::fs::Advice::DontNeed);
        return Ok(bytes);
    }
    fs::read(path)
}

pub fn available_system() -> Option<usize> {
    let meminfo = fs::read_to_string("/proc/meminfo").ok();
    let host = meminfo
        .as_deref()
        .and_then(|text| proc_kib(text, "MemAvailable:"));
    match (host, cgroup_available()) {
        (Some(host), Some(cgroup)) => Some(host.min(cgroup)),
        (host, cgroup) => host.or(cgroup),
    }
}

pub fn proc_kib(text: &str, label: &str) -> Option<usize> {
    text.lines()
        .find_map(|line| line.strip_prefix(label))?
        .split_whitespace()
        .next()?
        .parse::<usize>()
        .ok()?
        .checked_mul(1024)
}

fn cgroup_available() -> Option<usize> {
    let membership = fs::read_to_string("/proc/self/cgroup").ok()?;
    let group = Path::new(
        membership
            .lines()
            .find_map(|line| line.strip_prefix("0::"))?,
    );
    let mounts = fs::read_to_string("/proc/self/mountinfo").ok()?;
    let mut remaining = None;
    for line in mounts.lines() {
        let (mount, filesystem) = line.split_once(" - ")?;
        if filesystem.split_whitespace().next() != Some("cgroup2") {
            continue;
        }
        let fields: Vec<_> = mount.split_whitespace().collect();
        let root = Path::new(fields.get(3)?);
        let mountpoint = Path::new(fields.get(4)?);
        let path: PathBuf =
            mountpoint.join(group.strip_prefix(root).unwrap_or_else(|_| Path::new("")));
        for directory in path
            .ancestors()
            .take_while(|path| path.starts_with(mountpoint))
        {
            let read = |name| {
                fs::read_to_string(directory.join(name))
                    .ok()?
                    .trim()
                    .parse::<usize>()
                    .ok()
            };
            let Some((limit, current)) = read("memory.max").zip(read("memory.current")) else {
                continue;
            };
            let available = limit.saturating_sub(current);
            remaining =
                Some(remaining.map_or(available, |previous: usize| previous.min(available)));
        }
    }
    remaining
}

pub fn wait(cancelled: &AtomicBool) -> Result<(), String> {
    while pressure() {
        if cancelled.load(Relaxed) {
            return Err(crate::message!(processing_canceled).into());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    Ok(())
}
