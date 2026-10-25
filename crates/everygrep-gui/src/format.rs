//! Text formatting for the result table and status line.

use everygrep_core::localtime::Zone;

const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Groups digits with thin spaces: `617178` → `617 178`.
pub fn count(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3 * 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

/// File size with 1024 steps: `512 B`, `3.4 KB`, `120 MB`.
pub fn size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// Short local date: `25 Oct` in the current year, otherwise `25 Oct 2025`.
/// `None` for an unknown time (0).
pub fn date(mtime_ns: i64, now_ns: i64, zone: &Zone) -> String {
    if mtime_ns == 0 {
        return String::new();
    }
    let (y, m, d) = zone.local_date(mtime_ns.div_euclid(1_000_000_000));
    let (now_y, _, _) = zone.local_date(now_ns.div_euclid(1_000_000_000));
    let month = MONTHS[(m as usize).clamp(1, 12) - 1];
    if y == now_y {
        format!("{d} {month}")
    } else {
        format!("{d} {month} {y}")
    }
}

/// Full local timestamp for tooltips: `2026-10-25 22:41`.
pub fn timestamp(mtime_ns: i64, zone: &Zone) -> String {
    if mtime_ns == 0 {
        return String::new();
    }
    let secs = mtime_ns.div_euclid(1_000_000_000);
    let (y, m, d) = zone.local_date(secs);
    let local = zone.to_local(secs).rem_euclid(86_400);
    format!(
        "{y}-{m:02}-{d:02} {:02}:{:02}",
        local / 3600,
        local % 3600 / 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1000), "1\u{2009}000");
        assert_eq!(count(617_178), "617\u{2009}178");
        assert_eq!(count(1_234_567), "1\u{2009}234\u{2009}567");
    }

    #[test]
    fn sizes_use_1024_steps() {
        assert_eq!(size(0), "0 B");
        assert_eq!(size(1023), "1023 B");
        assert_eq!(size(1024), "1.0 KB");
        assert_eq!(size(3 << 20), "3.0 MB");
        assert_eq!(size(120 << 20), "120 MB");
        assert_eq!(size(u64::MAX), "16777216 TB");
    }

    #[test]
    fn dates_drop_the_current_year() {
        let utc = Zone::utc();
        let day = 86_400_000_000_000i64;
        // 2026-10-25 00:00 UTC
        let t = 20_751 * day;
        assert_eq!(date(t, t, &utc), "25 Oct");
        assert_eq!(date(t - 365 * day, t, &utc), "25 Oct 2025");
        assert_eq!(date(0, t, &utc), "");
        assert_eq!(
            timestamp(t + 3_600_000_000_000 * 22 + 41 * 60_000_000_000, &utc),
            "2026-10-25 22:41"
        );
    }
}
