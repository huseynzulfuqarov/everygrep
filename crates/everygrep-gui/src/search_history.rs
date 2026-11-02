//! Search statistics, separate from the session's Back/Forward trail.
//! Mutations read the latest file while holding its sibling lock, then use
//! the settings atomic writer (and its exclusive temporary-file helper).

use std::collections::HashMap;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::settings;

pub const MAX_ENTRIES: usize = 10_000;

const HEADER: &str = "EveryGrep search history 1";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Order {
    #[default]
    Date,
    Count,
    Text,
}

impl Order {
    pub const ALL: [Self; 3] = [Self::Date, Self::Count, Self::Text];
    pub fn id(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Count => "count",
            Self::Text => "text",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub search: String,
    pub count: u64,
    /// Seconds since the Unix epoch, independent of the display time zone.
    pub last: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Searches(pub Vec<Entry>);

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |t| t.as_secs())
}

/// A draft is harmless until the user explicitly applies it.
#[derive(Clone, Copy, Debug)]
pub struct Retention {
    pub draft: u32,
    pub committed: u32,
}
impl Retention {
    pub fn new(days: u32) -> Self {
        Self {
            draft: days,
            committed: days,
        }
    }
    pub fn edit(&mut self, days: i32) {
        self.draft = days.clamp(0, 36500) as u32;
    }
    pub fn commit(&mut self) -> u32 {
        self.committed = self.draft;
        self.committed
    }
}

impl Searches {
    /// Apply the same delta independently to the immediate in-memory view and
    /// the latest locked file; a completion must not replay it on the view.
    pub fn apply(&mut self, days: u32, time: u64, change: impl FnOnce(&mut Self)) {
        change(self);
        self.retain_days(days, time);
        self.limit();
    }

    pub fn record(&mut self, search: &str, time: u64) {
        if search.trim().is_empty() {
            return;
        }
        if let Some(e) = self.0.iter_mut().find(|e| e.search == search) {
            e.count = e.count.saturating_add(1);
            e.last = e.last.max(time);
        } else {
            self.0.push(Entry {
                search: search.into(),
                count: 1,
                last: time,
            });
        }
        self.limit();
    }

    /// Bound memory and disk even when retention is forever. Keep the newest
    /// searches, with exact text breaking timestamp ties deterministically.
    pub fn limit(&mut self) {
        if self.0.len() > MAX_ENTRIES {
            self.0.select_nth_unstable_by(MAX_ENTRIES, |a, b| {
                b.last.cmp(&a.last).then_with(|| a.search.cmp(&b.search))
            });
            self.0.truncate(MAX_ENTRIES);
        }
    }

    /// 0 days keeps entries forever. The exact cutoff is retained; a clock
    /// adjustment into the past never deletes entries from the future.
    pub fn retain_days(&mut self, days: u32, time: u64) {
        if days != 0 {
            let cutoff = time.saturating_sub(u64::from(days) * 86_400);
            self.0.retain(|e| e.last >= cutoff);
        }
    }

    /// Fold each candidate once, before sorting. Prefix suggestions do not
    /// sort or allocate sort keys for unrelated history entries.
    fn matching(&self, order: Order, accepts: impl Fn(&Entry, &str) -> bool) -> Vec<&Entry> {
        let mut rows: Vec<_> = self
            .0
            .iter()
            .filter_map(|entry| {
                let key = entry.search.to_lowercase();
                accepts(entry, &key).then_some((entry, key))
            })
            .collect();
        rows.sort_by(|(a, ka), (b, kb)| {
            let primary = match order {
                Order::Date => b.last.cmp(&a.last),
                Order::Count => b.count.cmp(&a.count),
                Order::Text => std::cmp::Ordering::Equal,
            };
            primary
                .then_with(|| ka.cmp(kb))
                .then_with(|| a.search.cmp(&b.search))
        });
        rows.into_iter().map(|(entry, _)| entry).collect()
    }

    /// Date and count sort descending, text ascending; ties use the folded
    /// search then its exact text for a deterministic order.
    pub fn sorted(&self, order: Order, filter: &str) -> Vec<&Entry> {
        let filter = filter.to_lowercase();
        self.matching(order, |_, key| key.contains(&filter))
    }

    /// Suggest whole earlier searches matching a case-insensitive prefix.
    /// Empty input and the exact current text never produce a suggestion.
    pub fn suggestions(&self, prefix: &str, order: Order) -> Vec<&Entry> {
        if prefix.trim().is_empty() {
            return Vec::new();
        }
        let folded = prefix.to_lowercase();
        self.matching(order, |entry, key| {
            entry.search != prefix && key.starts_with(&folded)
        })
        .into_iter()
        .take(12)
        .collect()
    }

    pub fn load(path: &Path) -> io::Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                // One-way migration: never rewrite the file older builds use.
                if path.file_name().is_some_and(|n| n == "search-history") {
                    return Self::load(&path.with_file_name("history"));
                }
                return Ok(Self::default());
            }
            Err(e) => return Err(e),
        };
        let mut entries = Self::default();
        let mut index = HashMap::<String, usize>::new();
        let mut lines = text.lines();
        if lines.next() != Some(HEADER) {
            if text.starts_with("EveryGrep search history ") {
                return Err(invalid());
            }
            // Read the earlier escaped-query format without losing searches.
            // It had no timestamps or counts: each occurrence counts once,
            // and the migration date is used as its last search date.
            let time = now();
            for line in text.lines() {
                let search = decode(line)?;
                if search.trim().is_empty() {
                    continue;
                }
                if let Some(&i) = index.get(&search) {
                    entries.0[i].count = entries.0[i].count.saturating_add(1);
                } else {
                    index.insert(search.clone(), entries.0.len());
                    entries.0.push(Entry {
                        search,
                        count: 1,
                        last: time,
                    });
                }
            }
            entries.limit();
            return Ok(entries);
        }
        for line in lines {
            let mut parts = line.splitn(3, '\t');
            let count = parts
                .next()
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|n| *n > 0);
            let last = parts.next().and_then(|s| s.parse::<u64>().ok());
            let search = parts.next().map(decode).transpose()?;
            let (Some(count), Some(last), Some(search)) = (count, last, search) else {
                return Err(invalid());
            };
            if search.trim().is_empty() || index.insert(search.clone(), entries.0.len()).is_some() {
                return Err(invalid());
            }
            entries.0.push(Entry {
                search,
                count,
                last,
            });
        }
        entries.limit();
        Ok(entries)
    }

    fn text(&self) -> String {
        let mut out = format!("{HEADER}\n");
        for e in &self.0 {
            out.push_str(&format!(
                "{}\t{}\t{}\n",
                e.count,
                e.last,
                settings::escape(e.search.as_bytes())
            ));
        }
        out
    }
}

fn decode(s: &str) -> io::Result<String> {
    settings::unescape(s)
        .and_then(|b| String::from_utf8(b).ok())
        .ok_or_else(invalid)
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        crate::i18n::tr("Invalid search history."),
    )
}

/// Apply an operation to the current file, never to a stale window snapshot.
/// Failed reads leave the file untouched. The lock file stays after clearing,
/// so another window cannot lock a different inode during a pending write.
pub fn update(
    path: &Path,
    days: u32,
    time: u64,
    change: impl FnOnce(&mut Searches),
) -> io::Result<Searches> {
    let _lock = settings::lock(path)?;
    let mut entries = Searches::load(path)?;
    let before = entries.clone();
    entries.apply(days, time, change);
    let size_changed =
        std::fs::metadata(path).map_or(true, |m| m.len() != entries.text().len() as u64);
    if entries != before || size_changed {
        write(path, &entries)?;
    }
    Ok(entries)
}

/// An explicit clear can recover a malformed file as well.
pub fn clear(path: &Path) -> io::Result<()> {
    let _lock = settings::lock(path)?;
    write(path, &Searches::default())?;
    if path.file_name().is_some_and(|n| n == "search-history") {
        // Explicit clear/disable also removes the migration source. Normal
        // reads and writes leave that older file untouched.
        let legacy = path.with_file_name("history");
        let _legacy_lock = settings::lock(&legacy)?;
        match std::fs::remove_file(legacy) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn write(path: &Path, entries: &Searches) -> io::Result<()> {
    let (mut file, tmp) = settings::create_temp(path)?;
    let written = (|| {
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        file.write_all(entries.text().as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::tests::Scratch;

    #[test]
    fn immediate_view_and_queued_disk_delta_count_once_and_disable_keeps_memory() {
        let dir = Scratch::new("history-pending");
        let path = dir.0.join("search-history");
        let mut memory = update(&path, 0, 1, |s| s.record("query", 1)).unwrap();
        let policy = dir.0.join("settings");
        settings::update(&policy, |s| s.history_on_disk = true).unwrap();
        let queue = settings::Writer::start();
        let (release, blocked) = std::sync::mpsc::channel();
        queue.run(move || {
            blocked.recv().unwrap();
        });
        let change = |s: &mut Searches| s.record("query", 2);
        memory.apply(0, 2, change);
        let file = path.clone();
        queue.run(move || {
            let saved = update(&file, 0, 2, change).unwrap();
            assert_eq!(saved.0[0].count, 2);
        });
        assert_eq!(memory.0[0].count, 2);
        // Turning off persistence queues a clear, without discarding the
        // immediately updated in-memory record or waiting for blocked I/O.
        let file = path.clone();
        let policy_file = policy.clone();
        queue.run(move || {
            settings::update(&policy_file, |s| {
                s.history_on_disk = false;
                s.history_days = 365;
            })
            .unwrap();
            clear(&file).unwrap();
        });
        release.send(()).unwrap();
        assert!(queue.wait_done(std::time::Duration::from_secs(30)));
        assert!(Searches::load(&path).unwrap().0.is_empty());
        assert_eq!(memory.0[0].count, 2);
        let saved_policy = settings::Settings::load(&policy).unwrap();
        assert!(!saved_policy.history_on_disk);
        assert_eq!(saved_policy.history_days, 365);
    }

    #[test]
    fn retention_edits_only_prune_at_the_committed_value() {
        let mut policy = Retention::new(90);
        let mut entries = Searches::default();
        let now = 400 * 86_400;
        for age in [1, 10, 40, 80] {
            entries.record(&age.to_string(), now - age * 86_400);
        }
        for draft in [3, 36, 365, 36, 3, 0] {
            policy.edit(draft);
            entries.retain_days(policy.committed, now);
            assert_eq!(entries.0.len(), 4);
        }
        policy.edit(365);
        entries.retain_days(policy.commit(), now);
        assert_eq!(entries.0.len(), 4);
        policy.edit(30);
        entries.retain_days(policy.commit(), now);
        assert_eq!(entries.0.len(), 2);
    }

    #[test]
    fn load_and_record_are_bounded_and_noop_preserves_the_file() {
        let dir = Scratch::new("history-cap");
        let path = dir.0.join("history");
        let mut text = format!("{HEADER}\n");
        for i in 0..MAX_ENTRIES + 50 {
            text.push_str(&format!("1\t{i}\tquery {i}\n"));
        }
        std::fs::write(&path, text).unwrap();
        let mut entries = update(&path, 0, 0, |_| {}).unwrap();
        let raw = std::fs::read_to_string(&path).unwrap();
        assert_eq!(raw.lines().count(), MAX_ENTRIES + 1);
        assert_eq!(entries.0.len(), MAX_ENTRIES);
        assert!(entries.0.iter().all(|e| e.last >= 50));
        entries.record("new", 100_000);
        assert_eq!(entries.0.len(), MAX_ENTRIES);
        assert!(entries.0.iter().any(|e| e.search == "new"));
        let path = dir.0.join("small-history");
        update(&path, 0, 1, |s| s.record("x", 1)).unwrap();
        let before = std::fs::metadata(&path).unwrap();
        update(&path, 0, 1, |_| {}).unwrap();
        use std::os::unix::fs::MetadataExt;
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), before.ino());
    }

    #[test]
    fn history_saves_run_in_order_off_the_caller_thread_and_finish() {
        let queue = settings::Writer::start();
        let dir = Scratch::new("history-queue");
        let path = dir.0.join("history");
        let caller = std::thread::current().id();
        for value in ["old", "clear", "new"] {
            let path = path.clone();
            queue.run(move || {
                assert_ne!(std::thread::current().id(), caller);
                if value == "clear" {
                    clear(&path).unwrap();
                } else {
                    update(&path, 0, 1, |s| s.record(value, 1)).unwrap();
                }
            });
        }
        assert!(queue.wait_done(std::time::Duration::from_secs(30)));
        let entries = Searches::load(&path).unwrap();
        assert_eq!(entries.0.len(), 1);
        assert_eq!(entries.0[0].search, "new");
    }

    #[test]
    fn storage_round_trip_and_old_format() {
        let dir = Scratch::new("search-history");
        let path = dir.0.join("state/history");
        assert_eq!(Searches::load(&path).unwrap(), Searches::default());
        let entries = update(&path, 0, 123, |s| {
            for text in [" a ", "a,b", "Şəkil\n\"sitat\"\t\\", " a ", "", "  "] {
                s.record(text, 123);
            }
        })
        .unwrap();
        assert_eq!(Searches::load(&path).unwrap(), entries);
        assert_eq!(entries.0.len(), 3);
        assert_eq!(entries.0[0].count, 2);
        std::fs::write(&path, "old\\nquery\nold\\nquery\n").unwrap();
        let new_path = path.with_file_name("search-history");
        let old = update(&new_path, 90, now(), |_| {}).unwrap();
        assert_eq!(old.0[0].search, "old\nquery");
        assert_eq!(old.0[0].count, 2);
        assert!(
            std::fs::read_to_string(&new_path)
                .unwrap()
                .starts_with(HEADER)
        );
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "old\\nquery\nold\\nquery\n"
        );
        clear(&new_path).unwrap();
        assert!(!path.exists());
        assert!(Searches::load(&new_path).unwrap().0.is_empty());
        assert_eq!(
            std::fs::read_dir(path.parent().unwrap()).unwrap().count(),
            3
        );
    }

    #[test]
    fn retention_cutoff_forever_and_saturation() {
        let mut s = Searches::default();
        for (q, t) in [("old", 9), ("edge", 10), ("new", 11), ("future", 100_000)] {
            s.record(q, t);
        }
        s.retain_days(0, u64::MAX);
        assert_eq!(s.0.len(), 4);
        s.retain_days(1, 86_410);
        assert_eq!(
            s.0.iter().map(|e| e.search.as_str()).collect::<Vec<_>>(),
            ["edge", "new", "future"]
        );
        s.0[0].count = u64::MAX;
        s.record("edge", 0);
        assert_eq!(s.0[0].count, u64::MAX);
        assert_eq!(s.0[0].last, 10);
    }

    #[test]
    fn concurrent_changes_and_clear_do_not_restore_stale_entries() {
        let dir = Scratch::new("search-history-lock");
        let path = dir.0.join("history");
        let jobs: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                std::thread::spawn(move || {
                    for _ in 0..10 {
                        update(&path, 0, 100, |s| s.record("same", 100)).unwrap();
                    }
                })
            })
            .collect();
        for job in jobs {
            job.join().unwrap();
        }
        assert_eq!(Searches::load(&path).unwrap().0[0].count, 80);
        update(&path, 0, 100, |s| s.0.clear()).unwrap();
        let after = update(&path, 0, 101, |s| s.record("new", 101)).unwrap();
        assert_eq!(after.0.len(), 1);
        assert_eq!(after.0[0].search, "new");
    }

    #[test]
    fn sorts_dates_counts_and_text_with_deterministic_ties() {
        let mut s = Searches::default();
        for (q, t) in [("z", 1), ("Beta", 3), ("alpha", 2), ("alpha", 2), ("Á", 3)] {
            s.record(q, t);
        }
        let names = |order| {
            s.sorted(order, "")
                .iter()
                .map(|e| e.search.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(names(Order::Date), ["Beta", "Á", "alpha", "z"]);
        assert_eq!(names(Order::Count), ["alpha", "Beta", "z", "Á"]);
        assert_eq!(names(Order::Text), ["alpha", "Beta", "z", "Á"]);
        assert_eq!(s.sorted(Order::Text, "ET")[0].search, "Beta");
    }

    #[test]
    fn suggestions_are_prefix_only_bounded_and_unicode_aware() {
        let mut s = Searches::default();
        for i in 0..20 {
            s.record(&format!("query {i:02}"), i);
        }
        s.record("ŞƏKİL", 21);
        assert!(s.suggestions("", Order::Date).is_empty());
        assert!(s.suggestions(" ", Order::Date).is_empty());
        assert!(s.suggestions("uery", Order::Date).is_empty());
        assert_eq!(s.suggestions("QUERY", Order::Date).len(), 12);
        assert_eq!(s.suggestions("query", Order::Date)[0].search, "query 19");
        assert_eq!(s.suggestions("şə", Order::Date)[0].search, "ŞƏKİL");
        assert!(s.suggestions("query 19", Order::Date).is_empty());
    }

    #[test]
    fn bad_or_unreadable_history_is_not_overwritten() {
        let dir = Scratch::new("search-history-bad");
        let path = dir.0.join("history");
        for bad in [
            format!("{HEADER}\nno fields\n"),
            format!("{HEADER}\n0\t1\tx\n"),
            "bad\\q\n".into(),
        ] {
            std::fs::write(&path, &bad).unwrap();
            assert!(update(&path, 0, 1, |s| s.record("x", 1)).is_err());
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
        }
        clear(&path).unwrap();
        assert!(Searches::load(&path).unwrap().0.is_empty());
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(update(&path, 0, 1, |_| {}).is_err());
    }
}
