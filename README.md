# DiskScope

A clean, native disk-usage analyzer for the GNOME desktop. Point it at a
folder and see — at a glance — what's eating your space, then drill in.

Built with **Rust + GTK4 + libadwaita**.

![status](https://img.shields.io/badge/status-v0.1-blue)

## What it does

Open it and you land on a **storage overview** (think Android's Storage
screen): a **disk capacity ring** (used vs. free of the whole filesystem,
green → amber → red as it fills), a **segmented usage bar** with a colour
**legend**, and a list of categories — Videos, Audio, Images, Documents,
Archives, Code, Applications, Other — each with its size and share of the
total. It scans your home directory by default.

From there you can:

- **Tap a category** to see its largest files and act on them.
- **Browse all folders** to drill the directory tree, where each entry shows a
  **colour-coded heat bar** (red = space hog → blue = small), size, and
  percentage of its parent. Click folders to drill in; use the breadcrumb or up
  button to climb out.
- **Act on what you find**: open any entry in your file manager, or move it to
  the Trash (with a confirmation) — DiskScope rescans automatically to show the
  space you freed.
- **Refresh** to rescan, or open a different folder entirely.

Categories are inferred from file extensions. Symlinks are never followed and
hardlinked files are counted only once, so nothing is double-counted. No
database, no settings, no daemon (YAGNI) — just answer "where did my space go?"
and let you fix it.

## Architecture

A two-crate workspace splits the pure logic from the GUI:

| Path | Role |
|------|------|
| `core/src/scan.rs` | The scan engine — walks the tree, totals sizes, sorts. **No GTK.** |
| `core/src/category.rs` | Classifies files into categories and aggregates per-category usage. **No GTK.** |
| `core/src/disk.rs` | Whole-filesystem capacity (total/free) via `statvfs`. **No GTK.** |
| `core/src/format.rs` | Byte → human-readable formatting. **No GTK.** |
| `core/src/lib.rs` | The `diskscope` library (re-exports the modules above). |
| `core/tests/scan_e2e.rs` | End-to-end tests over real temporary directory trees. |
| `app/src/ui.rs` | GTK4/libadwaita view: renders what the engine produces. |
| `app/src/main.rs` | Boots the application. |

Keeping all the logic in the GTK-free `core` crate is what makes the engine
**testable end to end without a display server**: `cargo test -p diskscope-core`
builds real folders on disk, scans them, and asserts on the results — no GTK
required. The `app` crate adds a thin, separately-testable rendering layer.

## Build & run

Requires the GTK4 and libadwaita development packages:

```sh
sudo apt-get install -y build-essential pkg-config libgtk-4-dev libadwaita-1-dev
```

Then:

```sh
cargo run -p diskscope                 # launch (empty; pick a folder in-app)
cargo run -p diskscope -- ~/Downloads  # launch and scan a folder immediately
cargo build --release
```

### No root? (user-local dev libraries)

If you can't `sudo` but the GTK/libadwaita **runtime** is already installed,
you can unpack just the `-dev` files into your home directory and point
pkg-config at them — see `env.sh`. Source it before `cargo` and everything
builds against the system runtime with no root access.

## Test

```sh
cargo test                     # whole workspace (22 tests)
cargo test -p diskscope-core   # just the engine — needs no GTK at all
```

The suite covers the scan engine end to end (real temp directory trees:
recursive sizes, sort order, hidden files, symlink safety, hardlink
de-duplication), size formatting,
the heat-map bucketing, breadcrumb path re-location, and a headless GTK test
that builds real widgets and asserts the rendered rows.

### Capturing a screenshot

Set `DISKSCOPE_SHOT` to render the window to a PNG in-process and exit — handy
on systems where the compositor blocks normal screenshots:

```sh
DISKSCOPE_SHOT=/tmp/shot.png cargo run -p diskscope -- ~/Downloads
```

## License

MIT
