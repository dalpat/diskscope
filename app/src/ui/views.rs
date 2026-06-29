// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! The render loop: turning the current `AppState` into the visible page —
//! storage overview, category list, folder browser, and the reclaim view.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use diskscope::category::{self, Category};
use diskscope::disk;
use diskscope::format::human_size;
use diskscope::reclaim::{self, Reclaimable};
use diskscope::scan::Node;

use super::rows::{build_row, category_css_class, category_row, placeholder_row, reclaim_row};
use super::actions::{reclaim_action_handler, reclaim_select_handler, row_action_handler};
use super::scanning::folder_node;
use super::{clear, clear_box, connect, icon_button, AppState, RowAction, Ui, View};

/// Re-render everything from the current state.
pub(super) fn render(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let s = state.borrow();
    let Some(root) = s.root.as_ref() else {
        ui.stack.set_visible_child_name("empty");
        return;
    };

    ui.home_button.set_sensitive(true);
    ui.refresh_button.set_sensitive(true);
    ui.search_button.set_sensitive(true);
    ui.up_button.set_sensitive(matches!(&s.view, View::Folder(p) if !p.is_empty()));

    match &s.view {
        View::Overview => render_overview(root, ui),
        View::Category(category) => render_category(root, *category, state, ui),
        View::Folder(path) => render_folder(root, path, state, ui),
        View::Reclaim => render_reclaim(root, state, ui, s.reclaim_perm_delete),
        View::Search => render_search(root, &s.search_query, s.search_min, state, ui),
    }
}

/// The search results page: every entry whose name matches the query, with its
/// location, acting in place. Directory results navigate; file results expose
/// the usual hover/menu actions (Open / Reveal / Trash …).
fn render_search(
    root: &Node,
    query: &str,
    min_size: u64,
    state: &Rc<RefCell<AppState>>,
    ui: &Rc<Ui>,
) {
    const SHOWN: usize = 200;

    clear_box(&ui.crumbs);
    let label = gtk::Label::new(Some("Search results"));
    label.add_css_class("dim-label");
    ui.crumbs.append(&label);

    clear(&ui.list);
    ui.search_paths.borrow_mut().clear();

    if query.is_empty() {
        ui.list.append(&placeholder_row("Type to search this folder by name."));
        ui.title.set_title("Search");
        ui.title.set_subtitle("");
        ui.stack.set_visible_child_name("list");
        return;
    }

    let hits = diskscope::scan::search(root, query, min_size);
    let total: u64 = hits.iter().map(|n| n.size).sum();

    let handler = row_action_handler(state, ui);
    if hits.is_empty() {
        ui.list.append(&placeholder_row(&format!("No matches for “{query}”.")));
    }
    let mut paths = ui.search_paths.borrow_mut();
    for node in hits.iter().take(SHOWN) {
        let location = file_location(&root.path, &node.path);
        ui.list.append(&build_row(node, total.max(1), Some(&location), Some(&handler)));
        paths.push(node.path.clone());
    }
    drop(paths);
    if hits.len() > SHOWN {
        ui.list
            .append(&placeholder_row(&format!("…and {} more matches", hits.len() - SHOWN)));
    }

    let plural = if hits.len() == 1 { "result" } else { "results" };
    ui.title.set_title(&format!("{} {plural}", hits.len()));
    ui.title.set_subtitle(&format!("“{query}” · {}", human_size(total)));
    ui.stack.set_visible_child_name("list");
}

/// The storage homepage: segmented bar + category list.
fn render_overview(root: &Node, ui: &Rc<Ui>) {
    let totals = category::category_totals(root);
    let used: u64 = totals.iter().map(|(_, bytes)| bytes).sum();

    ui.overview_total.set_text(&human_size(used));
    *ui.overview_data.borrow_mut() = totals.clone();
    ui.overview_bar.queue_draw();

    // Disk capacity ring + caption.
    let usage = disk::disk_usage(&root.path);
    *ui.capacity_data.borrow_mut() = usage;
    ui.capacity_ring.queue_draw();
    match usage {
        Some(u) if u.total > 0 => {
            let pct = (u.used() as f64 / u.total as f64 * 100.0).round() as u64;
            ui.capacity_percent.set_text(&format!("{pct}%"));
            ui.capacity_caption
                .set_text(&format!("{} free of {}", human_size(u.available), human_size(u.total)));
        }
        _ => {
            ui.capacity_percent.set_text("—");
            ui.capacity_caption.set_text("Disk capacity unavailable");
        }
    }

    // Colour legend under the bar.
    rebuild_legend(&ui.legend, &totals);

    clear(&ui.overview_list);
    if totals.is_empty() {
        ui.overview_list.append(&placeholder_row("Nothing to show — this folder is empty."));
    }
    for (category, bytes) in &totals {
        ui.overview_list.append(&category_row(*category, *bytes, used.max(1)));
    }

    ui.title.set_title("Storage");
    ui.title.set_subtitle(&root.path.display().to_string());
    ui.stack.set_visible_child_name("overview");
}

/// The largest files of one category, with Open / Trash actions.
fn render_category(root: &Node, category: Category, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    const SHOWN: usize = 100;

    let files = category::largest_in_category(root, category, usize::MAX);
    let total: u64 = files.iter().map(|f| f.size).sum::<u64>().max(1);

    // Breadcrumb area → a back link to the overview + the category name.
    clear_box(&ui.crumbs);
    let back = gtk::Button::with_label("‹ Storage");
    back.add_css_class("flat");
    ui.crumbs.append(&back);
    connect(&back, state, ui, |state, ui| {
        state.borrow_mut().view = View::Overview;
        render(state, ui);
    });
    let heading = gtk::Label::new(Some(category.label()));
    heading.add_css_class("dim-label");
    ui.crumbs.append(&heading);

    let handler = row_action_handler(state, ui);
    clear(&ui.list);
    if files.is_empty() {
        ui.list.append(&placeholder_row(&format!("No {} found.", category.label().to_lowercase())));
    }
    for file in files.iter().take(SHOWN) {
        let location = file_location(&root.path, &file.path);
        ui.list.append(&build_row(file, total, Some(&location), Some(&handler)));
    }
    if files.len() > SHOWN {
        ui.list.append(&placeholder_row(&format!("…and {} more files", files.len() - SHOWN)));
    }

    ui.title.set_title(category.label());
    ui.title.set_subtitle(&human_size(total));
    ui.stack.set_visible_child_name("list");
}

/// The directory tree browser.
fn render_folder(root: &Node, path: &[usize], state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let node = folder_node(root, path);
    rebuild_breadcrumb(root, path, state, ui);

    let handler = row_action_handler(state, ui);
    populate(&ui.list, node, Some(&handler));

    ui.title.set_title(&node.name);
    // Show the absolute path so it's always clear where you are.
    ui.title.set_subtitle(&node.path.display().to_string());
    ui.stack.set_visible_child_name("list");
}

/// The "free up space" view: regenerable artifacts in the scanned tree (computed
/// instantly from the in-memory tree) plus the system safe spots (Trash, caches)
/// measured on a worker thread. Shows the spinner while measuring, then the page.
fn render_reclaim(root: &Node, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>, perm: bool) {
    ui.title.set_title("Free up space");
    ui.title.set_subtitle("");
    // Keep the switch in step with the stored mode (no-op when already in sync).
    ui.reclaim_perm_switch.set_active(perm);
    // Note: the selection is cleared when the view is *entered* (see the Reclaim
    // button handler), not here — `render` holds an immutable borrow of the
    // state for the whole dispatch, so a `borrow_mut` here would panic.

    // Artifacts are cheap — they come straight from the scanned tree.
    let artifacts = reclaim::find_artifacts(root);
    *ui.reclaim_data.borrow_mut() = (Vec::new(), artifacts);

    // Measuring the caches/Trash needs disk I/O; do it off the UI thread, behind
    // the spinner, guarding against a stale result from a superseded entry.
    let generation = ui.reclaim_gen.get().wrapping_add(1);
    ui.reclaim_gen.set(generation);
    ui.stack.set_visible_child_name("scanning");

    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        // Break each spot into per-application entries (notably: never the whole
        // ~/.cache as one item), then order the whole list largest-first.
        let mut measured: Vec<Reclaimable> =
            reclaim::system_spots(&home).iter().flat_map(reclaim::breakdown).collect();
        measured.sort_by_key(|r| std::cmp::Reverse(r.size));
        let _ = sender.send_blocking(measured);
    });

    let state = state.clone();
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let Ok(system) = receiver.recv().await else {
            return;
        };
        // Drop the result if the user moved on or re-entered the view since.
        if ui.reclaim_gen.get() != generation
            || !matches!(state.borrow().view, View::Reclaim)
        {
            return;
        }
        ui.reclaim_data.borrow_mut().0 = system;
        populate_reclaim_lists(&state, &ui, perm);
        ui.stack.set_visible_child_name("reclaim");
    });
}

/// (Re)build the two reclaim lists from the last measured data, with each row's
/// primary action reflecting `perm` (permanent-delete vs move-to-Trash). Called
/// after a measurement and whenever the delete-mode switch flips.
pub(super) fn populate_reclaim_lists(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>, perm: bool) {
    let handler = reclaim_action_handler(state, ui);
    let on_select = reclaim_select_handler(state, ui);
    let selected = state.borrow().reclaim_selected.clone();
    let data = ui.reclaim_data.borrow();
    let (system, artifacts) = &*data;

    let total: u64 = system.iter().chain(artifacts).map(|r| r.size).sum();
    ui.reclaim_total.set_text(&human_size(total));

    clear(&ui.reclaim_system_list);
    clear(&ui.reclaim_artifact_list);

    ui.reclaim_system_caption.set_visible(!system.is_empty());
    ui.reclaim_system_list.set_visible(!system.is_empty());
    for r in system {
        ui.reclaim_system_list.append(&reclaim_row(r, perm, selected.contains(&r.path), &handler, &on_select));
    }

    ui.reclaim_artifact_caption.set_visible(!artifacts.is_empty());
    ui.reclaim_artifact_list.set_visible(!artifacts.is_empty());
    for r in artifacts {
        ui.reclaim_artifact_list.append(&reclaim_row(r, perm, selected.contains(&r.path), &handler, &on_select));
    }

    if system.is_empty() && artifacts.is_empty() {
        ui.reclaim_system_list.set_visible(true);
        ui.reclaim_system_list.append(&placeholder_row("Nothing to reclaim — you're all clean."));
    }

    drop(data);
    update_reclaim_selection_bar(state, ui);
}

/// Refresh the batch-selection bar from the current selection: hide it when
/// nothing is ticked, otherwise show the count, combined size, and a clear button
/// labelled for the active removal mode.
pub(super) fn update_reclaim_selection_bar(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let s = state.borrow();
    let data = ui.reclaim_data.borrow();
    let (mut count, mut bytes) = (0u64, 0u64);
    for r in data.0.iter().chain(data.1.iter()) {
        if s.reclaim_selected.contains(&r.path) {
            count += 1;
            bytes += r.size;
        }
    }

    ui.reclaim_select_bar.set_visible(count > 0);
    if count > 0 {
        ui.reclaim_select_label.set_text(&format!("{count} selected · {}", human_size(bytes)));
        let verb = if s.reclaim_perm_delete { "Delete" } else { "Move to Trash" };
        ui.reclaim_clear_button.set_label(&format!("{verb} ({count})"));
    }
}

/// A short label for where a file lives: its parent folder shown relative to the
/// scanned root (prefixed with the root's name), or the absolute parent path if
/// it lies outside the root.
fn file_location(root: &Path, file: &Path) -> String {
    let parent = file.parent().unwrap_or(file);
    let Ok(rel) = parent.strip_prefix(root) else {
        return parent.display().to_string();
    };
    let root_name = root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| root.display().to_string());
    if rel.as_os_str().is_empty() {
        root_name
    } else {
        format!("{root_name}/{}", rel.display())
    }
}

/// Rebuild the clickable breadcrumb: a home icon, then root → current folder.
fn rebuild_breadcrumb(root: &Node, path: &[usize], state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    clear_box(&ui.crumbs);

    let home = icon_button("go-home-symbolic", "Storage overview");
    ui.crumbs.append(&home);
    connect(&home, state, ui, |state, ui| {
        state.borrow_mut().view = View::Overview;
        render(state, ui);
    });

    let mut chain: Vec<&Node> = vec![root];
    let mut node = root;
    for &index in path {
        node = &node.children[index];
        chain.push(node);
    }

    let last = chain.len() - 1;
    for (level, node) in chain.iter().enumerate() {
        let sep = gtk::Label::new(Some("›"));
        sep.add_css_class("dim-label");
        ui.crumbs.append(&sep);

        let button = gtk::Button::with_label(&node.name);
        button.add_css_class("flat");
        if level == last {
            button.add_css_class("current-crumb");
        }
        ui.crumbs.append(&button);

        let state = state.clone();
        let ui = ui.clone();
        button.connect_clicked(move |_| {
            state.borrow_mut().view = View::Folder(path_prefix(&state, level));
            render(&state, &ui);
        });
    }
}

/// The current folder path truncated to `level` indices (for breadcrumb jumps).
fn path_prefix(state: &Rc<RefCell<AppState>>, level: usize) -> Vec<usize> {
    match &state.borrow().view {
        View::Folder(path) => path.iter().take(level).copied().collect(),
        _ => Vec::new(),
    }
}

/// Fill `list` with one row per child of `node` (largest-first), or a single
/// placeholder when empty. Clears existing rows first. Free of `Ui`/state so it
/// can be unit-tested with `None`.
pub(super) fn populate(list: &gtk::ListBox, node: &Node, handler: Option<&Rc<dyn Fn(RowAction, PathBuf)>>) {
    clear(list);
    let total = node.size.max(1);
    for child in &node.children {
        list.append(&build_row(child, total, None, handler));
    }
    if node.children.is_empty() {
        list.append(&placeholder_row("This folder is empty."));
    }
}

/// Rebuild the category colour legend (one dot + label per category present).
fn rebuild_legend(legend: &gtk::FlowBox, totals: &[(Category, u64)]) {
    while let Some(child) = legend.first_child() {
        legend.remove(&child);
    }
    for (category, _) in totals {
        let dot = gtk::Label::new(Some("●"));
        dot.add_css_class(category_css_class(*category));
        let label = gtk::Label::new(Some(category.label()));
        label.add_css_class("caption");
        label.add_css_class("dim-label");

        let item = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        item.append(&dot);
        item.append(&label);
        legend.insert(&item, -1);
    }
}
