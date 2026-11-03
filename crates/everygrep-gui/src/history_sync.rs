//! Synchronize history policy and clears without blocking the event loop.
use crate::{
    search_history::{self, Searches},
    settings::{self, Settings, Stamp},
};
use std::{io, path::Path};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Generation {
    #[default]
    Unknown,
    Known(Option<Stamp>),
}
#[derive(Default)]
pub struct Reader {
    seen: Option<(Option<Stamp>, Option<Stamp>, bool)>,
    pub generation: Option<Stamp>,
    initial: Option<Option<Stamp>>,
    local_clear: Option<(u64, Generation, Option<Stamp>)>,
}
pub struct Read {
    pub policy: Settings,
    pub entries: Option<Searches>,
    pub cleared: bool,
    pub generation: Option<Stamp>,
}
pub fn marker(path: &Path) -> std::path::PathBuf {
    path.with_extension("clear")
}
impl Reader {
    pub fn remember_clear(&mut self, epoch: u64, before: Generation, after: Option<Stamp>) {
        self.generation = after;
        self.local_clear = Some((epoch, before, after));
    }
    pub fn generation_for(&self, epoch: u64, queued: Generation) -> Option<Stamp> {
        match self.local_clear {
            Some((first, before, after)) if epoch >= first && queued == before => after,
            _ => match queued {
                Generation::Known(stamp) => stamp,
                Generation::Unknown => self.initial.flatten(),
            },
        }
    }

    pub fn invalidate(&mut self) {
        self.seen = None;
    }
    pub fn read(&mut self, settings_path: &Path, path: &Path) -> io::Result<Read> {
        let _settings = settings::lock(settings_path)?;
        let policy = Settings::load(settings_path)?;
        let _history = settings::lock(path)?;
        let generation = settings::stamp(&marker(path));
        self.initial.get_or_insert(generation);
        let cleared = generation != self.generation;
        let stamp = (settings::stamp(path), generation, policy.history_on_disk);
        let entries = if policy.history_on_disk && (self.seen != Some(stamp) || cleared) {
            let mut entries = Searches::load(path)?;
            entries.retain_days(policy.history_days, search_history::now());
            entries.limit_to(policy.history_entries);
            Some(entries)
        } else {
            None
        };
        self.seen = Some(stamp);
        Ok(Read {
            policy,
            entries,
            cleared,
            generation,
        })
    }
}

/// Check the latest choice under the same settings lock used by its writer.
/// A delta from before an explicit clear cannot recreate a removed search.
pub fn update(
    settings_path: &Path,
    path: &Path,
    generation: Option<Stamp>,
    time: u64,
    change: impl FnOnce(&mut Searches),
) -> io::Result<Option<Searches>> {
    let _settings = settings::lock(settings_path)?;
    let policy = Settings::load(settings_path)?;
    if !policy.history_enabled || policy.history_size == 0 || !policy.history_on_disk {
        return Ok(None);
    }
    let _history = settings::lock(path)?;
    if settings::stamp(&marker(path)) != generation {
        return Searches::load(path).map(Some);
    }
    search_history::update_locked(path, policy.history_days, time, |entries| {
        change(entries);
        entries.limit_to(policy.history_entries);
    })
    .map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(std::path::PathBuf);
    impl Temp {
        fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let p = std::env::temp_dir().join(format!(
                "everygrep-history-sync-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn second_window_sees_clear_and_cannot_replay_a_queued_visit() {
        let t = Temp::new();
        let s = t.0.join("settings");
        let h = t.0.join("search-history");
        std::fs::write(&s, "history-on-disk = true\n").unwrap();
        let mut b = Reader::default();
        b.read(&s, &h).unwrap();
        update(&s, &h, b.generation, search_history::now(), |e| {
            e.record("private", search_history::now())
        })
        .unwrap();
        assert_eq!(b.read(&s, &h).unwrap().entries.unwrap().0.len(), 1);
        search_history::clear(&h).unwrap();
        assert!(
            update(&s, &h, b.generation, search_history::now(), |e| e
                .record("stale", search_history::now()))
            .unwrap()
            .unwrap()
            .0
            .is_empty()
        );
        let read = b.read(&s, &h).unwrap();
        assert!(read.cleared);
        assert!(read.entries.unwrap().0.is_empty());
        b.generation = read.generation;
        assert!(!b.read(&s, &h).unwrap().cleared);
    }
    #[test]
    fn disabling_disk_storage_or_recording_blocks_another_windows_write() {
        let t = Temp::new();
        let s = t.0.join("settings");
        let h = t.0.join("search-history");
        for policy in [
            "history-on-disk = false",
            "history-on-disk = true\nhistory-enabled = false",
            "history-on-disk = true\nhistory-size = 0",
        ] {
            std::fs::write(&s, policy).unwrap();
            assert!(
                update(&s, &h, None, 1, |e| e.record("private", 1))
                    .unwrap()
                    .is_none()
            );
            assert!(!h.exists());
        }
    }
    #[test]
    fn memory_windows_observe_explicit_clear_but_storage_changes_keep_memory() {
        let t = Temp::new();
        let s = t.0.join("settings");
        let h = t.0.join("search-history");
        let mut b = Reader::default();
        b.read(&s, &h).unwrap();
        search_history::clear_data(&h).unwrap();
        assert!(!b.read(&s, &h).unwrap().cleared);
        search_history::clear(&h).unwrap();
        assert!(b.read(&s, &h).unwrap().cleared);
    }
}

#[cfg(test)]
mod limit_tests {
    #[test]
    fn history_options_roundtrip_and_keep_the_newest_distinct_searches() {
        let s = crate::settings::Settings::parse(
            "search-history-limit = 2\nalways-search-suggestions = false",
        );
        assert_eq!(s.history_entries, 2);
        assert!(!s.always_suggestions);
        assert_eq!(crate::settings::Settings::parse(&s.to_text()), s);
        let mut entries = crate::search_history::Searches::default();
        for (text, time) in [("old", 1), ("new", 3), ("middle", 2)] {
            entries.record(text, time);
        }
        entries.limit_to(s.history_entries);
        assert_eq!(entries.0.len(), 2);
        assert!(!entries.0.iter().any(|e| e.search == "old"));
        assert_eq!(
            crate::settings::Settings::parse("search-history-limit = 0")
                .problems
                .len(),
            1
        );
    }
}

#[cfg(test)]
mod queued_tests {
    use super::*;
    #[test]
    fn a_clear_notification_cannot_authorize_an_older_queued_delta() {
        let dir =
            std::env::temp_dir().join(format!("everygrep-history-queued-{}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let s = dir.join("settings");
        let h = dir.join("search-history");
        std::fs::write(&s, "history-on-disk = true").unwrap();
        let queued = Generation::Known(None);
        let mut reader = Reader::default();
        let cleared = search_history::clear_generation(&h).unwrap();
        reader.generation = cleared;
        let time = search_history::now();
        let result = update(&s, &h, reader.generation_for(0, queued), time, |e| {
            e.record("stale", time)
        })
        .unwrap()
        .unwrap();
        assert!(result.0.is_empty());
        reader.remember_clear(1, queued, cleared);
        let result = update(&s, &h, reader.generation_for(1, queued), time, |e| {
            e.record("after local clear", time)
        })
        .unwrap()
        .unwrap();
        assert_eq!(result.0.len(), 1);
        assert_eq!(reader.generation_for(2, queued), cleared);
        search_history::clear(&h).unwrap();
        let external = settings::stamp(&marker(&h));
        assert_ne!(external, cleared);
        assert_eq!(
            reader.generation_for(3, Generation::Known(external)),
            external
        );
        let result = update(&s, &h, reader.generation_for(0, queued), time, |e| {
            e.record("old queued", time)
        })
        .unwrap()
        .unwrap();
        assert!(result.0.is_empty());
        let mut opening = Reader::default();
        opening.read(&s, &h).unwrap();
        assert_eq!(opening.generation_for(0, Generation::Unknown), external);
        let result = update(
            &s,
            &h,
            opening.generation_for(0, Generation::Unknown),
            time,
            |e| e.record("first visit", time),
        )
        .unwrap()
        .unwrap();
        assert_eq!(result.0.len(), 1);
        let latest = search_history::clear_generation(&h).unwrap();
        opening.generation = latest;
        assert_eq!(opening.generation_for(0, Generation::Unknown), external);
        assert!(
            update(
                &s,
                &h,
                opening.generation_for(0, Generation::Unknown),
                time,
                |e| e.record("old startup request", time)
            )
            .unwrap()
            .unwrap()
            .0
            .is_empty()
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

/// Preserve another window's history choice when a different control changes.
pub fn apply_choices(target: &mut Settings, before: &Settings, wanted: &Settings) {
    macro_rules! apply { ($($field:ident),*) => { $(if before.$field != wanted.$field { target.$field = wanted.$field; })* }; }
    apply!(
        history_size,
        history_entries,
        history_on_disk,
        history_enabled,
        always_suggestions
    );
}

#[cfg(test)]
mod choice_tests {
    use super::*;
    #[test]
    fn unrelated_settings_edits_preserve_another_windows_privacy_choice() {
        let before = Settings::parse("history-on-disk = true");
        let mut wanted = before.clone();
        wanted.as_you_type = false;
        let mut latest = Settings::parse(
            "history-on-disk = false\nhistory-enabled = false\nsearch-history-limit = 5",
        );
        apply_choices(&mut latest, &before, &wanted);
        assert!(!latest.history_on_disk && !latest.history_enabled);
        assert_eq!(latest.history_entries, 5);
        assert!(!Settings::parse(&latest.to_text()).history_on_disk);
        let before = latest.clone();
        let mut wanted = before.clone();
        wanted.history_on_disk = true;
        apply_choices(&mut latest, &before, &wanted);
        assert!(latest.history_on_disk);
        assert!(!latest.history_enabled);
    }
}
