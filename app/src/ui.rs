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
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use diskscope::category::{self, Category};
use diskscope::disk::{self, DiskUsage};
use diskscope::format::human_size;
use diskscope::reclaim::{self, ReclaimKind, Reclaimable, Risk};
use diskscope::scan::{self, Node};

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
}

impl Default for AppState {
    fn default() -> Self {
        AppState { root: None, view: View::Overview, reclaim_perm_delete: false }
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
    // Shared list page (folder + category).
    crumbs: gtk::Box,
    list: gtk::ListBox,
    // Reclaim page.
    reclaim_total: gtk::Label,
    reclaim_perm_switch: gtk::Switch,
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

    let title = adw::WindowTitle::new("DiskScope", "");
    let header = adw::HeaderBar::new();
    header.pack_start(&home_button);
    header.pack_start(&up_button);
    header.pack_end(&open_button);
    header.pack_end(&refresh_button);
    header.set_title_widget(Some(&title));

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

    let spinner = gtk::Spinner::builder().spinning(true).width_request(48).height_request(48).build();
    let scanning = adw::StatusPage::builder()
        .title("Scanning…")
        .description("Walking the directory tree.")
        .child(&spinner)
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
        reclaim_total,
        reclaim_perm_switch: reclaim_perm_switch.clone(),
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
        state.borrow_mut().view = View::Reclaim;
        render(state, ui);
    });

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
                    _ => None,
                }
            };
            if let Some(path) = descend {
                state.borrow_mut().view = View::Folder(path);
                render(&state, &ui);
            }
        });
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

/// Show a folder chooser and, on confirmation, scan it fresh.
fn open_dialog(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
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
fn start_scan(path: PathBuf, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    scan_into(path, Restore::Reset, state, ui);
}

/// Rescan the current root, returning to the same view afterwards.
fn rescan_keeping_position(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
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

    let (sender, receiver) = async_channel::bounded(1);
    let scan_path = root_path.clone();
    std::thread::spawn(move || {
        let _ = sender.send_blocking(scan::scan(&scan_path));
    });

    let state = state.clone();
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        let Ok(result) = receiver.recv().await else {
            return;
        };
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
                ui.toasts.add_toast(adw::Toast::new(&format!(
                    "Couldn't scan {}: {err}",
                    root_path.display()
                )));
            }
        }
    });
}

/// Walk `root` to the folder at `path` (assumes a valid path).
fn folder_node<'a>(root: &'a Node, path: &[usize]) -> &'a Node {
    let mut node = root;
    for &index in path {
        node = &node.children[index];
    }
    node
}

/// Find the drill-down index path from `root` to `target`, if it still exists.
fn locate(root: &Node, target: &Path) -> Option<Vec<usize>> {
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

/// Re-render everything from the current state.
fn render(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let s = state.borrow();
    let Some(root) = s.root.as_ref() else {
        ui.stack.set_visible_child_name("empty");
        return;
    };

    ui.home_button.set_sensitive(true);
    ui.refresh_button.set_sensitive(true);
    ui.up_button.set_sensitive(matches!(&s.view, View::Folder(p) if !p.is_empty()));

    match &s.view {
        View::Overview => render_overview(root, ui),
        View::Category(category) => render_category(root, *category, state, ui),
        View::Folder(path) => render_folder(root, path, state, ui),
        View::Reclaim => render_reclaim(root, state, ui, s.reclaim_perm_delete),
    }
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
        measured.sort_by(|a, b| b.size.cmp(&a.size));
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
fn populate_reclaim_lists(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>, perm: bool) {
    let handler = reclaim_action_handler(state, ui);
    let data = ui.reclaim_data.borrow();
    let (system, artifacts) = &*data;

    let total: u64 = system.iter().chain(artifacts).map(|r| r.size).sum();
    ui.reclaim_total.set_text(&human_size(total));

    clear(&ui.reclaim_system_list);
    clear(&ui.reclaim_artifact_list);

    ui.reclaim_system_caption.set_visible(!system.is_empty());
    ui.reclaim_system_list.set_visible(!system.is_empty());
    for r in system {
        ui.reclaim_system_list.append(&reclaim_row(r, perm, &handler));
    }

    ui.reclaim_artifact_caption.set_visible(!artifacts.is_empty());
    ui.reclaim_artifact_list.set_visible(!artifacts.is_empty());
    for r in artifacts {
        ui.reclaim_artifact_list.append(&reclaim_row(r, perm, &handler));
    }

    if system.is_empty() && artifacts.is_empty() {
        ui.reclaim_system_list.set_visible(true);
        ui.reclaim_system_list.append(&placeholder_row("Nothing to reclaim — you're all clean."));
    }
}

/// One reclaimable-entry row: kind icon, label over its path, recovered size, and
/// a primary clear button. Trash is always emptied permanently; caches/artifacts
/// move to Trash by default, or delete permanently when `perm` is set. The
/// right-click menu always offers both Trash and permanent delete.
fn reclaim_row(
    item: &Reclaimable,
    perm: bool,
    handler: &Rc<dyn Fn(RowAction, PathBuf)>,
) -> gtk::ListBoxRow {
    let is_trash = item.kind == ReclaimKind::Trash;
    // Emptying the Trash is inherently permanent; for the rest, honour the mode.
    let primary = if is_trash || perm { RowAction::Delete } else { RowAction::Trash };

    let icon = gtk::Image::from_icon_name(reclaim_icon(item.kind));
    icon.set_pixel_size(22);

    let name = gtk::Label::builder().label(&item.label).xalign(0.0).build();

    // Blast radius: a risk badge plus what actually breaks if you delete this.
    let consequence = reclaim::consequence(item);
    let badge = gtk::Label::new(Some(consequence.risk.word()));
    badge.add_css_class("caption");
    badge.add_css_class(risk_class(consequence.risk));
    let summary = gtk::Label::builder()
        .label(&consequence.summary)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .build();
    summary.add_css_class("dim-label");
    summary.add_css_class("caption");
    let impact = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    impact.append(&badge);
    impact.append(&summary);

    let name_area = gtk::Box::new(gtk::Orientation::Vertical, 1);
    name_area.set_hexpand(true);
    name_area.set_valign(gtk::Align::Center);
    name_area.append(&name);
    name_area.append(&impact);

    // "How big" sits on the right: recovered size over the file count.
    let size = gtk::Label::builder().label(human_size(item.size)).xalign(1.0).build();
    size.add_css_class("numeric");
    let count = gtk::Label::builder().label(files_phrase(item.file_count)).xalign(1.0).build();
    count.add_css_class("dim-label");
    count.add_css_class("caption");
    count.add_css_class("numeric");
    let size_area = gtk::Box::new(gtk::Orientation::Vertical, 1);
    size_area.set_valign(gtk::Align::Center);
    size_area.set_width_request(96);
    size_area.append(&size);
    size_area.append(&count);

    let (icon_name, tooltip) = match primary {
        RowAction::Delete if is_trash => ("user-trash-symbolic", "Empty (delete permanently)"),
        RowAction::Delete => ("edit-delete-symbolic", "Delete permanently"),
        _ => ("user-trash-symbolic", "Move to Trash"),
    };
    let clear = action_button(icon_name, tooltip, handler, primary, &item.path);
    if primary == RowAction::Delete {
        clear.add_css_class("destructive-action");
    }

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_start(12)
        .margin_end(8)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    row_box.append(&icon);
    row_box.append(&name_area);
    row_box.append(&size_area);
    row_box.append(&clear);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row.set_activatable(false);
    row.set_tooltip_text(Some(&item.path.display().to_string()));

    // Right-click menu: open/reveal plus both removal modes.
    install_row_actions(&row, &item.path, handler);
    let menu = gio::Menu::new();
    menu.append(Some("Open"), Some("row.open"));
    menu.append(Some("Open Containing Folder"), Some("row.reveal"));
    menu.append(Some("Copy Full Path"), Some("row.copy-path"));
    let remove = gio::Menu::new();
    if !is_trash {
        remove.append(Some("Move to Trash"), Some("row.trash"));
    }
    remove.append(
        Some(if is_trash { "Empty Trash" } else { "Delete Permanently" }),
        Some("row.delete"),
    );
    menu.append_section(None, &remove);
    attach_menu(&row, menu);

    row
}

/// Symbolic icon for a reclaimable entry, by kind.
fn reclaim_icon(kind: ReclaimKind) -> &'static str {
    match kind {
        ReclaimKind::Trash => "user-trash-full-symbolic",
        ReclaimKind::Cache => "folder-download-symbolic",
        ReclaimKind::Artifact => "folder-symbolic",
    }
}

/// CSS accent class for a deletion risk level.
fn risk_class(risk: Risk) -> &'static str {
    match risk {
        Risk::Safe => "risk-safe",
        Risk::Rebuild => "risk-rebuild",
        Risk::Caution => "risk-caution",
    }
}

/// "1 file" / "1,234 files" — a grammatical, thousands-grouped count.
fn files_phrase(n: u64) -> String {
    let mut grouped = String::new();
    let digits = n.to_string();
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            grouped.push(',');
        }
        grouped.push(ch);
    }
    format!("{grouped} {}", if n == 1 { "file" } else { "files" })
}

/// Reclaim-view action dispatcher. Open / Reveal / Copy behave as elsewhere, but
/// the removal actions route through [`confirm_clear`], which shows the blast
/// radius (which app, how many files, how big, what happens) before committing.
fn reclaim_action_handler(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) -> Rc<dyn Fn(RowAction, PathBuf)> {
    let state = state.clone();
    let ui = ui.clone();
    Rc::new(move |action, path| match action {
        RowAction::Open => open_path(&path, &ui),
        RowAction::Reveal => reveal_path(&path, &ui),
        RowAction::Terminal => open_terminal(&path, &ui),
        RowAction::CopyPath => copy_path(&path, &ui),
        RowAction::Trash => confirm_clear(&path, false, &state, &ui),
        RowAction::Delete => confirm_clear(&path, true, &state, &ui),
    })
}

/// Show the blast radius for clearing `path`, then (on confirm) clear it.
///
/// `perm` requests a permanent delete; the Trash spot is always permanent. The
/// dialog spells out the impact — which app/area, file count, size — and what
/// actually happens, drawing the count/size from the already-measured data so it
/// never re-scans the disk while the user is deciding.
fn confirm_clear(path: &Path, perm: bool, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    // Recover the measured entry (label, size, count, kind) for this row.
    let item = {
        let data = ui.reclaim_data.borrow();
        data.0.iter().chain(data.1.iter()).find(|r| r.path == path).cloned()
    };
    let Some(item) = item else {
        return; // data changed under us; ignore the stale click
    };

    let permanent = perm || item.kind == ReclaimKind::Trash;
    let heading = match (permanent, item.kind) {
        (_, ReclaimKind::Trash) => "Empty the Trash?".to_string(),
        (true, _) => format!("Permanently delete {}?", item.label),
        (false, _) => format!("Move {} to Trash?", item.label),
    };

    // The blast radius: what breaks if this goes.
    let consequence = reclaim::consequence(&item);
    let reversibility = if permanent {
        "This frees the space now and cannot be undone."
    } else {
        "Moving to Trash is reversible; the space is freed once you empty the Trash."
    };

    let body = format!(
        "{}\n\nFrees {} across {}.\n\nWhat happens — {}: {}\n\n{reversibility}",
        item.path.display(),
        human_size(item.size),
        files_phrase(item.file_count),
        consequence.risk.word(),
        consequence.summary,
    );

    let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
    let confirm_label = if permanent { "Delete" } else { "Move to Trash" };
    dialog.add_responses(&[("cancel", "Cancel"), ("go", confirm_label)]);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let window = ui.window.clone();
    let state = state.clone();
    let ui = ui.clone();
    let path = path.to_path_buf();
    dialog.choose(&window, gio::Cancellable::NONE, move |response| {
        if response != "go" {
            return;
        }
        let result = if permanent {
            delete_path(&path)
        } else {
            gio::File::for_path(&path).trash(gio::Cancellable::NONE).map_err(|e| {
                std::io::Error::other(e.message().to_string())
            })
        };
        match result {
            Ok(()) => {
                ui.toasts.add_toast(adw::Toast::new(if permanent {
                    "Deleted — space freed"
                } else {
                    "Moved to Trash"
                }));
                rescan_keeping_position(&state, &ui);
            }
            Err(err) => {
                ui.toasts.add_toast(adw::Toast::new(&format!("Couldn't clear: {err}")));
            }
        }
    });
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

/// Build the dispatcher for per-row actions (trailing buttons + context menu).
fn row_action_handler(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) -> Rc<dyn Fn(RowAction, PathBuf)> {
    let state = state.clone();
    let ui = ui.clone();
    Rc::new(move |action, path| match action {
        RowAction::Open => open_path(&path, &ui),
        RowAction::Reveal => reveal_path(&path, &ui),
        RowAction::Terminal => open_terminal(&path, &ui),
        RowAction::CopyPath => copy_path(&path, &ui),
        RowAction::Trash => confirm_and_trash(path, &state, &ui),
        RowAction::Delete => confirm_and_delete(path, &state, &ui),
    })
}

/// Install the per-row "row.*" action group on `row`, one action per
/// [`RowAction`], each bound to this row's `path`. Menus (file or reclaim) then
/// reference whichever subset they choose to show.
fn install_row_actions(row: &gtk::ListBoxRow, path: &Path, handler: &Rc<dyn Fn(RowAction, PathBuf)>) {
    let actions = gio::SimpleActionGroup::new();
    for (name, action) in [
        ("open", RowAction::Open),
        ("reveal", RowAction::Reveal),
        ("terminal", RowAction::Terminal),
        ("copy-path", RowAction::CopyPath),
        ("trash", RowAction::Trash),
        ("delete", RowAction::Delete),
    ] {
        let item = gio::SimpleAction::new(name, None);
        let handler = handler.clone();
        let path = path.to_path_buf();
        item.connect_activate(move |_, _| handler(action, path.clone()));
        actions.add_action(&item);
    }
    row.insert_action_group("row", Some(&actions));
}

/// Make a secondary (right) click on `row` pop `menu` up at the pointer.
fn attach_menu(row: &gtk::ListBoxRow, menu: gio::Menu) {
    let gesture = gtk::GestureClick::new();
    gesture.set_button(gtk::gdk::BUTTON_SECONDARY);
    let row_weak = row.downgrade();
    gesture.connect_pressed(move |gesture, _, x, y| {
        gesture.set_state(gtk::EventSequenceState::Claimed);
        if let Some(row) = row_weak.upgrade() {
            show_row_menu(&row, &menu, x, y);
        }
    });
    row.add_controller(gesture);
}

/// Attach the standard file/folder right-click menu (Open / Reveal / Terminal /
/// Copy Path / Move to Trash), backed by the row's "row.*" action group.
fn attach_context_menu(row: &gtk::ListBoxRow, path: &Path, handler: &Rc<dyn Fn(RowAction, PathBuf)>) {
    install_row_actions(row, path, handler);

    let menu = gio::Menu::new();
    menu.append(Some("Open"), Some("row.open"));
    menu.append(Some("Open Containing Folder"), Some("row.reveal"));
    menu.append(Some("Open Terminal Here"), Some("row.terminal"));
    menu.append(Some("Copy Full Path"), Some("row.copy-path"));
    let trash_section = gio::Menu::new();
    trash_section.append(Some("Move to Trash"), Some("row.trash"));
    menu.append_section(None, &trash_section);

    attach_menu(row, menu);
}

/// Pop the context `menu` up over `row`, pointing at `(x, y)`.
///
/// Returns the popover so callers (and tests) can inspect it. The popover is
/// parented to `row` so its menu items resolve the row's "row.*" action group;
/// on dismissal it unparents itself. Crucially the unparent is **deferred to an
/// idle callback** rather than run synchronously inside the "closed" handler:
/// clicking a menu item closes the popover *before* GTK dispatches the item's
/// action, so unparenting in-line would sever the action group from the widget
/// tree and the click would silently do nothing.
fn show_row_menu(row: &gtk::ListBoxRow, menu: &gio::Menu, x: f64, y: f64) -> gtk::PopoverMenu {
    let popover = gtk::PopoverMenu::from_model(Some(menu));
    popover.set_parent(row);
    popover.set_has_arrow(false);
    popover.set_halign(gtk::Align::Start);
    popover.set_pointing_to(Some(&gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1)));
    popover.connect_closed(|popover| {
        let popover = popover.clone();
        glib::idle_add_local_once(move || popover.unparent());
    });
    popover.popup();
    popover
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
fn populate(list: &gtk::ListBox, node: &Node, handler: Option<&Rc<dyn Fn(RowAction, PathBuf)>>) {
    clear(list);
    let total = node.size.max(1);
    for child in &node.children {
        list.append(&build_row(child, total, None, handler));
    }
    if node.children.is_empty() {
        list.append(&placeholder_row("This folder is empty."));
    }
}

/// One file/folder row: icon, name (optionally over a `location` line), a
/// colour-coded bar, size, percentage, and — when `handler` is set — hover
/// Open / Trash buttons plus a right-click context menu.
fn build_row(
    node: &Node,
    total: u64,
    location: Option<&str>,
    handler: Option<&Rc<dyn Fn(RowAction, PathBuf)>>,
) -> gtk::ListBoxRow {
    let fraction = node.size as f64 / total as f64;
    let percent = (fraction * 100.0).round() as u64;

    let icon = gtk::Image::from_icon_name(if node.is_dir {
        "folder-symbolic"
    } else {
        "text-x-generic-symbolic"
    });

    let name = gtk::Label::builder()
        .label(&node.name)
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();

    // Filename, optionally over a dim "where it lives" line — used in category
    // lists, where files can come from anywhere in the tree.
    let name_area = gtk::Box::new(gtk::Orientation::Vertical, 0);
    name_area.set_hexpand(true);
    name_area.set_valign(gtk::Align::Center);
    name_area.append(&name);
    if let Some(location) = location {
        let loc = gtk::Label::builder()
            .label(location)
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .build();
        loc.add_css_class("dim-label");
        loc.add_css_class("caption");
        name_area.append(&loc);
    }

    let bar = gtk::ProgressBar::builder()
        .fraction(fraction)
        .width_request(150)
        .valign(gtk::Align::Center)
        .build();
    bar.add_css_class(bucket_class(fraction));

    let size = gtk::Label::builder().label(human_size(node.size)).width_chars(10).xalign(1.0).build();
    size.add_css_class("numeric");

    let percent_label =
        gtk::Label::builder().label(format!("{percent}%")).width_chars(4).xalign(1.0).build();
    percent_label.add_css_class("dim-label");
    percent_label.add_css_class("numeric");

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_start(12)
        .margin_end(8)
        .margin_top(8)
        .margin_bottom(8)
        .build();
    row_box.append(&icon);
    row_box.append(&name_area);
    row_box.append(&bar);
    row_box.append(&size);
    row_box.append(&percent_label);

    // Trailing actions fade in on hover. They always occupy their space and only
    // their opacity changes, so revealing them never reflows the row — that keeps
    // the destructive button discreet at rest without any layout flicker.
    let actions = handler.map(|handler| {
        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        actions.add_css_class("row-actions");
        actions.set_opacity(0.0);
        actions.append(&action_button("external-link-symbolic", "Open", handler, RowAction::Open, &node.path));
        actions.append(&action_button("user-trash-symbolic", "Move to Trash", handler, RowAction::Trash, &node.path));
        actions
    });
    if let Some(actions) = &actions {
        row_box.append(actions);
    }

    // A chevron marks the rows you can drill into (directories), matching the
    // category list; files get none.
    if node.is_dir {
        let chevron = gtk::Image::from_icon_name("go-next-symbolic");
        chevron.add_css_class("dim-label");
        row_box.append(&chevron);
    }

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row.set_activatable(node.is_dir);
    // The full path on hover answers "where is this?" everywhere.
    row.set_tooltip_text(Some(&node.path.display().to_string()));

    if let Some(handler) = handler {
        attach_context_menu(&row, &node.path, handler);
    }

    if let Some(actions) = actions {
        let motion = gtk::EventControllerMotion::new();
        let enter = actions.clone();
        motion.connect_enter(move |_, _, _| enter.set_opacity(1.0));
        motion.connect_leave(move |_| actions.set_opacity(0.0));
        row.add_controller(motion);
    }
    row
}

/// A category row for the overview: accent icon, name, size, percentage, chevron.
fn category_row(category: Category, bytes: u64, used: u64) -> gtk::ListBoxRow {
    let percent = (bytes as f64 / used as f64 * 100.0).round() as u64;

    let icon = gtk::Image::from_icon_name(category_icon(category));
    icon.add_css_class(category_css_class(category));
    icon.set_pixel_size(22);

    let name = gtk::Label::builder().label(category.label()).xalign(0.0).hexpand(true).build();

    let size = gtk::Label::builder().label(human_size(bytes)).xalign(1.0).build();
    size.add_css_class("numeric");

    let percent_label =
        gtk::Label::builder().label(format!("{percent}%")).width_chars(4).xalign(1.0).build();
    percent_label.add_css_class("dim-label");
    percent_label.add_css_class("numeric");

    let chevron = gtk::Image::from_icon_name("go-next-symbolic");
    chevron.add_css_class("dim-label");

    let row_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .margin_start(12)
        .margin_end(12)
        .margin_top(12)
        .margin_bottom(12)
        .build();
    row_box.append(&icon);
    row_box.append(&name);
    row_box.append(&size);
    row_box.append(&percent_label);
    row_box.append(&chevron);

    let row = gtk::ListBoxRow::new();
    row.set_child(Some(&row_box));
    row.set_activatable(true);
    row
}

/// A flat, icon-only trailing button that fires `action` for `path`.
fn action_button(
    icon: &str,
    tooltip: &str,
    handler: &Rc<dyn Fn(RowAction, PathBuf)>,
    action: RowAction,
    path: &Path,
) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk::Align::Center);

    let handler = handler.clone();
    let path = path.to_path_buf();
    button.connect_clicked(move |_| handler(action, path.clone()));
    button
}

/// Draw the segmented usage bar: one rounded, colour-coded slice per category.
fn draw_segments(cr: &gtk::cairo::Context, w: i32, h: i32, totals: &[(Category, u64)]) {
    let (w, h) = (w as f64, h as f64);
    let total: u64 = totals.iter().map(|(_, b)| b).sum();

    rounded_rect(cr, 0.0, 0.0, w, h, h / 2.0);
    cr.clip();

    // Track behind the segments.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.08);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    if total == 0 {
        return;
    }
    let mut x = 0.0;
    for (category, bytes) in totals {
        let seg = (*bytes as f64 / total as f64) * w;
        let (r, g, b) = category_color(*category);
        cr.set_source_rgb(r, g, b);
        // Slight overdraw avoids hairline gaps between slices.
        cr.rectangle(x, 0.0, seg + 0.7, h);
        let _ = cr.fill();
        x += seg;
    }
}

/// Draw the disk capacity ring: a full track plus a used arc, coloured by how
/// full the disk is (green → amber → red).
fn draw_ring(cr: &gtk::cairo::Context, w: i32, h: i32, usage: Option<DiskUsage>) {
    use std::f64::consts::PI;
    let (w, h) = (w as f64, h as f64);
    let (cx, cy) = (w / 2.0, h / 2.0);
    let thickness = 12.0;
    let radius = (w.min(h) / 2.0) - thickness / 2.0 - 1.0;

    cr.set_line_width(thickness);
    cr.set_line_cap(gtk::cairo::LineCap::Round);

    // Track.
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.12);
    cr.arc(cx, cy, radius, 0.0, 2.0 * PI);
    let _ = cr.stroke();

    let Some(usage) = usage.filter(|u| u.total > 0) else {
        return;
    };
    let fraction = (usage.used() as f64 / usage.total as f64).clamp(0.0, 1.0);
    // Below ~0.5% the round-capped arc collapses to a lone dot floating on an
    // otherwise empty ring; show just the clean track instead.
    if fraction < 0.005 {
        return;
    }
    let (r, g, b) = if fraction < 0.75 {
        (0.18, 0.76, 0.49) // healthy — green
    } else if fraction < 0.90 {
        (0.90, 0.65, 0.04) // getting full — amber
    } else {
        (0.88, 0.11, 0.14) // nearly full — red
    };
    cr.set_source_rgb(r, g, b);
    let start = -PI / 2.0;
    cr.arc(cx, cy, radius, start, start + fraction * 2.0 * PI);
    let _ = cr.stroke();
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

/// Trace a rounded rectangle path (no fill).
fn rounded_rect(cr: &gtk::cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    use std::f64::consts::PI;
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

/// CSS bucket for a usage fraction, mapping size to a heat colour.
fn bucket_class(fraction: f64) -> &'static str {
    if fraction >= 0.50 {
        "bucket-5"
    } else if fraction >= 0.25 {
        "bucket-4"
    } else if fraction >= 0.10 {
        "bucket-3"
    } else if fraction >= 0.03 {
        "bucket-2"
    } else {
        "bucket-1"
    }
}

/// Symbolic icon name for a category.
fn category_icon(category: Category) -> &'static str {
    match category {
        Category::Videos => "video-x-generic-symbolic",
        Category::Audio => "audio-x-generic-symbolic",
        Category::Images => "image-x-generic-symbolic",
        Category::Documents => "x-office-document-symbolic",
        Category::Archives => "package-x-generic-symbolic",
        Category::Code => "text-x-script-symbolic",
        Category::Applications => "application-x-executable-symbolic",
        Category::Other => "application-x-generic-symbolic",
    }
}

/// CSS accent class for a category (sets the icon colour).
fn category_css_class(category: Category) -> &'static str {
    match category {
        Category::Videos => "cat-videos",
        Category::Audio => "cat-audio",
        Category::Images => "cat-images",
        Category::Documents => "cat-documents",
        Category::Archives => "cat-archives",
        Category::Code => "cat-code",
        Category::Applications => "cat-applications",
        Category::Other => "cat-other",
    }
}

/// RGB (0–1) accent colour for a category — matches the CSS classes above.
fn category_color(category: Category) -> (f64, f64, f64) {
    let (r, g, b) = match category {
        Category::Videos => (224, 27, 36),
        Category::Audio => (145, 65, 172),
        Category::Images => (230, 97, 0),
        Category::Documents => (28, 113, 216),
        Category::Archives => (152, 106, 68),
        Category::Code => (46, 194, 126),
        Category::Applications => (229, 165, 10),
        Category::Other => (119, 118, 123),
    };
    (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0)
}

/// Open a file/folder with the desktop's default handler.
fn open_path(path: &Path, ui: &Rc<Ui>) {
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let window = ui.window.clone();
    let ui = ui.clone();
    launcher.launch(Some(&window), gio::Cancellable::NONE, move |result| {
        if let Err(err) = result {
            ui.toasts.add_toast(adw::Toast::new(&format!("Couldn't open: {err}")));
        }
    });
}

/// Reveal `path` in the file manager — open its containing folder with the item
/// selected, so you can see exactly where it lives.
fn reveal_path(path: &Path, ui: &Rc<Ui>) {
    let launcher = gtk::FileLauncher::new(Some(&gio::File::for_path(path)));
    let window = ui.window.clone();
    let ui = ui.clone();
    launcher.open_containing_folder(Some(&window), gio::Cancellable::NONE, move |result| {
        if let Err(err) = result {
            ui.toasts.add_toast(adw::Toast::new(&format!("Couldn't reveal: {err}")));
        }
    });
}

/// Copy `path`'s absolute location to the clipboard.
fn copy_path(path: &Path, ui: &Rc<Ui>) {
    ui.window.clipboard().set_text(&path.to_string_lossy());
    ui.toasts.add_toast(adw::Toast::new("Path copied to clipboard"));
}

/// Open a terminal at `path`'s folder (the folder itself if it is a directory,
/// otherwise its parent), trying common terminal emulators in turn.
fn open_terminal(path: &Path, ui: &Rc<Ui>) {
    let dir = if path.is_dir() {
        path.to_path_buf()
    } else {
        path.parent().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"))
    };
    const TERMINALS: [&str; 9] = [
        "x-terminal-emulator", // Debian/Ubuntu's configured default
        "gnome-terminal",
        "kgx", // GNOME Console
        "konsole",
        "xfce4-terminal",
        "kitty",
        "alacritty",
        "foot",
        "xterm",
    ];
    for terminal in TERMINALS {
        if std::process::Command::new(terminal).current_dir(&dir).spawn().is_ok() {
            return;
        }
    }
    ui.toasts.add_toast(adw::Toast::new("No terminal emulator found"));
}

/// Confirm, then move `path` to Trash and rescan to reflect freed space.
fn confirm_and_trash(path: PathBuf, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let dialog = adw::AlertDialog::new(
        Some("Move to Trash?"),
        Some(&format!("“{name}” will be moved to the Trash.")),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("trash", "Move to Trash")]);
    dialog.set_response_appearance("trash", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let window = ui.window.clone();
    let state = state.clone();
    let ui = ui.clone();
    dialog.choose(&window, gio::Cancellable::NONE, move |response| {
        if response == "trash" {
            match gio::File::for_path(&path).trash(gio::Cancellable::NONE) {
                Ok(()) => {
                    ui.toasts.add_toast(adw::Toast::new("Moved to Trash"));
                    rescan_keeping_position(&state, &ui);
                }
                Err(err) => {
                    ui.toasts.add_toast(adw::Toast::new(&format!("Couldn't move to Trash: {err}")));
                }
            }
        }
    });
}

/// Confirm, then **permanently** delete `path` and rescan. Unlike trashing, this
/// frees the space immediately and cannot be undone — used by the Reclaim view.
fn confirm_and_delete(path: PathBuf, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    let dialog = adw::AlertDialog::new(
        Some("Delete permanently?"),
        Some(&format!(
            "“{name}” and everything inside it will be permanently deleted. \
             This cannot be undone."
        )),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("delete", "Delete")]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let window = ui.window.clone();
    let state = state.clone();
    let ui = ui.clone();
    dialog.choose(&window, gio::Cancellable::NONE, move |response| {
        if response == "delete" {
            match delete_path(&path) {
                Ok(()) => {
                    ui.toasts.add_toast(adw::Toast::new("Deleted — space freed"));
                    rescan_keeping_position(&state, &ui);
                }
                Err(err) => {
                    ui.toasts.add_toast(adw::Toast::new(&format!("Couldn't delete: {err}")));
                }
            }
        }
    });
}

/// Permanently remove `path`, freeing its space.
///
/// For the Trash spot, deleting the directory itself would remove the user's
/// Trash root; its *contents* are cleared instead. Otherwise the entry is
/// removed outright — a whole directory tree, or a single file.
fn delete_path(path: &Path) -> std::io::Result<()> {
    if path.ends_with("Trash") {
        empty_dir(path)
    } else if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    }
}

/// Remove every entry inside `dir`, leaving the directory itself in place.
fn empty_dir(dir: &Path) -> std::io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
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

/// A non-interactive, centered placeholder row.
fn placeholder_row(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder().label(text).margin_top(24).margin_bottom(24).build();
    label.add_css_class("dim-label");

    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_child(Some(&label));
    row
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
        let row = reclaim_row(&item, false, &handler);

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
