//! Creating the harness's private output files, the journal and the audit,
//! under one directory policy.
//!
//! The parent directory is walked from `/` through pinned descriptors: each
//! component is opened relative to the previous directory with
//! `O_DIRECTORY | O_NOFOLLOW`, created with mode 0700 when missing, and
//! checked with `fstat`. As with OpenSSH `StrictModes`, every directory must
//! be readable and searchable by the effective user, owned by root or that
//! user, and sticky if group- or world-writable; the final one must be owned
//! by that user and writable by no one else. The file is then created relative
//! to the verified descriptor, so a directory swapped by path after a check
//! cannot redirect it. Whatever one call created is removed again, through
//! the same pinned descriptors, if that call fails.

use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use rustix::io::Errno;
use thiserror::Error;

const GROUP_OR_WORLD_WRITABLE: u32 = 0o022;
const STICKY: u32 = 0o1000;

#[derive(Debug, Error)]
pub(crate) enum OutputError {
    #[error("path {0:?} does not name a file")]
    InvalidPath(PathBuf),
    #[error("directory {path:?} is unsafe: {problem}")]
    UnsafeDirectory {
        path: PathBuf,
        mode: Option<u32>,
        problem: DirectoryProblem,
    },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<Errno> for OutputError {
    fn from(error: Errno) -> Self {
        Self::Io(error.into())
    }
}

/// Why a directory on the way to an output file was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryProblem {
    /// A component is not a directory, or is a symlink.
    NotADirectory,
    /// A directory cannot be opened for reading and searching by the
    /// effective user, which the descriptor walk needs.
    NotReadable,
    /// The output's own directory is not owned by the effective user.
    NotOwned,
    /// The output's own directory is group- or world-writable.
    WritableByOthers,
    /// An ancestor is owned by someone other than root or the effective user.
    AncestorNotOwned,
    /// An ancestor is group- or world-writable without the sticky bit.
    AncestorWritableWithoutSticky,
}

impl DirectoryProblem {
    /// What to do about it, naming the directory and the option that picks
    /// the output path.
    #[must_use]
    pub fn remedy(self, path: &Path, option: &str) -> String {
        let shown = path.display();
        match self {
            Self::WritableByOthers | Self::AncestorWritableWithoutSticky => format!(
                "run `chmod g-w,o-w {shown}`, or pass {option} a path under a private directory"
            ),
            Self::NotReadable => format!(
                "run `chmod u+rx {shown}`, or pass {option} a path whose directories you can read"
            ),
            Self::NotOwned | Self::AncestorNotOwned => {
                format!("pass {option} a path under a directory you own")
            }
            Self::NotADirectory => {
                format!("pass {option} a path whose directories are real directories, not symlinks")
            }
        }
    }
}

impl std::fmt::Display for DirectoryProblem {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotADirectory => "not a directory, or a symlink",
            Self::NotReadable => "not readable and searchable by the current user",
            Self::NotOwned => "not owned by the current user",
            Self::WritableByOthers => "group- or world-writable",
            Self::AncestorNotOwned => "an ancestor not owned by root or the current user",
            Self::AncestorWritableWithoutSticky => {
                "an ancestor that is group- or world-writable without the sticky bit"
            }
        })
    }
}

/// The full message for an unsafe directory, shared by the journal and audit
/// errors.
pub(crate) fn describe_unsafe(
    what: &str,
    option: &str,
    path: &Path,
    mode: Option<u32>,
    problem: DirectoryProblem,
) -> String {
    let mode = mode.map_or_else(
        || "unknown".to_string(),
        |mode| format!("{:04o}", mode & 0o7777),
    );
    format!(
        "{what} directory {} is unsafe: {problem} (mode {mode}); {}",
        path.display(),
        problem.remedy(path, option)
    )
}

/// Refuse a path that does not name a file: an empty path, a trailing `/`,
/// or a final component of `.` or `..`. This looks at the raw bytes, because
/// `Path::file_name` and `Path::parent` normalize a trailing `/` and `/.`
/// away and would turn `name/` into a file called `name`. Repeated
/// separators inside the path and relative paths are fine.
pub(crate) fn validate_file_path(path: &Path) -> Result<(), PathBuf> {
    let bytes = path.as_os_str().as_bytes();
    let last = bytes
        .rsplit(|byte| *byte == b'/')
        .next()
        .unwrap_or_default();
    if bytes.is_empty() || last.is_empty() || last == b"." || last == b".." {
        return Err(path.to_path_buf());
    }
    Ok(())
}

/// What to do when the output file already exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Existing {
    /// Refuse it: journals are never reused.
    Refuse,
    /// Append to it: audits accumulate.
    Append,
}

/// A device and inode, from rustix's `stat` on every side of a comparison.
type Identity = (u64, u64);

#[allow(clippy::unnecessary_cast, clippy::cast_sign_loss)]
fn identity(stat: &rustix::fs::Stat) -> Identity {
    (stat.st_dev as u64, stat.st_ino as u64)
}

struct CreatedEntry {
    parent: OwnedFd,
    name: OsString,
    identity: Identity,
}

/// What one `open_output` call created, so it can be removed again.
pub(crate) struct Rollback {
    file: Option<CreatedEntry>,
    directories: Vec<CreatedEntry>,
}

impl Rollback {
    /// Whether this call created the output file.
    pub(crate) fn created_file(&self) -> bool {
        self.file.is_some()
    }

    /// Remove what the call created: the file if its parent still holds that
    /// same, still empty, regular file, then each created directory, newest
    /// first, if it is still that same directory and empty. Everything goes
    /// through the pinned parent descriptors, never by path.
    ///
    /// Between `fstatat` and `unlinkat` the name could still be replaced, but
    /// only by someone who can write the parent, and the walk verified that
    /// only the operator can. Removal is best-effort; a failure leaves the
    /// entry in place.
    pub(crate) fn undo(self) {
        if let Some(file) = self.file {
            let same_empty_file =
                rustix::fs::statat(&file.parent, &file.name, AtFlags::SYMLINK_NOFOLLOW).is_ok_and(
                    |stat| {
                        identity(&stat) == file.identity
                            && stat.st_size == 0
                            && FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                    },
                );
            if same_empty_file {
                let _ = rustix::fs::unlinkat(&file.parent, &file.name, AtFlags::empty());
            }
        }
        for directory in self.directories.into_iter().rev() {
            let same_directory = rustix::fs::statat(
                &directory.parent,
                &directory.name,
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .is_ok_and(|stat| {
                identity(&stat) == directory.identity
                    && FileType::from_raw_mode(stat.st_mode) == FileType::Directory
            });
            if same_directory {
                // REMOVEDIR refuses a directory that is no longer empty.
                let _ =
                    rustix::fs::unlinkat(&directory.parent, &directory.name, AtFlags::REMOVEDIR);
            }
        }
    }
}

/// Open an output file under the directory policy, creating missing parent
/// directories with mode 0700 and a new file with mode 0600. Modes are set
/// explicitly after creation, so the umask cannot decide them; on Linux that
/// also resets the POSIX ACL mask, which limits what a default ACL grants.
pub(crate) fn open_output(
    path: &Path,
    existing: Existing,
) -> Result<(File, Rollback), OutputError> {
    validate_file_path(path).map_err(OutputError::InvalidPath)?;
    let invalid = || OutputError::InvalidPath(path.to_path_buf());
    let name = path.file_name().ok_or_else(invalid)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let start = resolved_directory(parent).map_err(|error| match error {
        OutputError::InvalidPath(_) => invalid(),
        other => other,
    })?;
    let mut rollback = Rollback {
        file: None,
        directories: Vec::new(),
    };
    let result = create_in(&start, name, existing, &mut rollback);
    match result {
        Ok(file) => Ok((file, rollback)),
        Err(error) => {
            rollback.undo();
            Err(error)
        }
    }
}

fn create_in(
    parent: &Path,
    name: &OsStr,
    existing: Existing,
    rollback: &mut Rollback,
) -> Result<File, OutputError> {
    let directory = open_verified_directory(parent, &mut rollback.directories)?;
    let mut flags =
        OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::WRONLY | OFlags::CLOEXEC;
    if existing == Existing::Append {
        flags |= OFlags::APPEND;
    }
    match rustix::fs::openat(&directory, name, flags, Mode::from_raw_mode(0o600)) {
        Ok(fd) => {
            let stat = rustix::fs::fstat(&fd)?;
            rollback.file = Some(CreatedEntry {
                parent: directory,
                name: name.to_os_string(),
                identity: identity(&stat),
            });
            rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o600))?;
            Ok(File::from(fd))
        }
        Err(Errno::EXIST) if existing == Existing::Append => {
            let fd = rustix::fs::openat(
                &directory,
                name,
                OFlags::WRONLY | OFlags::APPEND | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )?;
            let stat = rustix::fs::fstat(&fd)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
                return Err(std::io::Error::other("existing output is not a regular file").into());
            }
            Ok(File::from(fd))
        }
        Err(error) => Err(error.into()),
    }
}

/// An absolute starting path for the descriptor walk: the deepest existing
/// ancestor, canonicalized once, followed by the components that do not exist
/// yet. Those name nothing on disk, so `.` and `..` among them resolve
/// lexically. The walk itself re-resolves every component without following
/// links, so this path is only a starting point.
fn resolved_directory(parent: &Path) -> Result<PathBuf, OutputError> {
    let absolute = std::path::absolute(parent)?;
    let mut existing = absolute.clone();
    let mut rest = Vec::new();
    let mut resolved = loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            break canonical;
        }
        let Some(last) = existing.components().next_back() else {
            return Err(OutputError::InvalidPath(parent.to_path_buf()));
        };
        rest.push(last.as_os_str().to_os_string());
        if !existing.pop() {
            return Err(OutputError::InvalidPath(parent.to_path_buf()));
        }
    };
    for component in rest.iter().rev() {
        match Path::new(component).components().next() {
            Some(Component::CurDir) => {}
            Some(Component::ParentDir) => {
                resolved.pop();
            }
            _ => resolved.push(component),
        }
    }
    Ok(resolved)
}

/// Walk `directory` from `/` through pinned descriptors, creating missing
/// components and recording each one created, and return the verified final
/// directory.
fn open_verified_directory(
    directory: &Path,
    created: &mut Vec<CreatedEntry>,
) -> Result<OwnedFd, OutputError> {
    let euid = rustix::process::geteuid().as_raw();
    let walk = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut shown = PathBuf::from("/");
    let mut current = rustix::fs::open("/", walk, Mode::empty())
        .map_err(|error| walk_error(&shown, None, error))?;
    let components = directory
        .components()
        .filter_map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect::<Vec<_>>();
    check_directory(&current, &shown, euid, components.is_empty())?;
    for (position, name) in components.iter().enumerate() {
        shown.push(name);
        let next = match rustix::fs::openat(&current, *name, walk, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => {
                let made = match rustix::fs::mkdirat(&current, *name, Mode::from_raw_mode(0o700)) {
                    Ok(()) => true,
                    Err(Errno::EXIST) => false,
                    Err(error) => return Err(error.into()),
                };
                let fd = rustix::fs::openat(&current, *name, walk, Mode::empty())
                    .map_err(|error| walk_error(&shown, Some((&current, *name)), error))?;
                if made {
                    let stat = rustix::fs::fstat(&fd)?;
                    created.push(CreatedEntry {
                        parent: current.try_clone()?,
                        name: name.to_os_string(),
                        identity: identity(&stat),
                    });
                    // Set the mode explicitly rather than trusting the umask.
                    rustix::fs::fchmod(&fd, Mode::from_raw_mode(0o700))?;
                }
                fd
            }
            Err(error) => return Err(walk_error(&shown, Some((&current, *name)), error)),
        };
        check_directory(&next, &shown, euid, position + 1 == components.len())?;
        current = next;
    }
    Ok(current)
}

/// Turn a failed directory open into a typed refusal where it is one.
fn walk_error(path: &Path, entry: Option<(&OwnedFd, &OsStr)>, error: Errno) -> OutputError {
    let mode = || {
        entry.and_then(|(parent, name)| {
            rustix::fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                .ok()
                .map(|stat| u32::from(stat.st_mode))
        })
    };
    let problem = match error {
        Errno::NOTDIR | Errno::LOOP => DirectoryProblem::NotADirectory,
        Errno::ACCESS => DirectoryProblem::NotReadable,
        error => return error.into(),
    };
    OutputError::UnsafeDirectory {
        path: path.to_path_buf(),
        mode: mode(),
        problem,
    }
}

/// Apply the ownership and mode rules to one open directory.
fn check_directory(
    directory: &OwnedFd,
    path: &Path,
    euid: u32,
    is_final: bool,
) -> Result<(), OutputError> {
    let stat = rustix::fs::fstat(directory)?;
    let mode = u32::from(stat.st_mode);
    let owner = stat.st_uid;
    let problem = if is_final {
        if owner != euid {
            Some(DirectoryProblem::NotOwned)
        } else if mode & GROUP_OR_WORLD_WRITABLE != 0 {
            Some(DirectoryProblem::WritableByOthers)
        } else {
            None
        }
    } else if owner != 0 && owner != euid {
        Some(DirectoryProblem::AncestorNotOwned)
    } else if mode & GROUP_OR_WORLD_WRITABLE != 0 && mode & STICKY == 0 {
        Some(DirectoryProblem::AncestorWritableWithoutSticky)
    } else {
        None
    };
    match problem {
        Some(problem) => Err(OutputError::UnsafeDirectory {
            path: path.to_path_buf(),
            mode: Some(mode),
            problem,
        }),
        None => Ok(()),
    }
}
