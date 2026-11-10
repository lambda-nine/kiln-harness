//! Creating the harness's private output files: the journal and the audit.

use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

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
