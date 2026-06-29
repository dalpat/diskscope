// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! GTK4 + libadwaita front-end. A thin, stateful view over the scan engine.
//!
//! Views, dispatched by [`render`]:
//! - **Overview** — the Android-style storage homepage: a segmented usage bar
//!   plus a list of categories (Videos, Audio, …).
//! - **Category** — the largest files of one category, with Open / Trash.
//! - **Folder** — the classic directory tree browser with a breadcrumb.
//! - **Reclaim** — safe-to-clear space: Trash, caches, and regenerable project
//!   artifacts found in the scanned tree.

use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use diskscope::category::Category;
use diskscope::disk::DiskUsage;
use diskscope::reclaim::{self, Reclaimable, Risk};
use diskscope::scan::{CancelFlag, Node};

mod actions;
mod draw;
mod rows;
mod scanning;
mod views;

use draw::{draw_ring, draw_segments};
use scanning::{folder_node, locate, open_dialog, rescan_keeping_position, start_scan};
use views::{populate_reclaim_lists, render};

/// Theme additions: heat-map usage bars, category accent colours, crumb styling.
const CSS: &str = "
progressbar { min-height: 8px; }
progressbar > trough,
progressbar > trough > progress { border-radius: 8px; min-height: 8px; }
progressbar.bucket-5 > trough > progress { background-color: #e01b24; }
progressbar.bucket-4 > trough > progress { background-color: #ff7800; }
progressbar.bucket-3 > trough > progress { background-color: #f5c211; }
progressbar.bucket-2 > trough > progress { background-color: #2ec27e; }
progressbar.bucket-1 > trough > progress { background-color: #3584e4; }
.current-crumb { font-weight: bold; }
.crumbs button { padding: 2px 8px; min-height: 0; }
.row-actions { transition: opacity 150ms ease-in-out; }
.cat-videos { color: #e01b24; }
.cat-audio { color: #9141ac; }
.cat-images { color: #e66100; }
.cat-documents { color: #1c71d8; }
.cat-archives { color: #986a44; }
.cat-code { color: #2ec27e; }
.cat-applications { color: #e5a50a; }
.cat-other { color: #77767b; }
.risk-safe { color: #2ec27e; font-weight: bold; }
.risk-rebuild { color: #e5a50a; font-weight: bold; }
.risk-caution { color: #e01b24; font-weight: bold; }
";

/// Which screen the user is on.
#[derive(Clone)]
enum View {
    Overview,
    Category(Category),
    Folder(Vec<usize>), // drill-down path of child indices from the root
    Reclaim,            // safe-to-clear space (Trash, caches, build artifacts)
    Search,             // name search over the scanned tree (query in AppState)
}

/// What the user is currently looking at.
struct AppState {
    /// The scanned tree, or `None` before the first scan.
    root: Option<Node>,
    view: View,
    /// Reclaim view: when true the row's primary "clear" action deletes
    /// permanently instead of moving to Trash. Session-only (no persisted
    /// settings — see the project's YAGNI stance). Permanent delete is also
    /// always available per-row via the right-click menu.
    reclaim_perm_delete: bool,
    /// Search view: the current name query and the minimum-size filter (bytes).
    search_query: String,
    search_min: u64,
    /// Reclaim view: the set of entry paths currently ticked for a batch clear.
    reclaim_selected: HashSet<PathBuf>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState {
            root: None,
            view: View::Overview,
            reclaim_perm_delete: false,
            search_query: String::new(),
            search_min: 0,
            reclaim_selected: HashSet::new(),
        }
    }
}

/// Where to land after a rescan completes.
enum Restore {
    /// A fresh scan of a new root — land on the overview.
    Reset,
    /// Keep the same view (overview/category refresh).
    Keep(View),
    /// Return to the folder at this path, if it still exists.
    Folder(PathBuf),
}

/// Long-lived widgets the render loop updates.
struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    stack: gtk::Stack,
    title: adw::WindowTitle,
    home_button: gtk::Button,
    up_button: gtk::Button,
    refresh_button: gtk::Button,
    search_button: gtk::ToggleButton,
    /// The cancel flag of the scan currently in flight, if any. The "Scanning…"
    /// page's Cancel button flips it to abandon a slow walk; cleared once the
    /// scan settles (completed or cancelled).
    current_scan: Rc<RefCell<Option<CancelFlag>>>,
    /// Generation guard for the folder scan: a result from a superseded scan
    /// (another started while it ran) is dropped instead of clobbering state.
    scan_gen: Rc<Cell<u64>>,
    // Scanning page: live progress readouts updated from the walk's counters.
    scan_detail: gtk::Label,
    scan_path: gtk::Label,
    // Overview page.
    overview_total: gtk::Label,
    capacity_ring: gtk::DrawingArea,
    capacity_percent: gtk::Label,
    capacity_caption: gtk::Label,
    capacity_data: Rc<RefCell<Option<DiskUsage>>>,
    overview_bar: gtk::DrawingArea,
    legend: gtk::FlowBox,
    overview_list: gtk::ListBox,
    overview_data: Rc<RefCell<Vec<(Category, u64)>>>,
    // Shared list page (folder + category + search).
    crumbs: gtk::Box,
    list: gtk::ListBox,
    /// Paths of the rows currently shown in the search results, in row order, so
    /// activating a directory result can navigate straight to it.
    search_paths: Rc<RefCell<Vec<PathBuf>>>,
    // Reclaim page.
    reclaim_total: gtk::Label,
    reclaim_perm_switch: gtk::Switch,
    /// Batch-selection bar: shown when one or more entries are ticked.
    reclaim_select_bar: gtk::Box,
    reclaim_select_label: gtk::Label,
    reclaim_clear_button: gtk::Button,
    reclaim_system_caption: gtk::Label,
    reclaim_system_list: gtk::ListBox,
    reclaim_artifact_caption: gtk::Label,
    reclaim_artifact_list: gtk::ListBox,
    /// Last measured spots, kept so toggling the delete-mode switch can rebuild
    /// the rows without a rescan. `(system, artifacts)`.
    reclaim_data: Rc<RefCell<(Vec<Reclaimable>, Vec<Reclaimable>)>>,
    /// Generation guard: a stale background measurement (from a superseded
    /// entry into the view) is dropped instead of overwriting fresh results.
    reclaim_gen: Rc<Cell<u64>>,
}

/// An action invoked from a row's trailing buttons or right-click menu.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RowAction {
    /// Open with the desktop's default handler.
    Open,
    /// Reveal in the file manager (open the containing folder, item selected).
    Reveal,
    /// Open a terminal at the item's folder.
    Terminal,
    /// Copy the item's absolute path to the clipboard.
    CopyPath,
    /// Move to Trash (after confirmation).
    Trash,
    /// Permanently delete (after confirmation). Used by the Reclaim view.
    Delete,
}

/// Build and present the main window. Wired as the application's `activate`.
///
/// Scans `initial` if given, otherwise the user's home directory, landing on
/// the storage overview.
pub fn build_ui(app: &adw::Application, initial: Option<String>) {
    install_css();

    let state = Rc::new(RefCell::new(AppState::default()));

    // --- Header bar ---------------------------------------------------------
    let home_button = icon_button("go-home-symbolic", "Storage overview");
    let up_button = icon_button("go-up-symbolic", "Parent folder");
    up_button.set_sensitive(false);
    let open_button = icon_button("folder-open-symbolic", "Analyze another folder");
    let refresh_button = icon_button("view-refresh-symbolic", "Rescan");
    refresh_button.set_sensitive(false);
    let search_button = gtk::ToggleButton::new();
    search_button.set_icon_name("system-search-symbolic");
    search_button.set_tooltip_text(Some("Search files (Ctrl+F)"));
    search_button.add_css_class("flat");
    search_button.set_sensitive(false);

    let title = adw::WindowTitle::new("DiskScope", "");
    let header = adw::HeaderBar::new();
    header.pack_start(&home_button);
    header.pack_start(&up_button);
    header.pack_end(&open_button);
    header.pack_end(&refresh_button);
    header.pack_end(&search_button);
    header.set_title_widget(Some(&title));

    // --- Search bar (a second top bar, revealed by the header toggle) ----------
    let search_entry = gtk::SearchEntry::new();
    search_entry.set_placeholder_text(Some("Search files and folders by name"));
    search_entry.set_hexpand(true);
    let size_filter =
        gtk::DropDown::from_strings(&["Any size", "≥ 1 MB", "≥ 10 MB", "≥ 100 MB", "≥ 1 GB"]);
    size_filter.set_valign(gtk::Align::Center);
    size_filter.set_tooltip_text(Some("Only show matches at least this big"));
    let search_inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    search_inner.set_hexpand(true);
    search_inner.append(&search_entry);
    search_inner.append(&size_filter);
    let search_bar = gtk::SearchBar::builder().build();
    search_bar.set_child(Some(&search_inner));
    search_bar.connect_entry(&search_entry);
    search_button
        .bind_property("active", &search_bar, "search-mode-enabled")
        .bidirectional()
        .sync_create()
        .build();

    // --- Empty / scanning states -------------------------------------------
    let empty_button = gtk::Button::with_label("Open Folder…");
    empty_button.add_css_class("pill");
    empty_button.add_css_class("suggested-action");
    empty_button.set_halign(gtk::Align::Center);
    let empty = adw::StatusPage::builder()
        .icon_name("drive-harddisk-symbolic")
        .title("Analyze Disk Usage")
        .description("Open a folder to see what's taking up space.")
        .child(&empty_button)
        .build();

    // AdwSpinner, not GtkSpinner: the GTK spinner redraws its fading dots every
    // frame via a CSS animation, which keeps the GPU busy recompositing for the
    // whole scan (worst on the slow, near-full-disk scans). AdwSpinner renders a
    // single rotating paintable and is far cheaper to animate.
    let spinner = adw::Spinner::builder().width_request(48).height_request(48).build();
    // Live heartbeat: a running "<bytes> · <count> items" total over the current
    // path, updated on a timer from the scan's shared progress counters — so a
    // long walk visibly works instead of sitting behind a blind spinner.
    let scan_detail = gtk::Label::new(Some("Starting…"));
    scan_detail.add_css_class("title-4");
    scan_detail.set_margin_top(12);
    let scan_path = gtk::Label::builder()
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .max_width_chars(46)
        .build();
    scan_path.add_css_class("dim-label");
    scan_path.add_css_class("caption");
    let cancel_button = gtk::Button::with_label("Cancel");
    cancel_button.add_css_class("pill");
    cancel_button.set_halign(gtk::Align::Center);
    cancel_button.set_margin_top(16);
    let scanning_box = gtk::Box::new(gtk::Orientation::Vertical, 4);
    scanning_box.set_halign(gtk::Align::Center);
    scanning_box.append(&spinner);
    scanning_box.append(&scan_detail);
    scanning_box.append(&scan_path);
    scanning_box.append(&cancel_button);
    let scanning = adw::StatusPage::builder()
        .title("Scanning…")
        .description("Walking the directory tree.")
        .child(&scanning_box)
        .build();

    // --- Overview page ------------------------------------------------------
    let overview_data = Rc::new(RefCell::new(Vec::<(Category, u64)>::new()));
    let capacity_data = Rc::new(RefCell::new(None::<DiskUsage>));

    // Disk capacity ring (donut) with a centered "% used" label.
    let capacity_ring =
        gtk::DrawingArea::builder().content_width(116).content_height(116).build();
    {
        let data = capacity_data.clone();
        capacity_ring.set_draw_func(move |_area, cr, w, h| draw_ring(cr, w, h, *data.borrow()));
    }
    let capacity_percent = gtk::Label::new(Some("—"));
    capacity_percent.add_css_class("title-2");
    let percent_caption = gtk::Label::new(Some("disk used"));
    percent_caption.add_css_class("dim-label");
    percent_caption.add_css_class("caption");
    let percent_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    percent_box.set_halign(gtk::Align::Center);
    percent_box.set_valign(gtk::Align::Center);
    percent_box.append(&capacity_percent);
    percent_box.append(&percent_caption);
    let ring_overlay = gtk::Overlay::new();
    ring_overlay.set_child(Some(&capacity_ring));
    ring_overlay.add_overlay(&percent_box);

    // Scanned-folder total + disk free/total caption.
    let overview_total = gtk::Label::builder().xalign(0.0).label("—").build();
    overview_total.add_css_class("title-1");
    let used_caption = gtk::Label::builder().xalign(0.0).label("Total used in this folder").build();
    used_caption.add_css_class("dim-label");
    let capacity_caption = gtk::Label::builder().xalign(0.0).label("").build();
    capacity_caption.add_css_class("dim-label");
    capacity_caption.add_css_class("caption");
    let info_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    info_box.set_valign(gtk::Align::Center);
    info_box.append(&overview_total);
    info_box.append(&used_caption);
    info_box.append(&capacity_caption);

    let top_row = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    top_row.append(&ring_overlay);
    top_row.append(&info_box);

    // Segmented category bar + colour legend.
    let overview_bar = gtk::DrawingArea::builder()
        .content_height(20)
        .hexpand(true)
        .margin_top(8)
        .build();
    {
        let data = overview_data.clone();
        overview_bar.set_draw_func(move |_area, cr, w, h| draw_segments(cr, w, h, &data.borrow()));
    }
    let legend = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(14)
        .row_spacing(2)
        .max_children_per_line(8)
        .build();

    let overview_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    overview_list.add_css_class("boxed-list");
    overview_list.set_margin_top(6);

    let browse_button = gtk::Button::with_label("Browse all folders");
    browse_button.add_css_class("pill");

    let reclaim_button = gtk::Button::builder()
        .icon_name("user-trash-symbolic")
        .label("Free up space")
        .build();
    reclaim_button.add_css_class("pill");
    reclaim_button.add_css_class("suggested-action");

    let overview_buttons = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    overview_buttons.set_halign(gtk::Align::Center);
    overview_buttons.set_margin_top(6);
    overview_buttons.append(&browse_button);
    overview_buttons.append(&reclaim_button);

    // Anchor the bar/legend/list to "this folder", contrasting the ring's
    // whole-disk reading so the two aren't conflated.
    let breakdown_caption =
        gtk::Label::builder().xalign(0.0).label("This folder, by type").build();
    breakdown_caption.add_css_class("dim-label");
    breakdown_caption.add_css_class("caption");

    let overview_inner = gtk::Box::new(gtk::Orientation::Vertical, 12);
    overview_inner.set_margin_top(18);
    overview_inner.set_margin_bottom(18);
    overview_inner.set_margin_start(12);
    overview_inner.set_margin_end(12);
    overview_inner.append(&top_row);
    overview_inner.append(&breakdown_caption);
    overview_inner.append(&overview_bar);
    overview_inner.append(&legend);
    overview_inner.append(&overview_list);
    overview_inner.append(&overview_buttons);

    let overview = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&adw::Clamp::builder().maximum_size(640).child(&overview_inner).build())
        .build();

    // --- Shared list page (folder + category) ------------------------------
    let crumbs = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .margin_start(8)
        .margin_end(8)
        .margin_top(6)
        .margin_bottom(6)
        .build();
    crumbs.add_css_class("crumbs");
    let crumb_scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .child(&crumbs)
        .build();

    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    list.add_css_class("boxed-list");
    list.set_margin_start(12);
    list.set_margin_end(12);
    list.set_margin_top(12);
    list.set_margin_bottom(12);

    let list_scroller = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&adw::Clamp::builder().maximum_size(900).child(&list).build())
        .build();

    let list_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    list_page.append(&crumb_scroller);
    list_page.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
    list_page.append(&list_scroller);

    // --- Reclaim page ------------------------------------------------------
    let reclaim_total = gtk::Label::builder().xalign(0.0).label("—").build();
    reclaim_total.add_css_class("title-1");
    let reclaim_caption =
        gtk::Label::builder().xalign(0.0).label("Reclaimable space").build();
    reclaim_caption.add_css_class("dim-label");
    let reclaim_head = gtk::Box::new(gtk::Orientation::Vertical, 2);
    reclaim_head.set_hexpand(true);
    reclaim_head.append(&reclaim_total);
    reclaim_head.append(&reclaim_caption);

    // The "setting": flip the primary clear action from Trash to permanent.
    let reclaim_perm_switch = gtk::Switch::new();
    reclaim_perm_switch.set_valign(gtk::Align::Center);
    let perm_label = gtk::Label::new(Some("Delete permanently"));
    let perm_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    perm_box.set_valign(gtk::Align::Center);
    perm_box.append(&perm_label);
    perm_box.append(&reclaim_perm_switch);
    perm_box.set_tooltip_text(Some(
        "When on, the clear button deletes for good instead of moving to Trash.\n\
         Permanent delete is always available per item via right-click.",
    ));

    let reclaim_top = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    reclaim_top.append(&reclaim_head);
    reclaim_top.append(&perm_box);

    let reclaim_hint = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .label(
            "Each item shows what happens if you remove it. Most rebuild \
             automatically — give anything marked “Check first” a second look. \
             Moving to Trash is reversible; space is freed once the Trash is emptied.",
        )
        .build();
    reclaim_hint.add_css_class("dim-label");
    reclaim_hint.add_css_class("caption");

    // Batch-selection action bar — hidden until something is ticked.
    let reclaim_select_label = gtk::Label::builder().xalign(0.0).hexpand(true).build();
    reclaim_select_label.add_css_class("heading");
    let select_all_safe = gtk::Button::with_label("Select all Safe");
    select_all_safe.add_css_class("flat");
    let reclaim_clear_button = gtk::Button::with_label("Move to Trash");
    reclaim_clear_button.add_css_class("destructive-action");
    let reclaim_select_bar = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    reclaim_select_bar.add_css_class("card");
    reclaim_select_bar.set_margin_top(4);
    reclaim_select_bar.set_margin_bottom(4);
    {
        // Inner padding for the card.
        reclaim_select_bar.set_margin_start(0);
    }
    let select_bar_inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    select_bar_inner.set_margin_top(8);
    select_bar_inner.set_margin_bottom(8);
    select_bar_inner.set_margin_start(12);
    select_bar_inner.set_margin_end(12);
    select_bar_inner.set_hexpand(true);
    select_bar_inner.append(&reclaim_select_label);
    select_bar_inner.append(&select_all_safe);
    select_bar_inner.append(&reclaim_clear_button);
    reclaim_select_bar.append(&select_bar_inner);
    reclaim_select_bar.set_visible(false);

    let reclaim_system_caption =
        gtk::Label::builder().xalign(0.0).label("Safe to clear").build();
    reclaim_system_caption.add_css_class("dim-label");
    reclaim_system_caption.add_css_class("caption");
    let reclaim_system_list =
        gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    reclaim_system_list.add_css_class("boxed-list");

    let reclaim_artifact_caption =
        gtk::Label::builder().xalign(0.0).label("Regenerable in this folder").build();
    reclaim_artifact_caption.add_css_class("dim-label");
    reclaim_artifact_caption.add_css_class("caption");
    let reclaim_artifact_list =
        gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).build();
    reclaim_artifact_list.add_css_class("boxed-list");

    let reclaim_inner = gtk::Box::new(gtk::Orientation::Vertical, 10);
    reclaim_inner.set_margin_top(18);
    reclaim_inner.set_margin_bottom(18);
    reclaim_inner.set_margin_start(12);
    reclaim_inner.set_margin_end(12);
    reclaim_inner.append(&reclaim_top);
    reclaim_inner.append(&reclaim_hint);
    reclaim_inner.append(&reclaim_select_bar);
    reclaim_inner.append(&reclaim_system_caption);
    reclaim_inner.append(&reclaim_system_list);
    reclaim_inner.append(&reclaim_artifact_caption);
    reclaim_inner.append(&reclaim_artifact_list);

    let reclaim_page = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&adw::Clamp::builder().maximum_size(700).child(&reclaim_inner).build())
        .build();

    // --- Stack --------------------------------------------------------------
    let stack = gtk::Stack::new();
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&scanning, Some("scanning"));
    stack.add_named(&overview, Some("overview"));
    stack.add_named(&list_page, Some("list"));
    stack.add_named(&reclaim_page, Some("reclaim"));
    stack.set_visible_child_name("empty");

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&stack));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&search_bar);
    toolbar.set_content(Some(&toasts));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("DiskScope")
        .default_width(780)
        .default_height(600)
        .content(&toolbar)
        .build();

    let ui = Rc::new(Ui {
        window: window.clone(),
        toasts,
        stack,
        title,
        home_button: home_button.clone(),
        up_button: up_button.clone(),
        refresh_button: refresh_button.clone(),
        search_button: search_button.clone(),
        current_scan: Rc::new(RefCell::new(None)),
        scan_gen: Rc::new(Cell::new(0)),
        scan_detail,
        scan_path,
        overview_total,
        capacity_ring,
        capacity_percent,
        capacity_caption,
        capacity_data,
        overview_bar,
        legend,
        overview_list: overview_list.clone(),
        overview_data,
        crumbs,
        list: list.clone(),
        search_paths: Rc::new(RefCell::new(Vec::new())),
        reclaim_total,
        reclaim_perm_switch: reclaim_perm_switch.clone(),
        reclaim_select_bar,
        reclaim_select_label,
        reclaim_clear_button: reclaim_clear_button.clone(),
        reclaim_system_caption,
        reclaim_system_list: reclaim_system_list.clone(),
        reclaim_artifact_caption,
        reclaim_artifact_list: reclaim_artifact_list.clone(),
        reclaim_data: Rc::new(RefCell::new((Vec::new(), Vec::new()))),
        reclaim_gen: Rc::new(Cell::new(0)),
    });

    // --- Wiring -------------------------------------------------------------
    connect(&home_button, &state, &ui, |state, ui| {
        state.borrow_mut().view = View::Overview;
        render(state, ui);
    });
    connect(&up_button, &state, &ui, |state, ui| {
        let moved = {
            let mut s = state.borrow_mut();
            match &mut s.view {
                View::Folder(path) => path.pop().is_some(),
                _ => false,
            }
        };
        if moved {
            render(state, ui);
        }
    });
    connect(&refresh_button, &state, &ui, rescan_keeping_position);
    connect(&open_button, &state, &ui, open_dialog);
    connect(&empty_button, &state, &ui, open_dialog);
    connect(&browse_button, &state, &ui, |state, ui| {
        state.borrow_mut().view = View::Folder(Vec::new());
        render(state, ui);
    });
    connect(&reclaim_button, &state, &ui, |state, ui| {
        {
            let mut s = state.borrow_mut();
            s.reclaim_selected.clear(); // a fresh entry starts with nothing ticked
            s.view = View::Reclaim;
        }
        render(state, ui);
    });

    // Search: entering search mode shows the results page; leaving it returns to
    // the overview. Typing or changing the size filter re-runs the (instant,
    // in-memory) search and repaints the results.
    {
        let state = state.clone();
        let ui = ui.clone();
        let entry = search_entry.clone();
        search_bar.connect_search_mode_enabled_notify(move |bar| {
            if state.borrow().root.is_none() {
                return;
            }
            if bar.is_search_mode() {
                state.borrow_mut().view = View::Search;
            } else {
                let mut s = state.borrow_mut();
                s.search_query.clear();
                s.view = View::Overview;
                drop(s);
                entry.set_text("");
            }
            render(&state, &ui);
        });
    }
    {
        let state = state.clone();
        let ui = ui.clone();
        let size_filter = size_filter.clone();
        search_entry.connect_search_changed(move |entry| {
            let mut s = state.borrow_mut();
            if s.root.is_none() {
                return;
            }
            s.search_query = entry.text().to_string();
            s.search_min = size_threshold(size_filter.selected());
            s.view = View::Search;
            drop(s);
            render(&state, &ui);
        });
    }
    {
        let state = state.clone();
        let ui = ui.clone();
        size_filter.connect_selected_notify(move |dd| {
            let mut s = state.borrow_mut();
            if !matches!(s.view, View::Search) {
                return;
            }
            s.search_min = size_threshold(dd.selected());
            drop(s);
            render(&state, &ui);
        });
    }

    // Cancel button on the "Scanning…" page. If a folder scan is in flight, flip
    // its cancel flag — the worker returns `Interrupted` and its handler drops
    // back to the previous view. Otherwise (e.g. measuring reclaimable space,
    // which has no cancel flag) just leave the scanning page for the overview.
    {
        let state = state.clone();
        let ui = ui.clone();
        cancel_button.connect_clicked(move |_| {
            let cancelled = ui.current_scan.borrow().as_ref().map(CancelFlag::cancel).is_some();
            if !cancelled && state.borrow().root.is_some() {
                state.borrow_mut().view = View::Overview;
                render(&state, &ui);
            }
        });
    }

    // Delete-mode switch: rebuild the reclaim rows so each primary action button
    // reflects the new mode immediately (no rescan needed).
    {
        let state = state.clone();
        let ui = ui.clone();
        reclaim_perm_switch.connect_active_notify(move |sw| {
            let perm = sw.is_active();
            state.borrow_mut().reclaim_perm_delete = perm;
            populate_reclaim_lists(&state, &ui, perm);
        });
    }

    // "Select all Safe": tick every entry whose blast radius is Safe, then rebuild
    // the rows so the checkboxes reflect it.
    connect(&select_all_safe, &state, &ui, |state, ui| {
        {
            let data = ui.reclaim_data.borrow();
            let mut s = state.borrow_mut();
            for r in data.0.iter().chain(data.1.iter()) {
                if reclaim::consequence(r).risk == Risk::Safe {
                    s.reclaim_selected.insert(r.path.clone());
                }
            }
        }
        let perm = state.borrow().reclaim_perm_delete;
        populate_reclaim_lists(state, ui, perm);
    });

    // The batch clear button confirms once for everything selected, then clears.
    connect(&reclaim_clear_button, &state, &ui, actions::confirm_and_clear_selected);

    // Overview category row → open that category.
    {
        let state = state.clone();
        let ui = ui.clone();
        let data = ui.overview_data.clone();
        overview_list.connect_row_activated(move |_, row| {
            let index = row.index();
            if index < 0 {
                return;
            }
            if let Some((category, _)) = data.borrow().get(index as usize).copied() {
                state.borrow_mut().view = View::Category(category);
                render(&state, &ui);
            }
        });
    }

    // Folder list row → drill into a sub-folder.
    {
        let state = state.clone();
        let ui = ui.clone();
        list.connect_row_activated(move |_, row| {
            let index = row.index();
            if index < 0 {
                return;
            }
            let index = index as usize;
            let descend = {
                let s = state.borrow();
                match (&s.view, s.root.as_ref()) {
                    (View::Folder(path), Some(root)) => {
                        let folder = folder_node(root, path);
                        folder
                            .children
                            .get(index)
                            .filter(|c| c.is_dir)
                            .map(|_| {
                                let mut p = path.clone();
                                p.push(index);
                                p
                            })
                    }
                    // A directory search result jumps to that folder in the tree;
                    // file results aren't activatable (act via hover/menu instead).
                    (View::Search, Some(root)) => ui
                        .search_paths
                        .borrow()
                        .get(index)
                        .and_then(|target| locate(root, target))
                        .filter(|p| folder_node(root, p).is_dir),
                    _ => None,
                }
            };
            if let Some(path) = descend {
                ui.search_button.set_active(false);
                state.borrow_mut().view = View::Folder(path);
                render(&state, &ui);
            }
        });
    }

    // Type-to-search from anywhere in the window, plus an explicit Ctrl+F that
    // opens the search bar and focuses its entry.
    search_bar.set_key_capture_widget(Some(&window));
    {
        let search_button = search_button.clone();
        let search_entry = search_entry.clone();
        let shortcut = gtk::Shortcut::new(
            gtk::ShortcutTrigger::parse_string("<Control>f"),
            Some(gtk::CallbackAction::new(move |_, _| {
                if search_button.is_sensitive() {
                    search_button.set_active(true);
                    search_entry.grab_focus();
                }
                glib::Propagation::Stop
            })),
        );
        let controller = gtk::ShortcutController::new();
        controller.add_shortcut(shortcut);
        window.add_controller(controller);
    }

    window.present();

    // Scan the requested path, or fall back to the home directory.
    let target = initial.or_else(|| std::env::var("HOME").ok());
    if let Some(path) = target {
        let path = PathBuf::from(path);
        if path.exists() {
            start_scan(path, &state, &ui);
        }
    }

    maybe_capture(&window, app);
}

/// Map the size-filter dropdown's selected index to a minimum-size threshold in
/// bytes. Index 0 is "Any size" (no floor).
fn size_threshold(index: u32) -> u64 {
    match index {
        1 => 1 << 20,   // 1 MB
        2 => 10 << 20,  // 10 MB
        3 => 100 << 20, // 100 MB
        4 => 1 << 30,   // 1 GB
        _ => 0,
    }
}

/// A flat, icon-only header button.
fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).build();
    button.add_css_class("flat");
    button
}

/// Connect a button to a handler that receives the shared state and UI.
fn connect(
    button: &gtk::Button,
    state: &Rc<RefCell<AppState>>,
    ui: &Rc<Ui>,
    handler: impl Fn(&Rc<RefCell<AppState>>, &Rc<Ui>) + 'static,
) {
    let state = state.clone();
    let ui = ui.clone();
    button.connect_clicked(move |_| handler(&state, &ui));
}

/// Register the app's custom CSS once, for the whole display.
fn install_css() {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(CSS);
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}










































/// Remove every row from a list box.
fn clear(list: &gtk::ListBox) {
    while let Some(child) = list.first_child() {
        list.remove(&child);
    }
}

/// Remove every child from a box.
fn clear_box(container: &gtk::Box) {
    while let Some(child) = container.first_child() {
        container.remove(&child);
    }
}


/// Dev hook: if `DISKSCOPE_SHOT` is set, render the window to that PNG in-process
/// once it has settled, then quit. No effect on normal runs.
fn maybe_capture(window: &adw::ApplicationWindow, app: &adw::Application) {
    let Ok(path) = std::env::var("DISKSCOPE_SHOT") else {
        return;
    };
    let window = window.clone();
    let app = app.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(1200), move || {
        let (w, h) = (window.width().max(1), window.height().max(1));
        let paintable = gtk::WidgetPaintable::new(Some(&window));
        let snapshot = gtk::Snapshot::new();
        paintable.snapshot(&snapshot, w as f64, h as f64);
        if let (Some(node), Some(renderer)) =
            (snapshot.to_node(), window.native().and_then(|n| n.renderer()))
        {
            let texture = renderer.render_texture(&node, None);
            match texture.save_to_png(&path) {
                Ok(()) => eprintln!("saved screenshot to {path}"),
                Err(err) => eprintln!("screenshot failed: {err}"),
            }
        }
        app.quit();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::actions::{delete_path, show_row_menu};
    use super::draw::bucket_class;
    use super::rows::{build_row, category_row, reclaim_row};
    use super::scanning::locate;
    use super::views::populate;
    use diskscope::reclaim::ReclaimKind;
    use gtk::gio;
    use std::path::Path;

    fn row_count(list: &gtk::ListBox) -> usize {
        let mut n = 0;
        let mut child = list.first_child();
        while let Some(w) = child {
            n += 1;
            child = w.next_sibling();
        }
        n
    }

    fn node(name: &str, size: u64, is_dir: bool) -> Node {
        Node { name: name.into(), path: name.into(), size, is_dir, children: Vec::new() }
    }

    #[test]
    fn delete_path_removes_files_and_trees_but_only_empties_trash() {
        use std::fs;
        let tmp = tempfile::tempdir().unwrap();

        // A whole directory tree → removed outright.
        let tree = tmp.path().join("node_modules");
        fs::create_dir_all(tree.join("dep/sub")).unwrap();
        fs::write(tree.join("dep/file"), b"x").unwrap();
        delete_path(&tree).unwrap();
        assert!(!tree.exists(), "artifact directory should be gone");

        // A single file → removed.
        let file = tmp.path().join("blob");
        fs::write(&file, b"xyz").unwrap();
        delete_path(&file).unwrap();
        assert!(!file.exists(), "file should be gone");

        // The Trash spot → emptied, but the Trash directory itself kept.
        let trash = tmp.path().join("Trash");
        fs::create_dir_all(trash.join("files")).unwrap();
        fs::write(trash.join("files/old.bin"), vec![b'x'; 100]).unwrap();
        delete_path(&trash).unwrap();
        assert!(trash.is_dir(), "Trash root must remain");
        assert_eq!(fs::read_dir(&trash).unwrap().count(), 0, "Trash must be emptied");
    }

    #[test]
    fn reclaim_row_clear_actions_dispatch() {
        if gtk::init().is_err() {
            eprintln!("no display available — skipping GTK reclaim-row test");
            return;
        }
        use std::cell::Cell;
        let last = Rc::new(Cell::new(None::<RowAction>));
        let l = last.clone();
        let handler: Rc<dyn Fn(RowAction, PathBuf)> =
            Rc::new(move |action, _path| l.set(Some(action)));

        // An artifact in trash-default mode: the row resolves both removal modes.
        let item = Reclaimable {
            label: "Node.js packages".into(),
            path: "/p/node_modules".into(),
            size: 100,
            file_count: 42,
            kind: ReclaimKind::Artifact,
        };
        let on_select: Rc<dyn Fn(PathBuf, bool)> = Rc::new(|_, _| {});
        let row = reclaim_row(&item, false, false, &handler, &on_select);

        WidgetExt::activate_action(&row, "row.trash", None).unwrap();
        assert_eq!(last.get(), Some(RowAction::Trash));
        WidgetExt::activate_action(&row, "row.delete", None).unwrap();
        assert_eq!(last.get(), Some(RowAction::Delete));
    }

    #[test]
    fn bucket_class_maps_fraction_to_heat() {
        assert_eq!(bucket_class(0.9), "bucket-5");
        assert_eq!(bucket_class(0.30), "bucket-4");
        assert_eq!(bucket_class(0.15), "bucket-3");
        assert_eq!(bucket_class(0.05), "bucket-2");
        assert_eq!(bucket_class(0.001), "bucket-1");
    }

    /// Pump the GTK main loop until it runs dry (so popups map and deferred
    /// idle callbacks — like the popover's unparent — actually run).
    fn pump() {
        let ctx = glib::MainContext::default();
        for _ in 0..50 {
            if !ctx.iteration(false) {
                break;
            }
        }
    }

    /// Depth-first search for the first descendant of `widget` whose GType name
    /// matches `type_name` (e.g. "GtkModelButton" for a popover menu item).
    fn find_descendant(widget: &gtk::Widget, type_name: &str) -> Option<gtk::Widget> {
        let mut child = widget.first_child();
        while let Some(w) = child {
            if w.type_().name() == type_name {
                return Some(w);
            }
            if let Some(found) = find_descendant(&w, type_name) {
                return Some(found);
            }
            child = w.next_sibling();
        }
        None
    }

    #[test]
    fn context_menu_item_click_fires_handler() {
        if gtk::init().is_err() {
            eprintln!("no display available — skipping GTK context-menu test");
            return;
        }
        use std::cell::Cell;
        let fired = Rc::new(Cell::new(0u32));
        let f = fired.clone();
        let handler: Rc<dyn Fn(RowAction, PathBuf)> =
            Rc::new(move |_action, _path| f.set(f.get() + 1));

        // A real row, wired exactly as the app wires it (action group + gesture).
        let n = node("file", 10, false);
        let row = build_row(&n, 100, None, Some(&handler));

        // Sanity: the "row" action group resolves from the row itself.
        assert!(
            WidgetExt::activate_action(&row, "row.open", None).is_ok(),
            "row.open should resolve to an action"
        );

        // The row must live in a mapped window for the popover to pop up.
        let window = gtk::Window::new();
        let list = gtk::ListBox::new();
        list.append(&row);
        window.set_child(Some(&list));
        window.present();
        pump();

        // Pop the real context menu and *click* its first item the way a user
        // would — GTK closes the popover and dispatches the action. This is the
        // path that was silently failing.
        let menu = gio::Menu::new();
        menu.append(Some("Open"), Some("row.open"));
        let popover = show_row_menu(&row, &menu, 1.0, 1.0);
        pump();

        let item = find_descendant(popover.upcast_ref::<gtk::Widget>(), "GtkModelButton")
            .expect("popover should contain a GtkModelButton menu item");
        fired.set(0); // ignore the sanity activation above; count only the click
        item.activate();
        pump();

        assert_eq!(fired.get(), 1, "clicking the menu item should invoke the handler exactly once");

        window.destroy();
    }

    #[test]
    fn locate_finds_and_misses_paths() {
        let mut root = node("/r", 0, true);
        let mut sub = node("/r/sub", 0, true);
        sub.children = vec![node("/r/sub/leaf", 10, false)];
        root.children = vec![sub, node("/r/other", 5, false)];

        assert_eq!(locate(&root, Path::new("/r")), Some(vec![]));
        assert_eq!(locate(&root, Path::new("/r/sub")), Some(vec![0]));
        assert_eq!(locate(&root, Path::new("/r/sub/leaf")), Some(vec![0, 0]));
        assert_eq!(locate(&root, Path::new("/r/nope")), None);
    }

    #[test]
    fn renders_rows_from_a_scanned_tree() {
        if gtk::init().is_err() {
            eprintln!("no display available — skipping GTK render test");
            return;
        }

        let mut root = node("root", 0, true);
        root.children = vec![node("big", 900, true), node("small", 100, false)];
        root.size = 1000;

        let list = gtk::ListBox::new();
        populate(&list, &root, None);
        assert_eq!(row_count(&list), 2, "one row per child");
        assert!(list.row_at_index(0).unwrap().is_activatable(), "directory drills in");
        assert!(!list.row_at_index(1).unwrap().is_activatable(), "file does not");

        // A category row is always activatable (it opens the category).
        let cat = category_row(Category::Videos, 500, 1000);
        assert!(cat.is_activatable());

        // Empty directory → single non-activatable placeholder.
        populate(&list, &node("empty", 0, true), None);
        assert_eq!(row_count(&list), 1);
        assert!(!list.row_at_index(0).unwrap().is_activatable());
    }
}
