// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! Scan lifecycle and folder navigation: launching scans on a worker thread,
//! cancellation, restoring the view afterwards, and locating folders in the tree.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gio, glib};

use diskscope::format::{human_size, thousands};
use diskscope::scan::{self, CancelFlag, Node, ScanProgress};

use super::views::render;
use super::{AppState, Restore, Ui, View};

/// Show a folder chooser and, on confirmation, scan it fresh.
pub(super) fn open_dialog(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let dialog = gtk::FileDialog::builder().title("Choose a folder to analyze").modal(true).build();
    let window = ui.window.clone();
    let state = state.clone();
    let ui = ui.clone();
    dialog.select_folder(Some(&window), gio::Cancellable::NONE, move |result| {
        if let Ok(Some(path)) = result.map(|f| f.path()) {
            start_scan(path, &state, &ui);
        }
    });
}

/// Fresh scan of `path` as a new root (lands on the overview).
pub(super) fn start_scan(path: PathBuf, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    scan_into(path, Restore::Reset, state, ui);
}

/// Rescan the current root, returning to the same view afterwards.
pub(super) fn rescan_keeping_position(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let (root_path, restore) = {
        let s = state.borrow();
        let Some(root) = s.root.as_ref() else {
            return;
        };
        let restore = match &s.view {
            View::Folder(path) => Restore::Folder(folder_node(root, path).path.clone()),
            other => Restore::Keep(other.clone()),
        };
        (root.path.clone(), restore)
    };
    scan_into(root_path, restore, state, ui);
}

/// Scan `root_path` on a worker thread; on completion swap in the new tree and
/// restore the requested view.
fn scan_into(root_path: PathBuf, restore: Restore, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    ui.stack.set_visible_child_name("scanning");

    // A fresh cancel flag for this scan; the Cancel button holds a clone and can
    // flip it to abandon the walk. A generation tag lets a later scan supersede
    // this one — a stale result is dropped rather than clobbering fresh state.
    let cancel = CancelFlag::new();
    *ui.current_scan.borrow_mut() = Some(cancel.clone());
    let generation = ui.scan_gen.get().wrapping_add(1);
    ui.scan_gen.set(generation);

    // Reset the heartbeat readouts, then poll the shared progress counters on a
    // timer until this scan settles (or is superseded by a newer one).
    ui.scan_detail.set_text("Starting…");
    ui.scan_path.set_text("");
    let progress = Arc::new(ScanProgress::new());
    start_progress_timer(progress.clone(), generation, ui);

    let (sender, receiver) = async_channel::bounded(1);
    let scan_path = root_path.clone();
    std::thread::spawn(move || {
        let _ = sender.send_blocking(scan::scan_reporting(&scan_path, &cancel, &progress));
    });

    let state = state.clone();
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
        // Drop the result of a scan that a newer one has already superseded.
        if ui.scan_gen.get() != generation {
            return;
        }
        // This scan has settled; stop offering to cancel it.
        ui.current_scan.borrow_mut().take();
        match result {
            Ok(node) => {
                {
                    let mut s = state.borrow_mut();
                    let view = match restore {
                        Restore::Reset => View::Overview,
                        Restore::Keep(view) => view,
                        Restore::Folder(target) => {
                            View::Folder(locate(&node, &target).unwrap_or_default())
                        }
                    };
                    s.root = Some(node);
                    s.view = view;
                }
                render(&state, &ui);
            }
            Err(err) => {
                let fallback = if state.borrow().root.is_some() { "overview" } else { "empty" };
                ui.stack.set_visible_child_name(fallback);
                // A cancelled scan is a deliberate user action, not a failure:
                // slip back quietly without an alarming "couldn't scan" toast.
                let message = if err.kind() == std::io::ErrorKind::Interrupted {
                    "Scan cancelled".to_string()
                } else {
                    format!("Couldn't scan {}: {err}", root_path.display())
                };
                ui.toasts.add_toast(adw::Toast::new(&message));
            }
        }
    });
}

/// Poll `progress` on a timer and paint the scanning page's live readouts until
/// this scan (`generation`) settles or a newer one supersedes it.
///
/// The timer stops itself once `current_scan` is cleared (the result has landed)
/// or the generation moves on, so it never outlives the scan it reports on.
fn start_progress_timer(progress: Arc<ScanProgress>, generation: u64, ui: &Rc<Ui>) {
    let ui = ui.clone();
    glib::timeout_add_local(Duration::from_millis(120), move || {
        if ui.scan_gen.get() != generation || ui.current_scan.borrow().is_none() {
            return glib::ControlFlow::Break;
        }
        let (bytes, items) = (progress.bytes(), progress.items());
        ui.scan_detail.set_text(&format!("{} · {} items", human_size(bytes), thousands(items)));
        let current = progress.current();
        if !current.as_os_str().is_empty() {
            ui.scan_path.set_text(&current.display().to_string());
        }
        glib::ControlFlow::Continue
    });
}

/// Walk `root` to the folder at `path` (assumes a valid path).
pub(super) fn folder_node<'a>(root: &'a Node, path: &[usize]) -> &'a Node {
    let mut node = root;
    for &index in path {
        node = &node.children[index];
    }
    node
}

/// Find the drill-down index path from `root` to `target`, if it still exists.
pub(super) fn locate(root: &Node, target: &Path) -> Option<Vec<usize>> {
    let mut path = Vec::new();
    let mut node = root;
    loop {
        if node.path == target {
            return Some(path);
        }
        let (index, child) =
            node.children.iter().enumerate().find(|(_, c)| target.starts_with(&c.path))?;
        path.push(index);
        node = child;
    }
}
