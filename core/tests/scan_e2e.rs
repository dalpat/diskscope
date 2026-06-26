// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! End-to-end tests for the scan engine against real temporary directory trees.
//!
//! Each test builds an actual directory on disk, scans it, and asserts on the
//! resulting tree. No mocks, no display server — just the public library API.

use std::fs;
use std::io::Write;
use std::path::Path;

use diskscope::scan::{scan, Node};
use tempfile::tempdir;

/// Write a file of exactly `bytes` bytes at `path`.
fn write_file(path: &Path, bytes: usize) {
    let mut f = fs::File::create(path).unwrap();
    f.write_all(&vec![b'x'; bytes]).unwrap();
}

/// Find a direct child by name, or panic with a helpful message.
fn child<'a>(node: &'a Node, name: &str) -> &'a Node {
    node.children
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("expected child {name:?}, found {:?}", names(node)))
}

fn names(node: &Node) -> Vec<&str> {
    node.children.iter().map(|c| c.name.as_str()).collect()
}

/// Build the canonical fixture tree used by several tests:
///
/// ```text
/// root/
///   a.txt      100 B
///   big.bin   5000 B
///   .hidden     50 B
///   sub/
///     b.txt    300 B
///     c.txt    200 B
///   empty/
/// ```
fn fixture(root: &Path) {
    write_file(&root.join("a.txt"), 100);
    write_file(&root.join("big.bin"), 5000);
    write_file(&root.join(".hidden"), 50);
    fs::create_dir(root.join("sub")).unwrap();
    write_file(&root.join("sub/b.txt"), 300);
    write_file(&root.join("sub/c.txt"), 200);
    fs::create_dir(root.join("empty")).unwrap();
}

#[test]
fn aggregates_sizes_recursively() {
    let dir = tempdir().unwrap();
    fixture(dir.path());

    let tree = scan(dir.path()).unwrap();

    assert!(tree.is_dir);
    // 100 + 5000 + 50 + (300 + 200) + 0
    assert_eq!(tree.size, 5650);
    assert_eq!(child(&tree, "sub").size, 500);
}

#[test]
fn children_sorted_largest_first() {
    let dir = tempdir().unwrap();
    fixture(dir.path());

    let tree = scan(dir.path()).unwrap();

    // big.bin (5000) > sub (500) > a.txt (100) > .hidden (50) > empty (0)
    assert_eq!(names(&tree), vec!["big.bin", "sub", "a.txt", ".hidden", "empty"]);
}

#[test]
fn hidden_files_are_counted() {
    let dir = tempdir().unwrap();
    fixture(dir.path());

    let tree = scan(dir.path()).unwrap();

    assert_eq!(child(&tree, ".hidden").size, 50);
}

#[test]
fn empty_directory_has_zero_size_and_no_children() {
    let dir = tempdir().unwrap();
    fixture(dir.path());

    let tree = scan(dir.path()).unwrap();
    let empty = child(&tree, "empty");

    assert!(empty.is_dir);
    assert_eq!(empty.size, 0);
    assert!(empty.children.is_empty());
}

#[test]
fn scanning_a_single_file_returns_a_leaf() {
    let dir = tempdir().unwrap();
    let file = dir.path().join("solo.dat");
    write_file(&file, 1234);

    let node = scan(&file).unwrap();

    assert!(!node.is_dir);
    assert_eq!(node.size, 1234);
    assert_eq!(node.name, "solo.dat");
    assert!(node.children.is_empty());
}

#[test]
fn missing_path_is_an_error() {
    let dir = tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");

    assert!(scan(&missing).is_err());
}

#[test]
fn ties_break_by_name_for_stable_order() {
    let dir = tempdir().unwrap();
    write_file(&dir.path().join("zebra.txt"), 10);
    write_file(&dir.path().join("apple.txt"), 10);
    write_file(&dir.path().join("mango.txt"), 10);

    let tree = scan(dir.path()).unwrap();

    // Equal sizes → alphabetical, deterministically.
    assert_eq!(names(&tree), vec!["apple.txt", "mango.txt", "zebra.txt"]);
}

#[cfg(unix)]
#[test]
fn hardlinks_are_counted_once() {
    let dir = tempdir().unwrap();
    write_file(&dir.path().join("original.bin"), 4000);
    // A second name for the very same inode.
    fs::hard_link(dir.path().join("original.bin"), dir.path().join("clone.bin")).unwrap();

    let tree = scan(dir.path()).unwrap();

    // The shared 4000 bytes are counted once, not 8000.
    assert_eq!(tree.size, 4000);

    // One link carries the real size; the other reports 0. Which is which
    // depends on directory iteration order, so assert the multiset.
    let mut sizes: Vec<u64> = tree.children.iter().map(|c| c.size).collect();
    sizes.sort_unstable();
    assert_eq!(sizes, vec![0, 4000]);
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed() {
    use std::os::unix::fs::symlink;

    let dir = tempdir().unwrap();
    let target_dir = dir.path().join("real");
    fs::create_dir(&target_dir).unwrap();
    write_file(&target_dir.join("payload.bin"), 9000);

    // A symlink pointing at the heavy directory.
    symlink(&target_dir, dir.path().join("link")).unwrap();

    let tree = scan(dir.path()).unwrap();
    let link = child(&tree, "link");

    // The link must be a leaf, and must NOT inherit the 9000-byte payload.
    assert!(!link.is_dir);
    assert!(link.children.is_empty());
    assert!(link.size < 9000, "symlink size {} should not include target", link.size);

    // The real directory is still counted exactly once.
    assert_eq!(child(&tree, "real").size, 9000);
}
