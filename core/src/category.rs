// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! Classify files into storage categories (Videos, Audio, Images, …) and
//! aggregate usage per category — the data behind the storage overview.

use std::collections::HashMap;

use crate::scan::Node;

/// A storage category, by file type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Category {
    Videos,
    Audio,
    Images,
    Documents,
    Archives,
    Code,
    Applications,
    Other,
}

impl Category {
    /// All categories, in a stable order.
    pub const ALL: [Category; 8] = [
        Category::Videos,
        Category::Audio,
        Category::Images,
        Category::Documents,
        Category::Archives,
        Category::Code,
        Category::Applications,
        Category::Other,
    ];

    /// Human-readable name.
    pub fn label(self) -> &'static str {
        match self {
            Category::Videos => "Videos",
            Category::Audio => "Audio",
            Category::Images => "Images",
            Category::Documents => "Documents",
            Category::Archives => "Archives",
            Category::Code => "Code",
            Category::Applications => "Applications",
            Category::Other => "Other",
        }
    }

    /// Map a lowercased file extension to a category.
    pub fn from_extension(ext: &str) -> Category {
        match ext {
            "mp4" | "mkv" | "mov" | "avi" | "webm" | "flv" | "wmv" | "m4v" | "mpg" | "mpeg"
            | "3gp" | "ts" => Category::Videos,
            "mp3" | "flac" | "wav" | "ogg" | "m4a" | "aac" | "wma" | "opus" | "aiff" | "mid" => {
                Category::Audio
            }
            "jpg" | "jpeg" | "png" | "gif" | "bmp" | "webp" | "svg" | "heic" | "heif" | "tiff"
            | "tif" | "raw" | "cr2" | "nef" | "ico" | "psd" => Category::Images,
            "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "odt" | "ods" | "odp"
            | "txt" | "md" | "rtf" | "epub" | "csv" | "tex" => Category::Documents,
            "zip" | "tar" | "gz" | "bz2" | "xz" | "7z" | "rar" | "zst" | "tgz" | "iso" | "lz"
            | "lzma" | "cab" => Category::Archives,
            "rs" | "py" | "js" | "jsx" | "tsx" | "c" | "cpp" | "cc" | "h" | "hpp" | "java"
            | "go" | "rb" | "php" | "html" | "css" | "json" | "xml" | "yaml" | "yml" | "sh"
            | "toml" | "sql" | "lua" | "swift" | "kt" => Category::Code,
            "exe" | "msi" | "appimage" | "deb" | "rpm" | "flatpak" | "snap" | "apk" | "dmg"
            | "bin" | "run" => Category::Applications,
            _ => Category::Other,
        }
    }

    /// Classify a file by its name's extension. Dotfiles (`.bashrc`) and
    /// extension-less files are [`Category::Other`].
    pub fn classify(file_name: &str) -> Category {
        match extension_of(file_name) {
            Some(ext) => Category::from_extension(&ext.to_ascii_lowercase()),
            None => Category::Other,
        }
    }
}

/// The extension of a file name, if any. Ignores a leading dot so dotfiles are
/// treated as having no extension.
fn extension_of(name: &str) -> Option<&str> {
    let dot = name.rfind('.')?;
    if dot == 0 {
        return None; // ".bashrc"
    }
    let ext = &name[dot + 1..];
    (!ext.is_empty()).then_some(ext)
}

/// Total bytes per category across every file in the tree, largest-first.
/// Categories with zero bytes are omitted.
pub fn category_totals(root: &Node) -> Vec<(Category, u64)> {
    let mut totals: HashMap<Category, u64> = HashMap::new();
    accumulate(root, &mut totals);

    let mut totals: Vec<(Category, u64)> =
        totals.into_iter().filter(|&(_, bytes)| bytes > 0).collect();
    totals.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.label().cmp(b.0.label())));
    totals
}

fn accumulate(node: &Node, totals: &mut HashMap<Category, u64>) {
    if node.is_dir {
        for child in &node.children {
            accumulate(child, totals);
        }
    } else {
        *totals.entry(Category::classify(&node.name)).or_insert(0) += node.size;
    }
}

/// The largest files belonging to `category`, largest-first, capped at `limit`.
pub fn largest_in_category(root: &Node, category: Category, limit: usize) -> Vec<&Node> {
    let mut files = Vec::new();
    collect(root, category, &mut files);
    files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
    files.truncate(limit);
    files
}

fn collect<'a>(node: &'a Node, category: Category, out: &mut Vec<&'a Node>) {
    if node.is_dir {
        for child in &node.children {
            collect(child, category, out);
        }
    } else if Category::classify(&node.name) == category {
        out.push(node);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn file(name: &str, size: u64) -> Node {
        Node { name: name.into(), path: PathBuf::from(name), size, is_dir: false, children: Vec::new() }
    }

    fn dir(name: &str, children: Vec<Node>) -> Node {
        let size = children.iter().map(|c| c.size).sum();
        Node { name: name.into(), path: PathBuf::from(name), size, is_dir: true, children }
    }

    #[test]
    fn classifies_by_extension_case_insensitively() {
        assert_eq!(Category::classify("Movie.MP4"), Category::Videos);
        assert_eq!(Category::classify("song.flac"), Category::Audio);
        assert_eq!(Category::classify("pic.JPEG"), Category::Images);
        assert_eq!(Category::classify("report.pdf"), Category::Documents);
        assert_eq!(Category::classify("backup.tar.gz"), Category::Archives);
        assert_eq!(Category::classify("main.rs"), Category::Code);
        assert_eq!(Category::classify("Inkscape.AppImage"), Category::Applications);
        assert_eq!(Category::classify("mystery.qwerty"), Category::Other);
    }

    #[test]
    fn dotfiles_and_extensionless_are_other() {
        assert_eq!(Category::classify(".bashrc"), Category::Other);
        assert_eq!(Category::classify("Makefile"), Category::Other);
    }

    #[test]
    fn totals_aggregate_across_the_tree_largest_first() {
        let tree = dir(
            "root",
            vec![
                file("a.mp4", 1000),
                dir("sub", vec![file("b.mkv", 500), file("c.mp3", 300)]),
                file("d.pdf", 200),
                file("notes", 50), // Other
            ],
        );

        let totals = category_totals(&tree);

        assert_eq!(totals[0], (Category::Videos, 1500)); // 1000 + 500
        assert_eq!(totals[1], (Category::Audio, 300));
        assert_eq!(totals[2], (Category::Documents, 200));
        assert_eq!(totals[3], (Category::Other, 50));
        assert_eq!(totals.len(), 4); // empty categories omitted
    }

    #[test]
    fn largest_in_category_is_sorted_and_capped() {
        let tree = dir(
            "root",
            vec![
                file("small.mp4", 100),
                dir("sub", vec![file("huge.mp4", 9000), file("mid.mp4", 400)]),
                file("song.mp3", 5000), // different category
            ],
        );

        let top = largest_in_category(&tree, Category::Videos, 2);

        assert_eq!(top.len(), 2);
        assert_eq!(top[0].name, "huge.mp4");
        assert_eq!(top[1].name, "mid.mp4");
    }
}
