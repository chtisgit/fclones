use crate::error::error_kind;
use std::fs::File;
use std::{fs, io};

use crate::path::Path;

/// Portable file locking.
///
/// On Unix, advisory lock through fnctl is used.
/// On Windows, file is open in read-write mode.
///
/// The file must exist before locking.
pub struct FileLock {
    pub file: File,
}

impl FileLock {
    #[cfg(unix)]
    fn nix_as_io_error<T>(result: nix::Result<T>) -> io::Result<T> {
        match result {
            Ok(x) => Ok(x),
            Err(e) => Err(e.into()),
        }
    }

    /// Creates a libc::flock initialized to zeros.
    /// Should be safe, because flock contains primitive fields only, no references.
    #[cfg(unix)]
    fn new_flock() -> libc::flock {
        unsafe { std::mem::zeroed() }
    }

    #[cfg(unix)]
    #[allow(clippy::unnecessary_cast)]
    fn fcntl_lock(file: &File, lock_type: libc::c_int) -> io::Result<()> {
        use nix::fcntl::*;
        use std::os::unix::io::AsRawFd;
        let mut f = Self::new_flock();
        f.l_type = lock_type as i16;
        f.l_whence = libc::SEEK_SET as i16;
        let result = nix::fcntl::fcntl(file.as_raw_fd(), FcntlArg::F_SETLK(&f));
        Self::nix_as_io_error(result).map(|_| {})
    }

    #[cfg(unix)]
    #[allow(clippy::unnecessary_cast)]
    fn fcntl_unlock(file: &File) -> io::Result<()> {
        use nix::fcntl::*;
        use std::os::unix::io::AsRawFd;
        let mut f = Self::new_flock();
        f.l_type = libc::F_UNLCK as i16;
        f.l_whence = libc::SEEK_SET as i16;
        let result = nix::fcntl::fcntl(file.as_raw_fd(), FcntlArg::F_SETLK(&f));
        Self::nix_as_io_error(result).map(|_| {})
    }

    /// Returns true if opening a file for writing failed only because the file (or the whole
    /// file system) is read-only. Removing or replacing such a file can still be allowed,
    /// because that needs write access to the directory, not to the file itself.
    fn is_read_only_error(e: &io::Error) -> bool {
        #[cfg(unix)]
        if e.raw_os_error() == Some(libc::EROFS) {
            return true;
        }
        e.kind() == io::ErrorKind::PermissionDenied
    }

    /// Locks a file.
    ///
    /// The file is opened for writing and locked exclusively. If it cannot be opened for writing
    /// because it is read-only, it is opened for reading and locked with a shared lock instead,
    /// which still conflicts with any cooperating writer holding a lock.
    /// On error, the error message will contain the path.
    pub fn new(path: &Path) -> io::Result<FileLock> {
        let path_buf = path.to_path_buf();
        let open = |write: bool| {
            fs::OpenOptions::new()
                .read(!write)
                .write(write)
                .create(false)
                .open(&path_buf)
        };
        let open_error = |e: io::Error| {
            io::Error::new(
                error_kind(&e),
                format!("Failed to open file {} for locking: {}", path.display(), e),
            )
        };
        let (file, exclusive) = match open(true) {
            Ok(file) => (file, true),
            Err(e) if Self::is_read_only_error(&e) => (open(false).map_err(open_error)?, false),
            Err(e) => return Err(open_error(e)),
        };

        #[cfg(unix)]
        let lock_type = if exclusive {
            libc::F_WRLCK
        } else {
            libc::F_RDLCK
        };
        #[cfg(not(unix))]
        let _ = exclusive;

        #[cfg(unix)]
        if let Err(e) = Self::fcntl_lock(&file, lock_type) {
            return Err(io::Error::new(
                error_kind(&e),
                format!("Failed to lock file {}: {}", path.display(), e),
            ));
        };

        Ok(FileLock { file })
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        #[cfg(unix)]
        let _ = Self::fcntl_unlock(&self.file);
    }
}
