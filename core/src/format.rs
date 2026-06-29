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

/// Group a number with thousands separators, e.g. `1234567` → `"1,234,567"`.
///
/// ```
/// use diskscope::format::thousands;
/// assert_eq!(thousands(0), "0");
/// assert_eq!(thousands(42), "42");
/// assert_eq!(thousands(1_234_567), "1,234,567");
/// ```
pub fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
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
