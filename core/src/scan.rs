//! The disk-usage scan engine: walk a directory tree and total up sizes.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

/// A node in a scanned directory tree.
///
/// `size` is the apparent size in bytes: for a file it is the file length, for
/// a directory it is the recursive sum of its children's sizes.
///
/// Symlinks are **never followed** — they are recorded as leaf nodes with their
/// own (small) link size. This keeps scanning cycle-free and avoids
/// double-counting data that lives elsewhere in the tree.
///
/// Hardlinked files are counted **once**: the first link encountered carries the
/// real size, every later link to the same inode is recorded with size 0 so its
/// shared data is never double-counted (matching `du`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Final path component (e.g. `Videos`), or the full path for filesystem roots.
    pub name: String,
    /// Absolute or user-supplied path to this entry.
    pub path: PathBuf,
    /// Apparent size in bytes (recursive for directories).
    pub size: u64,
    /// Whether this entry is a directory.
    pub is_dir: bool,
    /// Direct children, sorted largest-first. Always empty for files.
    pub children: Vec<Node>,
}

/// Scan `root` and return its tree, with every directory's children sorted
/// largest-first.
///
/// Returns an error only if `root` itself cannot be read. Unreadable entries
/// encountered *inside* the tree (e.g. permission denied) are silently skipped
/// rather than aborting the whole scan — a partial answer beats no answer.
pub fn scan(root: impl AsRef<Path>) -> std::io::Result<Node> {
    let root = root.as_ref();
    let meta = fs::symlink_metadata(root)?;
    // Inodes already counted, so multiply-hardlinked files aren't double-counted.
    let mut seen = HashSet::new();
    Ok(build(root.to_path_buf(), &meta, &mut seen))
}

fn build(path: PathBuf, meta: &fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> Node {
    let name = display_name(&path);

    if !meta.is_dir() {
        // Files and symlinks are leaves; count their own length only.
        return Node { name, path, size: leaf_size(meta, seen), is_dir: false, children: Vec::new() };
    }

    let mut children: Vec<Node> = match fs::read_dir(&path) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let child_path = entry.path();
                // symlink_metadata so we never traverse into symlinked targets.
                let child_meta = fs::symlink_metadata(&child_path).ok()?;
                Some(build(child_path, &child_meta, seen))
            })
            .collect(),
        Err(_) => Vec::new(), // unreadable directory → treat as empty
    };

    // Largest first; ties broken by name for stable, predictable ordering.
    children.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    let size = children.iter().map(|c| c.size).sum();

    Node { name, path, size, is_dir: true, children }
}

/// The size to attribute to a leaf, de-duplicating hardlinks.
///
/// A regular file with more than one hardlink is counted only the first time its
/// inode is seen; subsequent links report 0 so their shared data isn't counted
/// twice. Symlinks and singly-linked files always report their own length.
#[cfg(unix)]
fn leaf_size(meta: &fs::Metadata, seen: &mut HashSet<(u64, u64)>) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // Only regular files share inodes meaningfully here; symlink metadata
    // reports the link itself (is_file() == false), so links are never deduped.
    if meta.file_type().is_file() && meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
        return 0; // this inode's bytes were already counted elsewhere in the tree
    }
    meta.len()
}

/// Hardlink de-duplication needs inode numbers, unavailable off Unix.
#[cfg(not(unix))]
fn leaf_size(meta: &fs::Metadata, _seen: &mut HashSet<(u64, u64)>) -> u64 {
    meta.len()
}

/// The label to show for a path: its final component, or the whole path when
/// there is no final component (e.g. the filesystem root `/`).
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}
