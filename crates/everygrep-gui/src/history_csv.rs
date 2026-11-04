//! UTF-8 CSV interchange for our search history. Dates are UTC, counts are
//! decimal, and headers are fixed so files work across interface languages.
//! Parsing completes before any history is changed; repeated imports merge
//! by maximum count/date, making importing the same snapshot idempotent.

use std::io;
use std::path::Path;

use everygrep_core::localtime::{Zone, days_from_civil, days_in_month};

use crate::search_history::{Entry, MAX_TIMESTAMP, Order, Searches};

const HEADER: [&str; 3] = ["query", "times_used", "last_used_utc"];

pub fn encode(searches: &Searches) -> String {
    let mut text = HEADER.join(",") + "\r\n";
    for entry in searches.sorted(Order::Date, "") {
        let search = if entry.search.starts_with(['=', '+', '-', '@', '\'']) {
            format!("'{}", entry.search)
        } else {
            entry.search.clone()
        };
        field(&mut text, &search);
        text.push_str(&format!(",{},{}\r\n", entry.count, utc_date(entry.last)));
    }
    text
}

fn field(out: &mut String, text: &str) {
    crate::csv::field(out, text);
}

pub fn utc_date(time: u64) -> String {
    let time = time.min(MAX_TIMESTAMP) as i64;
    let (y, m, d) = Zone::utc().local_date(time);
    let secs = time.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        secs % 3600 / 60,
        secs % 60
    )
}

fn date(text: &str) -> Option<u64> {
    let bytes = text.as_bytes();
    if bytes.len() != 20
        || !bytes.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b'T',
            13 | 16 => *c == b':',
            19 => *c == b'Z',
            _ => c.is_ascii_digit(),
        })
    {
        return None;
    }
    let year: i64 = text[..4].parse().ok()?;
    let month: u32 = text[5..7].parse().ok()?;
    let day: u32 = text[8..10].parse().ok()?;
    let hour: i64 = text[11..13].parse().ok()?;
    let minute: i64 = text[14..16].parse().ok()?;
    let second: i64 = text[17..19].parse().ok()?;
    if !(1970..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || day == 0
        || day > days_in_month(year, month)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    u64::try_from(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
        .ok()
}

pub fn decode(text: &str) -> io::Result<Searches> {
    let rows = rows(text.strip_prefix('\u{feff}').unwrap_or(text))?;
    if rows
        .first()
        .map(|r| r.iter().map(String::as_str).collect::<Vec<_>>())
        != Some(HEADER.to_vec())
    {
        return Err(invalid());
    }
    let mut entries = Searches::default();
    for row in &rows[1..] {
        if row.len() == 1 && row[0].is_empty() {
            continue;
        }
        if row.len() != 3 || row[0].trim().is_empty() {
            return Err(invalid());
        }
        let count = row[1]
            .parse::<u64>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(invalid)?;
        let last = date(&row[2]).ok_or_else(invalid)?;
        // Export protects formula starters and doubles an existing leading
        // apostrophe, so importing our CSV restores the exact query text.
        let search = row[0]
            .strip_prefix('\'')
            .filter(|s| s.starts_with(['=', '+', '-', '@', '\'']))
            .unwrap_or(&row[0]);
        entries.0.push(Entry {
            search: search.into(),
            count,
            last,
        });
    }
    let mut merged = Searches::default();
    merged.merge(&entries);
    Ok(merged)
}

pub fn import(path: &Path) -> io::Result<Searches> {
    decode(&std::fs::read_to_string(path)?)
}

pub fn export(path: &Path, entries: &Searches) -> io::Result<()> {
    crate::settings::write_atomic(path, encode(entries).as_bytes())
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        crate::i18n::tr(
            "Invalid history CSV (expected query,times_used,last_used_utc with UTC dates).",
        ),
    )
}

fn rows(text: &str) -> io::Result<Vec<Vec<String>>> {
    crate::csv::rows(text).map_err(|_| invalid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::Scratch;

    #[test]
    fn blank_records_are_ignored_but_empty_searches_are_invalid() {
        let csv = "query,times_used,last_used_utc\r\n\r\nx,1,2026-01-01T00:00:00Z\r\n\r\n";
        assert_eq!(decode(csv).unwrap().0[0].search, "x");
        assert_eq!(decode(&csv.replace("\r\n", "\n")).unwrap().0.len(), 1);
        assert!(decode("query,times_used,last_used_utc\n,1,2026-01-01T00:00:00Z\n").is_err());
        assert!(decode("query,times_used,last_used_utc\n,,\n").is_err());
    }

    #[test]
    fn spreadsheet_formulas_are_protected_and_import_restores_exact_text() {
        let mut entries = Searches::default();
        for query in [
            "=1+1",
            "+1",
            "-1",
            "@name",
            "'=literal",
            "''literal",
            "'plain",
            "plain",
        ] {
            entries.record(query, 1);
        }
        let csv = encode(&entries);
        for line in csv.lines().skip(1) {
            assert!(!line.starts_with(['=', '+', '-', '@']));
        }
        assert!(csv.contains("'=1+1,1,"));
        assert!(csv.contains("''=literal,1,"));
        assert_eq!(
            decode(&csv).unwrap().sorted(Order::Text, ""),
            entries.sorted(Order::Text, "")
        );
    }

    #[test]
    fn csv_round_trips_unicode_quotes_commas_and_newlines() {
        let dir = Scratch::new("history-csv");
        let path = dir.0.join("history.csv");
        let mut entries = Searches::default();
        for search in [
            "a,b",
            "\"Şəkillər\"\r\nnext",
            " spaced ",
            "back\\slash\t",
            "ext:txt",
        ] {
            entries.record(search, 1_793_622_400);
        }
        export(&path, &entries).unwrap();
        let csv = std::fs::read_to_string(&path).unwrap();
        assert!(csv.starts_with("query,times_used,last_used_utc\r\n"));
        assert!(csv.contains("\"\"Şəkillər\"\""));
        let decoded = import(&path).unwrap();
        assert_eq!(
            decoded.sorted(Order::Text, ""),
            entries.sorted(Order::Text, "")
        );
        assert_eq!(decode(&("\u{feff}".to_owned() + &csv)).unwrap(), decoded);
        assert_eq!(std::fs::read_dir(&dir.0).unwrap().count(), 1);
    }

    #[test]
    fn csv_rejects_bad_headers_values_and_malformed_quoting() {
        let head = "query,times_used,last_used_utc\n";
        for tail in [
            "x,0,2026-01-01T00:00:00Z",
            "x,-1,2026-01-01T00:00:00Z",
            "x,1,2026-02-29T00:00:00Z",
            "x,1,2026-01-01T24:00:00Z",
            "x,1,2026-01-01T00:00:60Z",
            "x,1,2026-01-01T00:00:00Z,extra",
            "\"x,1,2026-01-01T00:00:00Z",
            "x\"y,1,2026-01-01T00:00:00Z",
            "\"x\"junk,1,2026-01-01T00:00:00Z",
            " ,1,2026-01-01T00:00:00Z",
            "x,18446744073709551616,2026-01-01T00:00:00Z",
            "x,1,2026-01-01T00:00:00Z\r",
        ] {
            assert!(decode(&(head.to_owned() + tail)).is_err(), "{tail}");
        }
        assert!(decode("wrong,headers,here\n").is_err());
        assert!(decode("").is_err());
        assert!(decode(head).unwrap().0.is_empty());
    }

    #[test]
    fn dates_and_import_merge_are_stable_and_idempotent() {
        assert_eq!(date("1970-01-01T00:00:00Z"), Some(0));
        let leap = date("2024-02-29T23:59:59Z").unwrap();
        assert_eq!(utc_date(leap), "2024-02-29T23:59:59Z");
        let mut existing = Searches(vec![Entry {
            search: "x".into(),
            count: 10,
            last: leap,
        }]);
        let imported = decode("query,times_used,last_used_utc\nx,2,2024-01-01T00:00:00Z\nx,3,2024-03-01T00:00:00Z\ny,1,2024-03-01T00:00:00Z").unwrap();
        existing.merge(&imported);
        let once = existing.clone();
        existing.merge(&imported);
        assert_eq!(existing, once);
        assert_eq!(existing.0[0].count, 10);
        assert_eq!(existing.0[0].last, date("2024-03-01T00:00:00Z").unwrap());
        existing.retain_days(1, date("2024-03-03T00:00:00Z").unwrap());
        assert!(existing.0.is_empty());
    }
}
