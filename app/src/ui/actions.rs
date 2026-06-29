// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! Row actions: opening, revealing, copying paths, trashing and permanently
//! deleting entries, the right-click menus that invoke them, and the
//! confirmation dialogs (including the reclaim blast-radius prompt).

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use diskscope::format::human_size;
use diskscope::reclaim::{self, Reclaimable, ReclaimKind};
use diskscope::scan;

use super::rows::files_phrase;
use super::views::{populate_reclaim_lists, render, update_reclaim_selection_bar};
use super::{AppState, RowAction, Ui};

/// Build the dispatcher for per-row actions (trailing buttons + context menu).
pub(super) fn row_action_handler(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) -> Rc<dyn Fn(RowAction, PathBuf)> {
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

/// Reclaim-view action dispatcher. Open / Reveal / Copy behave as elsewhere, but
/// the removal actions route through [`confirm_clear`], which shows the blast
/// radius (which app, how many files, how big, what happens) before committing.
pub(super) fn reclaim_action_handler(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) -> Rc<dyn Fn(RowAction, PathBuf)> {
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

/// Build the handler a reclaim row's checkbox calls when toggled: add or remove
/// the entry's path from the selection, then refresh the batch-selection bar.
pub(super) fn reclaim_select_handler(
    state: &Rc<RefCell<AppState>>,
    ui: &Rc<Ui>,
) -> Rc<dyn Fn(PathBuf, bool)> {
    let state = state.clone();
    let ui = ui.clone();
    Rc::new(move |path, selected| {
        {
            let mut s = state.borrow_mut();
            if selected {
                s.reclaim_selected.insert(path);
            } else {
                s.reclaim_selected.remove(&path);
            }
        }
        update_reclaim_selection_bar(&state, &ui);
    })
}

/// Confirm clearing everything currently ticked in the Reclaim view in one
/// reviewed action, then remove each — patching the tree and measured data in
/// place (no rescan). Each item still uses its own removal mode: the Trash is
/// always emptied permanently, others honour the delete-mode switch.
pub(super) fn confirm_and_clear_selected(state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    let perm = state.borrow().reclaim_perm_delete;
    let items: Vec<Reclaimable> = {
        let s = state.borrow();
        let data = ui.reclaim_data.borrow();
        data.0
            .iter()
            .chain(data.1.iter())
            .filter(|r| s.reclaim_selected.contains(&r.path))
            .cloned()
            .collect()
    };
    if items.is_empty() {
        return;
    }

    let count = items.len();
    let bytes: u64 = items.iter().map(|r| r.size).sum();
    let mut listing: String = items
        .iter()
        .take(8)
        .map(|r| format!("• {} — {}", r.label, human_size(r.size)))
        .collect::<Vec<_>>()
        .join("\n");
    if count > 8 {
        listing.push_str(&format!("\n• …and {} more", count - 8));
    }
    let reversibility = if perm {
        "Permanently deletes the selected items now — this cannot be undone."
    } else {
        "Selected items move to Trash (reversible); the Trash itself is emptied for good."
    };
    let heading = format!("Clear {count} selected item{}?", if count == 1 { "" } else { "s" });
    let body = format!("Frees {} in total.\n\n{listing}\n\n{reversibility}", human_size(bytes));

    let dialog = adw::AlertDialog::new(Some(&heading), Some(&body));
    let confirm_label = if perm { "Delete All" } else { "Move All to Trash" };
    dialog.add_responses(&[("cancel", "Cancel"), ("go", confirm_label)]);
    dialog.set_response_appearance("go", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");

    let window = ui.window.clone();
    let state = state.clone();
    let ui = ui.clone();
    dialog.choose(&window, gio::Cancellable::NONE, move |response| {
        if response != "go" {
            return;
        }
        let (mut freed, mut failed) = (0u64, 0u32);
        for item in &items {
            let permanent = perm || item.kind == ReclaimKind::Trash;
            let result = if permanent {
                delete_path(&item.path)
            } else {
                gio::File::for_path(&item.path)
                    .trash(gio::Cancellable::NONE)
                    .map_err(|e| std::io::Error::other(e.message().to_string()))
            };
            match result {
                Ok(()) => {
                    freed += item.size;
                    if let Some(root) = state.borrow_mut().root.as_mut() {
                        scan::remove(root, &item.path);
                    }
                    let mut data = ui.reclaim_data.borrow_mut();
                    data.0.retain(|r| r.path != item.path);
                    data.1.retain(|r| r.path != item.path);
                }
                Err(_) => failed += 1,
            }
        }
        state.borrow_mut().reclaim_selected.clear();
        let perm_now = state.borrow().reclaim_perm_delete;
        populate_reclaim_lists(&state, &ui, perm_now);

        let message = if failed == 0 {
            format!("Cleared {count} — {} freed", human_size(freed))
        } else {
            format!("Cleared {} — {} freed; {failed} couldn't be removed", count - failed as usize, human_size(freed))
        };
        ui.toasts.add_toast(adw::Toast::new(&message));
    });
}

/// Patch the in-memory tree to drop `path`, then re-render the current view —
/// the instant alternative to a full rescan after a delete or trash. The bytes
/// are already gone from disk (or moved to Trash), so fixing up the cached tree
/// keeps the view correct without re-walking it. A `path` outside the scanned
/// root is simply a no-op.
fn prune_and_render(path: &Path, state: &Rc<RefCell<AppState>>, ui: &Rc<Ui>) {
    {
        let mut s = state.borrow_mut();
        if let Some(root) = s.root.as_mut() {
            scan::remove(root, path);
        }
    }
    render(state, ui);
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
                // Patch the tree and the already-measured reclaim data in place:
                // the cleared row drops out instantly, with no full rescan and no
                // re-measuring of the caches/Trash behind the spinner.
                {
                    let mut s = state.borrow_mut();
                    if let Some(root) = s.root.as_mut() {
                        scan::remove(root, &path);
                    }
                }
                {
                    let mut data = ui.reclaim_data.borrow_mut();
                    data.0.retain(|r| r.path != path);
                    data.1.retain(|r| r.path != path);
                }
                let perm_mode = state.borrow().reclaim_perm_delete;
                populate_reclaim_lists(&state, &ui, perm_mode);
            }
            Err(err) => {
                ui.toasts.add_toast(adw::Toast::new(&format!("Couldn't clear: {err}")));
            }
        }
    });
}

/// Install the per-row "row.*" action group on `row`, one action per
/// [`RowAction`], each bound to this row's `path`. Menus (file or reclaim) then
/// reference whichever subset they choose to show.
pub(super) fn install_row_actions(row: &gtk::ListBoxRow, path: &Path, handler: &Rc<dyn Fn(RowAction, PathBuf)>) {
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
pub(super) fn attach_menu(row: &gtk::ListBoxRow, menu: gio::Menu) {
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
pub(super) fn attach_context_menu(row: &gtk::ListBoxRow, path: &Path, handler: &Rc<dyn Fn(RowAction, PathBuf)>) {
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
pub(super) fn show_row_menu(row: &gtk::ListBoxRow, menu: &gio::Menu, x: f64, y: f64) -> gtk::PopoverMenu {
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
                    prune_and_render(&path, &state, &ui);
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
                    prune_and_render(&path, &state, &ui);
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
pub(super) fn delete_path(path: &Path) -> std::io::Result<()> {
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
