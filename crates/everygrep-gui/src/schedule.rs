//! When the folders to index are scanned again from scratch by themselves
//! (Settings → Indexing → Rescan). Watching keeps the index current; a
//! rescan also finds what no event reported, such as a file whose contents
//! changed without touching its folder.

use everygrep_core::localtime::Zone;

/// Most hours between two rescans (a week).
pub const MAX_HOURS: u32 = 7 * 24;

const DAY: i64 = 86_400;

/// How often the folders to index are scanned again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Schedule {
    /// Only when asked for (Rebuild index now) or needed (lost events).
    #[default]
    Never,
    /// This many hours (1 to [`MAX_HOURS`]) after the last full scan or
    /// scheduled attempt or cancellation, or program start before any of them.
    EveryHours(u32),
    /// Every day when the local clock passes this minute of the day
    /// (0 to 1439). A time passed while the computer slept or the program
    /// did not run is caught up at the next check.
    Daily(u32),
}

impl Schedule {
    /// The ids of the kinds in the settings file, in the order of the
    /// settings window's choice.
    pub const KINDS: [&'static str; 3] = ["never", "hours", "daily"];

    /// The kind's position in [`KINDS`](Self::KINDS).
    pub fn kind(self) -> usize {
        match self {
            Schedule::Never => 0,
            Schedule::EveryHours(_) => 1,
            Schedule::Daily(_) => 2,
        }
    }

    /// Whether a rescan is due at `now`, when the last full scan, scheduled
    /// attempt/cancellation (or start) was at `last_scan`; last checked at
    /// `checked`; all in seconds since 1970.
    pub fn due(self, last_scan: i64, checked: i64, now: i64, zone: &Zone) -> bool {
        match self {
            Schedule::Never => false,
            Schedule::EveryHours(h) => now.saturating_sub(last_scan) >= i64::from(h.max(1)) * 3600,
            Schedule::Daily(minute) => {
                // Days counted from the chosen time of day: the count grows
                // each time the local clock passes it.
                let at = i64::from(minute) * 60;
                let day = |t: i64| (zone.to_local(t) - at).div_euclid(DAY);
                now > checked && day(now) > day(checked) && day(now) > day(last_scan)
            }
        }
    }
}

/// `HH:MM` for a minute of the day.
pub fn time_text(minute: u32) -> String {
    format!("{:02}:{:02}", minute / 60 % 24, minute % 60)
}

/// The minute of the day of `H:MM` or `HH:MM` (24-hour clock).
pub fn parse_time(text: &str) -> Option<u32> {
    let (h, m) = text.trim().split_once(':')?;
    if h.is_empty() || h.len() > 2 || m.len() != 2 {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3600;

    #[test]
    fn never_is_never_due() {
        let zone = Zone::utc();
        assert!(!Schedule::Never.due(0, 0, 100 * DAY, &zone));
    }

    #[test]
    fn hours_count_from_the_last_scan() {
        let zone = Zone::utc();
        let s = Schedule::EveryHours(6);
        let last = 10 * DAY;
        assert!(!s.due(last, last, last + 6 * HOUR - 1, &zone));
        assert!(s.due(last, last, last + 6 * HOUR, &zone));
        assert!(s.due(last, last, last + 30 * HOUR, &zone));
        // A clock set back never makes it due.
        assert!(!s.due(last, last, last - DAY, &zone));
    }

    #[test]
    fn daily_is_due_once_when_the_local_time_passes() {
        // UTC+4, as Baku.
        let zone = Zone::from_posix_tz("<+04>-4").unwrap();
        let s = Schedule::Daily(3 * 60);
        // 2026-11-02 02:59 local = 22:59 UTC the day before.
        let midnight_utc = 20_394 * DAY;
        let local = |h: i64, m: i64| midnight_utc + h * HOUR + m * 60 - 4 * HOUR;
        let last = local(1, 0);
        assert!(!s.due(last, local(2, 0), local(2, 59), &zone));
        assert!(s.due(last, local(2, 59), local(3, 0), &zone));
        // Checked again after it ran: not due until the next day.
        assert!(!s.due(local(3, 0), local(3, 0), local(3, 1), &zone));
        assert!(!s.due(local(3, 0), local(3, 1), local(23, 59), &zone));
        assert!(s.due(local(3, 0), local(23, 59), local(24 + 3, 0), &zone));
        // Asleep over the time: due at the first check after waking.
        assert!(s.due(last, local(2, 30), local(9, 15), &zone));
        // Rebuilt by hand after the time today: not due again today.
        assert!(!s.due(local(3, 30), local(2, 30), local(9, 15), &zone));
    }

    #[test]
    fn times_are_read_and_written() {
        assert_eq!(parse_time("3:05"), Some(185));
        assert_eq!(parse_time("03:05"), Some(185));
        assert_eq!(parse_time(" 23:59 "), Some(1439));
        assert_eq!(parse_time("00:00"), Some(0));
        for bad in ["24:00", "12:60", "12", "1:5", "123:00", ":30", "a:bc", ""] {
            assert_eq!(parse_time(bad), None, "{bad}");
        }
        assert_eq!(time_text(185), "03:05");
        assert_eq!(time_text(0), "00:00");
    }
}
