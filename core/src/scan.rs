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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

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

/// Live, thread-safe progress of an in-flight [`scan`], so a long walk can show
/// its heartbeat instead of a blind spinner.
///
/// The walker bumps these counters as it goes; the UI reads them on a timer.
/// Counters use relaxed atomics (the displayed figures are advisory, not a
/// synchronisation point) and the "current path" is updated only per directory,
/// so reporting adds negligible overhead to the walk.
#[derive(Debug, Default)]
pub struct ScanProgress {
    items: AtomicU64,
    bytes: AtomicU64,
    current: Mutex<PathBuf>,
}

impl ScanProgress {
    /// A fresh, zeroed progress handle.
    pub fn new() -> Self {
        Self::default()
    }

    /// Entries (files **and** directories) visited so far.
    pub fn items(&self) -> u64 {
        self.items.load(Ordering::Relaxed)
    }

    /// Bytes totalled so far (hardlink-deduplicated, like the final tree).
    pub fn bytes(&self) -> u64 {
        self.bytes.load(Ordering::Relaxed)
    }

    /// The directory the walk is currently reading, or empty before it starts.
    pub fn current(&self) -> PathBuf {
        self.current.lock().expect("progress path mutex poisoned").clone()
    }

    fn record(&self, bytes: u64) {
        self.items.fetch_add(1, Ordering::Relaxed);
        if bytes > 0 {
            self.bytes.fetch_add(bytes, Ordering::Relaxed);
        }
    }

    fn enter(&self, dir: &Path) {
        if let Ok(mut current) = self.current.lock() {
            current.clear();
            current.push(dir);
        }
    }
}

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
    scan_reporting(root, cancel, &ScanProgress::new())
}

/// Like [`scan_cancellable`], but reports live progress into `progress` as it
/// walks, so the UI can show a running item count, byte total, and current path.
pub fn scan_reporting(
    root: impl AsRef<Path>,
    cancel: &CancelFlag,
    progress: &ScanProgress,
) -> std::io::Result<Node> {
    let root = root.as_ref();
    let meta = fs::symlink_metadata(root)?;
    // Inodes already counted, so multiply-hardlinked files aren't double-counted.
    let seen = Seen::default();
    // Run the walk on the bounded, de-prioritised pool rather than rayon's global
    // (all-cores) pool — see [`scan_pool`] for why a metadata-I/O-bound walk must
    // not be handed every core.
    let node =
        scan_pool().install(|| build(root.to_path_buf(), &meta, &seen, cancel, progress));
    if cancel.is_cancelled() {
        return Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "scan cancelled"));
    }
    Ok(node)
}

/// A dedicated, **bounded and de-prioritised** thread pool for the walk.
///
/// The walk is dominated by `readdir`/`stat` syscalls, not computation. Handing
/// it rayon's global (one-thread-per-core) pool just thrashes the disk with
/// concurrent seeks and pins every core with threads that are mostly blocked on
/// the filesystem — which starves the desktop and makes the whole machine feel
/// frozen during a big scan. So we:
///
/// - **cap the thread count** well below the core count (metadata I/O does not
///   speed up past a handful of concurrent readers, and on a spinning disk more
///   readers is *slower*), and
/// - **drop each worker's CPU and I/O priority** so the scan only gets resources
///   the rest of the system isn't using.
///
/// Built once and reused for every scan.
fn scan_pool() -> &'static rayon::ThreadPool {
    static POOL: OnceLock<rayon::ThreadPool> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads =
            std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(1, 8);
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .thread_name(|i| format!("diskscope-scan-{i}"))
            .start_handler(|_| lower_thread_priority())
            .build()
            .expect("build scan thread pool")
    })
}

/// Lower the calling worker thread's scheduling and I/O priority so a scan yields
/// to interactive work. Linux applies both per-thread, so doing this in each
/// pool worker's start handler de-prioritises the whole walk without touching the
/// UI thread (which lives in another process thread entirely).
fn lower_thread_priority() {
    #[cfg(unix)]
    unsafe {
        // CPU: be nice to everything else. On Linux the nice value is per-thread.
        let _ = libc::nice(10);
    }
    #[cfg(target_os = "linux")]
    unsafe {
        // I/O: idle class — take disk time only when nothing else wants it.
        // ioprio_set(IOPRIO_WHO_PROCESS = 1, who = 0 = self,
        //            IOPRIO_PRIO_VALUE(IOPRIO_CLASS_IDLE = 3, 0) = 3 << 13).
        let _ = libc::syscall(
            libc::SYS_ioprio_set,
            1 as libc::c_long,
            0 as libc::c_long,
            (3 << 13) as libc::c_long,
        );
    }
}

/// Find every entry whose name contains `needle` (case-insensitive) and whose
/// size is at least `min_size`, anywhere in the in-memory tree, largest-first.
///
/// Both files and directories can match (so a search for `node_modules` finds
/// the directory, reporting its recursive size). The `root` itself is never a
/// match. An empty `needle` returns nothing. Operates purely on the already
/// scanned tree — no disk I/O — so it's instant and safe to call on every
/// keystroke.
pub fn search<'a>(root: &'a Node, needle: &str, min_size: u64) -> Vec<&'a Node> {
    let mut out = Vec::new();
    let needle = needle.to_lowercase();
    if !needle.is_empty() {
        for child in &root.children {
            search_into(child, &needle, min_size, &mut out);
        }
    }
    out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    out
}

fn search_into<'a>(node: &'a Node, needle: &str, min_size: u64, out: &mut Vec<&'a Node>) {
    if node.size >= min_size && node.name.to_lowercase().contains(needle) {
        out.push(node);
    }
    for child in &node.children {
        search_into(child, needle, min_size, out);
    }
}

/// Remove the entry at `target` from an in-memory tree, returning the bytes
/// freed, and decrement every ancestor's cached `size` by that amount so the
/// tree stays self-consistent **without a rescan**.
///
/// This is what lets the UI react to a delete or trash instantly: the full sized
/// tree is already in memory, so dropping one node and fixing up the sizes along
/// its path is O(depth) — there is no need to re-walk the disk to learn what the
/// user just removed.
///
/// Returns `None` (leaving the tree untouched) if `target` is not inside `node`
/// — e.g. it lives outside the scanned root. An exact match on a direct child
/// wins over descending, so a directory is removed whole rather than recursed
/// into.
pub fn remove(node: &mut Node, target: &Path) -> Option<u64> {
    if let Some(index) = node.children.iter().position(|c| c.path == target) {
        let freed = node.children.remove(index).size;
        node.size -= freed;
        return Some(freed);
    }
    let child = node.children.iter_mut().find(|c| target.starts_with(&c.path))?;
    let freed = remove(child, target)?;
    node.size -= freed;
    Some(freed)
}

fn build(
    path: PathBuf,
    meta: &fs::Metadata,
    seen: &Seen,
    cancel: &CancelFlag,
    progress: &ScanProgress,
) -> Node {
    let name = display_name(&path);

    // Cancelled: stop descending and unwind as a cheap, zero-size stub. The
    // top-level caller turns the set flag into an `Interrupted` error, so these
    // stubs are never surfaced as a real (under-counted) tree.
    if cancel.is_cancelled() {
        return Node { name, path, size: 0, is_dir: meta.is_dir(), children: Vec::new() };
    }

    if !meta.is_dir() {
        // Files and symlinks are leaves; count their own length only.
        let size = leaf_size(meta, seen);
        progress.record(size);
        return Node { name, path, size, is_dir: false, children: Vec::new() };
    }

    progress.enter(&path);
    progress.record(0); // the directory itself is one visited entry

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
        .map(|(child_path, child_meta)| build(child_path, &child_meta, seen, cancel, progress))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let size = children.iter().map(|c| c.size).sum();
        Node { name: name.into(), path: name.into(), size, is_dir: true, children }
    }
    fn file(name: &str, size: u64) -> Node {
        Node { name: name.into(), path: name.into(), size, is_dir: false, children: Vec::new() }
    }

    #[test]
    fn remove_prunes_and_decrements_every_ancestor() {
        // root(150) → proj(150) → [ big.bin(100), src(50) → main.rs(50) ]
        let mut root = dir(
            "root",
            vec![dir(
                "proj",
                vec![
                    file("proj/big.bin", 100),
                    dir("proj/src", vec![file("proj/src/main.rs", 50)]),
                ],
            )],
        );
        assert_eq!(root.size, 150);

        // Removing a nested file reports the bytes freed and shrinks every
        // ancestor by exactly that much — no rescan involved.
        assert_eq!(remove(&mut root, Path::new("proj/big.bin")), Some(100));
        assert_eq!(root.size, 50);
        assert_eq!(root.children[0].size, 50, "proj total drops by the freed file");
        assert_eq!(root.children[0].children.len(), 1, "the file node is gone");

        // A path outside the tree is a no-op, and the tree is left intact.
        assert_eq!(remove(&mut root, Path::new("elsewhere")), None);
        assert_eq!(root.size, 50);

        // A directory target is removed whole (its full recursive size frees).
        assert_eq!(remove(&mut root, Path::new("proj/src")), Some(50));
        assert_eq!(root.size, 0);
        assert!(root.children[0].children.is_empty(), "proj is now empty");
    }

    #[test]
    fn search_matches_by_name_and_size_largest_first() {
        let root = dir(
            "root",
            vec![
                dir("node_modules", vec![file("dep.js", 800)]),
                dir("Downloads", vec![file("ubuntu.iso", 5000), file("notes.txt", 5)]),
            ],
        );

        // Case-insensitive substring over the whole tree, largest-first.
        let hits = search(&root, "ISO", 0);
        let names: Vec<&str> = hits.iter().map(|n| n.name.as_str()).collect();
        assert_eq!(names, ["ubuntu.iso"], "matches the .iso file by substring");

        // A directory matches too (reporting its recursive size).
        let hits = search(&root, "node", 0);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "node_modules");
        assert_eq!(hits[0].size, 800);

        // The size floor filters out small matches: "u" is in both ubuntu.iso
        // (5000) and node_modules' subtree, but only the big one clears 1000.
        let hits = search(&root, "u", 1000);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "ubuntu.iso");

        // An empty needle matches nothing; the root itself is never a hit.
        assert!(search(&root, "", 0).is_empty());
        assert!(search(&root, "root", 0).is_empty());
    }
}
