// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! Whole-filesystem capacity (total / free), via `statvfs`. Used for the
//! capacity ring on the overview — distinct from the *scanned folder's* size.

use std::path::Path;

/// Capacity of the filesystem a path lives on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiskUsage {
    /// Total bytes on the filesystem.
    pub total: u64,
    /// Bytes available to an unprivileged user.
    pub available: u64,
}

impl DiskUsage {
    /// Bytes in use (total minus available).
    pub fn used(&self) -> u64 {
        self.total.saturating_sub(self.available)
    }
}

/// Query the filesystem capacity for `path`, or `None` if it can't be read.
#[cfg(unix)]
pub fn disk_usage(path: &Path) -> Option<DiskUsage> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` only reads through the valid C string and writes into
    // the zeroed struct; we check the return code before using it.
    unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        if libc::statvfs(c_path.as_ptr(), &mut stat) != 0 {
            return None;
        }
        // Prefer the fragment size; fall back to the block size if it's unset.
        let unit = if stat.f_frsize > 0 { stat.f_frsize } else { stat.f_bsize } as u64;
        Some(DiskUsage {
            total: stat.f_blocks as u64 * unit,
            available: stat.f_bavail as u64 * unit,
        })
    }
}

/// Capacity is unavailable on non-unix targets.
#[cfg(not(unix))]
pub fn disk_usage(_path: &Path) -> Option<DiskUsage> {
    None
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn root_filesystem_reports_sane_capacity() {
        let usage = disk_usage(Path::new("/")).expect("root filesystem should be queryable");
        assert!(usage.total > 0, "total capacity should be positive");
        assert!(usage.available <= usage.total, "available cannot exceed total");
        assert!(usage.used() <= usage.total, "used cannot exceed total");
    }

    #[test]
    fn missing_path_returns_none() {
        assert!(disk_usage(Path::new("/no/such/path/diskscope")).is_none());
    }
}
