//! Text formatting for the result table and status line.

use everygrep_core::localtime::Zone;

use crate::i18n::tr;

/// Short month name for the Date column, 1 to 12.
fn month(m: u32) -> &'static str {
    match m {
        1 => tr("Jan"),
        2 => tr("Feb"),
        3 => tr("Mar"),
        4 => tr("Apr"),
        5 => tr("May"),
        6 => tr("Jun"),
        7 => tr("Jul"),
        8 => tr("Aug"),
        9 => tr("Sep"),
        10 => tr("Oct"),
        11 => tr("Nov"),
        _ => tr("Dec"),
    }
}

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
    let month = month(m);
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

/// Longest path, in characters, the status line shows in full.
pub const STATUS_PATH_MAX: usize = 60;

/// A path short enough for the status line: the home folder as `~`, and
/// when it is still longer than `max` characters, the start (`~` or `/`)
/// with only the last components, e.g. `~/…/Photos/2024/img.jpg`. Fewer
/// components are kept until it fits; the name always stays.
pub fn short_path(path: &str, home: Option<&str>, max: usize) -> String {
    let home = home.filter(|h| !h.is_empty() && *h != "/");
    let (start, rest) = match home.and_then(|h| path.strip_prefix(h)) {
        Some("") => return "~".to_owned(),
        Some(rest) if rest.starts_with('/') => ("~", &rest[1..]),
        _ => match path.strip_prefix('/') {
            Some(rest) => ("", rest),
            None => (".", path),
        },
    };
    let shown = match start {
        "~" => format!("~/{rest}"),
        _ => path.to_owned(),
    };
    if shown.chars().count() <= max {
        return shown;
    }
    let parts: Vec<&str> = rest.split('/').filter(|p| !p.is_empty()).collect();
    for keep in (1..=3).rev() {
        if parts.len() <= keep {
            continue;
        }
        let tail = parts[parts.len() - keep..].join("/");
        let short = match start {
            "." => format!("…/{tail}"),
            _ => format!("{start}/…/{tail}"),
        };
        if short.chars().count() <= max || keep == 1 {
            return short;
        }
    }
    shown
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

    #[test]
    fn long_paths_keep_the_start_and_the_last_components() {
        let home = Some("/home/me");
        assert_eq!(short_path("/home/me/a.txt", home, 60), "~/a.txt");
        assert_eq!(short_path("/home/me", home, 60), "~");
        assert_eq!(short_path("/home/meat/a", home, 60), "/home/meat/a");
        assert_eq!(short_path("/srv/a", None, 60), "/srv/a");
        let deep = "/home/me/one/two/three/four/five/six/seven/Photos/2024/img.jpg";
        assert_eq!(
            short_path(deep, home, 60),
            "~/one/two/three/four/five/six/seven/Photos/2024/img.jpg"
        );
        assert_eq!(short_path(deep, home, 30), "~/…/Photos/2024/img.jpg");
        assert_eq!(short_path(deep, None, 30), "/…/Photos/2024/img.jpg");
        // Fewer components until it fits; the name always stays.
        assert_eq!(short_path(deep, home, 18), "~/…/2024/img.jpg");
        assert_eq!(short_path(deep, home, 5), "~/…/img.jpg");
        let long_name = format!("/home/me/x/{}", "n".repeat(80));
        assert_eq!(
            short_path(&long_name, home, 60),
            format!("~/…/{}", "n".repeat(80))
        );
        // Characters, not bytes, are counted.
        let wide = format!("/home/me/{}", "ə".repeat(40));
        assert_eq!(short_path(&wide, home, 60), format!("~/{}", "ə".repeat(40)));
    }
}
