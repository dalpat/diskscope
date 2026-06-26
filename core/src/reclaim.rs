//! Find space that can be reclaimed **without losing important files**.
//!
//! Two sources, both safe by construction:
//! - **System safe spots** — regenerable locations under the user's home: the
//!   Trash (already-discarded files) and the cache directory (`~/.cache`, which
//!   apps rebuild on demand). See [`system_spots`].
//! - **Project artifacts** — build outputs and downloaded dependencies found
//!   *inside the scanned tree* (`node_modules`, Rust `target/`, `__pycache__`,
//!   …). These are reproducible from source. See [`find_artifacts`].
//!
//! Like the rest of `core`, this module is GTK-free and unit-testable.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::scan::{self, Node};

/// What kind of reclaimable space an entry represents — drives copy and icons
/// in the UI, and how aggressively it's safe to clear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReclaimKind {
    /// Files already moved to the Trash. Clearing means emptying them for good.
    Trash,
    /// Regenerable application caches (rebuilt automatically when next needed).
    Cache,
    /// A build output or dependency directory reproducible from project source.
    Artifact,
}

/// One location whose contents can be cleared to free space safely.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reclaimable {
    /// Human-readable description (e.g. "Node.js packages").
    pub label: String,
    /// The directory to clear.
    pub path: PathBuf,
    /// Recovered bytes (recursive size of `path`).
    pub size: u64,
    /// Number of files inside `path` — the "blast radius" count shown before a
    /// deletion so the user can gauge impact.
    pub file_count: u64,
    /// Which kind of safe-to-clear space this is.
    pub kind: ReclaimKind,
}

/// How risky it is to delete an entry — the headline of its blast radius.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// Rebuilt automatically with no action and nothing lost.
    Safe,
    /// Nothing important is lost, but you must rebuild/re-fetch it (often needing
    /// a network connection or a build step) before that workflow works again.
    Rebuild,
    /// Unrecognised or potentially state-bearing — verify before deleting.
    Caution,
}

impl Risk {
    /// A one-word badge for the UI.
    pub fn word(self) -> &'static str {
        match self {
            Risk::Safe => "Safe",
            Risk::Rebuild => "Rebuilds",
            Risk::Caution => "Check first",
        }
    }
}

/// The blast radius of deleting an entry: how risky, and what actually happens —
/// what breaks, what rebuilds, and what is *not* affected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Consequence {
    pub risk: Risk,
    /// One plain-language sentence on the impact.
    pub summary: String,
}

/// Describe what deleting `item` will do — the "what breaks" assessment shown to
/// the user before they decide. Keyed off the entry's kind and directory name.
pub fn consequence(item: &Reclaimable) -> Consequence {
    let name =
        item.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    match item.kind {
        ReclaimKind::Trash => Consequence {
            risk: Risk::Safe,
            summary: "Files you already deleted. Emptying frees the space for good; \
                      nothing you currently use is affected."
                .into(),
        },
        ReclaimKind::Cache => cache_consequence(&name),
        ReclaimKind::Artifact => artifact_consequence(&name),
    }
}

fn consequence_of(risk: Risk, summary: &str) -> Consequence {
    Consequence { risk, summary: summary.to_string() }
}

fn cache_consequence(name: &str) -> Consequence {
    match name {
        "google-chrome" | "chromium" | "BraveSoftware" | "microsoft-edge" | "vivaldi"
        | "mozilla" => consequence_of(
            Risk::Safe,
            "The browser re-downloads cached web content as you browse. Bookmarks, \
             history, passwords and open tabs live elsewhere and are not touched.",
        ),
        "thumbnails" => consequence_of(
            Risk::Safe,
            "Thumbnails regenerate automatically as you open folders; file managers \
             are briefly slower the first time.",
        ),
        "tracker3" | "tracker" => consequence_of(
            Risk::Safe,
            "The file-search index rebuilds in the background; searches may be \
             incomplete for a short while.",
        ),
        "fontconfig" => consequence_of(
            Risk::Safe,
            "Rebuilt automatically the next time an application loads fonts.",
        ),
        "mesa_shader_cache" | "mesa_shader_cache_db" | "radv_builtin_shaders" | "nvidia" => {
            consequence_of(
                Risk::Safe,
                "Games and GPU apps recompile shaders on next launch — the first run \
                 is slower, then performance returns to normal.",
            )
        }
        "go-build" => consequence_of(
            Risk::Safe,
            "Go recompiles from source on your next build — slower once, no downloads.",
        ),
        "pip" => consequence_of(
            Risk::Rebuild,
            "pip re-downloads packages the next time you install something (needs \
             internet). Already-installed environments keep working.",
        ),
        "yarn" | "node-gyp" => consequence_of(
            Risk::Rebuild,
            "Re-downloaded the next time you install packages (needs internet); \
             installed projects keep working.",
        ),
        "ms-playwright" => consequence_of(
            Risk::Rebuild,
            "Playwright re-downloads its browser binaries on next use (needs internet).",
        ),
        "JetBrains" => consequence_of(
            Risk::Rebuild,
            "The IDE re-indexes your projects on next launch — the first start is \
             slower; your code and settings are untouched.",
        ),
        "spotify" | "vlc" | "gstreamer-1.0" => consequence_of(
            Risk::Safe,
            "Re-downloaded or re-streamed as needed; your library and settings stay.",
        ),
        _ => consequence_of(
            Risk::Caution,
            "Unrecognised cache. Most apps rebuild these automatically, but if it \
             belongs to something you rely on, confirm it still works afterwards.",
        ),
    }
}

fn artifact_consequence(name: &str) -> Consequence {
    match name {
        "node_modules" => consequence_of(
            Risk::Rebuild,
            "The project won't build or run until you reinstall dependencies \
             (npm install / yarn / pnpm install) — needs internet.",
        ),
        "target" => consequence_of(
            Risk::Rebuild,
            "The next `cargo build` recompiles from scratch — slower once, no downloads.",
        ),
        "__pycache__" => consequence_of(
            Risk::Safe,
            "Python regenerates this bytecode automatically on the next run — no impact.",
        ),
        ".pytest_cache" | ".mypy_cache" | ".ruff_cache" => consequence_of(
            Risk::Safe,
            "Rebuilt automatically the next time you run the tool.",
        ),
        ".gradle" => consequence_of(
            Risk::Rebuild,
            "Gradle re-downloads dependencies and rebuilds caches on the next build \
             (needs internet).",
        ),
        ".tox" => consequence_of(
            Risk::Rebuild,
            "tox recreates its environments on the next run (needs internet).",
        ),
        ".next" | ".nuxt" | ".svelte-kit" | ".turbo" | ".parcel-cache" | "build" | "dist" => {
            consequence_of(Risk::Rebuild, "Recreated by your next build or dev run.")
        }
        _ => consequence_of(Risk::Rebuild, "Recreated by your build tooling."),
    }
}

/// A candidate system location, before its size is measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemSpot {
    pub label: &'static str,
    pub path: PathBuf,
    pub kind: ReclaimKind,
}

/// The known-safe system locations under `home` that currently **exist**.
///
/// Uses the XDG defaults (`~/.local/share/Trash`, `~/.cache`). Non-existent
/// spots are omitted so callers never offer to clear something that isn't there.
pub fn system_spots(home: &Path) -> Vec<SystemSpot> {
    let candidates = [
        SystemSpot {
            label: "Trash",
            path: home.join(".local/share/Trash"),
            kind: ReclaimKind::Trash,
        },
        SystemSpot {
            label: "Application caches",
            path: home.join(".cache"),
            kind: ReclaimKind::Cache,
        },
    ];
    candidates.into_iter().filter(|spot| spot.path.is_dir()).collect()
}

/// Measure a system spot by scanning it, yielding a single [`Reclaimable`] for
/// the whole spot. An empty or unreadable spot reports size 0. Does real I/O —
/// run it off the UI thread.
pub fn measure(spot: &SystemSpot) -> Reclaimable {
    let (size, file_count) = scan::scan(&spot.path)
        .map(|node| (node.size, count_files(&node)))
        .unwrap_or((0, 0));
    Reclaimable {
        label: spot.label.to_string(),
        path: spot.path.clone(),
        size,
        file_count,
        kind: spot.kind,
    }
}

/// Break a system spot into the entries actually offered to the user.
///
/// The cache directory is **never** offered as one giant item — deleting all of
/// `~/.cache` at once is a foot-gun that can disrupt running apps. Instead it is
/// split into one entry **per application** (its immediate sub-entries), each
/// labelled with a friendly name where known, so the user clears caches
/// selectively and can see exactly which app each one belongs to. Other spots
/// (the Trash) stay as a single entry. Largest entries first.
pub fn breakdown(spot: &SystemSpot) -> Vec<Reclaimable> {
    if spot.kind != ReclaimKind::Cache {
        return vec![measure(spot)];
    }
    let Ok(node) = scan::scan(&spot.path) else {
        return Vec::new();
    };
    let mut entries: Vec<Reclaimable> = node
        .children
        .iter()
        .map(|child| Reclaimable {
            label: cache_label(&child.name),
            path: child.path.clone(),
            size: child.size,
            file_count: count_files(child),
            kind: ReclaimKind::Cache,
        })
        .collect();
    entries.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    entries
}

/// Count the files (leaf entries) within an already-scanned node — recursive for
/// directories, 1 for a file or symlink. Pure, in-memory; no extra I/O.
pub fn count_files(node: &Node) -> u64 {
    if node.is_dir {
        node.children.iter().map(count_files).sum()
    } else {
        1
    }
}

/// A friendly name for a `~/.cache` sub-entry, so the user recognises which app
/// a cache belongs to. Unknown entries keep their raw directory name.
fn cache_label(name: &str) -> String {
    let friendly = match name {
        "google-chrome" => "Google Chrome cache",
        "chromium" => "Chromium cache",
        "BraveSoftware" => "Brave cache",
        "microsoft-edge" => "Microsoft Edge cache",
        "vivaldi" => "Vivaldi cache",
        "mozilla" => "Firefox cache",
        "thumbnails" => "Thumbnail previews",
        "tracker3" | "tracker" => "File-search index",
        "fontconfig" => "Font cache",
        "mesa_shader_cache" | "mesa_shader_cache_db" => "GPU shader cache",
        "nvidia" => "NVIDIA shader cache",
        "pip" => "pip download cache",
        "go-build" => "Go build cache",
        "yarn" => "Yarn package cache",
        "node-gyp" => "node-gyp cache",
        "ms-playwright" => "Playwright browsers cache",
        "JetBrains" => "JetBrains IDE cache",
        "spotify" => "Spotify cache",
        "vlc" => "VLC cache",
        "gstreamer-1.0" => "GStreamer plugin cache",
        "winetricks" => "Winetricks cache",
        _ => return name.to_string(),
    };
    friendly.to_string()
}

/// Find regenerable build/dependency directories within an already-scanned tree.
///
/// Walks `root`, and whenever it recognises a directory as a project artifact it
/// records it and does **not** descend further — so a `node_modules` nested
/// inside another is counted once, via its top-most parent. Results are sorted
/// largest-first.
pub fn find_artifacts(root: &Node) -> Vec<Reclaimable> {
    let mut out = Vec::new();
    collect_artifacts(root, &mut out);
    out.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.path.cmp(&b.path)));
    out
}

fn collect_artifacts(dir: &Node, out: &mut Vec<Reclaimable>) {
    // Sibling names let us disambiguate generic dir names (e.g. only treat
    // `target/` as a Rust build dir when a `Cargo.toml` sits beside it).
    let siblings: HashSet<&str> = dir.children.iter().map(|c| c.name.as_str()).collect();
    for child in &dir.children {
        if !child.is_dir {
            continue;
        }
        if let Some(label) = artifact_label(&child.name, &siblings) {
            out.push(Reclaimable {
                label: label.to_string(),
                path: child.path.clone(),
                size: child.size,
                file_count: count_files(child),
                kind: ReclaimKind::Artifact,
            });
        } else {
            collect_artifacts(child, out);
        }
    }
}

/// Recognise a directory as a regenerable project artifact, returning a label.
///
/// Unambiguous, tool-specific names match on their own. Generic names that could
/// plausibly be real user folders (`target`, `build`, `dist`) only match when a
/// sibling marker file proves they belong to a known build tool.
fn artifact_label(name: &str, siblings: &HashSet<&str>) -> Option<&'static str> {
    let unambiguous = match name {
        "node_modules" => "Node.js packages",
        "__pycache__" => "Python bytecode cache",
        ".pytest_cache" => "pytest cache",
        ".mypy_cache" => "mypy cache",
        ".ruff_cache" => "Ruff cache",
        ".gradle" => "Gradle cache",
        ".tox" => "tox environments",
        ".next" => "Next.js build",
        ".nuxt" => "Nuxt build",
        ".svelte-kit" => "SvelteKit build",
        ".turbo" => "Turbo cache",
        ".parcel-cache" => "Parcel cache",
        _ => "",
    };
    if !unambiguous.is_empty() {
        return Some(unambiguous);
    }
    match name {
        "target" if siblings.contains("Cargo.toml") => Some("Rust build output"),
        "build" | "dist" if siblings.contains("package.json") => Some("Build output"),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn dir(name: &str, size: u64, children: Vec<Node>) -> Node {
        Node { name: name.into(), path: name.into(), size, is_dir: true, children }
    }
    fn file(name: &str, size: u64) -> Node {
        Node { name: name.into(), path: name.into(), size, is_dir: false, children: Vec::new() }
    }

    #[test]
    fn finds_unambiguous_artifacts_without_descending_into_them() {
        // node_modules (2 files) with a nested node_modules — counted once.
        let inner = dir(
            "node_modules",
            50,
            vec![file("a.js", 30), dir("node_modules", 20, vec![file("b.js", 20)])],
        );
        let proj = dir("proj", 150, vec![inner, dir("src", 100, vec![file("main.js", 100)])]);
        let root = dir("root", 150, vec![proj]);

        let found = find_artifacts(&root);
        assert_eq!(found.len(), 1, "nested node_modules counted via its parent only");
        assert_eq!(found[0].size, 50);
        assert_eq!(found[0].file_count, 2, "blast radius counts files recursively");
        assert_eq!(found[0].kind, ReclaimKind::Artifact);
    }

    #[test]
    fn cache_spot_breaks_down_per_application() {
        let home = tempdir().unwrap();
        let cache = home.path().join(".cache");
        fs::create_dir_all(cache.join("google-chrome/Default")).unwrap();
        fs::create_dir_all(cache.join("obscure-tool")).unwrap();
        fs::write(cache.join("google-chrome/Default/big"), vec![b'x'; 5000]).unwrap();
        fs::write(cache.join("google-chrome/Default/small"), vec![b'x'; 10]).unwrap();
        fs::write(cache.join("obscure-tool/blob"), vec![b'x'; 100]).unwrap();

        let cache_spot =
            system_spots(home.path()).into_iter().find(|s| s.kind == ReclaimKind::Cache).unwrap();
        let entries = breakdown(&cache_spot);

        // One entry per app — never a single ~/.cache blob.
        assert_eq!(entries.len(), 2);
        // Largest first, with a friendly name and a real file count.
        assert_eq!(entries[0].label, "Google Chrome cache");
        assert_eq!(entries[0].file_count, 2);
        assert!(entries[0].path.ends_with("google-chrome"));
        // Unknown apps keep their directory name so the user still recognises them.
        assert_eq!(entries[1].label, "obscure-tool");
    }

    #[test]
    fn consequence_explains_what_breaks_per_entry() {
        let chrome = Reclaimable {
            label: "Google Chrome cache".into(),
            path: "/h/.cache/google-chrome".into(),
            size: 1,
            file_count: 1,
            kind: ReclaimKind::Cache,
        };
        let c = consequence(&chrome);
        assert_eq!(c.risk, Risk::Safe);
        assert!(c.summary.to_lowercase().contains("bookmarks"), "reassures logins/bookmarks kept");

        let modules = Reclaimable {
            label: "Node.js packages".into(),
            path: "/p/node_modules".into(),
            size: 1,
            file_count: 1,
            kind: ReclaimKind::Artifact,
        };
        let c = consequence(&modules);
        assert_eq!(c.risk, Risk::Rebuild, "deleting deps breaks the build until reinstalled");
        assert!(c.summary.contains("install"));

        let unknown = Reclaimable {
            label: "weird-app".into(),
            path: "/h/.cache/weird-app".into(),
            size: 1,
            file_count: 1,
            kind: ReclaimKind::Cache,
        };
        assert_eq!(consequence(&unknown).risk, Risk::Caution, "unknown caches warn the user");
    }

    #[test]
    fn trash_spot_stays_a_single_entry() {
        let home = tempdir().unwrap();
        fs::create_dir_all(home.path().join(".local/share/Trash/files")).unwrap();
        fs::write(home.path().join(".local/share/Trash/files/x"), b"x").unwrap();
        let trash =
            system_spots(home.path()).into_iter().find(|s| s.kind == ReclaimKind::Trash).unwrap();
        assert_eq!(breakdown(&trash).len(), 1, "Trash is offered as one item");
    }

    #[test]
    fn generic_names_need_a_marker_sibling() {
        // `target` beside Cargo.toml → Rust build; a lone `target` → not matched.
        let rust = dir("rust", 0, vec![file("Cargo.toml", 1), dir("target", 900, vec![])]);
        let plain = dir("notes", 0, vec![dir("target", 800, vec![])]);
        let root = dir("root", 0, vec![rust, plain]);

        let found = find_artifacts(&root);
        let paths: Vec<_> = found.iter().map(|r| r.path.to_string_lossy().into_owned()).collect();
        assert!(paths.contains(&"target".to_string()), "Rust target should match");
        assert_eq!(found.len(), 1, "the unmarked `target` must not be flagged");
    }

    #[test]
    fn system_spots_lists_only_existing_dirs() {
        let home = tempdir().unwrap();
        // Nothing created yet → no spots.
        assert!(system_spots(home.path()).is_empty());

        fs::create_dir_all(home.path().join(".cache")).unwrap();
        fs::create_dir_all(home.path().join(".local/share/Trash")).unwrap();
        let spots = system_spots(home.path());
        let labels: Vec<_> = spots.iter().map(|s| s.label).collect();
        assert!(labels.contains(&"Trash"));
        assert!(labels.contains(&"Application caches"));
    }

    #[test]
    fn measure_sums_a_real_directory() {
        let home = tempdir().unwrap();
        let cache = home.path().join(".cache");
        fs::create_dir_all(&cache).unwrap();
        fs::write(cache.join("blob"), vec![b'x'; 1234]).unwrap();

        let spots = system_spots(home.path());
        let cache_spot = spots.iter().find(|s| s.kind == ReclaimKind::Cache).unwrap();
        assert_eq!(measure(cache_spot).size, 1234);
    }
}
