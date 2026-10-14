//! One running instance per home directory. The lock is an OS file lock, so
//! the system releases it when the process dies, however it dies; a stale
//! lock file after a crash never blocks the next start.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::RuntimeError;
use crate::paths::HomePaths;

const LOCK_FILE: &str = "instance.lock";

/// Held for the life of the process. Dropping it releases the lock.
pub struct InstanceLock {
    _guard: fd_lock::RwLockWriteGuard<'static, File>,
    path: PathBuf,
}

impl InstanceLock {
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Takes the lock, or reports that another instance holds it.
pub fn acquire(paths: &HomePaths) -> Result<InstanceLock, RuntimeError> {
    std::fs::create_dir_all(paths.root()).map_err(|e| RuntimeError::io(paths.root(), e))?;
    let path = paths.root().join(LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| RuntimeError::io(&path, e))?;
    // The lock lives as long as the process; leaking the handle keeps the
    // guard's borrow simple.
    let lock: &'static mut fd_lock::RwLock<File> = Box::leak(Box::new(fd_lock::RwLock::new(file)));
    match lock.try_write() {
        Ok(mut guard) => {
            let _ = guard.set_len(0);
            let _ = writeln!(&mut *guard, "{}", std::process::id());
            let _ = guard.flush();
            Ok(InstanceLock {
                _guard: guard,
                path,
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
            Err(RuntimeError::AlreadyRunning(path))
        }
        Err(e) => Err(RuntimeError::io(&path, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_one_instance_per_home() {
        let tmp = tempfile::tempdir().unwrap();
        let paths = HomePaths::at(tmp.path().join("home"));
        let first = acquire(&paths).unwrap();
        assert!(first.path().is_file());
        assert!(matches!(
            acquire(&paths),
            Err(RuntimeError::AlreadyRunning(_))
        ));
        drop(first);
        assert!(acquire(&paths).is_ok(), "released with the guard");
    }
}
