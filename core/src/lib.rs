//! diskscope core — pure, GUI-independent disk-usage logic.
//!
//! This crate deliberately has **no GTK dependency**. The directory-scanning
//! engine and value formatting live here so they can be tested end to end
//! against real temporary directory trees, with no display server involved.
//! The GTK front-end (the binary target) is a thin layer over this API.

pub mod category;
pub mod disk;
pub mod format;
pub mod reclaim;
pub mod scan;
