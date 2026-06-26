// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 Dalpat Singh

//! Human-readable formatting helpers.

/// Format a byte count as a human-readable string using binary (1024) units.
///
/// Whole bytes are shown without a decimal; larger units use one decimal place.
///
/// ```
/// use diskscope::format::human_size;
/// assert_eq!(human_size(0), "0 B");
/// assert_eq!(human_size(512), "512 B");
/// assert_eq!(human_size(1024), "1.0 KB");
/// assert_eq!(human_size(1_572_864), "1.5 MB");
/// ```
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{size:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::human_size;

    #[test]
    fn formats_bytes_without_decimals() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(1), "1 B");
        assert_eq!(human_size(1023), "1023 B");
    }

    #[test]
    fn formats_each_binary_unit() {
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1_572_864), "1.5 MB");
        assert_eq!(human_size(1024 * 1024 * 1024), "1.0 GB");
        assert_eq!(human_size(1024u64.pow(4)), "1.0 TB");
        assert_eq!(human_size(1024u64.pow(5)), "1.0 PB");
    }

    #[test]
    fn saturates_at_largest_unit() {
        // Far beyond a petabyte still reports in PB rather than panicking.
        assert!(human_size(u64::MAX).ends_with(" PB"));
    }
}
