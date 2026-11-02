//! Search history for Back and Forward.
//!
//! A query is recorded when the user pauses typing, presses Enter or opens a
//! result. Like a web browser, recording after going back drops the entries
//! ahead. Empty queries and a repeat of the current entry are not recorded.
//!
//! The navigation trail lives in memory, separately from search statistics.

use crate::settings::DEFAULT_HISTORY;

#[derive(Debug)]
pub struct History {
    entries: Vec<String>,
    /// Index of the entry shown now, if any.
    pos: Option<usize>,
    /// Most entries kept; the oldest go first. 0 records nothing.
    max: usize,
}

impl Default for History {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY)
    }
}

impl History {
    /// An empty history that keeps up to `max` entries.
    pub fn new(max: usize) -> Self {
        Self {
            entries: Vec::new(),
            pos: None,
            max,
        }
    }

    /// A history holding `entries` (oldest first), positioned after the
    /// newest one.
    pub fn with_entries(max: usize, entries: Vec<String>) -> Self {
        let mut h = Self::new(max);
        entries.iter().for_each(|e| {
            h.record(e);
        });
        h
    }

    /// Seed the trail after an asynchronous load, unless navigation already began.
    pub fn seed(&mut self, entries: Vec<String>) {
        if self.entries.is_empty() {
            *self = Self::with_entries(self.max, entries);
        }
    }

    /// Changes the number of entries kept, dropping the oldest ones.
    pub fn set_max(&mut self, max: usize) {
        self.max = max;
        let extra = self.entries.len().saturating_sub(max);
        if extra > 0 {
            self.entries.drain(..extra);
            self.pos = self
                .pos
                .and_then(|p| p.checked_sub(extra))
                .or((!self.entries.is_empty()).then_some(0));
        }
    }

    /// Records `query` as the newest entry after the current one. Returns
    /// whether the entries changed.
    pub fn record(&mut self, query: &str) -> bool {
        if self.max == 0 || query.is_empty() || self.current() == Some(query) {
            return false;
        }
        if let Some(pos) = self.pos {
            self.entries.truncate(pos + 1);
        }
        self.entries.push(query.to_owned());
        if self.entries.len() > self.max {
            self.entries.remove(0);
        }
        self.pos = Some(self.entries.len() - 1);
        true
    }

    /// The query to show for Back, given the text now in the search field.
    /// Text that was typed but not yet recorded is recorded first, so Forward
    /// returns to it. With an empty field, Back shows the current entry.
    pub fn back(&mut self, shown: &str) -> Option<&str> {
        self.record(shown);
        let pos = self.pos?;
        if self.entries[pos] == shown {
            self.pos = Some(pos.checked_sub(1)?);
        }
        self.current()
    }

    /// The query to show for Forward, given the text now in the search field.
    pub fn forward(&mut self, shown: &str) -> Option<&str> {
        self.record(shown);
        let pos = self.pos?;
        if pos + 1 >= self.entries.len() {
            return None;
        }
        self.pos = Some(pos + 1);
        self.current()
    }

    fn current(&self) -> Option<&str> {
        self.pos.map(|p| self.entries[p].as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with(entries: &[&str]) -> History {
        let mut h = History::default();
        entries.iter().for_each(|e| {
            h.record(e);
        });
        h
    }

    #[test]
    fn back_and_forward_walk_the_entries() {
        let mut h = with(&["a", "b", "c"]);
        assert_eq!(h.back("c"), Some("b"));
        assert_eq!(h.back("b"), Some("a"));
        assert_eq!(h.back("a"), None);
        assert_eq!(h.forward("a"), Some("b"));
        assert_eq!(h.forward("b"), Some("c"));
        assert_eq!(h.forward("c"), None);
    }

    #[test]
    fn empty_and_repeated_queries_are_not_recorded() {
        let mut h = with(&["a", "a", "", "b", "b"]);
        assert_eq!(h.entries, ["a", "b"]);
        h.record("a");
        assert_eq!(h.entries, ["a", "b", "a"]);
    }

    #[test]
    fn unrecorded_text_is_kept_for_forward() {
        let mut h = with(&["a", "b"]);
        assert_eq!(h.back("b-typed"), Some("b"));
        assert_eq!(h.forward("b"), Some("b-typed"));
    }

    #[test]
    fn back_from_an_empty_field_shows_the_last_entry() {
        let mut h = with(&["a", "b"]);
        assert_eq!(h.back(""), Some("b"));
        assert_eq!(h.back("b"), Some("a"));
        assert_eq!(History::default().back(""), None);
        assert_eq!(History::default().forward(""), None);
    }

    #[test]
    fn recording_after_back_drops_the_entries_ahead() {
        let mut h = with(&["a", "b", "c"]);
        h.back("c");
        h.back("b");
        h.record("x");
        assert_eq!(h.entries, ["a", "x"]);
        assert_eq!(h.forward("x"), None);
    }

    #[test]
    fn oldest_entries_go_first() {
        let mut h = History::default();
        for i in 0..DEFAULT_HISTORY + 5 {
            h.record(&i.to_string());
        }
        assert_eq!(h.entries.len(), DEFAULT_HISTORY);
        assert_eq!(h.entries[0], "5");
    }

    #[test]
    fn size_zero_turns_the_history_off() {
        let mut h = History::new(0);
        assert!(!h.record("a"));
        assert_eq!(h.back("a"), None);
        assert_eq!(h.forward("b"), None);
        assert!(h.entries.is_empty());
    }

    #[test]
    fn shrinking_drops_the_oldest_and_keeps_the_position() {
        let mut h = with(&["a", "b", "c", "d"]);
        h.back("d");
        h.set_max(2);
        assert_eq!(h.entries, ["c", "d"]);
        assert_eq!(h.current(), Some("c"));
        let mut h = with(&["a", "b", "c", "d"]);
        h.back("d");
        h.back("c");
        h.back("b");
        h.set_max(2);
        assert_eq!(h.current(), Some("c"));
        h.set_max(0);
        assert!(h.entries.is_empty() && h.current().is_none());
        assert!(!h.record("x"));
    }
}
