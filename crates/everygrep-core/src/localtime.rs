//! Local calendar time from the system time zone.
//!
//! Zones are read from TZif files (RFC 8536, versions 1 to 3; later versions are
//! read the same way). When a file has the 64-bit data block of version 2 and
//! later, that block is used and the 32-bit one is skipped.
//!
//! [`Zone::local`] honours `$TZ`: `TZ=Europe/Paris` reads
//! `/usr/share/zoneinfo/Europe/Paris`, `TZ=:/path` or `TZ=/path` reads that file,
//! and a POSIX string such as `TZ=CET-1CEST,M3.5.0,M10.5.0/3` is evaluated
//! directly. Without `$TZ` it reads `/etc/localtime`. When nothing can be read
//! the zone is UTC.
//!
//! Times after the last transition in the file are evaluated with the POSIX TZ
//! string from the file's footer. Version 1 files have no footer; for them (and
//! when a footer cannot be parsed) the offset of the last transition is used.
//! Leap second records are skipped, so files from the `right/` tree are off by
//! the accumulated leap seconds.
//!
//! Parsing never panics: malformed input gives `None`.

use std::io::Read;
use std::path::Path;

const DAY: i64 = 86_400;

/// Offsets beyond this range are rejected (RFC 8536 allows -89999..=93599).
const OFFSET_RANGE: std::ops::RangeInclusive<i64> = -89_999..=93_599;

/// Times are clamped to about ±1 million years before evaluating POSIX rules,
/// which keeps all calendar arithmetic far away from overflow.
const RULE_LIMIT_SECS: i64 = 1 << 45;

/// Years accepted by [`Zone::local_midnight_to_unix`]; others are clamped.
const YEAR_LIMIT: i64 = 1_000_000;

/// Largest zone file read from disk.
const MAX_FILE_BYTES: u64 = 1 << 20;

/// A time zone: UTC offsets over time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Zone {
    /// Transition instants in Unix seconds, ascending.
    transitions: Vec<i64>,
    /// UTC offset in seconds that starts at the transition with the same index.
    offsets: Vec<i32>,
    /// Offset before the first transition (local time type 0).
    initial: i32,
    /// Rule for times after the last transition (all times when there are none).
    rule: Option<Rule>,
}

/// A POSIX TZ string such as `CET-1CEST,M3.5.0,M10.5.0/3`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Rule {
    Fixed(i32),
    Dst {
        std: i32,
        dst: i32,
        start: RuleDate,
        /// Local (standard) time of day of the start, in seconds.
        start_time: i32,
        end: RuleDate,
        /// Local (daylight) time of day of the end, in seconds.
        end_time: i32,
    },
}

/// Day on which a POSIX rule switches.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RuleDate {
    /// `Jn`: day 1..=365, February 29 is never counted.
    Julian1(u16),
    /// `n`: day 0..=365, February 29 is counted in leap years.
    Julian0(u16),
    /// `Mm.w.d`: weekday `d` (0 = Sunday) of week `w` (5 = last) of month `m`.
    MonthWeekDay { month: u8, week: u8, weekday: u8 },
}

impl Zone {
    /// Coordinated Universal Time.
    pub fn utc() -> Self {
        Self::fixed(0)
    }

    fn fixed(offset: i32) -> Self {
        Self {
            transitions: Vec::new(),
            offsets: Vec::new(),
            initial: offset,
            rule: None,
        }
    }

    /// The system zone: `$TZ` when set, else `/etc/localtime`, else UTC.
    pub fn local() -> Self {
        match std::env::var_os("TZ") {
            None => Self::from_file(Path::new("/etc/localtime")),
            Some(tz) => tz.to_str().and_then(Self::from_tz_var),
        }
        .unwrap_or_else(Self::utc)
    }

    /// Interprets a `$TZ` value.
    fn from_tz_var(tz: &str) -> Option<Self> {
        if let Some(path) = tz.strip_prefix(':') {
            return Self::from_tz_name(path);
        }
        if tz.is_empty() {
            return Some(Self::utc());
        }
        Self::from_tz_name(tz).or_else(|| Self::from_posix_tz(tz))
    }

    /// Reads an absolute zone file path or a name below `/usr/share/zoneinfo`.
    fn from_tz_name(name: &str) -> Option<Self> {
        if name.is_empty() {
            return Some(Self::utc());
        }
        if name.starts_with('/') {
            return Self::from_file(Path::new(name));
        }
        if name.split('/').any(|part| part == "..") {
            return None;
        }
        Self::from_file(&Path::new("/usr/share/zoneinfo").join(name))
    }

    fn from_file(path: &Path) -> Option<Self> {
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(MAX_FILE_BYTES)
            .read_to_end(&mut bytes)
            .ok()?;
        Self::from_tzif(&bytes)
    }

    /// Parses the contents of a TZif file. Returns `None` when they are malformed.
    pub fn from_tzif(bytes: &[u8]) -> Option<Self> {
        let mut r = Reader { bytes, pos: 0 };
        let (version, v1) = read_header(&mut r)?;
        if version == 0 {
            return read_block(&mut r, &v1, 4);
        }
        r.take(v1.block_len(4)?)?;
        let (_, v2) = read_header(&mut r)?;
        let mut zone = read_block(&mut r, &v2, 8)?;
        zone.rule = read_footer(r.rest());
        Some(zone)
    }

    /// Parses a POSIX TZ string such as `<+04>-4` or `CET-1CEST,M3.5.0,M10.5.0/3`.
    pub fn from_posix_tz(tz: &str) -> Option<Self> {
        let rule = parse_rule(tz)?;
        let mut zone = Self::fixed(0);
        zone.rule = Some(rule);
        Some(zone)
    }

    /// UTC offset in seconds (east of Greenwich is positive) at `unix_secs`.
    pub fn offset_at_utc(&self, unix_secs: i64) -> i32 {
        let idx = self.transitions.partition_point(|&t| t <= unix_secs);
        match (idx, &self.rule) {
            (i, Some(rule)) if i == self.transitions.len() => rule.offset_at(unix_secs),
            (0, _) => self.initial,
            (i, _) => self.offsets[i - 1],
        }
    }

    /// Local wall clock time at `unix_secs`, as seconds since 1970-01-01 00:00 local.
    pub fn to_local(&self, unix_secs: i64) -> i64 {
        unix_secs.saturating_add(i64::from(self.offset_at_utc(unix_secs)))
    }

    /// Local calendar date `(year, month, day)` at `unix_secs`.
    pub fn local_date(&self, unix_secs: i64) -> (i64, u32, u32) {
        civil_from_days(self.to_local(unix_secs).div_euclid(DAY))
    }

    /// Start of the local day `y-m-d` in Unix seconds.
    ///
    /// This is the earliest instant whose local time is 00:00 on that day; when
    /// a transition skips 00:00, it is the first existing moment after it.
    /// Months and days outside their range roll over as in [`days_from_civil`];
    /// years are clamped to ±1,000,000.
    pub fn local_midnight_to_unix(&self, y: i64, m: u32, d: u32) -> i64 {
        self.local_time_to_unix(y, m, d, 0)
    }

    /// The instant whose local time is `secs` seconds after the start of the
    /// local date `y-m-d` on the wall clock (`secs` below 86 400), in Unix
    /// seconds. A local time that occurs twice (when clocks go back) is its
    /// earlier instant; one that is skipped (when clocks go forward) is the
    /// first existing moment after it. Dates roll over and years are clamped
    /// as in [`Zone::local_midnight_to_unix`].
    pub fn local_time_to_unix(&self, y: i64, m: u32, d: u32, secs: i64) -> i64 {
        let y = y.clamp(-YEAR_LIMIT, YEAR_LIMIT);
        self.local_to_unix(days_from_civil(y, m, d) * DAY + secs)
    }

    /// Earliest instant whose local time is `local`, or the first instant after
    /// it when `local` falls into a gap. Assumes at most one transition within a
    /// day of `local`.
    fn local_to_unix(&self, local: i64) -> i64 {
        let before = self.offset_at_utc(local.saturating_sub(DAY));
        let after = self.offset_at_utc(local.saturating_add(DAY));
        let mut candidates = vec![before, after];
        for off in [before, after] {
            candidates.push(self.offset_at_utc(local.saturating_sub(i64::from(off))));
        }
        let exact = candidates
            .iter()
            .map(|&off| local.saturating_sub(i64::from(off)))
            .filter(|&t| self.to_local(t) == local)
            .min();
        if let Some(t) = exact {
            return t;
        }
        // In a gap: the first instant whose local time is at or past `local`.
        let (lo_off, hi_off) = (before.max(after), before.min(after));
        let mut lo = local.saturating_sub(i64::from(lo_off));
        let mut hi = local.saturating_sub(i64::from(hi_off));
        if self.to_local(hi) < local {
            return local.saturating_sub(i64::from(before));
        }
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.to_local(mid) >= local {
                hi = mid;
            } else {
                lo = mid + 1;
            }
        }
        lo
    }
}

impl Rule {
    fn offset_at(&self, unix_secs: i64) -> i32 {
        let (std, dst, start, start_time, end, end_time) = match *self {
            Rule::Fixed(off) => return off,
            Rule::Dst {
                std,
                dst,
                start,
                start_time,
                end,
                end_time,
            } => (std, dst, start, start_time, end, end_time),
        };
        let t = unix_secs.clamp(-RULE_LIMIT_SECS, RULE_LIMIT_SECS);
        let (year, _, _) = civil_from_days((t + i64::from(std)).div_euclid(DAY));
        let start_at = start.days(year) * DAY + i64::from(start_time) - i64::from(std);
        let end_at = end.days(year) * DAY + i64::from(end_time) - i64::from(dst);
        let in_dst = if start_at < end_at {
            start_at <= t && t < end_at
        } else {
            // Southern hemisphere: daylight time spans the turn of the year.
            !(end_at <= t && t < start_at)
        };
        if in_dst { dst } else { std }
    }
}

impl RuleDate {
    /// Days since 1970-01-01 of this rule date in `year`.
    fn days(self, year: i64) -> i64 {
        let jan1 = days_from_civil(year, 1, 1);
        match self {
            RuleDate::Julian1(n) => {
                let n = i64::from(n);
                jan1 + n - 1 + i64::from(is_leap(year) && n >= 60)
            }
            RuleDate::Julian0(n) => jan1 + i64::from(n),
            RuleDate::MonthWeekDay {
                month,
                week,
                weekday,
            } => {
                let first = days_from_civil(year, u32::from(month), 1);
                let first_weekday = weekday_of(first);
                let mut day =
                    (i64::from(weekday) - first_weekday).rem_euclid(7) + (i64::from(week) - 1) * 7;
                let len = i64::from(days_in_month(year, u32::from(month)));
                while day >= len {
                    day -= 7;
                }
                first + day
            }
        }
    }
}

/// Days since 1970-01-01 of the proleptic Gregorian date `y-m-d`
/// (Howard Hinnant's `days_from_civil`). Months and days outside their range
/// do not panic but give dates that roll over. Valid for |y| below about 10^12.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let (m, d) = (i64::from(m), i64::from(d));
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9).rem_euclid(12);
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Proleptic Gregorian date `(year, month, day)` of a day count since
/// 1970-01-01 (Howard Hinnant's `civil_from_days`).
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Whether `y` is a Gregorian leap year.
pub fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// Number of days in month `m` (1..=12) of year `y`; 0 for other months.
pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        2 => 28,
        _ => 0,
    }
}

/// Day of the week (0 = Sunday) of a day count since 1970-01-01, a Thursday.
fn weekday_of(days: i64) -> i64 {
    (days + 4).rem_euclid(7)
}

/// Bounds-checked cursor over the file bytes.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let out = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(out)
    }

    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_be_bytes(self.take(4)?.try_into().ok()?))
    }

    fn rest(&self) -> &'a [u8] {
        self.bytes.get(self.pos..).unwrap_or(&[])
    }
}

/// Record counts from a TZif header.
struct Counts {
    isutcnt: usize,
    isstdcnt: usize,
    leapcnt: usize,
    timecnt: usize,
    typecnt: usize,
    charcnt: usize,
}

impl Counts {
    /// Length of the data block that follows the header.
    fn block_len(&self, time_size: usize) -> Option<usize> {
        let parts = [
            self.timecnt.checked_mul(time_size)?,
            self.timecnt,
            self.typecnt.checked_mul(6)?,
            self.charcnt,
            self.leapcnt.checked_mul(time_size + 4)?,
            self.isstdcnt,
            self.isutcnt,
        ];
        parts.iter().try_fold(0usize, |sum, &p| sum.checked_add(p))
    }
}

/// Reads the 44-byte header; the version is 0 for version 1 files.
fn read_header(r: &mut Reader<'_>) -> Option<(u8, Counts)> {
    if r.take(4)? != b"TZif" {
        return None;
    }
    let version = match r.take(1)?[0] {
        0 => 0,
        v @ b'2'..=b'9' => v - b'0',
        _ => return None,
    };
    r.take(15)?;
    let mut next = || r.u32().and_then(|n| usize::try_from(n).ok());
    let counts = Counts {
        isutcnt: next()?,
        isstdcnt: next()?,
        leapcnt: next()?,
        timecnt: next()?,
        typecnt: next()?,
        charcnt: next()?,
    };
    if counts.typecnt == 0 {
        return None;
    }
    Some((version, counts))
}

/// Reads one data block with `time_size`-byte transition times.
fn read_block(r: &mut Reader<'_>, c: &Counts, time_size: usize) -> Option<Zone> {
    let times = r.take(c.timecnt.checked_mul(time_size)?)?;
    let indices = r.take(c.timecnt)?;
    let types = r.take(c.typecnt.checked_mul(6)?)?;
    r.take(c.charcnt)?;
    r.take(c.leapcnt.checked_mul(time_size + 4)?)?;
    r.take(c.isstdcnt)?;
    r.take(c.isutcnt)?;

    let offsets_by_type = types
        .chunks_exact(6)
        .map(|t| {
            let off = i32::from_be_bytes(t[..4].try_into().ok()?);
            OFFSET_RANGE.contains(&i64::from(off)).then_some(off)
        })
        .collect::<Option<Vec<i32>>>()?;
    let transitions: Vec<i64> = times
        .chunks_exact(time_size)
        .map(|b| match *b {
            [a, b, c, d] => i64::from(i32::from_be_bytes([a, b, c, d])),
            _ => i64::from_be_bytes(b.try_into().unwrap_or([0; 8])),
        })
        .collect();
    if transitions.windows(2).any(|w| w[0] >= w[1]) {
        return None;
    }
    let offsets = indices
        .iter()
        .map(|&i| offsets_by_type.get(usize::from(i)).copied())
        .collect::<Option<Vec<i32>>>()?;
    Some(Zone {
        transitions,
        offsets,
        initial: offsets_by_type[0],
        rule: None,
    })
}

/// The footer is a POSIX TZ string between two newlines. An empty, missing or
/// unparsable footer gives `None`, so the last transition's offset stays in use.
fn read_footer(rest: &[u8]) -> Option<Rule> {
    let body = rest.strip_prefix(b"\n")?;
    let end = body.iter().position(|&b| b == b'\n')?;
    parse_rule(std::str::from_utf8(&body[..end]).ok()?)
}

/// Parses a POSIX TZ string.
fn parse_rule(tz: &str) -> Option<Rule> {
    let mut p = TzParser { s: tz.as_bytes() };
    p.name()?;
    let std = -p.time(false)?;
    if p.s.is_empty() {
        return Some(Rule::Fixed(std));
    }
    p.name()?;
    let dst = match p.s.first() {
        Some(b',') => std.checked_add(3600)?,
        _ => -p.time(false)?,
    };
    p.expect(b',')?;
    let start = p.date()?;
    let start_time = p.rule_time()?;
    p.expect(b',')?;
    let end = p.date()?;
    let end_time = p.rule_time()?;
    if !p.s.is_empty() {
        return None;
    }
    for off in [std, dst] {
        if !OFFSET_RANGE.contains(&i64::from(off)) {
            return None;
        }
    }
    Some(Rule::Dst {
        std,
        dst,
        start,
        start_time,
        end,
        end_time,
    })
}

/// Cursor over a POSIX TZ string.
struct TzParser<'a> {
    s: &'a [u8],
}

impl TzParser<'_> {
    fn expect(&mut self, c: u8) -> Option<()> {
        let rest = self.s.strip_prefix(&[c])?;
        self.s = rest;
        Some(())
    }

    /// A zone abbreviation: letters, or anything between `<` and `>`.
    fn name(&mut self) -> Option<()> {
        let len = if self.s.first() == Some(&b'<') {
            self.s.iter().position(|&b| b == b'>')? + 1
        } else {
            self.s
                .iter()
                .take_while(|b| b.is_ascii_alphabetic())
                .count()
        };
        if len < 3 {
            return None;
        }
        self.s = &self.s[len..];
        Some(())
    }

    fn number(&mut self, max: i32) -> Option<i32> {
        let len = self.s.iter().take_while(|b| b.is_ascii_digit()).count();
        if len == 0 || len > 3 {
            return None;
        }
        let n = std::str::from_utf8(&self.s[..len]).ok()?.parse().ok()?;
        self.s = &self.s[len..];
        (n <= max).then_some(n)
    }

    /// `[+-]hh[:mm[:ss]]` in seconds; `long` allows hours up to 167.
    fn time(&mut self, long: bool) -> Option<i32> {
        let sign = match self.s.first() {
            Some(b'-') => -1,
            Some(b'+') => 1,
            _ => 0,
        };
        if sign != 0 {
            self.s = &self.s[1..];
        }
        let mut secs = self.number(if long { 167 } else { 24 })? * 3600;
        for unit in [60, 1] {
            if self.expect(b':').is_none() {
                break;
            }
            secs += self.number(59)? * unit;
        }
        Some(if sign < 0 { -secs } else { secs })
    }

    /// Optional `/time` after a rule date; the default is 02:00.
    fn rule_time(&mut self) -> Option<i32> {
        if self.expect(b'/').is_some() {
            self.time(true)
        } else {
            Some(7200)
        }
    }

    fn date(&mut self) -> Option<RuleDate> {
        if self.expect(b'J').is_some() {
            let n = self.number(365)?;
            return (n >= 1).then_some(RuleDate::Julian1(n as u16));
        }
        if self.expect(b'M').is_some() {
            let month = self.number(12)?;
            self.expect(b'.')?;
            let week = self.number(5)?;
            self.expect(b'.')?;
            let weekday = self.number(6)?;
            return (month >= 1 && week >= 1).then_some(RuleDate::MonthWeekDay {
                month: month as u8,
                week: week as u8,
                weekday: weekday as u8,
            });
        }
        Some(RuleDate::Julian0(self.number(365)? as u16))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_zone(name: &str) -> Option<Vec<u8>> {
        let path = Path::new("/usr/share/zoneinfo").join(name);
        match std::fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                println!("skipping: cannot read {}: {e}", path.display());
                None
            }
        }
    }

    /// Unix seconds of a UTC date and time.
    fn utc(y: i64, m: u32, d: u32, h: i64) -> i64 {
        days_from_civil(y, m, d) * DAY + h * 3600
    }

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(days_from_civil(1600, 2, 29), -135_081);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));
        assert_eq!(civil_from_days(days_from_civil(2024, 2, 29)), (2024, 2, 29));
        assert_eq!(
            days_from_civil(2024, 3, 1) - days_from_civil(2024, 2, 28),
            2
        );
        assert_eq!(
            days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28),
            1
        );
        for days in (-800_000..800_000).step_by(37) {
            let (y, m, d) = civil_from_days(days);
            assert!((1..=12).contains(&m) && d >= 1 && d <= days_in_month(y, m));
            assert_eq!(days_from_civil(y, m, d), days, "{y}-{m}-{d}");
        }
    }

    #[test]
    fn leap_years_and_month_lengths() {
        assert!(is_leap(2000) && is_leap(2024) && is_leap(-4) && is_leap(1600));
        assert!(!is_leap(1900) && !is_leap(2026) && !is_leap(2100));
        assert_eq!(days_in_month(2024, 2), 29);
        assert_eq!(days_in_month(2026, 2), 28);
        assert_eq!(days_in_month(2026, 13), 0);
    }

    #[test]
    fn baku_file_offsets() {
        let Some(bytes) = read_zone("Asia/Baku") else {
            return;
        };
        let zone = Zone::from_tzif(&bytes).expect("Baku parses");
        assert_eq!(zone.offset_at_utc(utc(2026, 10, 25, 12)), 14_400);
        assert_eq!(zone.offset_at_utc(utc(2014, 7, 15, 12)), 18_000);
        assert_eq!(zone.offset_at_utc(utc(2014, 1, 15, 12)), 14_400);
        assert_eq!(zone.offset_at_utc(utc(2300, 7, 1, 0)), 14_400);
        assert_eq!(
            zone.local_midnight_to_unix(2026, 10, 25),
            utc(2026, 10, 24, 20)
        );
    }

    #[test]
    fn berlin_file_dst_day_is_23_hours() {
        let Some(bytes) = read_zone("Europe/Berlin") else {
            return;
        };
        let zone = Zone::from_tzif(&bytes).expect("Berlin parses");
        let start = zone.local_midnight_to_unix(2026, 3, 29);
        let next = zone.local_midnight_to_unix(2026, 3, 30);
        assert_eq!(start, utc(2026, 3, 28, 23));
        assert_eq!(next - start, 23 * 3600);
        let autumn = zone.local_midnight_to_unix(2026, 10, 25);
        assert_eq!(
            zone.local_midnight_to_unix(2026, 10, 26) - autumn,
            25 * 3600
        );
        assert_eq!(zone.offset_at_utc(utc(2026, 7, 1, 0)), 7200);
        assert_eq!(zone.offset_at_utc(utc(2026, 12, 1, 0)), 3600);
    }

    #[test]
    fn footer_rule_agrees_with_table() {
        for (name, rule) in [
            ("Europe/Berlin", "CET-1CEST,M3.5.0,M10.5.0/3"),
            ("America/New_York", "EST5EDT,M3.2.0,M11.1.0"),
            ("Australia/Sydney", "AEST-10AEDT,M10.1.0,M4.1.0/3"),
        ] {
            let Some(bytes) = read_zone(name) else {
                continue;
            };
            let table = Zone::from_tzif(&bytes).expect("zone parses");
            let posix = Zone::from_posix_tz(rule).expect("rule parses");
            let mut t = utc(2008, 1, 1, 0);
            while t < utc(2036, 1, 1, 0) {
                assert_eq!(table.offset_at_utc(t), posix.offset_at_utc(t), "{name} {t}");
                t += 1800 + 3 * DAY;
            }
        }
    }

    #[test]
    fn posix_rules() {
        let baku = Zone::from_posix_tz("<+04>-4").unwrap();
        assert_eq!(baku.offset_at_utc(0), 14_400);
        let sydney = Zone::from_posix_tz("AEST-10AEDT,M10.1.0,M4.1.0/3").unwrap();
        assert_eq!(sydney.offset_at_utc(utc(2030, 1, 10, 0)), 11 * 3600);
        assert_eq!(sydney.offset_at_utc(utc(2030, 7, 10, 0)), 10 * 3600);
        let julian = Zone::from_posix_tz("XST3XDT,J60/0,300").unwrap();
        assert_eq!(julian.offset_at_utc(utc(2024, 3, 1, 4)), -2 * 3600);
        assert_eq!(julian.offset_at_utc(utc(2024, 2, 29, 12)), -3 * 3600);
        for bad in [
            "",
            "C",
            "CET",
            "CET-1CEST,M13.1.0,M10.5.0",
            "CET-1CEST,J0,J5",
            "CET-1CEST",
            "CET-99",
            "<+04",
            "CET-1CEST,M3.5.0,M10.5.0/3x",
        ] {
            assert!(Zone::from_posix_tz(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn midnight_in_a_gap_moves_to_the_transition() {
        // Clocks jump from 00:00 to 01:00 on the first Sunday of April.
        let zone = Zone::from_posix_tz("XST-2XDT,M4.1.0/0,M10.5.0/1").unwrap();
        // 2026-04-05 is a Sunday; 00:00 local does not exist.
        let start = zone.local_midnight_to_unix(2026, 4, 5);
        assert_eq!(start, utc(2026, 4, 4, 22));
        assert_eq!(
            zone.to_local(start),
            days_from_civil(2026, 4, 5) * DAY + 3600
        );
        // Clocks go from 01:00 back to 00:00 on 2026-10-25: 00:00 occurs twice.
        let twice = zone.local_midnight_to_unix(2026, 10, 25);
        assert_eq!(twice, utc(2026, 10, 24, 21));
    }

    #[test]
    fn local_date_and_utc() {
        let zone = Zone::utc();
        assert_eq!(zone.local_date(-1), (1969, 12, 31));
        assert_eq!(zone.local_midnight_to_unix(1960, 6, 1), utc(1960, 6, 1, 0));
        assert_eq!(Zone::from_tz_var(""), Some(Zone::utc()));
        assert!(Zone::from_tz_name("../../etc/passwd").is_none());
    }

    #[test]
    fn version_one_files_use_the_32_bit_block() {
        let mut f = b"TZif\0".to_vec();
        f.extend([0; 15]);
        for n in [0u32, 0, 0, 1, 2, 4] {
            f.extend(n.to_be_bytes());
        }
        f.extend(1_000_000i32.to_be_bytes());
        f.push(1);
        f.extend(3600i32.to_be_bytes());
        f.extend([0, 0]);
        f.extend(7200i32.to_be_bytes());
        f.extend([1, 0]);
        f.extend(b"A\0B\0");
        let zone = Zone::from_tzif(&f).unwrap();
        assert_eq!(zone.offset_at_utc(0), 3600);
        assert_eq!(zone.offset_at_utc(1_000_000), 7200);
        assert_eq!(zone.offset_at_utc(i64::MAX), 7200);
        assert!(Zone::from_tzif(&f[..f.len() - 1]).is_none());
        f[48] = 9; // a transition index past the type table
        assert!(Zone::from_tzif(&f).is_none());
    }

    /// Exercises a parsed zone at extreme inputs; none of these may panic.
    fn poke(zone: &Zone) {
        for t in [i64::MIN, -1, 0, 1, i64::MAX, 1 << 40, -(1 << 40)] {
            zone.offset_at_utc(t);
            zone.local_date(t);
        }
        for y in [i64::MIN, -1, 1970, 2026, i64::MAX] {
            zone.local_midnight_to_unix(y, 0, 0);
            zone.local_midnight_to_unix(y, 12, 31);
            zone.local_midnight_to_unix(y, u32::MAX, u32::MAX);
        }
    }

    #[test]
    fn malformed_files_never_panic() {
        let mut samples = vec![Vec::new(), b"TZif".to_vec(), b"TZif2".to_vec()];
        let mut huge = b"TZif2".to_vec();
        huge.extend([0; 15]);
        huge.extend([0xff; 24]);
        samples.push(huge);
        if let Some(berlin) = read_zone("Europe/Berlin") {
            let full = Zone::from_tzif(&berlin);
            assert!(full.is_some());
            for len in 0..berlin.len() {
                let cut = Zone::from_tzif(&berlin[..len]);
                if let Some(zone) = &cut {
                    poke(zone);
                }
            }
            samples.push(berlin);
        }
        // Deterministic pseudo-random corruption of each sample.
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for sample in samples.clone() {
            for _ in 0..300 {
                let mut bytes = sample.clone();
                if bytes.is_empty() {
                    bytes = (0..64).map(|_| next() as u8).collect();
                }
                for _ in 0..1 + next() % 8 {
                    let i = (next() as usize) % bytes.len();
                    bytes[i] = next() as u8;
                }
                if let Some(zone) = Zone::from_tzif(&bytes) {
                    poke(&zone);
                }
            }
        }
        for _ in 0..2000 {
            let mut bytes = b"TZif2".to_vec();
            bytes.extend((0..next() % 200).map(|_| next() as u8));
            if let Some(zone) = Zone::from_tzif(&bytes) {
                poke(&zone);
            }
        }
        for len in 0..80 {
            let garbage: Vec<u8> = (0..len).map(|_| next() as u8).collect();
            assert!(Zone::from_tzif(&garbage).is_none() || garbage.starts_with(b"TZif"));
        }
    }
}
