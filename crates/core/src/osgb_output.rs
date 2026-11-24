//! Advisory directory locks shared by OSGB staging and publication.
#[cfg(unix)]
use rustix::fs::{FlockOperation, flock};
use std::{fs::File, io, path::Path};

#[cfg(unix)]
pub struct OutputLock(File);

#[cfg(windows)]
pub struct OutputLock {
    _file: File,
}

#[cfg(unix)]
impl OutputLock {
    pub fn acquire(directory: &Path) -> io::Result<Self> {
        let file = File::open(directory)?;
        flock(&file, FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self(file))
    }
}

#[cfg(windows)]
impl OutputLock {
    pub fn acquire(directory: &Path) -> io::Result<Self> {
        use std::{fs::OpenOptions, os::windows::fs::OpenOptionsExt};

        const FILE_ATTRIBUTE_HIDDEN: u32 = 0x2;
        const FILE_FLAG_DELETE_ON_CLOSE: u32 = 0x0400_0000;
        const GENERIC_WRITE: u32 = 0x4000_0000;
        const DELETE: u32 = 0x0001_0000;

        // An exclusive handle keeps other processes out. Windows removes
        // the lock file when the handle closes, including after a crash.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .access_mode(GENERIC_WRITE | DELETE)
            .share_mode(0)
            .attributes(FILE_ATTRIBUTE_HIDDEN)
            .custom_flags(FILE_FLAG_DELETE_ON_CLOSE)
            .open(directory.join(".webgis-osgb.lock"))?;
        Ok(Self { _file: file })
    }
}

#[cfg(unix)]
impl Drop for OutputLock {
    fn drop(&mut self) {
        // A concurrently forked worker can briefly share the file description
        // before close-on-exec takes effect. Release the lock explicitly.
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}
