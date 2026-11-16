// The non-unix cfg does not use all imports
#![allow(unused_imports)]

use crate::semaphore::Semaphore;
use lazy_static::lazy_static;
use std::sync::Arc;

#[cfg(unix)]
// Get the maximum number of open file descriptors for this process, and if
// the hard limit is larger than the soft limit increase it.
fn rlimit_nofile() -> libc::rlim_t {
    let mut file_limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut file_limit) != 0 {
            return 200;
        }
    }

    if file_limit.rlim_max > file_limit.rlim_cur {
        let prev = file_limit.rlim_cur;
        file_limit.rlim_cur = file_limit.rlim_max;
        unsafe {
            if libc::setrlimit(libc::RLIMIT_NOFILE, &file_limit) == 0 {
                file_limit.rlim_max
            } else {
                prev
            }
        }
    } else {
        file_limit.rlim_cur
    }
}

#[cfg(unix)]
// stdin, stdout, stderr, plus two as a buffer
const OTHER_OPEN_FILES: isize = 3 + 2;

/// Number of files that may be open at the same time for the given RLIMIT_NOFILE value.
/// The limit can be RLIM_INFINITY (u64::MAX), which must not wrap to a negative number.
#[cfg(unix)]
fn open_files_permits(limit: libc::rlim_t) -> isize {
    let limit = std::cmp::min(limit, i32::MAX as libc::rlim_t) as isize;
    std::cmp::max(limit - OTHER_OPEN_FILES, 64 /* fallback value */)
}

#[cfg(all(test, unix))]
mod test {
    #[test]
    fn open_files_permits() {
        assert_eq!(
            super::open_files_permits(1024),
            1024 - super::OTHER_OPEN_FILES
        );
        assert_eq!(super::open_files_permits(10), 64);
        assert!(super::open_files_permits(libc::RLIM_INFINITY) > 1_000_000);
    }
}

#[cfg(unix)]
lazy_static! {
    // Globally track the number of opened files so many parallel operations do not raise
    // "Too many open files (os error 24)".
    pub static ref RLIMIT_OPEN_FILES: Arc<Semaphore> =
        Arc::new(Semaphore::new(open_files_permits(rlimit_nofile())));
}

#[cfg(not(unix))]
pub mod not_unix {
    #[derive(Clone, Copy)]
    pub struct NoRlimit;
    impl NoRlimit {
        pub fn new() -> Self {
            Self {}
        }
        pub fn clone(self) -> Self {
            self
        }
        pub fn access_owned(self) -> () {}
    }
}
#[cfg(not(unix))]
lazy_static! {
    pub static ref RLIMIT_OPEN_FILES: not_unix::NoRlimit = not_unix::NoRlimit::new();
}
