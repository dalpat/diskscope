// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! Custom Cairo drawing for the overview: the segmented usage bar and the disk
//! capacity ring, plus the heat-map colour mapping shared with the row bars.

use diskscope::category::Category;
use diskscope::disk::DiskUsage;

/// Draw the segmented usage bar: one rounded, colour-coded slice per category.
pub(super) fn draw_segments(cr: &gtk::cairo::Context, w: i32, h: i32, totals: &[(Category, u64)]) {
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
pub(super) fn draw_ring(cr: &gtk::cairo::Context, w: i32, h: i32, usage: Option<DiskUsage>) {
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
pub(super) fn bucket_class(fraction: f64) -> &'static str {
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

/// RGB (0–1) accent colour for a category — matches the CSS classes in the view.
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
