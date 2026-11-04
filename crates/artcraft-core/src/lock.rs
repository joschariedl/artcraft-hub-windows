//! Cross-process exclusivity for every operation that mutates the install
//! tree, the state file or the download cache.

use std::fs::{File, OpenOptions};
use crate::error::{Error, Result};
use crate::paths::Paths;

#[cfg(unix)]
use std::os::fd::AsRawFd;

/// An exclusive, non-blocking operation lock. Released on drop.
pub struct OperationLock {
    #[cfg(unix)]
    file: File,
    #[cfg(not(unix))]
    _phantom: (),
}

#[cfg(unix)]
impl OperationLock {
    pub fn acquire(paths: &Paths) -> Result<Self> {
        paths.ensure()?;
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(paths.lock_file())?;
        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let error = std::io::Error::last_os_error();
            return if error.kind() == std::io::ErrorKind::WouldBlock {
                Err(Error::Busy)
            } else {
                Err(Error::Io(error))
            };
        }
        Ok(Self { file })
    }
}

#[cfg(not(unix))]
impl OperationLock {
    pub fn acquire(_paths: &Paths) -> Result<Self> {
        Ok(Self { _phantom: () })
    }
}

#[cfg(unix)]
impl Drop for OperationLock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn temp_paths() -> (tempfile::TempDir, Paths) {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = Paths::from_env();
        let home = dir.path();
        paths.config_dir = home.join("config");
        paths.state_dir = home.join("state");
        paths.cache_dir = home.join("cache");
        paths.data_dir = home.join("data");
        paths.install_root = home.join("opt");
        paths.bin_dir = home.join("bin");
        paths.applications_dir = home.join("data/applications");
        paths.desktop_dir = paths.applications_dir.clone();
        paths.icons_dir = home.join("data/icons");
        paths.mime_dir = home.join("data/mime");
        paths.metainfo_dir = home.join("data/metainfo");
        paths.ensure().unwrap();
        (dir, paths)
    }

    #[cfg(unix)]
    #[test]
    fn a_second_operation_is_rejected_while_the_first_is_active() {
        let (_dir, paths) = temp_paths();
        let first = OperationLock::acquire(&paths).unwrap();
        assert!(matches!(
            OperationLock::acquire(&paths),
            Err(Error::Busy)
        ));
        drop(first);
        assert!(OperationLock::acquire(&paths).is_ok());
    }
}
