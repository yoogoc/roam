//! Display formatting for entry metadata.
//!
//! Lives in core rather than the view layer because it is pure and worth
//! testing without a window. The placeholder for missing metadata is a single
//! em dash — a listing that omitted size must not render as "0 B".

use jiff::{Timestamp, Zoned};

pub const MISSING: &str = "—";

/// Human-readable byte count, binary units.
pub fn size(bytes: Option<u64>) -> String {
    let Some(bytes) = bytes else {
        return MISSING.to_string();
    };

    const UNITS: [&str; 6] = ["B", "KB", "MB", "GB", "TB", "PB"];
    const STEP: f64 = 1024.0;

    if bytes < 1024 {
        return format!("{bytes} B");
    }

    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= STEP && unit < UNITS.len() - 1 {
        value /= STEP;
        unit += 1;
    }

    if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Local-time modification stamp, minute precision.
pub fn modified(ts: Option<Timestamp>) -> String {
    match ts {
        Some(ts) => Zoned::new(ts, jiff::tz::TimeZone::system())
            .strftime("%Y-%m-%d %H:%M")
            .to_string(),
        None => MISSING.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_size_is_a_dash_not_zero() {
        assert_eq!(size(None), MISSING);
        assert_eq!(size(Some(0)), "0 B");
    }

    #[test]
    fn sizes_scale_through_units() {
        assert_eq!(size(Some(1)), "1 B");
        assert_eq!(size(Some(1023)), "1023 B");
        assert_eq!(size(Some(1024)), "1.0 KB");
        assert_eq!(size(Some(1536)), "1.5 KB");
        assert_eq!(size(Some(1024 * 1024)), "1.0 MB");
        assert_eq!(size(Some(5 * 1024 * 1024 * 1024)), "5.0 GB");
    }

    #[test]
    fn large_values_in_a_unit_drop_the_decimal() {
        // Keeps the column narrow: "150 MB", not "150.0 MB".
        assert_eq!(size(Some(150 * 1024 * 1024)), "150 MB");
    }

    #[test]
    fn unknown_mtime_is_a_dash() {
        assert_eq!(modified(None), MISSING);
    }

    #[test]
    fn known_mtime_renders_to_minute_precision() {
        let ts: Timestamp = "2026-02-26T13:58:00Z".parse().unwrap();
        let rendered = modified(Some(ts));
        assert_eq!(rendered.len(), "2026-02-26 13:58".len(), "got {rendered}");
    }
}
