//! Removing redundant files.

use std::cmp::{max, min, Reverse};
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use std::io::{ErrorKind, Write};
use std::ops::{Add, AddAssign};
use std::sync::mpsc::channel;
use std::sync::Arc;
use std::time::SystemTime;
use std::{fmt, fs, io};

use chrono::{DateTime, FixedOffset, Local};
use priority_queue::PriorityQueue;
use rand::distributions::Alphanumeric;
use rand::Rng;
use rayon::iter::{IntoParallelIterator, ParallelBridge, ParallelIterator};

use crate::config::{DedupeConfig, Priority};
use crate::device::DiskDevices;
use crate::file::{FileId, FileLen, FileMetadata};
use crate::group::{FileGroup, FileSubGroup};
use crate::lock::FileLock;
use crate::log::{Log, LogExt};
use crate::path::Path;
use crate::util::{max_result, min_result, try_sort_by_key};
use crate::{Error, TIMESTAMP_FMT};

/// Defines what to do with redundant files
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DedupeOp {
    /// Removes redundant files.
    Remove,
    /// Moves redundant files to a different dir
    Move(Arc<Path>),
    /// Replaces redundant files with soft-links (ln -s on Unix).
    SymbolicLink,
    /// Replaces redundant files with hard-links (ln on Unix).
    HardLink,
    /// Reflink redundant files (cp --reflink=always, only some filesystems).
    RefLink,
}

/// Convenience struct for holding a path to a file and its metadata together
#[derive(Clone, Debug)]
pub struct PathAndMetadata {
    pub path: Path,
    pub metadata: FileMetadata,
}

impl PathAndMetadata {
    pub fn new(path: Path) -> io::Result<PathAndMetadata> {
        let metadata = FileMetadata::new(&path).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to read metadata of {}: {}", path.display(), e),
            )
        })?;
        Ok(PathAndMetadata { metadata, path })
    }
}

impl AsRef<PathAndMetadata> for PathAndMetadata {
    fn as_ref(&self) -> &PathAndMetadata {
        self
    }
}

impl AsRef<Path> for PathAndMetadata {
    fn as_ref(&self) -> &Path {
        &self.path
    }
}

impl AsRef<FileId> for PathAndMetadata {
    fn as_ref(&self) -> &FileId {
        self.metadata.as_ref()
    }
}

impl From<PathAndMetadata> for Path {
    fn from(value: PathAndMetadata) -> Self {
        value.path
    }
}

impl Display for PathAndMetadata {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.pad(self.path.display().as_str())
    }
}

/// Portable abstraction for commands used to remove duplicates
#[derive(Debug)]
pub enum FsCommand {
    Remove {
        file: PathAndMetadata,
    },
    Move {
        source: PathAndMetadata,
        target: Path,
        use_rename: bool, // try to move the file directly by issuing fs rename command
    },
    SoftLink {
        target: Arc<PathAndMetadata>,
        link: PathAndMetadata,
    },
    HardLink {
        target: Arc<PathAndMetadata>,
        link: PathAndMetadata,
    },
    RefLink {
        target: Arc<PathAndMetadata>,
        link: PathAndMetadata,
    },
}

impl FsCommand {
    /// Obtains a lock to the file if lock == true.
    fn maybe_lock(path: &Path, lock: bool) -> io::Result<Option<FileLock>> {
        if lock {
            match FileLock::new(path) {
                Ok(lock) => Ok(Some(lock)),
                Err(e) if e.kind() == ErrorKind::Unsupported => Ok(None),
                Err(e) => Err(e),
            }
        } else {
            Ok(None)
        }
    }

    pub fn remove(path: &Path) -> io::Result<()> {
        fs::remove_file(path.to_path_buf()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to remove file {}: {}", path.display(), e),
            )
        })
    }

    #[cfg(unix)]
    fn symlink_internal(target: &std::path::Path, link: &std::path::Path) -> io::Result<()> {
        std::os::unix::fs::symlink(target, link)
    }

    #[cfg(windows)]
    fn symlink_internal(target: &std::path::Path, link: &std::path::Path) -> io::Result<()> {
        std::os::windows::fs::symlink_file(target, link)
    }

    fn symlink(target: &Path, link: &Path) -> io::Result<()> {
        Self::symlink_internal(&target.to_path_buf(), &link.to_path_buf()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to create symbolic link {} -> {}: {}",
                    link.display(),
                    target.display(),
                    e
                ),
            )
        })
    }

    fn hardlink(target: &Path, link: &Path) -> io::Result<()> {
        fs::hard_link(target.to_path_buf(), link.to_path_buf()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to create hard link {} -> {}: {}",
                    link.display(),
                    target.display(),
                    e
                ),
            )
        })
    }

    fn check_can_rename(source: &Path, target: &Path) -> io::Result<()> {
        if target.to_path_buf().exists() {
            return Err(io::Error::new(
                ErrorKind::AlreadyExists,
                format!(
                    "Cannot move {} to {}: Target already exists",
                    source.display(),
                    target.display()
                ),
            ));
        }
        Ok(())
    }

    fn mkdirs(path: &Path) -> io::Result<()> {
        fs::create_dir_all(path.to_path_buf()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to create directory {}: {}", path.display(), e),
            )
        })
    }

    /// Renames/moves a file from one location to another.
    /// If the target exists, it would be overwritten.
    pub fn unsafe_rename(source: &Path, target: &Path) -> io::Result<()> {
        fs::rename(source.to_path_buf(), target.to_path_buf()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to rename file from {} to {}: {}",
                    source.display(),
                    target.display(),
                    e
                ),
            )
        })
    }

    /// Copies a file from one location to another.
    /// If the target exists, it would be overwritten.
    fn unsafe_copy(source: &Path, target: &Path) -> io::Result<()> {
        fs::copy(source.to_path_buf(), target.to_path_buf()).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "Failed to copy file from {} to {}: {}",
                    source.display(),
                    target.display(),
                    e
                ),
            )
        })?;
        Ok(())
    }

    /// Moves the file from one location to another by single `fs::rename` command.
    /// Fails if target exists.
    fn move_rename(source: &Path, target: &Path) -> io::Result<()> {
        Self::check_can_rename(source, target)?;
        Self::mkdirs(target.parent().unwrap())?;
        Self::unsafe_rename(source, target)?;
        Ok(())
    }

    /// Moves the file by copying it first to another location and then removing the original.
    /// Fails if target exists.
    fn move_copy(source: &Path, target: &Path) -> io::Result<()> {
        Self::check_can_rename(source, target)?;
        Self::mkdirs(target.parent().unwrap())?;
        Self::unsafe_copy(source, target)?;
        Self::remove(source)?;
        Ok(())
    }

    /// Returns a random temporary file name in the same directory, guaranteed to not collide with
    /// any other file in the same directory
    pub fn temp_file(path: &Path) -> Path {
        // Most file systems limit a file name to 255 bytes (NTFS: 255 UTF-16 units).
        const MAX_NAME_LEN: usize = 255;
        let suffix = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(24)
            .map(char::from)
            .collect::<String>();
        let mut name = path
            .file_name()
            .expect("must be a regular file with a name");
        if name.len() + 1 + suffix.len() <= MAX_NAME_LEN {
            name.push(".");
            name.push(&suffix);
        } else {
            // Appending to a long name would fail with "File name too long"
            name = OsString::from(format!(".fclones-tmp.{suffix}"));
        }
        match path.parent() {
            Some(parent) => parent.join(Path::from(name)),
            None => Path::from(name),
        }
    }

    /// Safely moves the file to a different location and invokes the function.
    /// If the function fails, moves the file back to the original location.
    /// If the function succeeds, removes the file permanently.
    pub fn safe_remove<R>(
        path: &Path,
        f: impl FnOnce(&Path) -> io::Result<R>,
        log: &dyn Log,
    ) -> io::Result<R> {
        let tmp = Self::temp_file(path);
        Self::unsafe_rename(path, &tmp)?;
        let result = match f(path) {
            Ok(result) => result,
            Err(e) => {
                // Try to undo the move if possible
                if let Err(remove_err) = Self::unsafe_rename(&tmp, path) {
                    log.warn(format!(
                        "Failed to undo move from {} to {}: {}",
                        &path.display(),
                        &tmp.display(),
                        remove_err
                    ))
                }
                return Err(e);
            }
        };
        // Cleanup the temp file.
        if let Err(e) = Self::remove(&tmp) {
            log.warn(format!(
                "Failed to remove temporary {}: {}",
                &tmp.display(),
                e
            ))
        }
        Ok(result)
    }

    /// Executes the command and returns the number of bytes reclaimed
    pub fn execute(
        &self,
        should_lock: bool,
        ignore_xattr_errors: bool,
        log: &dyn Log,
    ) -> io::Result<FileLen> {
        match self {
            FsCommand::Remove { file } => {
                let _lock = Self::maybe_lock(&file.path, should_lock)?;
                Self::remove(&file.path)?;
                Ok(file.metadata.len())
            }
            FsCommand::SoftLink { target, link } => {
                let _lock = Self::maybe_lock(&link.path, should_lock)?;
                Self::safe_remove(&link.path, |link| Self::symlink(&target.path, link), log)?;
                Ok(link.metadata.len())
            }
            FsCommand::HardLink { target, link } => {
                let _lock = Self::maybe_lock(&link.path, should_lock)?;
                Self::safe_remove(&link.path, |link| Self::hardlink(&target.path, link), log)?;
                Ok(link.metadata.len())
            }
            FsCommand::RefLink { target, link } => {
                let _lock = Self::maybe_lock(&link.path, should_lock)?;
                crate::reflink::reflink(target, link, ignore_xattr_errors, log)?;
                Ok(link.metadata.len())
            }
            FsCommand::Move {
                source,
                target,
                use_rename,
            } => {
                let _lock = Self::maybe_lock(&source.path, should_lock)?;
                let len = source.metadata.len();
                if *use_rename && Self::move_rename(&source.path, target).is_ok() {
                    return Ok(len);
                }
                Self::move_copy(&source.path, target)?;
                Ok(len)
            }
        }
    }

    /// Returns the path to be affected by running the command.
    /// For commands that move or remove the file, it returns the path to the file before removal.
    /// For commands that create a link, returns the path to file that will be replaced by the link.
    pub fn file_to_remove(&self) -> &Path {
        match self {
            FsCommand::Remove { file, .. }
            | FsCommand::SoftLink { link: file, .. }
            | FsCommand::HardLink { link: file, .. }
            | FsCommand::RefLink { link: file, .. }
            | FsCommand::Move { source: file, .. } => &file.path,
        }
    }

    /// Returns the file that is removed, moved or replaced by a link.
    pub fn dropped_file(&self) -> &PathAndMetadata {
        match self {
            FsCommand::Remove { file, .. }
            | FsCommand::SoftLink { link: file, .. }
            | FsCommand::HardLink { link: file, .. }
            | FsCommand::RefLink { link: file, .. }
            | FsCommand::Move { source: file, .. } => file,
        }
    }

    /// Returns the size of the file affected by this command. Note that this space is only
    /// reclaimed if no other hard link to the file remains.
    pub fn space_to_reclaim(&self) -> FileLen {
        match self {
            FsCommand::Remove { file, .. }
            | FsCommand::SoftLink { link: file, .. }
            | FsCommand::HardLink { link: file, .. }
            | FsCommand::RefLink { link: file, .. }
            | FsCommand::Move { source: file, .. } => file.metadata.len(),
        }
    }

    /// Formats the command as a string that can be pasted to a Unix shell (e.g. bash)
    #[cfg(unix)]
    pub fn to_shell_str(&self) -> Vec<String> {
        let mut result = Vec::new();
        match self {
            FsCommand::Remove { file, .. } => {
                let path = file.path.quote();
                result.push(format!("rm {path}"));
            }
            FsCommand::SoftLink { target, link, .. } => {
                let tmp = Self::temp_file(&link.path);
                let target = target.path.quote();
                let link = link.path.quote();
                result.push(format!("mv {} {}", link, tmp.quote()));
                result.push(format!("ln -s {target} {link}"));
                result.push(format!("rm {}", tmp.quote()));
            }
            FsCommand::HardLink { target, link, .. } => {
                let tmp = Self::temp_file(&link.path);
                let target = target.path.quote();
                let link = link.path.quote();
                result.push(format!("mv {} {}", link, tmp.quote()));
                result.push(format!("ln {target} {link}"));
                result.push(format!("rm {}", tmp.quote()));
            }
            FsCommand::RefLink { target, link, .. } => {
                let tmp = Self::temp_file(&link.path);
                let target = target.path.quote();
                let link = link.path.quote();
                // Not really what happens on Linux, there the `mv` is also a reflink.
                result.push(format!("mv {} {}", link, tmp.quote()));
                if cfg!(target_os = "macos") {
                    result.push(format!("cp -c {target} {link}"));
                } else {
                    result.push(format!("cp --reflink=always {target} {link}"));
                };
                result.push(format!("rm {}", tmp.quote()));
            }
            FsCommand::Move {
                source,
                target,
                use_rename,
            } => {
                let source = source.path.quote();
                // fclones creates the target directory, so must the script
                if let Some(parent) = target.parent() {
                    result.push(format!("mkdir -p {}", parent.quote()));
                }
                let target = target.quote();
                if *use_rename {
                    result.push(format!("mv {} {}", &source, &target));
                } else {
                    result.push(format!("cp {} {}", &source, &target));
                    result.push(format!("rm {}", &source));
                }
            }
        }
        result
    }

    #[cfg(windows)]
    pub fn to_shell_str(&self) -> Vec<String> {
        let mut result = Vec::new();
        match self {
            FsCommand::Remove { file, .. } => {
                let path = file.path.quote();
                result.push(format!("del {}", path));
            }
            FsCommand::SoftLink { target, link, .. } => {
                let tmp = Self::temp_file(&link.path);
                let target = target.path.quote();
                let link = link.path.quote();
                result.push(format!("move {} {}", link, tmp.quote()));
                // mklink [/H] <link> <target>
                result.push(format!("mklink {} {}", link, target));
                result.push(format!("del {}", tmp.quote()));
            }
            FsCommand::HardLink { target, link, .. } => {
                let tmp = Self::temp_file(&link.path);
                let target = target.path.quote();
                let link = link.path.quote();
                result.push(format!("move {} {}", link, tmp.quote()));
                result.push(format!("mklink /H {} {}", link, target));
                result.push(format!("del {}", tmp.quote()));
            }
            FsCommand::RefLink { target, link, .. } => {
                result.push(format!(":: deduplicate {} {}", link, target));
            }
            FsCommand::Move {
                source,
                target,
                use_rename,
            } => {
                let source = source.path.quote();
                // fclones creates the target directory, so must the script
                // (cmd.exe mkdir creates intermediate directories)
                if let Some(parent) = target.parent() {
                    result.push(format!("if not exist {0} mkdir {0}", parent.quote()));
                }
                let target = target.quote();
                if *use_rename {
                    result.push(format!("move {} {}", &source, &target));
                } else {
                    result.push(format!("copy {} {}", &source, &target));
                    result.push(format!("del {}", &source));
                }
            }
        }
        result
    }
}

/// Provides information about the number of deduplicated files and reclaimed disk space
#[derive(Default)]
pub struct DedupeResult {
    pub processed_count: u64,
    pub reclaimed_space: FileLen,
}

impl Add<DedupeResult> for DedupeResult {
    type Output = DedupeResult;

    fn add(self, rhs: Self) -> Self::Output {
        DedupeResult {
            processed_count: self.processed_count + rhs.processed_count,
            reclaimed_space: self.reclaimed_space + rhs.reclaimed_space,
        }
    }
}

impl AddAssign for DedupeResult {
    fn add_assign(&mut self, rhs: Self) {
        self.processed_count += rhs.processed_count;
        self.reclaimed_space += rhs.reclaimed_space;
    }
}

/// Returns the time of the last status change (ctime) of the file.
/// Returns `None` on systems without a status change time.
fn status_change_time(metadata: &FileMetadata) -> Option<DateTime<Local>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        DateTime::from_timestamp(metadata.ctime(), metadata.ctime_nsec() as u32).map(Into::into)
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

/// Returns true if any of the files have been modified after the given timestamp.
/// Also returns true if file timestamp could not be read.
fn was_modified(files: &[PathAndMetadata], after: DateTime<FixedOffset>, log: &dyn Log) -> bool {
    let mut result = false;
    let after: DateTime<Local> = after.into();
    for PathAndMetadata {
        path: p,
        metadata: m,
        ..
    } in files.iter()
    {
        match m.modified() {
            Ok(file_timestamp) => {
                let file_timestamp: DateTime<Local> = file_timestamp.into();
                // The modification time can be set to anything by the user or by tools that
                // preserve it (cp -a, rsync -t, touch -r), so on Unix also check the status
                // change time, which is updated whenever the file content is changed.
                let file_timestamp =
                    status_change_time(m).map_or(file_timestamp, |c| max(file_timestamp, c));
                if file_timestamp > after {
                    log.warn(format!(
                        "File {} was updated after {} (at {})",
                        p.display(),
                        after.format(TIMESTAMP_FMT),
                        file_timestamp.format(TIMESTAMP_FMT)
                    ));
                    result = true;
                }
            }
            Err(e) => {
                log.warn(format!(
                    "Failed to read modification time of file {}: {}",
                    p.display(),
                    e
                ));
                result = true;
            }
        }
    }
    result
}

/// Returns true if given path matches any of the `keep` patterns
fn should_keep(path: &Path, config: &DedupeConfig) -> bool {
    let matches_any_name = config
        .keep_name_patterns
        .iter()
        .any(|p| match path.file_name_cstr() {
            Some(name) => p.matches(name.to_string_lossy().as_ref()),
            None => false,
        });
    let matches_any_path = || {
        config
            .keep_path_patterns
            .iter()
            .any(|p| p.matches_path(&path.to_path_buf()))
    };

    matches_any_name || matches_any_path()
}

/// Returns true if given path matches all of the `drop` patterns.
/// If there are no `drop` patterns, returns true.
fn may_drop(path: &Path, config: &DedupeConfig) -> bool {
    let matches_any_name = || {
        config
            .name_patterns
            .iter()
            .any(|p| match path.file_name_cstr() {
                Some(name) => p.matches(name.to_string_lossy().as_ref()),
                None => false,
            })
    };
    let matches_any_path = || {
        config
            .path_patterns
            .iter()
            .any(|p| p.matches_path(&path.to_path_buf()))
    };

    (config.name_patterns.is_empty() && config.path_patterns.is_empty())
        || matches_any_name()
        || matches_any_path()
}

impl<P: AsRef<PathAndMetadata>> FileSubGroup<P> {
    /// Returns the time of the earliest creation of a file in the subgroup
    pub fn created(&self) -> Result<SystemTime, Error> {
        Ok(min_result(self.files.iter().map(|f| {
            let f = f.as_ref();
            f.metadata.created().map_err(|e| {
                format!(
                    "Failed to read creation time of file {}: {}",
                    f.path.display(),
                    e
                )
            })
        }))?
        .unwrap())
    }

    /// Returns the time of the latest modification of a file in the subgroup
    pub fn modified(&self) -> Result<SystemTime, Error> {
        Ok(max_result(self.files.iter().map(|f| {
            let f = f.as_ref();
            f.metadata.modified().map_err(|e| {
                format!(
                    "Failed to read modification time of file {}: {}",
                    f.path.display(),
                    e
                )
            })
        }))?
        .unwrap())
    }

    /// Returns the time of the latest access of a file in the subgroup
    pub fn accessed(&self) -> Result<SystemTime, Error> {
        Ok(max_result(self.files.iter().map(|f| {
            let f = f.as_ref();
            f.metadata.accessed().map_err(|e| {
                format!(
                    "Failed to read access time of file {}: {}",
                    f.path.display(),
                    e
                )
            })
        }))?
        .unwrap())
    }

    /// Returns the time of the latest status change of a file in the subgroup
    #[cfg(unix)]
    pub fn status_changed(&self) -> Result<(i64, i64), Error> {
        use std::os::unix::fs::MetadataExt;
        Ok(self
            .files
            .iter()
            .map(|f| {
                let f = f.as_ref();
                (f.metadata.ctime(), f.metadata.ctime_nsec())
            })
            .max()
            .unwrap())
    }

    /// Returns true if any of the files in the subgroup must be kept
    pub fn should_keep(&self, config: &DedupeConfig) -> bool {
        self.files
            .iter()
            .any(|f| should_keep(&f.as_ref().path, config))
    }

    /// Returns true if all files in the subgroup can be dropped
    pub fn may_drop(&self, config: &DedupeConfig) -> bool {
        self.files
            .iter()
            .all(|f| may_drop(&f.as_ref().path, config))
    }

    /// Returns the number of components of the least nested path
    pub fn min_nesting(&self) -> usize {
        self.files
            .iter()
            .map(|f| f.as_ref().path.component_count())
            .min()
            .unwrap()
    }

    /// Returns the number of components of the most nested path
    pub fn max_nesting(&self) -> usize {
        self.files
            .iter()
            .map(|f| f.as_ref().path.component_count())
            .max()
            .unwrap()
    }
}

/// Sort files so that files with highest priority (newest, most recently updated,
/// recently accessed, etc) are sorted last.
/// In cases when metadata of a file cannot be accessed, an error message is pushed
/// in the result vector and such file is placed at the beginning of the list.
pub fn sort_by_priority<P>(files: &mut [FileSubGroup<P>], priority: &Priority) -> Vec<Error>
where
    P: AsRef<PathAndMetadata>,
{
    match priority {
        Priority::Top => {
            files.reverse();
            vec![]
        }
        Priority::Bottom => vec![],
        Priority::Newest => try_sort_by_key(files, |m| m.created()),
        Priority::Oldest => try_sort_by_key(files, |m| m.created().map(Reverse)),
        Priority::MostRecentlyModified => try_sort_by_key(files, |m| m.modified()),
        Priority::LeastRecentlyModified => try_sort_by_key(files, |m| m.modified().map(Reverse)),
        Priority::MostRecentlyAccessed => try_sort_by_key(files, |m| m.accessed()),
        Priority::LeastRecentlyAccessed => try_sort_by_key(files, |m| m.accessed().map(Reverse)),
        #[cfg(unix)]
        Priority::MostRecentStatusChange => try_sort_by_key(files, |m| m.status_changed()),
        #[cfg(unix)]
        Priority::LeastRecentStatusChange => {
            try_sort_by_key(files, |m| m.status_changed().map(Reverse))
        }
        Priority::MostNested => {
            files.sort_by_key(|m| m.max_nesting());
            vec![]
        }
        Priority::LeastNested => {
            files.sort_by_key(|m| Reverse(m.min_nesting()));
            vec![]
        }
    }
}

#[derive(Debug)]
pub struct PartitionedFileGroup {
    pub to_keep: Vec<PathAndMetadata>,
    pub to_drop: Vec<PathAndMetadata>,
}

impl PartitionedFileGroup {
    /// Returns the destination path where the file should be moved when the
    /// dedupe mode was selected to move
    fn move_target(target_dir: &Arc<Path>, source_path: &Path) -> Path {
        let root = source_path
            .root()
            .map(|p| p.to_string_lossy().replace(['/', '\\', ':'], ""));
        let suffix = source_path.strip_root();
        match root {
            None => target_dir.join(suffix),
            Some(root) => Arc::new(target_dir.join(Path::from(root))).join(suffix),
        }
    }

    fn are_on_same_mount(devices: &DiskDevices, file1: &Path, file2: &Path) -> bool {
        // If the mount points are unknown, try a rename anyway: moving falls back to copying
        // when the rename fails.
        let mount1 = devices.get_mount_point(file1);
        let mount2 = devices.get_mount_point(file2);
        mount1 == mount2
    }

    /// Returns a list of commands that would remove redundant files in this group when executed.
    pub fn dedupe_script(mut self, strategy: &DedupeOp, devices: &DiskDevices) -> Vec<FsCommand> {
        if self.to_drop.is_empty() {
            return vec![];
        }
        assert!(
            !self.to_keep.is_empty(),
            "No files would be left after deduplicating"
        );
        let mut commands = Vec::new();
        // Link to a regular file rather than to a symbolic link, if possible.
        let target_index = self
            .to_keep
            .iter()
            .position(|f| !is_symlink(&f.path))
            .unwrap_or(0);
        let retained_file = Arc::new(self.to_keep.swap_remove(target_index));
        for dropped_file in self.to_drop {
            match strategy {
                DedupeOp::SymbolicLink => commands.push(FsCommand::SoftLink {
                    target: retained_file.clone(),
                    link: dropped_file,
                }),
                DedupeOp::HardLink => commands.push(FsCommand::HardLink {
                    target: retained_file.clone(),
                    link: dropped_file,
                }),
                DedupeOp::RefLink => commands.push(FsCommand::RefLink {
                    target: retained_file.clone(),
                    link: dropped_file,
                }),
                DedupeOp::Remove => commands.push(FsCommand::Remove { file: dropped_file }),
                DedupeOp::Move(target_dir) => {
                    let source = dropped_file;
                    let source_path = &source.path;
                    let use_rename = Self::are_on_same_mount(devices, source_path, target_dir);
                    let target = Self::move_target(target_dir, source_path);
                    commands.push(FsCommand::Move {
                        source,
                        target,
                        use_rename,
                    })
                }
            }
        }
        commands
    }
}

/// Attempts to retrieve the metadata of all the files in the file group.
/// If metadata is inaccessible for a file, a warning is emitted to the log, and None gets returned.
fn fetch_files_metadata<P>(group: FileGroup<P>, log: &dyn Log) -> Option<FileGroup<PathAndMetadata>>
where
    P: Into<Path>,
{
    group
        .try_map_all(|p| {
            PathAndMetadata::new(p.into()).map_err(|e| {
                log.warn(&e);
            })
        })
        .ok()
}

/// Partitions a group of files into files to keep and files that can be safely dropped
/// (or linked).
fn partition(
    group: FileGroup<PathAndMetadata>,
    config: &DedupeConfig,
    log: &dyn Log,
) -> Result<PartitionedFileGroup, Error> {
    let file_len = group.file_len;
    let file_hash = group.file_hash.clone();
    let mut files = group.files;

    let error = |msg: &str| {
        Err(Error::from(format!(
            "Could not determine files to drop in group with hash {} and len {}: {}",
            file_hash, file_len.0, msg
        )))
    };

    // We don't want to remove dirs or symlinks
    files.retain(|m| {
        let is_file = m.metadata.is_file();
        if !is_file {
            log.warn(format!(
                "Skipping file {}: Not a regular file",
                m.path.display()
            ));
        }
        is_file
    });

    // If file has a different length, then we really know it has been modified.
    // Therefore, it does not belong to the group and we can safely skip it.
    if !config.no_check_size {
        files.retain(|m| {
            let len_ok = m.metadata.len() == file_len;
            if !len_ok {
                log.warn(format!(
                    "Skipping file {} with length {} different than the group length {}",
                    m.path.display(),
                    m.metadata.len(),
                    file_len.0,
                ));
            }
            len_ok
        });
    }

    // Bail out as well if any file has been modified after `config.modified_before`.
    // We need to skip the whole group, because we don't know if these files are really different.
    if let Some(max_timestamp) = config.modified_before {
        if was_modified(&files, max_timestamp, log) {
            return error("Some files could be updated since the previous run of fclones");
        }
    }

    // Position of each file in the report, for the `top` and `bottom` priorities.
    let position: HashMap<Path, usize> = files.iter().map(|f| f.path.clone()).zip(0..).collect();

    let mut file_sub_groups =
        FileSubGroup::group(files, &config.isolated_roots, !config.match_links);

    // Sort files to remove in user selected order.
    // The priorities at the beginning of the argument list have precedence over
    // the priorities given at the end of the argument list, therefore we're applying
    // them in reversed order.
    // `top` and `bottom` must be stable sorts by the position in the report, like the other
    // priorities are stable sorts by their key. Reversing the vector (or doing nothing) would
    // instead invert or keep whatever order the previously applied priority established.
    let mut sort_errors = Vec::new();
    let first_position = |g: &FileSubGroup<PathAndMetadata>| {
        g.files.iter().map(|f| position[&f.path]).min().unwrap_or(0)
    };
    for priority in config.priority.iter().rev() {
        match priority {
            Priority::Top => file_sub_groups.sort_by_key(|g| Reverse(first_position(g))),
            Priority::Bottom => file_sub_groups.sort_by_key(first_position),
            _ => sort_errors.extend(sort_by_priority(&mut file_sub_groups, priority)),
        }
    }

    if !sort_errors.is_empty() {
        for e in sort_errors {
            log.warn(e);
        }
        return error("Metadata of some files could not be read.");
    }

    // Split the set of file subgroups into two sets - a set that we want to keep intact and a set
    // that we can remove or replace with links:
    let (mut to_retain, mut to_drop): (Vec<_>, Vec<_>) = file_sub_groups
        .into_iter()
        .partition(|m| m.should_keep(config) || !m.may_drop(config));

    // If the set to retain is smaller than the number of files we must keep (rf), then
    // move some higher priority files from `to_drop` and append them to `to_retain`.
    let n = max(1, config.rf_over.unwrap_or(1));
    let missing_count = min(to_drop.len(), n.saturating_sub(to_retain.len()));
    to_retain.extend(to_drop.drain(0..missing_count));

    assert!(to_retain.len() >= n || to_drop.is_empty());
    let mut to_keep: Vec<PathAndMetadata> = to_retain.into_iter().flat_map(|g| g.files).collect();
    let to_drop: Vec<PathAndMetadata> = to_drop.into_iter().flat_map(|g| g.files).collect();

    // A symbolic link and its target can end up in different subgroups (with --match-links or
    // with isolated roots). Never drop a regular file whose data is kept only through symbolic
    // links pointing to it: removing or replacing it would leave the kept links dangling.
    let data_kept: HashSet<FileId> = to_keep
        .iter()
        .filter(|f| !is_symlink(&f.path))
        .map(|f| f.metadata.file_id())
        .collect();
    let kept_ids: HashSet<FileId> = to_keep.iter().map(|f| f.metadata.file_id()).collect();
    let (needed, to_drop): (Vec<_>, Vec<_>) = to_drop.into_iter().partition(|f| {
        let id = f.metadata.file_id();
        kept_ids.contains(&id) && !data_kept.contains(&id) && !is_symlink(&f.path)
    });
    for f in needed {
        log.warn(format!(
            "Keeping {}: a symbolic link to it is kept, removing it would break that link",
            f.path.display()
        ));
        to_keep.push(f);
    }

    Ok(PartitionedFileGroup { to_keep, to_drop })
}

/// Returns true if the path is a symbolic link (does not follow the link).
fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path.to_path_buf())
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Generates a list of commands that will remove the redundant files in the groups provided
/// by the `groups` iterator.
///
/// Calling this is perfectly safe - the function does not perform any disk changes.
///
/// This function performs extensive checks if files can be removed.
/// It rejects a group of files if:
/// - metadata of any files in the group cannot be read,
/// - any file in the group was modified after the `modified_before` configuration property
///
/// Additionally it will never emit commands to remove a file which:
/// - has length that does not match the file length recorded in the group metadata
/// - was matched by any of the `retain_path` or `retain_name` patterns
/// - was not matched by all `drop_path` and `drop_name` patterns
///
/// The commands in the list are grouped into vectors where each
/// vector has its sequential id. This id allows to convert the parallel iterator into a
/// sequential iterator with the same order as the groups in the input file.
/// Unfortunately Rayon does not allow to convert a parallel iterator
/// to a sequential iterator easily, so we need this hack with prepending ids of each group.
///
/// # Parameters
/// - `groups`: iterator over groups of identical files
/// - `op`: what to do with duplicates
/// - `config`: controls which files from each group to remove / link
/// - `log`: logging target
pub fn dedupe<'a, I, P>(
    groups: I,
    op: DedupeOp,
    config: &'a DedupeConfig,
    log: &'a dyn Log,
) -> impl ParallelIterator<Item = (usize, Vec<FsCommand>)> + 'a
where
    I: IntoIterator<Item = FileGroup<P>> + 'a,
    I::IntoIter: Send,
    P: Into<Path> + AsRef<Path> + fmt::Debug + Send + 'a,
{
    let devices = DiskDevices::new(&HashMap::new());
    let disallow_cross_device = op == DedupeOp::HardLink || op == DedupeOp::RefLink;
    groups
        .into_iter()
        .enumerate()
        .par_bridge()
        .map(move |(i, group)| {
            let mut commands = Vec::new();
            if let Some(group) = fetch_files_metadata(group, log) {
                let groups = if disallow_cross_device {
                    group.partition_by_key(|p| p.metadata.device_id())
                } else {
                    vec![group]
                };
                for group in groups {
                    match partition(group, config, log) {
                        Ok(group) => commands.extend(group.dedupe_script(&op, &devices)),
                        Err(e) => log.warn(e),
                    }
                }
            }
            (i, commands)
        })
}

/// Runs a deduplication script generated by [`dedupe`].
///
/// Calling this function is going to change the contents of the file-system.
/// No safety checks are performed.
/// Commands are executed in parallel, on the default Rayon thread-pool.
/// On command execution failure, a warning is logged and the execution of remaining commands
/// continues.
/// Returns the number of files processed and the amount of disk space reclaimed.
pub fn run_script<I>(
    script: I,
    should_lock: bool,
    ignore_xattr_errors: bool,
    log: &dyn Log,
) -> DedupeResult
where
    I: IntoParallelIterator<Item = (usize, Vec<FsCommand>)>,
{
    script
        .into_par_iter()
        .flat_map(|(_, cmd_vec)| cmd_vec)
        .filter_map(
            |cmd| match cmd.execute(should_lock, ignore_xattr_errors, log) {
                Ok(_) => Some(ReclaimedSpace::from_file(cmd.dropped_file())),
                Err(e) => {
                    log.warn(e);
                    None
                }
            },
        )
        .reduce(ReclaimedSpace::default, ReclaimedSpace::merge)
        .into_result()
}

/// Accumulates the processed files and the disk space freed by processing them.
///
/// Data of a file with several hard links is freed only if all of its links are removed or
/// replaced, and then only once, so the space is counted per inode and only if the number of
/// processed links equals the link count of the inode.
#[derive(Default)]
struct ReclaimedSpace {
    processed_count: u64,
    inodes: HashMap<FileId, (u64, u64, FileLen)>, // id -> (processed links, link count, len)
}

impl ReclaimedSpace {
    fn from_file(file: &PathAndMetadata) -> ReclaimedSpace {
        let mut result = ReclaimedSpace::default();
        result.add(file);
        result
    }

    fn add(&mut self, file: &PathAndMetadata) {
        self.processed_count += 1;
        let entry = self.inodes.entry(file.metadata.file_id()).or_insert((
            0,
            link_count(&file.metadata),
            file.metadata.len(),
        ));
        entry.0 += 1;
    }

    fn merge(mut self, other: ReclaimedSpace) -> ReclaimedSpace {
        self.processed_count += other.processed_count;
        for (id, (count, nlink, len)) in other.inodes {
            self.inodes.entry(id).or_insert((0, nlink, len)).0 += count;
        }
        self
    }

    fn into_result(self) -> DedupeResult {
        DedupeResult {
            processed_count: self.processed_count,
            reclaimed_space: self
                .inodes
                .values()
                .filter(|(count, nlink, _)| count >= nlink)
                .map(|(_, _, len)| *len)
                .sum(),
        }
    }
}

#[cfg(unix)]
fn link_count(metadata: &FileMetadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.nlink()
}

#[cfg(not(unix))]
fn link_count(_metadata: &FileMetadata) -> u64 {
    1
}

/// We need this so we can put command vectors in a priority queue.
struct FsCommandGroup {
    index: usize,
    commands: Vec<FsCommand>,
}

impl FsCommandGroup {
    pub fn new(index: usize, commands: Vec<FsCommand>) -> FsCommandGroup {
        FsCommandGroup { index, commands }
    }
}

impl PartialEq<Self> for FsCommandGroup {
    fn eq(&self, other: &Self) -> bool {
        self.index == other.index
    }
}

impl Eq for FsCommandGroup {}

impl Hash for FsCommandGroup {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.index.hash(state)
    }
}

/// Prints a script generated by [`dedupe`] to stdout.
///
/// Does not perform any filesystem changes.
/// Returns the number of files processed and the amount of disk space that would be
/// reclaimed if all commands of the script were executed with no error.
pub fn log_script(
    script: impl IntoParallelIterator<Item = (usize, Vec<FsCommand>)> + Send,
    mut out: impl Write + Send,
) -> io::Result<DedupeResult> {
    // Unfortunately the items may come in any order from the ParallelIterator,
    // and that order may change with each run, because multiple threads race to produce
    // the next item. However, we want to print the commands
    // in the same deterministic order as the groups in the input file.
    // That's why we first send the commands to a PriorityQueue through a channel in order to
    // get them all on a single thread. The queue sorts them by their sequential identifiers.
    // Because the identifiers are consecutive integers, we know which item should be printed next.

    let (tx, rx) = channel();

    // Scope needed so the script iterator doesn't need to be 'static.
    // This way we tell the compiler the background thread we start to process the iterator
    // terminates before exiting this function.
    crossbeam_utils::thread::scope(move |s| {
        // Process items in parallel in a background thread, so we can read as soon as they
        // are produced:
        s.spawn(move |_| {
            script
                .into_par_iter()
                .for_each_with(tx, |tx, item| tx.send(item).unwrap())
        });

        let mut queue = PriorityQueue::new();
        let mut next_group_index = 0;
        let mut reclaimed = ReclaimedSpace::default();

        while let Ok((group_index, commands)) = rx.recv() {
            // Push the command group we received from the iterator.
            // We may receive them in an incorrect order, so we push them to a PriorityQueue.
            queue.push(
                FsCommandGroup::new(group_index, commands),
                Reverse(group_index), // we want to get items with lowest-index first
            );

            // Process items in the queue as soon as possible to save memory.
            while let Some((group, _)) = queue.peek() {
                // Only process the item if it is the next one we expect.
                // If we see an out-of-order item, we simply need to wait for more items
                // to be pushed.
                if group.index != next_group_index {
                    break;
                }
                // We got the right item, let's pop it from the queue and log it:
                next_group_index += 1;
                let cmd_vec = queue.pop().unwrap().0.commands;
                for cmd in cmd_vec {
                    reclaimed.add(cmd.dropped_file());
                    for line in cmd.to_shell_str() {
                        writeln!(out, "{line}")?;
                    }
                }
            }
        }

        Ok(reclaimed.into_result())
    })
    .unwrap()
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;
    use std::default::Default;
    use std::fs::{create_dir, create_dir_all};
    use std::path::PathBuf;
    use std::str::FromStr;
    use std::{thread, time};

    use chrono::Duration;
    use itertools::Itertools;

    use crate::config::GroupConfig;
    use crate::file::FileHash;
    use crate::group_files;
    use crate::log::StdLog;
    use crate::pattern::Pattern;
    use crate::util::test::{create_file, create_file_newer_than, read_file, with_dir, write_file};

    use super::*;

    #[test]
    fn test_temp_file_name_generation() {
        let path = Path::from("/foo/bar");
        let temp = FsCommand::temp_file(&path);
        assert_ne!(path, temp);
        assert_ne!(
            path.file_name().unwrap().len(),
            temp.file_name().unwrap().len()
        );
        assert_eq!(path.parent(), temp.parent());
    }

    /// Appending 25 characters to a long name exceeded the 255 byte limit, so linking or
    /// deduplicating files with long names failed with "File name too long" (upstream #353).
    #[test]
    fn test_hard_link_command_works_with_long_file_names() {
        with_dir("dedupe/long_names", |root| {
            let log = StdLog::new();
            let target_path = root.join("t".repeat(250));
            let link_path = root.join("l".repeat(250));
            write_file(&target_path, "content");
            write_file(&link_path, "content");
            let tmp = FsCommand::temp_file(&Path::from(&link_path));
            assert!(tmp.file_name().unwrap().len() <= 255);
            assert_eq!(tmp.parent(), Path::from(&link_path).parent());

            FsCommand::HardLink {
                target: Arc::new(PathAndMetadata::new(Path::from(&target_path)).unwrap()),
                link: PathAndMetadata::new(Path::from(&link_path)).unwrap(),
            }
            .execute(false, false, &log)
            .unwrap();
            assert_eq!(
                FileId::new(&Path::from(&link_path)).unwrap(),
                FileId::new(&Path::from(&target_path)).unwrap()
            );
        })
    }

    #[test]
    fn test_remove_command_removes_file() {
        with_dir("dedupe/remove_cmd", |root| {
            let log = StdLog::new();
            let file_path = root.join("file");
            create_file(&file_path);
            let file = PathAndMetadata::new(Path::from(&file_path)).unwrap();
            let cmd = FsCommand::Remove { file };
            cmd.execute(true, false, &log).unwrap();
            assert!(!file_path.exists())
        })
    }

    /// Locking must not prevent removing or linking read-only files: those operations need
    /// write access to the directory only.
    #[test]
    #[cfg(unix)]
    fn test_commands_work_on_read_only_files_with_locking() {
        use std::os::unix::fs::PermissionsExt;
        if unsafe { libc::geteuid() } == 0 {
            return; // root can open read-only files for writing anyway
        }
        with_dir("dedupe/read_only_lock", |root| {
            let log = StdLog::new();
            let read_only = |p: &PathBuf| {
                fs::set_permissions(p, fs::Permissions::from_mode(0o444)).unwrap();
            };

            let file_path = root.join("file");
            write_file(&file_path, "content");
            read_only(&file_path);
            let file = PathAndMetadata::new(Path::from(&file_path)).unwrap();
            FsCommand::Remove { file }
                .execute(true, false, &log)
                .unwrap();
            assert!(!file_path.exists());

            let target_path = root.join("target");
            let link_path = root.join("link");
            write_file(&target_path, "content");
            write_file(&link_path, "content");
            read_only(&link_path);
            FsCommand::HardLink {
                target: Arc::new(PathAndMetadata::new(Path::from(&target_path)).unwrap()),
                link: PathAndMetadata::new(Path::from(&link_path)).unwrap(),
            }
            .execute(true, false, &log)
            .unwrap();
            assert_eq!(
                FileId::new(&Path::from(&link_path)).unwrap(),
                FileId::new(&Path::from(&target_path)).unwrap()
            );
        })
    }

    #[test]
    fn test_move_command_moves_file_by_rename() {
        with_dir("dedupe/move_rename_cmd", |root| {
            let log = StdLog::new();
            let file_path = root.join("file");
            let target = Path::from(root.join("target"));
            create_file(&file_path);
            let file = PathAndMetadata::new(Path::from(&file_path)).unwrap();
            let cmd = FsCommand::Move {
                source: file,
                target: target.clone(),
                use_rename: true,
            };
            cmd.execute(true, false, &log).unwrap();
            assert!(!file_path.exists());
            assert!(target.to_path_buf().exists());
        })
    }

    #[test]
    fn test_move_command_moves_file_by_copy() {
        with_dir("dedupe/move_copy_cmd", |root| {
            let log = StdLog::new();
            let file_path = root.join("file");
            let target = Path::from(root.join("target"));
            create_file(&file_path);
            let file = PathAndMetadata::new(Path::from(&file_path)).unwrap();
            let cmd = FsCommand::Move {
                source: file,
                target: target.clone(),
                use_rename: false,
            };
            cmd.execute(true, false, &log).unwrap();
            assert!(!file_path.exists());
            assert!(target.to_path_buf().exists());
        })
    }

    #[test]
    fn test_move_fails_if_target_exists() {
        with_dir("dedupe/move_target_exists", |root| {
            let log = StdLog::new();
            let file_path = root.join("file");
            let target = root.join("target");
            create_file(&file_path);
            create_file(&target);
            let file = PathAndMetadata::new(Path::from(&file_path)).unwrap();
            let cmd = FsCommand::Move {
                source: file,
                target: Path::from(&target),
                use_rename: false,
            };
            assert!(cmd.execute(true, false, &log).is_err());
        })
    }

    #[test]
    fn test_soft_link_command_replaces_file_with_a_link() {
        with_dir("dedupe/soft_link_cmd", |root| {
            let log = StdLog::new();
            let file_path_1 = root.join("file_1");
            let file_path_2 = root.join("file_2");
            write_file(&file_path_1, "foo");
            write_file(&file_path_2, "");

            let file_1 = PathAndMetadata::new(Path::from(&file_path_1)).unwrap();
            let file_2 = PathAndMetadata::new(Path::from(&file_path_2)).unwrap();
            let cmd = FsCommand::SoftLink {
                target: Arc::new(file_1),
                link: file_2,
            };
            cmd.execute(true, false, &log).unwrap();

            assert!(file_path_1.exists());
            assert!(file_path_2.exists());
            assert!(fs::symlink_metadata(&file_path_2)
                .unwrap()
                .file_type()
                .is_symlink());
            assert_eq!(read_file(&file_path_2), "foo");
        })
    }

    #[test]
    fn test_hard_link_command_replaces_file_with_a_link() {
        with_dir("dedupe/hard_link_cmd", |root| {
            let log = StdLog::new();
            let file_path_1 = root.join("file_1");
            let file_path_2 = root.join("file_2");

            write_file(&file_path_1, "foo");
            write_file(&file_path_2, "");

            let file_1 = PathAndMetadata::new(Path::from(&file_path_1)).unwrap();
            let file_2 = PathAndMetadata::new(Path::from(&file_path_2)).unwrap();
            let cmd = FsCommand::HardLink {
                target: Arc::new(file_1),
                link: file_2,
            };
            cmd.execute(true, false, &log).unwrap();

            assert!(file_path_1.exists());
            assert!(file_path_2.exists());
            assert_eq!(read_file(&file_path_2), "foo");
        })
    }

    /// Creates 3 empty files with different creation time and returns a FileGroup describing them
    fn make_group(root: &PathBuf, file_hash: FileHash) -> FileGroup<Path> {
        create_dir_all(root).unwrap();
        let file_1 = root.join("file_1");
        let file_2 = root.join("file_2");
        let file_3 = root.join("file_3");
        create_file(&file_1);
        let ctime_1 = fs::metadata(&file_1).unwrap().modified().unwrap();
        let ctime_2 = create_file_newer_than(&file_2, ctime_1);
        create_file_newer_than(&file_3, ctime_2);

        FileGroup {
            file_len: FileLen(0),
            file_hash,
            files: vec![
                Path::from(&file_1),
                Path::from(&file_2),
                Path::from(&file_3),
            ],
        }
    }

    #[test]
    fn test_partition_selects_files_for_removal() {
        with_dir("dedupe/partition/basic", |root| {
            let group = make_group(root, FileHash::from_str("00").unwrap());
            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let config = DedupeConfig::default();
            let partitioned = partition(group, &config, &StdLog::new()).unwrap();
            assert_eq!(partitioned.to_keep.len(), 1);
            assert_eq!(partitioned.to_drop.len(), 2);
        })
    }

    #[test]
    fn test_partition_bails_out_if_file_modified_too_late() {
        with_dir("dedupe/partition/modification", |root| {
            let group = make_group(root, FileHash::from_str("00").unwrap());
            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let config = DedupeConfig {
                modified_before: Some(DateTime::from(Local::now() - Duration::days(1))),
                ..DedupeConfig::default()
            };
            let partitioned = partition(group, &config, &StdLog::new());
            assert!(partitioned.is_err());
        })
    }

    /// Upstream issue #84: a file whose content was replaced after `fclones group` while its
    /// modification time was preserved (`cp -a`) must not be removed or linked.
    #[test]
    #[cfg(unix)]
    fn test_partition_bails_out_if_file_changed_with_preserved_mtime() {
        with_dir("dedupe/partition/preserved_mtime", |root| {
            let group = make_group(root, FileHash::from_str("00").unwrap());
            let file = group.files[1].to_path_buf();
            let mtime =
                filetime::FileTime::from_last_modification_time(&fs::metadata(&file).unwrap());
            let report_time = Local::now();
            thread::sleep(time::Duration::from_millis(20));
            write_file(&file, "");
            filetime::set_file_mtime(&file, mtime).unwrap();

            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let config = DedupeConfig {
                modified_before: Some(DateTime::from(report_time)),
                ..DedupeConfig::default()
            };
            assert!(partition(group, &config, &StdLog::new()).is_err());
        })
    }

    #[test]
    fn test_partition_skips_file_with_different_len() {
        with_dir("dedupe/partition/file_len", |root| {
            let group = make_group(root, FileHash::from_str("00").unwrap());
            let path = group.files[0].clone();
            write_file(&path.to_path_buf(), "foo");
            let config = DedupeConfig {
                priority: vec![Priority::MostRecentlyModified],
                ..DedupeConfig::default()
            };
            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let partitioned = partition(group, &config, &StdLog::new()).unwrap();
            assert!(!partitioned.to_drop.iter().any(|m| m.path == path));
            assert!(!partitioned.to_keep.iter().any(|m| m.path == path));
        })
    }

    fn path_set(v: &[PathAndMetadata]) -> HashSet<&Path> {
        v.iter().map(|f| &f.path).collect()
    }

    #[test]
    fn test_partition_respects_creation_time_priority() {
        with_dir("dedupe/partition/ctime_priority", |root| {
            if fs::metadata(root).unwrap().created().is_err() {
                // can't run the test because the filesystem doesn't support fetching
                // file creation time
                return;
            }
            let group = make_group(root, FileHash::from_str("00").unwrap());
            let mut config = DedupeConfig {
                priority: vec![Priority::Newest],
                ..DedupeConfig::default()
            };
            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let partitioned_1 = partition(group.clone(), &config, &StdLog::new()).unwrap();
            config.priority = vec![Priority::Oldest];
            let partitioned_2 = partition(group, &config, &StdLog::new()).unwrap();

            assert_ne!(
                path_set(&partitioned_1.to_keep),
                path_set(&partitioned_2.to_keep)
            );
            assert_ne!(
                path_set(&partitioned_1.to_drop),
                path_set(&partitioned_2.to_drop)
            );
        });
    }

    /// `top`/`bottom` combined with another priority: the first priority decides, the second
    /// only breaks ties. They used to be implemented as reverse()/no-op, which inverted or
    /// ignored them as soon as another priority was given.
    #[test]
    fn test_partition_top_and_bottom_combined_with_other_priorities() {
        with_dir("dedupe/partition/top_bottom", |root| {
            // Listed a, b, c; a is the most recently modified, c the least recently.
            let paths = ["a", "b", "c"].map(|n| root.join(n));
            for (i, p) in paths.iter().enumerate() {
                write_file(p, "aaa");
                let mtime = filetime::FileTime::from_unix_time(1_600_000_000 - 1000 * i as i64, 0);
                filetime::set_file_mtime(p, mtime).unwrap();
            }
            let dropped = |priority: Vec<Priority>| {
                let group = FileGroup {
                    file_len: FileLen(3),
                    file_hash: FileHash::from_str("00").unwrap(),
                    files: paths.iter().map(|p| Path::from(p)).collect(),
                }
                .map(|p| PathAndMetadata::new(p).unwrap());
                let config = DedupeConfig {
                    priority,
                    ..DedupeConfig::default()
                };
                let mut names: Vec<String> = partition(group, &config, &StdLog::new())
                    .unwrap()
                    .to_drop
                    .iter()
                    .map(|f| f.path.file_name().unwrap().to_string_lossy().to_string())
                    .collect();
                names.sort();
                names
            };
            use Priority::*;
            assert_eq!(dropped(vec![Top]), ["a", "b"]);
            assert_eq!(dropped(vec![Top, MostRecentlyModified]), ["a", "b"]);
            assert_eq!(dropped(vec![Bottom]), ["b", "c"]);
            assert_eq!(dropped(vec![Bottom, MostRecentlyModified]), ["b", "c"]);
            // The second priority must still work as a tie breaker:
            assert_eq!(dropped(vec![MostRecentlyModified, Top]), ["a", "b"]);
        })
    }

    #[test]
    fn test_partition_respects_modification_time_priority() {
        with_dir("dedupe/partition/mtime_priority", |root| {
            let group = make_group(root, FileHash::from_str("00").unwrap());

            thread::sleep(time::Duration::from_millis(10));
            let path = group.files[0].clone();
            write_file(&path.to_path_buf(), "foo");

            // note that fetching metadata happens after we wrote the file
            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let config = DedupeConfig {
                priority: vec![Priority::MostRecentlyModified],
                ..DedupeConfig::default()
            };
            let partitioned_1 = partition(group.clone(), &config, &StdLog::new()).unwrap();

            let config = DedupeConfig {
                priority: vec![Priority::LeastRecentlyModified],
                ..DedupeConfig::default()
            };
            let partitioned_2 = partition(group, &config, &StdLog::new()).unwrap();

            assert_ne!(
                path_set(&partitioned_1.to_keep),
                path_set(&partitioned_2.to_keep)
            );
            assert_ne!(
                path_set(&partitioned_1.to_drop),
                path_set(&partitioned_2.to_drop)
            );
        });
    }

    #[test]
    fn test_partition_respects_keep_patterns() {
        with_dir("dedupe/partition/keep", |root| {
            let group = make_group(root, FileHash::from_str("00").unwrap());
            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let mut config = DedupeConfig {
                priority: vec![Priority::LeastRecentlyModified],
                keep_name_patterns: vec![Pattern::glob("*_1").unwrap()],
                ..DedupeConfig::default()
            };

            let p = partition(group.clone(), &config, &StdLog::new()).unwrap();
            assert_eq!(p.to_keep.len(), 1);
            assert_eq!(&p.to_keep[0].path, &group.files[0].path);

            config.keep_name_patterns = vec![];
            config.keep_path_patterns = vec![Pattern::glob("**/file_1").unwrap()];
            let p = partition(group.clone(), &config, &StdLog::new()).unwrap();
            assert_eq!(p.to_keep.len(), 1);
            assert_eq!(&p.to_keep[0].path, &group.files[0].path);
        })
    }

    #[test]
    fn test_partition_respects_drop_patterns() {
        with_dir("dedupe/partition/drop", |root| {
            let group = make_group(root, FileHash::from_str("00").unwrap());
            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let mut config = DedupeConfig {
                priority: vec![Priority::LeastRecentlyModified],
                name_patterns: vec![Pattern::glob("*_3").unwrap()],
                ..DedupeConfig::default()
            };
            let p = partition(group.clone(), &config, &StdLog::new()).unwrap();
            assert_eq!(p.to_drop.len(), 1);
            assert_eq!(&p.to_drop[0].path, &group.files[2].path);

            config.name_patterns = vec![];
            config.path_patterns = vec![Pattern::glob("**/file_3").unwrap()];
            let p = partition(group.clone(), &config, &StdLog::new()).unwrap();
            assert_eq!(p.to_drop.len(), 1);
            assert_eq!(&p.to_drop[0].path, &group.files[2].path);
        })
    }

    #[test]
    fn test_partition_respects_isolated_roots() {
        with_dir("dedupe/partition/isolated_roots", |root| {
            let root1 = root.join("root1");
            let root2 = root.join("root2");
            create_dir(&root1).unwrap();
            create_dir(&root2).unwrap();

            let group1 = make_group(&root1, FileHash::from_str("00").unwrap());
            let group2 = make_group(&root2, FileHash::from_str("00").unwrap());
            let group = FileGroup {
                file_len: group1.file_len,
                file_hash: group1.file_hash,
                files: group1.files.into_iter().chain(group2.files).collect(),
            };

            let config = DedupeConfig {
                isolated_roots: vec![Path::from(&root1), Path::from(&root2)],
                ..DedupeConfig::default()
            };

            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let p = partition(group, &config, &StdLog::new()).unwrap();
            assert_eq!(p.to_drop.len(), 3);
            assert!(p
                .to_drop
                .iter()
                .all(|f| f.path.to_path_buf().starts_with(&root2)));
            assert_eq!(p.to_keep.len(), 3);
            assert!(p
                .to_keep
                .iter()
                .all(|f| f.path.to_path_buf().starts_with(&root1)));
        })
    }

    #[test]
    fn test_partition_respects_links() {
        with_dir("dedupe/partition/links", |root| {
            let root_a = root.join("root_a");
            let root_b = root.join("root_b");
            create_dir(&root_a).unwrap();
            create_dir(&root_b).unwrap();

            let file_a1 = root_a.join("file_a1");
            let file_a2 = root_a.join("file_a2");
            write_file(&file_a1, "aaa");
            fs::hard_link(&file_a1, &file_a2).unwrap();

            let file_b1 = root_b.join("file_b1");
            let file_b2 = root_b.join("file_b2");
            write_file(&file_b1, "aaa");
            fs::hard_link(&file_b1, &file_b2).unwrap();

            let group = FileGroup {
                file_len: FileLen(3),
                file_hash: FileHash::from_str("00").unwrap(),
                files: vec![
                    Path::from(&file_b1),
                    Path::from(&file_a2),
                    Path::from(&file_a1),
                    Path::from(&file_b2),
                ],
            };

            let config = DedupeConfig::default();
            let group = group.map(|p| PathAndMetadata::new(p).unwrap());
            let p = partition(group, &config, &StdLog::new()).unwrap();

            // drop A files because file_a2 appears after file_b1 in the files vector
            assert_eq!(p.to_drop.len(), 2);
            assert!(p
                .to_drop
                .iter()
                .all(|f| f.path.to_path_buf().starts_with(&root_a)));
            assert_eq!(p.to_keep.len(), 2);
            assert!(p
                .to_keep
                .iter()
                .all(|f| f.path.to_path_buf().starts_with(&root_b)));
        })
    }

    #[test]
    fn test_run_dedupe_script() {
        with_dir("dedupe/partition/run_dedupe_script", |root| {
            let mut log = StdLog::new();
            log.no_progress = true;
            log.log_stderr_to_stdout = true;

            let group = make_group(root, FileHash::from_str("00").unwrap());
            let config = DedupeConfig {
                priority: vec![Priority::LeastRecentlyModified],
                ..DedupeConfig::default()
            };
            let script = dedupe(vec![group], DedupeOp::Remove, &config, &log);
            let dedupe_result = run_script(script, !config.no_lock, false, &log);
            assert_eq!(dedupe_result.processed_count, 2);
            assert!(!root.join("file_1").exists());
            assert!(!root.join("file_2").exists());
            assert!(root.join("file_3").exists());
        });
    }

    /// The printed move script must work like the real move: it has to create the target
    /// directory first.
    #[test]
    #[cfg(unix)]
    fn test_move_script_creates_target_dir() {
        with_dir("dedupe/move_script", |root| {
            let file_path = root.join("file");
            write_file(&file_path, "content");
            let target = Path::from(root.join("attic/deep/dir/file"));
            let cmd = FsCommand::Move {
                source: PathAndMetadata::new(Path::from(&file_path)).unwrap(),
                target: target.clone(),
                use_rename: true,
            };
            let script = cmd.to_shell_str().join("\n");
            assert!(script.starts_with("mkdir -p "), "{script}");
            let status = std::process::Command::new("sh")
                .args(["-e", "-c", &script])
                .status()
                .unwrap();
            assert!(status.success(), "{script}");
            assert!(!file_path.exists());
            assert_eq!(read_file(&target.to_path_buf()), "content");
        })
    }

    /// `mklink [/H] <link> <target>`: the arguments were in the wrong order (upstream #367).
    #[test]
    #[cfg(windows)]
    fn test_windows_link_script_argument_order() {
        with_dir("dedupe/mklink_script", |root| {
            let target_path = root.join("target");
            let link_path = root.join("link");
            write_file(&target_path, "content");
            write_file(&link_path, "content");
            let target = Arc::new(PathAndMetadata::new(Path::from(&target_path)).unwrap());
            let link = PathAndMetadata::new(Path::from(&link_path)).unwrap();
            let expected = format!("{} {}", link.path.quote(), target.path.quote());
            let soft = FsCommand::SoftLink {
                target: target.clone(),
                link: link.clone(),
            }
            .to_shell_str();
            let hard = FsCommand::HardLink { target, link }.to_shell_str();
            assert_eq!(soft[1], format!("mklink {expected}"));
            assert_eq!(hard[1], format!("mklink /H {expected}"));
        })
    }

    #[test]
    fn test_log_dedupe_script() {
        with_dir("dedupe/partition/log_dedupe_script", |root| {
            let mut log = StdLog::new();
            log.no_progress = true;
            log.log_stderr_to_stdout = true;

            let group_1 = make_group(&root.join("group_1"), FileHash::from_str("00").unwrap());
            let group_2 = make_group(&root.join("group_2"), FileHash::from_str("01").unwrap());
            let group_3 = make_group(&root.join("group_3"), FileHash::from_str("02").unwrap());
            let groups = vec![group_1, group_2, group_3];

            let config = DedupeConfig {
                priority: vec![Priority::LeastRecentlyModified],
                ..DedupeConfig::default()
            };

            let script = dedupe(groups, DedupeOp::Remove, &config, &log);

            let mut out = Vec::new();
            let dedupe_result = log_script(script, &mut out).unwrap();
            assert_eq!(dedupe_result.processed_count, 6);

            let out = String::from_utf8(out).unwrap();
            let out_lines = out.lines().collect_vec();
            assert_eq!(out_lines.len(), 6);
            assert!(out_lines[0].contains("group_1"));
            assert!(out_lines[1].contains("group_1"));
            assert!(out_lines[2].contains("group_2"));
            assert!(out_lines[3].contains("group_2"));
            assert!(out_lines[4].contains("group_3"));
            assert!(out_lines[5].contains("group_3"));
        });
    }

    /// Space is freed only when all hard links of a file are removed, and then only once.
    #[test]
    #[cfg(unix)]
    fn test_reclaimed_space_counts_hard_linked_data_once() {
        with_dir("dedupe/reclaimed_hard_links", |root| {
            let log = StdLog::new();
            let h1 = root.join("h1");
            let h2 = root.join("h2");
            let h3 = root.join("h3");
            let outside = root.join("outside");
            write_file(&h1, "0123456789");
            fs::hard_link(&h1, &h2).unwrap();
            fs::hard_link(&h1, &h3).unwrap();
            let remove = |paths: &[&PathBuf]| -> Vec<(usize, Vec<FsCommand>)> {
                let cmds = paths
                    .iter()
                    .map(|p| FsCommand::Remove {
                        file: PathAndMetadata::new(Path::from(*p)).unwrap(),
                    })
                    .collect();
                vec![(0, cmds)]
            };

            // two of three links: nothing is freed
            let result = log_script(remove(&[&h2, &h3]), Vec::new()).unwrap();
            assert_eq!(result.processed_count, 2);
            assert_eq!(result.reclaimed_space, FileLen(0));
            // all three links: the data is freed once
            let result = log_script(remove(&[&h1, &h2, &h3]), Vec::new()).unwrap();
            assert_eq!(result.reclaimed_space, FileLen(10));
            let result = run_script(remove(&[&h1, &h2, &h3]), false, false, &log);
            assert_eq!(result.processed_count, 3);
            assert_eq!(result.reclaimed_space, FileLen(10));

            // a file with a link outside the script frees nothing
            write_file(&h1, "0123456789");
            fs::hard_link(&h1, &outside).unwrap();
            let result = run_script(remove(&[&h1]), false, false, &log);
            assert_eq!(result.reclaimed_space, FileLen(0));
        })
    }

    #[test]
    fn test_hard_link_merges_subgroups_of_hard_links() {
        with_dir("dedupe/merge_subgroups_of_hardlinks", |root| {
            let mut log = StdLog::new();
            log.no_progress = true;
            log.log_stderr_to_stdout = true;

            let file_a1 = root.join("file_a1");
            let file_a2 = root.join("file_a2");
            let file_b1 = root.join("file_b1");
            let file_b2 = root.join("file_b2");

            write_file(&file_a1, "foo");
            write_file(&file_b1, "foo");

            let file_id = FileId::new(&Path::from(&file_a1)).unwrap();

            fs::hard_link(&file_a1, &file_a2).unwrap();
            fs::hard_link(&file_b1, &file_b2).unwrap();

            let group_config = GroupConfig {
                paths: vec![Path::from(root)],
                ..GroupConfig::default()
            };

            let groups = group_files(&group_config, &log).unwrap().groups;
            let dedupe_config = DedupeConfig::default();
            let script = dedupe(groups, DedupeOp::HardLink, &dedupe_config, &log);
            let dedupe_result = run_script(script, false, false, &log);
            assert_eq!(dedupe_result.processed_count, 2);
            assert!(file_a1.exists());
            assert!(file_a2.exists());
            assert!(file_b1.exists());
            assert!(file_b2.exists());

            assert_eq!(read_file(&file_a1), "foo");

            assert_eq!(FileId::new(&Path::from(&file_a2)).unwrap(), file_id);
            assert_eq!(FileId::new(&Path::from(&file_b1)).unwrap(), file_id);
            assert_eq!(FileId::new(&Path::from(&file_b2)).unwrap(), file_id);
        })
    }

    /// With `--symbolic-links --match-links`, a symlink and its target are separate subgroups.
    /// When the link is kept, its target must not be removed or replaced (that used to delete
    /// the only copy of the data and leave a dangling link).
    #[test]
    #[cfg(unix)]
    fn test_never_drops_target_of_kept_symlink() {
        use std::os::unix::fs;

        for op in [DedupeOp::Remove, DedupeOp::HardLink, DedupeOp::SymbolicLink] {
            with_dir("dedupe/keep_symlink_target", |root| {
                let log = StdLog::new();
                let target = root.join("b");
                let link = root.join("a"); // sorts first, so it is the file kept by default
                write_file(&target, "real data");
                fs::symlink("b", &link).unwrap();

                let group_config = GroupConfig {
                    paths: vec![Path::from(root)],
                    symbolic_links: true,
                    match_links: true,
                    ..GroupConfig::default()
                };
                let groups = group_files(&group_config, &log).unwrap().groups;
                assert_eq!(groups.len(), 1);
                let dedupe_config = DedupeConfig {
                    match_links: true,
                    ..DedupeConfig::default()
                };
                let script = dedupe(groups, op.clone(), &dedupe_config, &log);
                run_script(script, false, false, &log);

                assert_eq!(read_file(&target), "real data", "{op:?}");
                assert_eq!(read_file(&link), "real data", "{op:?}");
            })
        }
    }

    #[test]
    #[cfg(unix)]
    fn test_remove_removes_subgroups_of_soft_links() {
        use std::os::unix::fs;

        with_dir("dedupe/remove_subgroups_with_symlinks", |root| {
            let mut log = StdLog::new();
            log.no_progress = true;
            log.log_stderr_to_stdout = true;

            let file_a1 = root.join("file_a1");
            let file_a2 = root.join("file_a2");
            let file_b1 = root.join("file_b1");
            let file_b2 = root.join("file_b2");

            write_file(&file_a1, "foo");
            write_file(&file_b1, "foo");

            fs::symlink(&file_a1, &file_a2).unwrap();
            fs::symlink(&file_b1, &file_b2).unwrap();

            let group_config = GroupConfig {
                paths: vec![Path::from(root)],
                symbolic_links: true,
                ..GroupConfig::default()
            };

            let groups = group_files(&group_config, &log).unwrap().groups;
            let dedupe_config = DedupeConfig::default();
            let script = dedupe(groups, DedupeOp::Remove, &dedupe_config, &log);
            let dedupe_result = run_script(script, false, false, &log);
            assert_eq!(dedupe_result.processed_count, 2);

            assert!(file_a1.exists());
            assert!(file_a2.exists());
            assert!(!file_b1.exists());
            assert!(!file_b2.exists());

            assert_eq!(read_file(&file_a1), "foo");
            assert_eq!(read_file(&file_a2), "foo");
        })
    }
}
