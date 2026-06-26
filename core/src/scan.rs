// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! The disk-usage scan engine: walk a directory tree and total up sizes.
//!
//! The walk is **parallel** (via rayon) so a large, often near-full tree
//! overlaps its many `stat` calls across cores instead of serialising them, and
//! **cancellable** via a [`CancelFlag`] so a slow scan can be abandoned from the
//! UI without waiting for it to finish.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use rayon::prelude::*;

/// A shared, cheaply-clonable flag for cancelling an in-progress [`scan`].
///
/// All clones share one underlying state, so the UI can hold one clone, hand
/// another to the scanning worker thread, and cancel the scan by flipping the
/// flag the worker is polling. A cancelled scan returns
/// [`std::io::ErrorKind::Interrupted`].
#[derive(Debug, Clone, Default)]
pub struct CancelFlag(Arc<AtomicBool>);

impl CancelFlag {
    /// A fresh flag that is not yet cancelled.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Observable through every clone.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

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

/// Shared inode set used to de-duplicate hardlinks across parallel walkers.
///
/// Only files with more than one link ever touch the lock, so the common case
/// (single-link files) stays contention-free.
type Seen = Mutex<HashSet<(u64, u64)>>;

/// Scan `root` and return its tree, with every directory's children sorted
/// largest-first.
///
/// Returns an error only if `root` itself cannot be read. Unreadable entries
/// encountered *inside* the tree (e.g. permission denied) are silently skipped
/// rather than aborting the whole scan — a partial answer beats no answer.
pub fn scan(root: impl AsRef<Path>) -> std::io::Result<Node> {
    scan_cancellable(root, &CancelFlag::new())
}

/// Scan `root`, polling `cancel` throughout so the walk can be abandoned early.
///
/// Behaves exactly like [`scan`] when the flag is never set. If the flag is
/// (or becomes) cancelled, the walk unwinds quickly and the call returns
/// [`std::io::ErrorKind::Interrupted`] instead of a partial tree, so callers
/// never mistake an aborted scan for a real result.
pub fn scan_cancellable(root: impl AsRef<Path>, cancel: &CancelFlag) -> std::io::Result<Node> {
    let root = root.as_ref();
    let meta = fs::symlink_metadata(root)?;
    // Inodes already counted, so multiply-hardlinked files aren't double-counted.
    let seen = Seen::default();
    let node = build(root.to_path_buf(), &meta, &seen, cancel);
    if cancel.is_cancelled() {
        return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "scan cancelled"));
    }
    Ok(node)
}

fn build(path: PathBuf, meta: &fs::Metadata, seen: &Seen, cancel: &CancelFlag) -> Node {
    let name = display_name(&path);

    // Cancelled: stop descending and unwind as a cheap, zero-size stub. The
    // top-level caller turns the set flag into an `Interrupted` error, so these
    // stubs are never surfaced as a real (under-counted) tree.
    if cancel.is_cancelled() {
        return Node { name, path, size: 0, is_dir: meta.is_dir(), children: Vec::new() };
    }

    if !meta.is_dir() {
        // Files and symlinks are leaves; count their own length only.
        return Node { name, path, size: leaf_size(meta, seen), is_dir: false, children: Vec::new() };
    }

    // Read this directory's entries up front (checking for cancellation so even a
    // single huge directory stays interruptible), then build their subtrees in
    // parallel. symlink_metadata so we never traverse into symlinked targets.
    let mut entries: Vec<(PathBuf, fs::Metadata)> = Vec::new();
    if let Ok(read_dir) = fs::read_dir(&path) {
        for entry in read_dir.flatten() {
            if cancel.is_cancelled() {
                break;
            }
            let child_path = entry.path();
            if let Ok(child_meta) = fs::symlink_metadata(&child_path) {
                entries.push((child_path, child_meta));
            }
        }
    }

    let mut children: Vec<Node> = entries
        .into_par_iter()
        .map(|(child_path, child_meta)| build(child_path, &child_meta, seen, cancel))
        .collect();

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
///
/// Under the parallel walk, *which* of several hardlinks wins the real size is
/// unspecified (it races on the shared set), but the total is always counted
/// once — matching `du` and the engine's pre-parallel behaviour.
#[cfg(unix)]
fn leaf_size(meta: &fs::Metadata, seen: &Seen) -> u64 {
    use std::os::unix::fs::MetadataExt;
    // Only regular files share inodes meaningfully here; symlink metadata
    // reports the link itself (is_file() == false), so links are never deduped.
    if meta.file_type().is_file() && meta.nlink() > 1 {
        let mut seen = seen.lock().expect("inode set mutex poisoned");
        if !seen.insert((meta.dev(), meta.ino())) {
            return 0; // this inode's bytes were already counted elsewhere in the tree
        }
    }
    meta.len()
}

/// Hardlink de-duplication needs inode numbers, unavailable off Unix.
#[cfg(not(unix))]
fn leaf_size(meta: &fs::Metadata, _seen: &Seen) -> u64 {
    meta.len()
}

/// The label to show for a path: its final component, or the whole path when
/// there is no final component (e.g. the filesystem root `/`).
fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}
