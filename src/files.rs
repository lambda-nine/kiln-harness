//! Opening operator-supplied input files without blocking or following a
//! final symlink.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use rustix::fs::{Mode, OFlags};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum InputFileError {
    /// A FIFO, device, directory, socket, or a symlink as the final
    /// component.
    #[error("not a regular file (a symlink as the last path component is refused)")]
    NotRegularFile,
    #[error("larger than {limit} bytes")]
    TooLarge { limit: usize },
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// Open `path` read-only as a regular file.
///
/// `O_NONBLOCK` keeps a FIFO or device from blocking the open itself, and
/// `O_NOFOLLOW` refuses a symlink as the last component. `fstat` on the
/// open descriptor then decides, so no path can be swapped in between, and
/// only a regular file is made blocking again.
///
/// # Errors
///
/// Returns `NotRegularFile` for anything but a regular file, and an I/O
/// error when the path cannot be opened.
pub fn open_regular_file(path: &Path) -> Result<File, InputFileError> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| match error {
        rustix::io::Errno::LOOP => InputFileError::NotRegularFile,
        error => std::io::Error::from(error).into(),
    })?;
    let stat = rustix::fs::fstat(&fd).map_err(std::io::Error::from)?;
    if rustix::fs::FileType::from_raw_mode(stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(InputFileError::NotRegularFile);
    }
    let flags = rustix::fs::fcntl_getfl(&fd).map_err(std::io::Error::from)?;
    rustix::fs::fcntl_setfl(&fd, flags - OFlags::NONBLOCK).map_err(std::io::Error::from)?;
    Ok(File::from(fd))
}

/// Read a regular file of at most `limit` bytes, opened as
/// [`open_regular_file`] does. A file whose size is over the limit is
/// refused before any read, and at most `limit + 1` bytes are ever read, so
/// a file that grows after the check is refused too.
///
/// # Errors
///
/// Returns the open's errors, `TooLarge`, or an I/O error.
pub fn read_regular_file(path: &Path, limit: usize) -> Result<Vec<u8>, InputFileError> {
    let file = open_regular_file(path)?;
    if file.metadata()?.len() > limit as u64 {
        return Err(InputFileError::TooLarge { limit });
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(InputFileError::TooLarge { limit });
    }
    Ok(bytes)
}
