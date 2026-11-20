//! Advisory directory locks shared by OSGB staging and publication.
use rustix::fs::{FlockOperation, flock};
use std::{fs::File, io, path::Path};

pub struct OutputLock(File);

impl OutputLock {
    pub fn acquire(directory: &Path) -> io::Result<Self> {
        let file = File::open(directory)?;
        flock(&file, FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self(file))
    }
}

impl Drop for OutputLock {
    fn drop(&mut self) {
        // A concurrently forked worker can briefly share the file description
        // before close-on-exec takes effect. Release the lock explicitly.
        let _ = flock(&self.0, FlockOperation::Unlock);
    }
}
