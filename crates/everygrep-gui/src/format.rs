//! Text formatting for the result table and status line.

use std::sync::atomic::{AtomicU8, Ordering};

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

/// The unit sizes are shown in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SizeUnit {
    /// Bytes below one kilobyte, otherwise the largest unit that keeps the
    /// number at 1 or more (the format before the setting existed).
    #[default]
    Auto,
    Bytes,
    Kilo,
    Mega,
    Giga,
}

impl SizeUnit {
    pub const ALL: [SizeUnit; 5] = [
        SizeUnit::Auto,
        SizeUnit::Bytes,
        SizeUnit::Kilo,
        SizeUnit::Mega,
        SizeUnit::Giga,
    ];

    /// The word in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            SizeUnit::Auto => "auto",
            SizeUnit::Bytes => "bytes",
            SizeUnit::Kilo => "kb",
            SizeUnit::Mega => "mb",
            SizeUnit::Giga => "gb",
        }
    }

    pub fn from_id(id: &str) -> Option<SizeUnit> {
        SizeUnit::ALL.into_iter().find(|u| u.id() == id)
    }
}

/// How big a kilobyte is and how the units are written.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SizeStandard {
    /// 1 KB = 1024 bytes (the format before the setting existed).
    #[default]
    Jedec,
    /// 1 KiB = 1024 bytes.
    Iec,
    /// 1 KB = 1000 bytes.
    Metric,
}

impl SizeStandard {
    pub const ALL: [SizeStandard; 3] =
        [SizeStandard::Jedec, SizeStandard::Iec, SizeStandard::Metric];

    /// The word in the settings file.
    pub fn id(self) -> &'static str {
        match self {
            SizeStandard::Jedec => "jedec",
            SizeStandard::Iec => "iec",
            SizeStandard::Metric => "metric",
        }
    }

    pub fn from_id(id: &str) -> Option<SizeStandard> {
        SizeStandard::ALL.into_iter().find(|s| s.id() == id)
    }

    fn base(self) -> u64 {
        match self {
            SizeStandard::Jedec | SizeStandard::Iec => 1024,
            SizeStandard::Metric => 1000,
        }
    }

    fn units(self) -> [&'static str; 5] {
        match self {
            SizeStandard::Iec => ["B", "KiB", "MiB", "GiB", "TiB"],
            SizeStandard::Jedec | SizeStandard::Metric => ["B", "KB", "MB", "GB", "TB"],
        }
    }
}

/// The size format of the window: a unit and a standard.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SizeStyle {
    pub unit: SizeUnit,
    pub standard: SizeStandard,
}

impl SizeStyle {
    fn pack(self) -> u8 {
        (self.unit as u8) << 4 | self.standard as u8
    }

    fn unpack(v: u8) -> SizeStyle {
        SizeStyle {
            unit: SizeUnit::ALL
                .get(usize::from(v >> 4))
                .copied()
                .unwrap_or_default(),
            standard: SizeStandard::ALL
                .get(usize::from(v & 0xf))
                .copied()
                .unwrap_or_default(),
        }
    }
}

/// The size format [`size`] uses, packed (see [`SizeStyle::pack`]).
static STYLE: AtomicU8 = AtomicU8::new(0);

/// Shows sizes in `style` from now on.
pub fn set_size_style(style: SizeStyle) {
    STYLE.store(style.pack(), Ordering::Relaxed);
}

/// The size format in use.
pub fn size_style() -> SizeStyle {
    SizeStyle::unpack(STYLE.load(Ordering::Relaxed))
}

/// File size in the size format of the window (by default 1024 steps:
/// `512 B`, `3.4 KB`, `120 MB`).
pub fn size(bytes: u64) -> String {
    size_in(bytes, size_style())
}

/// File size in `style`. Auto picks the unit; a fixed unit always uses it,
/// with one decimal below 10 and grouped digits above (`1 234 KB`), and
/// an empty file is `0` of it.
pub fn size_in(bytes: u64, style: SizeStyle) -> String {
    let base = style.standard.base();
    let units = style.standard.units();
    let fixed = match style.unit {
        SizeUnit::Auto => None,
        SizeUnit::Bytes => return format!("{} B", count_u64(bytes)),
        SizeUnit::Kilo => Some(1),
        SizeUnit::Mega => Some(2),
        SizeUnit::Giga => Some(3),
    };
    if let Some(unit) = fixed {
        let value = bytes as f64 / (base as f64).powi(unit);
        return if bytes == 0 {
            format!("0 {}", units[unit as usize])
        } else if value < 10.0 {
            format!("{value:.1} {}", units[unit as usize])
        } else {
            format!(
                "{} {}",
                count_u64(value.round() as u64),
                units[unit as usize]
            )
        };
    }
    if bytes < base {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= base as f64 && unit < units.len() - 1 {
        value /= base as f64;
        unit += 1;
    }
    if value < 10.0 {
        format!("{value:.1} {}", units[unit])
    } else {
        format!("{value:.0} {}", units[unit])
    }
}

/// [`count`] for a `u64`.
fn count_u64(n: u64) -> String {
    usize::try_from(n).map_or_else(|_| n.to_string(), count)
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

/// A compact path that preserves the final component. For an unusually long
/// filename, retain both its beginning and its end (including its extension).
pub fn middle_path(path: &str, max: usize) -> String {
    let count = path.chars().count();
    if count <= max {
        return path.into();
    }
    if max == 0 {
        return String::new();
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    let n = name.chars().count();
    if n + 2 <= max {
        let prefix: String = path.chars().take(max - n - 2).collect();
        return format!("{prefix}…/{name}");
    }
    let start = (max - 1) / 2;
    let end = max - 1 - start;
    format!(
        "{}…{}",
        name.chars().take(start).collect::<String>(),
        name.chars().skip(n - end).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_paths_keep_the_filename_and_elide_the_middle() {
        assert_eq!(middle_path("/short/file.txt", 30), "/short/file.txt");
        assert_eq!(
            middle_path("/very/long/folder/report.txt", 20),
            "/very/lo…/report.txt"
        );
        assert_eq!(middle_path("/uzun/qovluq/şəkil.txt", 12), "/…/şəkil.txt");
        assert_eq!(
            middle_path("/dir/very-long-filename.txt", 12),
            "very-…me.txt"
        );
        for limit in 0..30 {
            let text = middle_path("/qovluq/şəkillər/çox-uzun-ad.txt", limit);
            assert!(text.chars().count() <= limit);
        }
    }

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
    fn the_default_style_is_the_old_format() {
        let d = SizeStyle::default();
        for bytes in [
            0,
            1,
            1023,
            1024,
            1536,
            10 << 10,
            3 << 20,
            120 << 20,
            5 << 40,
        ] {
            assert_eq!(size_in(bytes, d), size(bytes), "{bytes}");
        }
        assert_eq!(SizeStyle::unpack(d.pack()), d);
        for unit in SizeUnit::ALL {
            for standard in SizeStandard::ALL {
                let s = SizeStyle { unit, standard };
                assert_eq!(SizeStyle::unpack(s.pack()), s);
                assert_eq!(SizeUnit::from_id(unit.id()), Some(unit));
                assert_eq!(SizeStandard::from_id(standard.id()), Some(standard));
            }
        }
    }

    #[test]
    fn sizes_follow_the_unit_and_the_standard() {
        let style = |unit, standard| SizeStyle { unit, standard };
        use SizeStandard::{Iec, Jedec, Metric};
        use SizeUnit::{Auto, Bytes, Giga, Kilo, Mega};
        // Auto: the unit grows with the size, in steps of the standard.
        assert_eq!(size_in(1023, style(Auto, Iec)), "1023 B");
        assert_eq!(size_in(1536, style(Auto, Iec)), "1.5 KiB");
        assert_eq!(size_in(3 << 20, style(Auto, Iec)), "3.0 MiB");
        assert_eq!(size_in(999, style(Auto, Metric)), "999 B");
        assert_eq!(size_in(1000, style(Auto, Metric)), "1.0 KB");
        assert_eq!(size_in(1023, style(Auto, Metric)), "1.0 KB");
        assert_eq!(size_in(120_000_000, style(Auto, Metric)), "120 MB");
        // Fixed units, digits grouped by thin spaces.
        assert_eq!(
            size_in(1_234_567, style(Bytes, Jedec)),
            "1\u{2009}234\u{2009}567 B"
        );
        assert_eq!(size_in(0, style(Kilo, Jedec)), "0 KB");
        assert_eq!(size_in(512, style(Kilo, Jedec)), "0.5 KB");
        assert_eq!(size_in(5 << 20, style(Kilo, Jedec)), "5\u{2009}120 KB");
        assert_eq!(size_in(5 << 20, style(Kilo, Iec)), "5\u{2009}120 KiB");
        assert_eq!(size_in(5_000_000, style(Kilo, Metric)), "5\u{2009}000 KB");
        assert_eq!(size_in(3 << 20, style(Mega, Jedec)), "3.0 MB");
        assert_eq!(size_in(1 << 30, style(Mega, Iec)), "1\u{2009}024 MiB");
        assert_eq!(size_in(1 << 20, style(Giga, Jedec)), "0.0 GB");
        assert_eq!(size_in(15_000_000_000, style(Giga, Metric)), "15 GB");
        assert_eq!(
            size_in(u64::MAX, style(Bytes, Jedec)),
            "18\u{2009}446\u{2009}744\u{2009}073\u{2009}709\u{2009}551\u{2009}615 B"
        );
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
