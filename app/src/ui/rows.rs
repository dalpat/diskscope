// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! Row builders: the file/folder rows, category rows, and reclaimable-entry
//! rows, plus their small presentation helpers (icons, risk badge, counts).

use std::path::{Path, PathBuf};
use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;

use diskscope::category::Category;
use diskscope::format::human_size;
use diskscope::reclaim::{self, ReclaimKind, Reclaimable, Risk};
use diskscope::scan::Node;

use super::actions::{attach_context_menu, attach_menu, install_row_actions};
use super::draw::bucket_class;
use super::RowAction;

/// One file/folder row: icon, name (optionally over a `location` line), a
/// colour-coded bar, size, percentage, and — when `handler` is set — hover
/// Open / Trash buttons plus a right-click context menu.
pub(super) fn build_row(
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
pub(super) fn category_row(category: Category, bytes: u64, used: u64) -> gtk::ListBoxRow {
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

/// One reclaimable-entry row: kind icon, label over its path, recovered size, and
/// a primary clear button. Trash is always emptied permanently; caches/artifacts
/// move to Trash by default, or delete permanently when `perm` is set. The
/// right-click menu always offers both Trash and permanent delete.
pub(super) fn reclaim_row(
    item: &Reclaimable,
    perm: bool,
    selected: bool,
    handler: &Rc<dyn Fn(RowAction, PathBuf)>,
    on_select: &Rc<dyn Fn(PathBuf, bool)>,
) -> gtk::ListBoxRow {
    let is_trash = item.kind == ReclaimKind::Trash;
    // Emptying the Trash is inherently permanent; for the rest, honour the mode.
    let primary = if is_trash || perm { RowAction::Delete } else { RowAction::Trash };

    // Leading checkbox feeds the batch-selection bar.
    let check = gtk::CheckButton::builder().active(selected).valign(gtk::Align::Center).build();
    {
        let on_select = on_select.clone();
        let path = item.path.clone();
        check.connect_toggled(move |c| on_select(path.clone(), c.is_active()));
    }

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
    row_box.append(&check);
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
pub(super) fn files_phrase(n: u64) -> String {
    format!("{} {}", diskscope::format::thousands(n), if n == 1 { "file" } else { "files" })
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
pub(super) fn category_css_class(category: Category) -> &'static str {
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

/// A non-interactive, centered placeholder row.
pub(super) fn placeholder_row(text: &str) -> gtk::ListBoxRow {
    let label = gtk::Label::builder().label(text).margin_top(24).margin_bottom(24).build();
    label.add_css_class("dim-label");

    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_child(Some(&label));
    row
}
